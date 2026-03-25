use crate::cron::config::CronConfig;
use crate::cron::worker::gc_database;
use crate::pool::TikvClientPool;
use crate::storage::worker::GcInstanceState;
use crate::storage::TikvStore;
use crate::worker::config::WorkerConfig;
use crate::worker::metrics::WorkerMetrics;
use crate::worker::now_epoch_ms;
use anyhow::Result;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tikv_client::{Timestamp, TimestampExt};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

mod hnsw_impl;

/// Timeout for PD TSO requests in GC loops. We route this through the
/// tikv-client timeout API so GC gets a bounded wait on a dedicated TSO stream
/// without disturbing the shared timestamp stream used by foreground traffic.
const TSO_TIMEOUT_SEC: u64 = 30;
/// Total wall-clock bound for GC safepoint updates, including any internal
/// PD-client retries. This keeps the advancer loop from getting wedged on a
/// long retry storm even though individual PD requests already have timeouts.
const SAFEPOINT_UPDATE_TIMEOUT_SEC: u64 = 30;

pub struct WorkerGc {
    system_store: Arc<TikvStore>,
    pool: Arc<TikvClientPool>,
    config: WorkerConfig,
    metrics: Arc<WorkerMetrics>,
}

pub struct WorkerGcHandles {
    pub gc_loop_handle: JoinHandle<()>,
    pub hnsw_sweep_handle: JoinHandle<()>,
}

struct ClaimGcBatch {
    scanned: usize,
    cleaned: u32,
    last_key: Option<Vec<u8>>,
}

impl WorkerGc {
    pub fn new(
        system_store: Arc<TikvStore>,
        pool: Arc<TikvClientPool>,
        config: WorkerConfig,
        metrics: Arc<WorkerMetrics>,
    ) -> Self {
        Self {
            system_store,
            pool,
            config,
            metrics,
        }
    }

