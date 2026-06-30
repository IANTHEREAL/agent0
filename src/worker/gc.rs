use crate::cron::config::CronConfig;
use crate::pool::TikvClientPool;
use crate::storage::worker::{GcInstanceState, GcPublishMode};
use crate::storage::TikvStore;
use crate::worker::config::{GcRegistryMode, WorkerConfig};
use crate::worker::executor_lease::WorkerExecutorLeaseCoordinator;
use crate::worker::metrics::WorkerMetrics;
use crate::worker::now_epoch_ms;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use pgwire::tokio::CancellationToken;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tikv_client::{Timestamp, TimestampExt};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

mod hnsw_impl;

/// Timeout for PD TSO requests in GC loops. We route this through the
/// tikv-client timeout API so GC gets a bounded wait on a dedicated TSO stream
/// without disturbing the shared timestamp stream used by foreground traffic.
const TSO_TIMEOUT_SEC: u64 = 30;
/// Total wall-clock bound for GC safepoint updates, including any internal
/// PD-client retries. This keeps the advancer loop from getting wedged on a
/// long retry storm even though individual PD requests already have timeouts.
const SAFEPOINT_UPDATE_TIMEOUT_SEC: u64 = 30;
/// Bound a single publisher attempt so a wedged TiKV/PD call cannot keep a
/// SQL-serving process invisible past the self-fence deadline.
const GC_REGISTRY_PUBLISH_TIMEOUT_SEC: u64 = 30;

pub struct WorkerGc {
    system_store: Arc<TikvStore>,
    #[allow(dead_code)]
    pool: Arc<TikvClientPool>,
    config: WorkerConfig,
    executor_lease: Arc<WorkerExecutorLeaseCoordinator>,
}

pub struct WorkerGcHandles {
    pub gc_loop_handle: JoinHandle<()>,
}

#[derive(Clone)]
pub struct GcRegistryStores {
    legacy: Option<Arc<TikvStore>>,
    new: Option<Arc<TikvStore>>,
}

impl GcRegistryStores {
    pub fn new(legacy: Option<Arc<TikvStore>>, new: Option<Arc<TikvStore>>) -> Self {
        Self { legacy, new }
    }

    fn legacy_store(&self) -> Result<&Arc<TikvStore>> {
        self.legacy
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GC registry mode requires a legacy GC registry store"))
    }

    fn new_store(&self) -> Result<&Arc<TikvStore>> {
        self.new
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GC registry mode requires a new GC registry store"))
    }

    fn clock_store(&self, mode: GcRegistryMode) -> Result<&TikvStore> {
        match mode {
            GcRegistryMode::Legacy => Ok(self.legacy_store()?.as_ref()),
            GcRegistryMode::Migrating => Ok(self.new_store()?.as_ref()),
            GcRegistryMode::New => Ok(self.new_store()?.as_ref()),
        }
    }

    fn publish_targets(&self, mode: GcRegistryMode) -> Result<Vec<(&TikvStore, &'static str)>> {
        match mode {
            GcRegistryMode::Legacy => Ok(vec![(self.legacy_store()?.as_ref(), "legacy")]),
            GcRegistryMode::Migrating => Ok(vec![
                (self.legacy_store()?.as_ref(), "legacy"),
                (self.new_store()?.as_ref(), "new"),
            ]),
            GcRegistryMode::New => Ok(vec![(self.new_store()?.as_ref(), "new")]),
        }
    }

    fn scan_sources(&self, mode: GcRegistryMode) -> Result<Vec<(&TikvStore, &'static str)>> {
        match mode {
            GcRegistryMode::Legacy => Ok(vec![(self.legacy_store()?.as_ref(), "legacy")]),
            GcRegistryMode::Migrating => Ok(vec![
                (self.legacy_store()?.as_ref(), "legacy"),
                (self.new_store()?.as_ref(), "new"),
            ]),
            GcRegistryMode::New => Ok(vec![(self.new_store()?.as_ref(), "new")]),
        }
    }
}

fn gc_publish_mode_for_registry_mode(mode: GcRegistryMode) -> GcPublishMode {
    match mode {
        GcRegistryMode::Legacy => GcPublishMode::OldOnly,
        GcRegistryMode::Migrating => GcPublishMode::DualWrite,
        GcRegistryMode::New => GcPublishMode::NewPrimary,
    }
}

struct ClaimGcBatch {
    scanned: usize,
    cleaned: u32,
    last_key: Option<Vec<u8>>,
}

impl WorkerGc {
    pub fn new_with_executor_lease(
        system_store: Arc<TikvStore>,
        pool: Arc<TikvClientPool>,
        config: WorkerConfig,
        executor_lease: Arc<WorkerExecutorLeaseCoordinator>,
    ) -> Self {
        Self {
            system_store,
            pool,
            config,
            executor_lease,
        }
    }

    /// Spawn worker-only GC loops (orphan-claim cleanup).
    /// Publisher and advancer are spawned separately at the top level of main.rs.
    pub fn spawn_worker_gc_only(self: Arc<Self>) -> WorkerGcHandles {
        let gc_loop_handle = tokio::spawn(async move { self.run_gc_loop().await });
        WorkerGcHandles { gc_loop_handle }
    }

    async fn run_gc_loop(&self) {
        let jitter = rand_jitter_secs(60);
        tokio::time::sleep(Duration::from_secs(jitter)).await;

        let mut interval = tokio::time::interval(Duration::from_secs(self.config.gc_interval_sec));
        loop {
            interval.tick().await;
            if let Err(e) = self.gc_tick().await {
                warn!("Worker GC tick error: {}", e);
            }
        }
    }
}

// ============================================================================
// Free functions: GC publisher + advancer, spawned at top level of main.rs.
// These are NOT methods on WorkerGc — they live outside any config-gated block.
// ============================================================================