    /// Spawn worker-only GC loops (orphan claims + cron cleanup + HNSW sweep).
    /// Publisher and advancer are spawned separately at the top level of main.rs.
    pub fn spawn_worker_gc_only(self: Arc<Self>) -> WorkerGcHandles {
        let gc_self = self.clone();
        let gc_loop_handle = tokio::spawn(async move { gc_self.run_gc_loop().await });
        let hnsw_sweep_handle = tokio::spawn(async move { self.run_hnsw_sweep_loop().await });
        WorkerGcHandles {
            gc_loop_handle,
            hnsw_sweep_handle,
        }
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

    async fn run_hnsw_sweep_loop(&self) {
        let jitter = rand_jitter_secs(60);
        tokio::time::sleep(Duration::from_secs(jitter)).await;

        let mut interval =
            tokio::time::interval(Duration::from_secs(self.config.hnsw_sweep_interval_sec));
        loop {
            interval.tick().await;
            if let Err(e) = self.sweep_hnsw_delta_backlogs().await {
                warn!("HNSW sweep error: {}", e);
            }
            // S3 orphan sweep: only run if S3 offload is configured.
            if crate::sql::hnsw::s3::hnsw_s3_client().is_some() {
                if let Err(e) = self.sweep_hnsw_s3_orphans().await {
                    warn!("HNSW S3 sweep error: {}", e);
                }
            }
        }
    }
}

// ============================================================================
// Free functions: GC publisher + advancer, spawned at top level of main.rs.
// These are NOT methods on WorkerGc — they live outside any config-gated block.
// ============================================================================

/// GC registry publisher loop — UNCONDITIONAL for every SQL-serving process.
/// Publishes this instance's min_start_ts to `_sys_worker` every interval.
pub async fn run_gc_publisher_loop(store: &TikvStore, config: &WorkerConfig) {
    info!(
        interval_sec = config.gc_safepoint_interval_sec,
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
        if let Err(e) = publish_gc_instance_state_once(store, config).await {
            warn!("GC registry publish failed: {e}");
            // Retry with exponential backoff instead of waiting a full interval.
            // A single missed heartbeat at edge configs could make our row stale,
            // so we retry promptly to keep the heartbeat alive.
            let mut backoff = Duration::from_secs(1);
            loop {
                tokio::time::sleep(backoff).await;
                match publish_gc_instance_state_once(store, config).await {
                    Ok(()) => {
                        info!("GC registry publish succeeded after retry");
                        break;
                    }
                    Err(retry_err) => {
                        warn!(
                            "GC registry publish retry failed (backoff={backoff:?}): {retry_err}"
                        );
                        backoff = (backoff * 2).min(max_retry_delay);
                        if backoff >= max_retry_delay {
                            warn!("GC registry publish retries exhausted; will retry on next tick");
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Publish this process's GC registry row once.
///
/// Startup uses this synchronously before the SQL listener begins accepting
/// connections so the process participates in cluster GC coordination from the
/// first served transaction.
pub async fn publish_gc_instance_state_once(
    store: &TikvStore,
    config: &WorkerConfig,
) -> Result<()> {
    let current_version = publish_gc_instance_state(store, config).await?;
    if !config.gc_safepoint_enabled {
        reap_stale_gc_instance_states(
            store,
            current_version,
            heartbeat_timeout_sec(config),
            "publisher",
        )
        .await?;
    }
    Ok(())
}

/// GC safepoint advancer loop — OPTIONAL, controlled by gc_safepoint_enabled.
/// Reads all instances' states, computes global min, advances PD safepoint.
pub async fn run_gc_advancer_loop(
    store: &TikvStore,
    config: &WorkerConfig,
    metrics: &WorkerMetrics,
) {
    info!(
        interval_sec = config.gc_safepoint_interval_sec,
        life_time_sec = config.gc_life_time_sec,
        "GC safepoint advancer started"
    );

    let mut interval = tokio::time::interval(Duration::from_secs(config.gc_safepoint_interval_sec));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(e) = advance_gc_safepoint(store, config, metrics).await {
            warn!("TiKV GC safepoint advance failed: {}", e);
            metrics
                .gc_safepoint_advance_err
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
pub async fn clear_gc_instance_state(store: &TikvStore, config: &WorkerConfig) -> Result<()> {
    // Phase 1: neutralize — publish None so the row cannot pin GC.
    let neutralize_result: Result<()> = async {
        let client = store
            .transaction_client()
            .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;
        let current_ts = client
            .current_timestamp_with_timeout(Duration::from_secs(TSO_TIMEOUT_SEC))
            .await
            .map_err(|e| anyhow::anyhow!("failed to get shutdown timestamp from PD: {}", e))?;
        let mut txn = store.begin().await?;
        store
            .put_gc_instance_state(
                &mut txn,
                &config.gc_instance_id,
                None, // no min_start_ts — row cannot clamp safepoint
                current_ts.version(),
            )
            .await?;
        txn.commit().await?;
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
    let mut txn = store.begin().await?;
    store
        .delete_gc_instance_state(&mut txn, &config.gc_instance_id)
        .await?;
    txn.commit().await?;
    Ok(())
}

async fn publish_gc_instance_state(store: &TikvStore, config: &WorkerConfig) -> Result<u64> {
    let client = store
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

    let mut txn = store.begin().await?;
    store
        .put_gc_instance_state(
            &mut txn,
            &config.gc_instance_id,
            local_min,
            current_ts.version(),
        )
        .await?;
    txn.commit().await?;
    Ok(current_ts.version())
}

async fn advance_gc_safepoint(
    store: &TikvStore,
    config: &WorkerConfig,
    metrics: &WorkerMetrics,
) -> Result<()> {
    let client = store
        .transaction_client()
        .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;

    let current_ts = client
        .current_timestamp_with_timeout(Duration::from_secs(TSO_TIMEOUT_SEC))
        .await
        .map_err(|e| anyhow::anyhow!("failed to get current timestamp from PD: {}", e))?;

    let current_version = current_ts.version();

    // Read ALL instances' states from shared registry.
    let time_based_safepoint = compute_safepoint_version(current_version, config.gc_life_time_sec);

    let all_states = {
        let mut txn = store.begin().await?;
        let states = store.scan_gc_instance_states(&mut txn).await?;
        txn.rollback().await.ok();
        states
    };

    let hb_timeout = heartbeat_timeout_sec(config);

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

    if let Err(e) = reap_stale_gc_instance_states_from_scan(
        store,
        current_version,
        hb_timeout,
        &all_states,
        "advancer",
    )
    .await
    {
        warn!("GC registry stale-state reap failed: {}", e);
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
        self.cleanup_orphan_claims().await?;
        self.cleanup_cron_runs().await?;
        Ok(())
    }

    /// Periodic sweep: discover HNSW indexes with pending deltas and enqueue
    /// merge tasks. Uses the same shared helper as startup reconciliation.
    /// Configurable via `DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC` (default 600s).
    async fn sweep_hnsw_delta_backlogs(&self) -> Result<()> {
        let mut txn = self.system_store.begin().await?;
        let all_entries = self.system_store.list_worker_registry(&mut txn).await?;
        txn.commit().await?;

        let mut total_observed = 0u32;
        let mut total_enqueued = 0u32;
        let mut total_enqueue_errors = 0u32;
        // Iterate ALL registry entries — discovery does NOT depend on any
        // task-type bit. The shared helper inspects schemas + probes deltas.
        for entry in &all_entries {
            match crate::worker::engine::enqueue_pending_hnsw_merges(
                &self.system_store,
                &self.pool,
                &entry.keyspace,
                entry.db_id,
            )
            .await
            {
                Ok(r) => {
                    total_observed += r.observed;
                    total_enqueued += r.enqueued;
                    total_enqueue_errors += r.enqueue_errors;
                }
                Err(e) => warn!(
                    "HNSW sweep error for keyspace={} db_id={}: {}",
                    entry.keyspace, entry.db_id, e
                ),
            }
        }
        // Gauge: overwrite with total observed across all DBs this sweep.
        self.metrics
            .hnsw_pending_indexes_observed
            .store(total_observed as u64, std::sync::atomic::Ordering::Relaxed);
        // Counters: cumulative fetch_add.
        if total_enqueued > 0 {
            self.metrics
                .hnsw_sweep_enqueued
                .fetch_add(total_enqueued as u64, std::sync::atomic::Ordering::Relaxed);
        }
        if total_enqueue_errors > 0 {
            self.metrics.hnsw_sweep_enqueue_errors.fetch_add(
                total_enqueue_errors as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        if total_observed > 0 {
            info!(
                total_observed,
                total_enqueued, total_enqueue_errors, "HNSW periodic sweep complete"
            );
        }
        Ok(())
    }

    /// Scan worker registry for keyspaces with cron jobs and GC their run history.
    async fn cleanup_cron_runs(&self) -> Result<()> {
        let mut cron_config = CronConfig::from_env();
        cron_config.orphan_timeout_sec =
            effective_cron_orphan_timeout_sec(&cron_config, &self.config);

        let mut txn = self.system_store.begin().await?;
        let registry_entries = self.system_store.list_worker_registry(&mut txn).await?;
        txn.commit().await?;

        for entry in registry_entries {
            if !entry.has_cron() {
                continue;
            }

            let handle = match self.pool.acquire(Some(entry.keyspace.clone())).await {
                Ok(h) => h,
                Err(e) => {
                    warn!(
                        "cron GC: failed to acquire store for keyspace={}: {}",
                        entry.keyspace, e
                    );
                    continue;
                }
            };
            let store = handle.store().clone();

            if let Err(e) = gc_database(&store, entry.db_id, &cron_config).await {
                warn!(
                    "cron GC error for keyspace={} db_id={}: {}",
                    entry.keyspace, entry.db_id, e
                );
            }
        }

        Ok(())
    }

    /// Scan all claims and delete those older than orphan_timeout_sec.
    /// Orphaned claims are NOT re-enqueued — the next cron fire or scheduler
    /// handles retries. One-shot tasks stay failed.
    async fn cleanup_orphan_claims(&self) -> Result<()> {
        let batch_size = self.config.gc_batch_size.max(1);
        let now_ms = now_epoch_ms();
        let timeout_ms = (self.config.orphan_timeout_sec as i64).saturating_mul(1000);
        let cutoff = now_ms.saturating_sub(timeout_ms);

        let (cleaned, _) = run_claim_gc_batches(batch_size, |start_after, requested_batch_size| {
            self.cleanup_orphan_claims_batch(start_after, requested_batch_size, cutoff)
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
        cutoff: i64,
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
                if claim.claimed_at < cutoff {
                    self.system_store
                        .delete_worker_claim_by_raw_key(&mut txn, &key)
                        .await?;
                    cleaned += 1;
                    warn!(
                        "GC: cleaned orphan claim worker={} type={:?} claimed_at={}",
                        claim.worker_id, claim.task_type, claim.claimed_at
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

fn effective_cron_orphan_timeout_sec(
    cron_config: &CronConfig,
    worker_config: &WorkerConfig,
) -> u64 {
    let worker_timeout_sec = worker_config.cron_job_timeout_ms.saturating_add(999) / 1000;
    cron_config.orphan_timeout_sec.max(worker_timeout_sec)
}

include!("gc/hnsw_helpers.rs");

#[cfg(test)]
mod tests;