/// GC registry publisher loop — UNCONDITIONAL for every SQL-serving process.
/// Publishes this instance's min_start_ts to the configured GC registry target(s).
pub async fn run_gc_publisher_loop(
    registry: &GcRegistryStores,
    config: &WorkerConfig,
    self_fence: CancellationToken,
    last_successful_publish: Arc<Mutex<Instant>>,
) {
    info!(
        interval_sec = config.gc_safepoint_interval_sec,
        mode = config.gc_registry_mode.as_str(),
        "GC registry publisher started (unconditional)"
    );

    let mut interval = tokio::time::interval(Duration::from_secs(config.gc_safepoint_interval_sec));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // No skip-first-tick — the loop must publish immediately on both startup
    // and restart.  On startup the synchronous publish already ran, so the
    // first tick is a harmless redundant publish.  On restart (after panic +
    // cooldown) publishing immediately closes the blind window.
    let max_retry_delay = Duration::from_secs(config.gc_safepoint_interval_sec / 2);
    loop {
        interval.tick().await;
        match publish_gc_liveness_once_with_timeout(registry, config).await {
            Ok(current_version) => {
                record_gc_liveness_publish_success(&last_successful_publish);
                metrics::counter!("db9_server_gc_liveness_publish_ok").increment(1);
                reap_stale_gc_instance_states_after_publish(registry, config, current_version)
                    .await;
            }
            Err(e) => {
                metrics::counter!("db9_server_gc_liveness_publish_err").increment(1);
                warn!("GC registry publish failed: {e}");
                let last_success = read_gc_liveness_last_success(&last_successful_publish);
                if gc_liveness_self_fence_elapsed(last_success, config) {
                    trigger_gc_liveness_self_fence(&self_fence, last_success, config, &e);
                    park_gc_publisher_after_self_fence().await;
                }
                // Retry with exponential backoff instead of waiting a full interval.
                // A single missed heartbeat at edge configs could make our row stale,
                // so we retry promptly to keep the heartbeat alive.
                let mut backoff = Duration::from_secs(1);
                loop {
                    tokio::time::sleep(backoff).await;
                    match publish_gc_liveness_once_with_timeout(registry, config).await {
                        Ok(current_version) => {
                            record_gc_liveness_publish_success(&last_successful_publish);
                            metrics::counter!("db9_server_gc_liveness_publish_ok").increment(1);
                            reap_stale_gc_instance_states_after_publish(
                                registry,
                                config,
                                current_version,
                            )
                            .await;
                            info!("GC registry publish succeeded after retry");
                            break;
                        }
                        Err(retry_err) => {
                            metrics::counter!("db9_server_gc_liveness_publish_err").increment(1);
                            warn!(
                                "GC registry publish retry failed (backoff={backoff:?}): {retry_err}"
                            );
                            let last_success =
                                read_gc_liveness_last_success(&last_successful_publish);
                            if gc_liveness_self_fence_elapsed(last_success, config) {
                                trigger_gc_liveness_self_fence(
                                    &self_fence,
                                    last_success,
                                    config,
                                    &retry_err,
                                );
                                park_gc_publisher_after_self_fence().await;
                            }
                            backoff = (backoff * 2).min(max_retry_delay);
                            if backoff >= max_retry_delay {
                                warn!(
                                    "GC registry publish retries exhausted; will retry on next tick"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn read_gc_liveness_last_success(clock: &Arc<Mutex<Instant>>) -> Instant {
    *clock.lock()
}

fn record_gc_liveness_publish_success(clock: &Arc<Mutex<Instant>>) {
    *clock.lock() = Instant::now();
}

async fn publish_gc_liveness_once_with_timeout(
    registry: &GcRegistryStores,
    config: &WorkerConfig,
) -> Result<u64> {
    tokio::time::timeout(
        Duration::from_secs(GC_REGISTRY_PUBLISH_TIMEOUT_SEC),
        publish_gc_instance_state(registry, config),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "GC registry publish timed out after {}s",
            GC_REGISTRY_PUBLISH_TIMEOUT_SEC
        )
    })?
}

async fn reap_stale_gc_instance_states_after_publish(
    registry: &GcRegistryStores,
    config: &WorkerConfig,
    current_version: u64,
) {
    if config.gc_safepoint_enabled {
        return;
    }

    let sources = match registry.scan_sources(config.gc_registry_mode) {
        Ok(sources) => sources,
        Err(e) => {
            warn!("GC registry stale-state reap source selection failed after publish: {e}");
            return;
        }
    };

    for (store, source) in sources {
        match tokio::time::timeout(
            Duration::from_secs(GC_REGISTRY_PUBLISH_TIMEOUT_SEC),
            reap_stale_gc_instance_states(
                store,
                current_version,
                heartbeat_timeout_sec(config),
                source,
            ),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!(
                    source,
                    "GC registry stale-state reap failed after publish: {e}"
                );
            }
            Err(_) => {
                warn!(
                    source,
                    timeout_sec = GC_REGISTRY_PUBLISH_TIMEOUT_SEC,
                    "GC registry stale-state reap timed out after publish"
                );
            }
        }
    }
}

pub(crate) fn gc_liveness_self_fence_after(config: &WorkerConfig) -> Duration {
    Duration::from_secs(config.gc_safepoint_interval_sec)
}

fn gc_liveness_self_fence_elapsed(last_successful_publish: Instant, config: &WorkerConfig) -> bool {
    last_successful_publish.elapsed() >= gc_liveness_self_fence_after(config)
}

fn trigger_gc_liveness_self_fence(
    self_fence: &CancellationToken,
    last_successful_publish: Instant,
    config: &WorkerConfig,
    err: &anyhow::Error,
) {
    if !self_fence.is_cancelled() {
        let stale_for_ms = last_successful_publish.elapsed().as_millis() as u64;
        error!(
            stale_for_ms,
            self_fence_after_ms = gc_liveness_self_fence_after(config).as_millis() as u64,
            error = %err,
            "GC liveness publish failed past self-fence deadline; stopping SQL admission"
        );
        metrics::gauge!("db9_server_gc_self_fenced").set(1.0);
        self_fence.cancel();
    }
}

async fn park_gc_publisher_after_self_fence() {
    // Keep this task parked until main's shutdown path aborts it. Returning
    // would make the supervisor restart the publisher while the process is
    // intentionally self-fenced.
    std::future::pending::<()>().await;
}

/// Publish this process's GC registry row once.
///
/// Startup uses this synchronously before the SQL listener begins accepting
/// connections so the process participates in cluster GC coordination from the
/// first served transaction.
pub async fn publish_gc_instance_state_once(
    registry: &GcRegistryStores,
    config: &WorkerConfig,
) -> Result<()> {
    let current_version = publish_gc_instance_state(registry, config).await?;
    if !config.gc_safepoint_enabled {
        for (store, source) in registry.scan_sources(config.gc_registry_mode)? {
            reap_stale_gc_instance_states(
                store,
                current_version,
                heartbeat_timeout_sec(config),
                source,
            )
            .await?;
        }
    }
    Ok(())
}

/// GC safepoint advancer loop — OPTIONAL, controlled by gc_safepoint_enabled.
/// Reads all instances' states, computes global min, advances PD safepoint.
pub async fn run_gc_advancer_loop(
    registry: &GcRegistryStores,
    config: &WorkerConfig,
    metrics: &WorkerMetrics,
) {
    info!(
        interval_sec = config.gc_safepoint_interval_sec,
        life_time_sec = config.gc_life_time_sec,
        mode = config.gc_registry_mode.as_str(),
        "GC safepoint advancer started"
    );

    let mut interval = tokio::time::interval(Duration::from_secs(config.gc_safepoint_interval_sec));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(e) = advance_gc_safepoint(registry, config, metrics).await {
            warn!("TiKV GC safepoint advance failed: {}", e);
            metrics
                .gc_safepoint_advance_err
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            metrics::counter!("db9_server_gc_safepoint_advance_total", "result" => "err")
                .increment(1);
        }
    }
}

/// Remove this process's GC registry row during graceful shutdown.
///
/// First publishes `min_start_ts = None` so the row stops clamping the
/// cluster safepoint even if the subsequent delete fails (transient TiKV
/// error, timeout).  A leaked row with `None` is harmless — the advancer
/// skips it during safepoint computation and the stale-row reaper will
/// eventually delete it.
pub async fn clear_gc_instance_state(
    registry: &GcRegistryStores,
    config: &WorkerConfig,
) -> Result<()> {
    // Phase 1: neutralize — publish None so the row cannot pin GC.
    let neutralize_result: Result<()> = async {
        let clock_store = registry.clock_store(config.gc_registry_mode)?;
        let client = clock_store
            .transaction_client()
            .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;
        let current_ts = client
            .current_timestamp_with_timeout(Duration::from_secs(TSO_TIMEOUT_SEC))
            .await
            .map_err(|e| anyhow::anyhow!("failed to get shutdown timestamp from PD: {}", e))?;
        let publish_mode = gc_publish_mode_for_registry_mode(config.gc_registry_mode);
        for (store, _) in registry.publish_targets(config.gc_registry_mode)? {
            let mut txn = store.begin().await?;
            store
                .put_gc_instance_state(
                    &mut txn,
                    &config.gc_instance_id,
                    None, // no min_start_ts — row cannot clamp safepoint
                    current_ts.version(),
                    publish_mode,
                )
                .await?;
            txn.commit().await?;
        }
        Ok(())
    }
    .await;

    if let Err(e) = &neutralize_result {
        warn!(
            "Failed to neutralize GC registry row during shutdown (will still attempt delete): {}",
            e
        );
    }

    // Phase 2: delete — best-effort removal of the row.
    for (store, _) in registry.publish_targets(config.gc_registry_mode)? {
        let mut txn = store.begin().await?;
        store
            .delete_gc_instance_state(&mut txn, &config.gc_instance_id)
            .await?;
        txn.commit().await?;
    }
    Ok(())
}

async fn publish_gc_instance_state(
    registry: &GcRegistryStores,
    config: &WorkerConfig,
) -> Result<u64> {
    let clock_store = registry.clock_store(config.gc_registry_mode)?;
    let client = clock_store
        .transaction_client()
        .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;

    let current_ts = client
        .current_timestamp_with_timeout(Duration::from_secs(TSO_TIMEOUT_SEC))
        .await
        .map_err(|e| anyhow::anyhow!("failed to get current timestamp from PD: {}", e))?;

    // Reap quarantined worker entries whose TTL has expired before reading
    // min_start_ts.  Practical hold time is max(QUARANTINE_TTL, this tick
    // interval) — acceptable since both are far shorter than gc_life_time.
    if let Some(registry) = crate::worker::active_txn_registry::global_registry() {
        registry.reap_quarantined();
    }

    let local_min =
        crate::worker::active_txn_registry::global_registry().and_then(|r| r.min_start_ts());
    let publish_mode = gc_publish_mode_for_registry_mode(config.gc_registry_mode);

    for (store, source) in registry.publish_targets(config.gc_registry_mode)? {
        let mut txn = store.begin().await?;
        store
            .put_gc_instance_state(
                &mut txn,
                &config.gc_instance_id,
                local_min,
                current_ts.version(),
                publish_mode,
            )
            .await?;
        txn.commit().await?;
        metrics::counter!("db9_server_gc_registry_publish_total", "source" => source).increment(1);
    }
    Ok(current_ts.version())
}

async fn scan_gc_instance_states(store: &TikvStore) -> Result<Vec<GcInstanceState>> {
    let mut txn = store.begin().await?;
    let result = store.scan_gc_instance_states(&mut txn).await;
    txn.rollback().await.ok();
    result
}

async fn scan_gc_instance_states_from_sources(
    registry: &GcRegistryStores,
    mode: GcRegistryMode,
) -> Result<Vec<GcInstanceState>> {
    let mut all_states = Vec::new();
    for (store, source) in registry.scan_sources(mode)? {
        let states = scan_gc_instance_states(store)
            .await
            .with_context(|| format!("failed to read {source} GC registry source"))?;
        metrics::gauge!("db9_server_gc_safepoint_sources_read", "source" => source).set(1.0);
        all_states.extend(states);
    }
    Ok(all_states)
}

async fn advance_gc_safepoint(
    registry: &GcRegistryStores,
    config: &WorkerConfig,
    metrics: &WorkerMetrics,
) -> Result<()> {
    let clock_store = registry.clock_store(config.gc_registry_mode)?;
    let client = clock_store
        .transaction_client()
        .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;

    let current_ts = client
        .current_timestamp_with_timeout(Duration::from_secs(TSO_TIMEOUT_SEC))
        .await
        .map_err(|e| anyhow::anyhow!("failed to get current timestamp from PD: {}", e))?;

    let current_version = current_ts.version();

    // Read ALL instances' states from shared registry.
    let time_based_safepoint = compute_safepoint_version(current_version, config.gc_life_time_sec);

    let all_states =
        scan_gc_instance_states_from_sources(registry, config.gc_registry_mode).await?;

    let hb_timeout = heartbeat_timeout_sec(config);

    let old_only_publishers = live_old_only_gc_publishers(current_version, hb_timeout, &all_states);
    metrics::gauge!("db9_server_gc_old_only_publishers").set(old_only_publishers.len() as f64);
    if config.gc_registry_mode == GcRegistryMode::Migrating && !old_only_publishers.is_empty() {
        debug!(
            old_only_publishers = old_only_publishers.len(),
            "GC registry new-only gate remains blocked by live old-only publishers"
        );
    }

    let effective_life_time_sec = effective_cluster_gc_life_time_sec(
        current_version,
        config.gc_life_time_sec,
        hb_timeout,
        &all_states,
    );
    if effective_life_time_sec > config.gc_life_time_sec {
        info!(
            local_life_time = config.gc_life_time_sec,
            cluster_legacy_timeout_floor = effective_life_time_sec,
            "GC life_time raised to cover legacy mixed-version worker timeout floor"
        );
    }

    let safepoint_version = compute_cluster_gc_safepoint(
        current_version,
        config.gc_life_time_sec,
        hb_timeout,
        &all_states,
    );

    for state in &all_states {
        if !is_live_gc_instance_state(current_version, hb_timeout, state) {
            debug!(
                instance_id = state.instance_id,
                updated_at = state.updated_at_version,
                "Ignoring stale GC instance state"
            );
            continue;
        }
        if let Some(ts) = state.min_start_ts {
            let txn_floor = ts.saturating_sub(1);
            if txn_floor == safepoint_version && txn_floor < time_based_safepoint {
                info!(
                    instance_id = state.instance_id,
                    min_active_start_ts = ts,
                    time_based = time_based_safepoint,
                    clamped_to = txn_floor,
                    "GC safepoint clamped by active transaction on instance"
                );
            }
        }
    }

    for (store, source) in registry.scan_sources(config.gc_registry_mode)? {
        match scan_gc_instance_states(store).await {
            Ok(states) => {
                if let Err(e) = reap_stale_gc_instance_states_from_scan(
                    store,
                    current_version,
                    hb_timeout,
                    &states,
                    source,
                )
                .await
                {
                    warn!("GC registry stale-state reap failed: {}", e);
                }
            }
            Err(e) => {
                warn!(
                    source,
                    "GC registry stale-state reap scan failed after safepoint source read: {}", e
                );
            }
        }
    }

    if safepoint_version == 0 {
        info!("GC safepoint would be 0; skipping (cluster just started?)");
        return Ok(());
    }

    let safepoint = Timestamp::from_version(safepoint_version);

    // Only advance safepoint in PD — no lock resolution.
    // PD takes max(current, proposed) so this is idempotent and safe
    // to call from multiple db9 instances concurrently.
    // update_safepoint returns true if PD accepted our exact proposal,
    // false if PD already had a higher value (another instance advanced it).
    match tokio::time::timeout(
        Duration::from_secs(SAFEPOINT_UPDATE_TIMEOUT_SEC),
        client.update_safepoint(safepoint),
    )
    .await
    {
        Ok(Ok(accepted)) => {
            if accepted {
                metrics
                    .gc_safepoint_last_version
                    .store(safepoint_version, std::sync::atomic::Ordering::Relaxed);
                metrics
                    .gc_safepoint_advance_ok
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                metrics::gauge!("db9_server_gc_safepoint_version").set(safepoint_version as f64);
                metrics::counter!("db9_server_gc_safepoint_advance_total", "result" => "ok")
                    .increment(1);
                info!(
                    safepoint_version,
                    life_time_sec = config.gc_life_time_sec,
                    "TiKV GC safepoint advanced by this instance"
                );
            } else {
                debug!(
                    safepoint_version,
                    "TiKV GC safepoint proposal accepted, PD already at higher value"
                );
            }
            Ok(())
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("update_safepoint failed: {}", e)),
        Err(_) => Err(anyhow::anyhow!(
            "update_safepoint timed out after {}s",
            SAFEPOINT_UPDATE_TIMEOUT_SEC
        )),
    }
}

impl WorkerGc {
    async fn gc_tick(&self) -> Result<()> {
        if !self.executor_lease.ensure_current_executor().await {
            return Ok(());
        }
        self.cleanup_hnsw_s3_external_object_intents().await?;
        self.cleanup_orphan_claims().await?;
        Ok(())
    }

    /// Scan all claims and delete those whose LEASE has expired. A live
    /// (renewed) lease is never reaped, so a long-running BgSql/BgDdl task that
    /// keeps renewing its claim cannot be reaped mid-flight and double-executed.
    /// Orphaned claims are NOT re-enqueued — the next cron fire or scheduler
    /// handles retries. One-shot tasks stay failed.
    ///
    /// Legacy `claimed_at`-only rows (from a pre-lease binary, mixed-version
    /// window) synthesize their lease as `claimed_at + orphan_timeout`, so this
    /// reaper and an old age-based reaper agree until the fleet upgrades.
    async fn cleanup_orphan_claims(&self) -> Result<()> {
        let batch_size = self.config.gc_batch_size.max(1);
        let now_ms = now_epoch_ms();
        let legacy_orphan_timeout_ms = (self.config.orphan_timeout_sec as i64).saturating_mul(1000);

        let (cleaned, _) = run_claim_gc_batches(batch_size, |start_after, requested_batch_size| {
            self.cleanup_orphan_claims_batch(
                start_after,
                requested_batch_size,
                now_ms,
                legacy_orphan_timeout_ms,
            )
        })
        .await?;

        if cleaned > 0 {
            info!("GC: cleaned {} orphan claims", cleaned);
        }

        Ok(())
    }

    async fn cleanup_orphan_claims_batch(
        &self,
        start_after: Option<Vec<u8>>,
        batch_size: usize,
        now_ms: i64,
        legacy_orphan_timeout_ms: i64,
    ) -> Result<ClaimGcBatch> {
        let mut txn = self.system_store.begin().await?;

        let batch_result = async {
            let claims = self
                .system_store
                .list_worker_claims_batch(&mut txn, start_after.as_deref(), Some(batch_size))
                .await?;
            let scanned = claims.len();
            let last_key = claims.last().map(|(key, _)| key.clone());
            let mut cleaned = 0u32;

            for (key, claim) in claims {
                if claim.is_expired(now_ms, legacy_orphan_timeout_ms) {
                    self.system_store
                        .delete_worker_claim_by_raw_key(&mut txn, &key)
                        .await?;
                    cleaned += 1;
                    warn!(
                        "GC: cleaned expired-lease claim worker={} type={:?} claimed_at={} lease_until={}",
                        claim.worker_id,
                        claim.task_type,
                        claim.claimed_at,
                        claim.effective_lease_until(legacy_orphan_timeout_ms)
                    );
                }
            }

            Ok::<ClaimGcBatch, anyhow::Error>(ClaimGcBatch {
                scanned,
                cleaned,
                last_key,
            })
        }
        .await;

        match batch_result {
            Ok(batch) => {
                if batch.cleaned > 0 {
                    txn.commit().await?;
                } else {
                    txn.rollback().await.ok();
                }
                Ok(batch)
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e)
            }
        }
    }
}

async fn run_claim_gc_batches<F, Fut>(batch_size: usize, mut run_batch: F) -> Result<(u32, usize)>
where
    F: FnMut(Option<Vec<u8>>, usize) -> Fut,
    Fut: Future<Output = Result<ClaimGcBatch>>,
{
    let batch_size = batch_size.max(1).min(u32::MAX as usize);
    let mut total_cleaned = 0u32;
    let mut batch_count = 0usize;
    let mut start_after: Option<Vec<u8>> = None;

    loop {
        let batch = run_batch(start_after.clone(), batch_size).await?;
        if batch.scanned == 0 {
            break;
        }

        batch_count += 1;
        total_cleaned = total_cleaned.saturating_add(batch.cleaned);
        start_after = batch.last_key;

        if batch.scanned < batch_size {
            break;
        }
    }

    Ok((total_cleaned, batch_count))
}

/// Compute the GC safepoint version from the current TSO version and retention window.
///
/// TSO version layout: `physical_ms << 18 | logical`.
/// Returns 0 if the subtraction would underflow (cluster just started).
pub(crate) fn compute_safepoint_version(current_version: u64, life_time_sec: u64) -> u64 {
    let life_time_ms = life_time_sec * 1000;
    current_version.saturating_sub(life_time_ms << 18)
}

/// Multiplier applied to gc_safepoint_interval_sec to derive the instance
/// heartbeat timeout.  At 3x, an instance survives 2 consecutive missed
/// heartbeats before being declared dead.
///
/// The heartbeat timeout MUST be:
///   publish_interval < heartbeat_timeout < gc_life_time
/// This is guaranteed by `WorkerConfig::validate_gc_config()` which enforces
/// gc_life_time >= 3 * gc_safepoint_interval.
const HEARTBEAT_TIMEOUT_MULTIPLIER: u64 = 3;

/// Compute the heartbeat timeout from the publish interval.
/// This is intentionally much shorter than gc_life_time_sec (24h) — using
/// gc_life_time_sec would keep a crashed instance's min_start_ts in the
/// safepoint calculation for up to 24 hours, stalling TiKV GC cluster-wide.
pub(crate) fn heartbeat_timeout_sec(config: &WorkerConfig) -> u64 {
    config
        .gc_safepoint_interval_sec
        .saturating_mul(HEARTBEAT_TIMEOUT_MULTIPLIER)
}

pub(crate) fn is_live_gc_instance_state(
    current_version: u64,
    heartbeat_timeout_sec: u64,
    state: &GcInstanceState,
) -> bool {
    state.updated_at_version >= compute_safepoint_version(current_version, heartbeat_timeout_sec)
}

pub(crate) fn effective_cluster_gc_life_time_sec(
    current_version: u64,
    life_time_sec: u64,
    heartbeat_timeout: u64,
    states: &[GcInstanceState],
) -> u64 {
    // Mixed-version rollout compatibility: legacy rows may still carry the
    // timeout-derived floor that old nodes rely on for untracked worker txns.
    let mut effective_life_time_sec = life_time_sec;
    for state in states {
        if !is_live_gc_instance_state(current_version, heartbeat_timeout, state) {
            continue;
        }
        if let Some(legacy_timeout_sec) = state.legacy_max_untracked_timeout_sec {
            effective_life_time_sec = effective_life_time_sec.max(legacy_timeout_sec);
        }
    }
    effective_life_time_sec
}

fn stale_gc_instance_ids(
    current_version: u64,
    heartbeat_timeout: u64,
    states: &[GcInstanceState],
) -> Vec<String> {
    states
        .iter()
        .filter(|state| !is_live_gc_instance_state(current_version, heartbeat_timeout, state))
        .map(|state| state.instance_id.clone())
        .collect()
}

pub(crate) fn live_old_only_gc_publishers(
    current_version: u64,
    heartbeat_timeout: u64,
    states: &[GcInstanceState],
) -> Vec<String> {
    states
        .iter()
        .filter(|state| {
            is_live_gc_instance_state(current_version, heartbeat_timeout, state)
                && state.publish_mode == GcPublishMode::OldOnly
        })
        .map(|state| state.instance_id.clone())
        .collect()
}

pub(crate) fn compute_cluster_gc_safepoint(
    current_version: u64,
    life_time_sec: u64,
    heartbeat_timeout: u64,
    states: &[GcInstanceState],
) -> u64 {
    let effective_life_time_sec = effective_cluster_gc_life_time_sec(
        current_version,
        life_time_sec,
        heartbeat_timeout,
        states,
    );
    let mut safepoint = compute_safepoint_version(current_version, effective_life_time_sec);
    for state in states {
        if !is_live_gc_instance_state(current_version, heartbeat_timeout, state) {
            continue;
        }
        if let Some(ts) = state.min_start_ts {
            safepoint = safepoint.min(ts.saturating_sub(1));
        }
    }
    safepoint
}

async fn reap_stale_gc_instance_states(
    store: &TikvStore,
    current_version: u64,
    heartbeat_timeout: u64,
    trigger: &'static str,
) -> Result<usize> {
    let all_states = {
        let mut txn = store.begin().await?;
        let states = store.scan_gc_instance_states(&mut txn).await?;
        txn.rollback().await.ok();
        states
    };
    reap_stale_gc_instance_states_from_scan(
        store,
        current_version,
        heartbeat_timeout,
        &all_states,
        trigger,
    )
    .await
}

async fn reap_stale_gc_instance_states_from_scan(
    store: &TikvStore,
    current_version: u64,
    heartbeat_timeout: u64,
    states: &[GcInstanceState],
    trigger: &'static str,
) -> Result<usize> {
    let stale_ids = stale_gc_instance_ids(current_version, heartbeat_timeout, states);
    if stale_ids.is_empty() {
        return Ok(0);
    }

    let mut txn = store.begin().await?;
    let mut deleted = 0usize;
    for instance_id in &stale_ids {
        let Some(current_state) = store
            .get_gc_instance_state_for_update(&mut txn, instance_id)
            .await?
        else {
            continue;
        };
        if is_live_gc_instance_state(current_version, heartbeat_timeout, &current_state) {
            continue;
        }
        store
            .delete_gc_instance_state(&mut txn, instance_id)
            .await?;
        deleted += 1;
    }
    if deleted > 0 {
        txn.commit().await?;
    } else {
        txn.rollback().await.ok();
    }

    info!(
        trigger,
        deleted, "GC registry reaped stale instance state rows"
    );
    Ok(deleted)
}

/// Generate random jitter in seconds (0..max_secs) using time-based seed.
fn rand_jitter_secs(max_secs: u64) -> u64 {
    use std::time::SystemTime;
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (seed % max_secs as u128) as u64
}

/// SINGLE SOURCE OF TRUTH for the cron control-plane "effective orphan floor".
///
/// A cron run's frozen CONTROL/ACTIVE deadline must cover the LONGEST a
/// legitimate run can take. With no per-job `max_runtime_ms`, that legitimate
/// window is the EXECUTION timeout `cron_job_timeout_ms` (claim_and_execute_core
/// uses `max_runtime_ms.unwrap_or(cron_job_timeout_ms)`), NOT the bare control
/// `orphan_timeout_sec`. If the frozen deadline were only `now + orphan_timeout`
/// (default 5 min) while the run may legitimately execute up to `cron_job_timeout`
/// (default 30 min), a later fire would see the still-running ACTIVE as expired,
/// mint a fresh fence, and take over — violating per-job no-overlap. So the floor
/// is `max(orphan_timeout, cron_job_timeout)`; the per-claim path then takes a
/// further `max` with `job.max_runtime_ms` via `cron_effective_orphan_deadline_ms`.
///
/// EVERY site that derives a cron control/orphan floor — the per-claim path
/// (`claim_and_record_cron_run`), the bulk migration (`ensure_cron_control_migrated`),
/// the straggler fold, and the GC reaper view (`effective_cron_orphan_timeout_sec`)
/// — MUST route through this one function so the formula cannot drift (a second
/// inline copy is exactly the class that cost prior rounds).
pub(crate) fn effective_cron_orphan_floor_ms(
    orphan_timeout_sec: u64,
    cron_job_timeout_ms: u64,
) -> i64 {
    let orphan_timeout_ms = orphan_timeout_sec.saturating_mul(1000);
    // The default-job (no per-job `max_runtime_ms`) execution window, taken through
    // the ONE source the executor and the per-claim deadline also use, so the floor
    // cannot drift from the window a default run actually executes for.
    let default_window_ms =
        crate::storage::cron::cron_execution_window_ms(None, cron_job_timeout_ms);
    let floor = orphan_timeout_ms.max(default_window_ms);
    i64::try_from(floor).unwrap_or(i64::MAX)
}

pub(crate) fn effective_cron_orphan_timeout_sec(
    cron_config: &CronConfig,
    worker_config: &WorkerConfig,
) -> u64 {
    let floor_ms = effective_cron_orphan_floor_ms(
        cron_config.orphan_timeout_sec,
        worker_config.cron_job_timeout_ms,
    );
    // Ceil-div back to whole seconds so the seconds view is never SHORTER than
    // the canonical ms floor.
    (floor_ms.max(0) as u64).saturating_add(999) / 1000
}

mod hnsw_helpers;
use hnsw_helpers::*;

#[cfg(test)]
mod tests;
