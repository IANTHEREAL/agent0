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

    /// Sweep orphaned HNSW S3 graph objects.
    ///
    /// Sweep orphaned or retired HNSW S3 graph objects.
    ///
    /// Correctness rule:
    /// - Anything that was ever referenced by committed TiKV metadata must be
    ///   reclaimed using a safepoint-aware marker, never wall-clock age.
    /// - Objects without committed TiKV lifecycle state are treated as
    ///   speculative uploads and left leak-safe for now. Writers upload to S3
    ///   before committing TiKV metadata, so GC must not guess whether a
    ///   no-meta object will later become live.
    async fn sweep_hnsw_s3_orphans(&self) -> Result<()> {
        let s3 = crate::sql::hnsw::s3::hnsw_s3_client()
            .ok_or_else(|| anyhow::anyhow!("HNSW S3 client not available"))?;

        let client = self
            .system_store
            .transaction_client()
            .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;
        let gc_safepoint = client.get_gc_safepoint().await?;
        let seal_safepoint = client
            .current_timestamp_with_timeout(Duration::from_secs(TSO_TIMEOUT_SEC))
            .await
            .map_err(|e| anyhow::anyhow!("failed to get current timestamp from PD: {}", e))?
            .version();
        let mut txn = self.system_store.begin().await?;
        let all_entries = self.system_store.list_worker_registry(&mut txn).await?;
        txn.commit().await?;

        let mut total_deleted = 0u64;

        for entry in &all_entries {
            // List all S3 objects for this (keyspace, db_id).
            let all_objects = match s3.list_objects(&entry.keyspace, entry.db_id).await {
                Ok(objs) => objs,
                Err(e) => {
                    warn!(
                        keyspace = %entry.keyspace,
                        db_id = entry.db_id,
                        error = %e,
                        "HNSW S3 sweep: failed to list objects"
                    );
                    continue;
                }
            };

            if all_objects.is_empty() {
                continue;
            }

            let handle = match self.pool.acquire(Some(entry.keyspace.clone())).await {
                Ok(h) => h,
                Err(e) => {
                    warn!(
                        keyspace = %entry.keyspace,
                        db_id = entry.db_id,
                        error = %e,
                        "HNSW S3 sweep: failed to acquire tenant store"
                    );
                    continue;
                }
            };
            let store = handle.store();

            // Group objects by (table_id, index_id).
            let mut index_objects: HashMap<(u64, u64), Vec<&crate::sql::hnsw::s3::S3ObjectInfo>> =
                HashMap::new();
            for obj in &all_objects {
                if let Some((table_id, index_id, _version)) =
                    crate::sql::hnsw::s3::parse_s3_key(&obj.key)
                {
                    index_objects
                        .entry((table_id, index_id))
                        .or_default()
                        .push(obj);
                }
            }

            // Read all HNSW metas for this database.
            let metas = match self.read_all_hnsw_metas(store.as_ref(), entry.db_id).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(
                        keyspace = %entry.keyspace,
                        db_id = entry.db_id,
                        error = %e,
                        "HNSW S3 sweep: failed to read HNSW metas"
                    );
                    continue;
                }
            };

            // Process each index's objects.
            for ((table_id, index_id), objects) in &index_objects {
                let meta_ref = metas.get(&(*table_id, *index_id));
                let prefix_gc = match self
                    .read_hnsw_s3_prefix_gc_marker(
                        store.as_ref(),
                        entry.db_id,
                        *table_id,
                        *index_id,
                    )
                    .await
                {
                    Ok(marker) => marker,
                    Err(e) => {
                        warn!(
                            keyspace = %entry.keyspace,
                            db_id = entry.db_id,
                            table_id,
                            index_id,
                            error = %e,
                            "HNSW S3 sweep: failed to read prefix GC marker"
                        );
                        continue;
                    }
                };

                if let Some(mut marker) = prefix_gc {
                    let can_delete_whole_prefix = meta_ref
                        .map(|meta| meta.dropped_at.is_some() || meta.graph_version == 0)
                        .unwrap_or(true);

                    if !can_delete_whole_prefix {
                        warn!(
                            keyspace = %entry.keyspace,
                            db_id = entry.db_id,
                            table_id,
                            index_id,
                            current_version = meta_ref.map(|m| m.graph_version).unwrap_or(0),
                            "HNSW S3 sweep: deleting stale prefix GC marker on live index"
                        );
                        self.delete_hnsw_s3_prefix_gc_marker(
                            store.as_ref(),
                            entry.db_id,
                            *table_id,
                            *index_id,
                        )
                        .await?;
                    } else if marker.delete_after_safepoint.is_none() {
                        marker.delete_after_safepoint = Some(seal_safepoint);
                        self.write_hnsw_s3_prefix_gc_marker(
                            store.as_ref(),
                            entry.db_id,
                            *table_id,
                            *index_id,
                            &marker,
                        )
                        .await?;
                    } else if gc_safepoint >= marker.delete_after_safepoint.unwrap() {
                        match s3
                            .delete_prefix(&entry.keyspace, entry.db_id, *table_id, *index_id)
                            .await
                        {
                            Ok(count) => {
                                total_deleted += count;
                                self.delete_hnsw_s3_prefix_gc_marker(
                                    store.as_ref(),
                                    entry.db_id,
                                    *table_id,
                                    *index_id,
                                )
                                .await?;
                                self.delete_hnsw_s3_retired_version_markers_for_index(
                                    store.as_ref(),
                                    entry.db_id,
                                    *table_id,
                                    *index_id,
                                )
                                .await?;
                                if meta_ref.is_some_and(|m| m.dropped_at.is_some()) {
                                    self.delete_hnsw_meta(
                                        store.as_ref(),
                                        entry.db_id,
                                        *table_id,
                                        *index_id,
                                    )
                                    .await?;
                                }
                                debug!(
                                    keyspace = %entry.keyspace,
                                    db_id = entry.db_id,
                                    table_id,
                                    index_id,
                                    gc_safepoint,
                                    delete_after = marker.delete_after_safepoint,
                                    "HNSW S3 sweep: cleaned whole prefix after safepoint advanced"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    keyspace = %entry.keyspace,
                                    db_id = entry.db_id,
                                    table_id,
                                    index_id,
                                    error = %e,
                                    "HNSW S3 sweep: failed to delete prefix"
                                );
                            }
                        }
                    }
                    if can_delete_whole_prefix {
                        continue;
                    }
                }

                match meta_ref {
                    None => {
                        // No committed meta. This could be:
                        // (a) a writer-owned upload not yet committed (in-flight
                        //     CREATE INDEX inside an explicit transaction)
                        // (b) a rollback orphan (BEGIN; CREATE INDEX; ROLLBACK;)
                        // (c) a crash orphan (S3 PUT succeeded, TiKV commit crashed)
                        //
                        // We CANNOT safely use wall-clock age to distinguish these:
                        // an explicit transaction can hold an uncommitted S3 upload
                        // for longer than gc_life_time_sec. Deleting based on age
                        // would destroy a graph that the user is about to COMMIT.
                        //
                        // Correctness > cleanup: leave these objects alone. The S3
                        // storage cost of orphans is negligible. Rollback/crash
                        // orphans are bounded (one per failed CREATE INDEX) and
                        // will be overwritten if the same index is re-created.
                        debug!(
                            keyspace = %entry.keyspace,
                            db_id = entry.db_id,
                            table_id,
                            index_id,
                            object_count = objects.len(),
                            "HNSW S3 sweep: leaving no-meta objects untouched \
                             (may be uncommitted explicit transaction)"
                        );
                    }
                    Some(meta) if meta.dropped_at.is_some() => {
                        let marker = crate::sql::hnsw::storage::HnswS3PrefixGc {
                            delete_after_safepoint: Some(seal_safepoint),
                            reason: Some("drop".to_string()),
                        };
                        self.write_hnsw_s3_prefix_gc_marker(
                            store.as_ref(),
                            entry.db_id,
                            *table_id,
                            *index_id,
                            &marker,
                        )
                        .await?;
                    }
                    Some(meta) => {
                        let current_version = meta.graph_version;
                        if current_version == 0 {
                            let marker = crate::sql::hnsw::storage::HnswS3PrefixGc {
                                delete_after_safepoint: Some(seal_safepoint),
                                reason: Some("truncate".to_string()),
                            };
                            self.write_hnsw_s3_prefix_gc_marker(
                                store.as_ref(),
                                entry.db_id,
                                *table_id,
                                *index_id,
                                &marker,
                            )
                            .await?;
                            continue;
                        }

                        for obj in objects {
                            let version = match crate::sql::hnsw::s3::parse_s3_key(&obj.key) {
                                Some((_, _, v)) => v,
                                None => continue,
                            };
                            let retired_marker = self
                                .read_hnsw_s3_retired_version_marker(
                                    store.as_ref(),
                                    entry.db_id,
                                    *table_id,
                                    *index_id,
                                    version,
                                )
                                .await?;
                            match classify_live_hnsw_s3_version(
                                current_version,
                                version,
                                retired_marker.is_some(),
                            ) {
                                LiveHnswS3VersionDisposition::Current {
                                    clear_stale_retired_marker,
                                } => {
                                    if clear_stale_retired_marker {
                                        self.delete_hnsw_s3_retired_version_marker(
                                            store.as_ref(),
                                            entry.db_id,
                                            *table_id,
                                            *index_id,
                                            version,
                                        )
                                        .await?;
                                        warn!(
                                            keyspace = %entry.keyspace,
                                            db_id = entry.db_id,
                                            table_id,
                                            index_id,
                                            version,
                                            "HNSW S3 sweep: removed stale retired marker from current live version"
                                        );
                                    }
                                }
                                LiveHnswS3VersionDisposition::FutureSpeculative {
                                    clear_stale_retired_marker,
                                } => {
                                    // Writers upload S3 graphs before committing the TiKV
                                    // metadata flip to the new graph_version. Therefore a
                                    // version greater than current_version may still be the
                                    // next live graph in-flight; GC must never infer
                                    // retirement from object listing alone for this case.
                                    //
                                    // Future/speculative versions are left alone. They
                                    // could be an in-flight merge upload or an uncommitted
                                    // explicit transaction. A crashed merge will overwrite
                                    // on retry; a rolled-back txn leaves a small orphan.
                                    // Correctness > cleanup.
                                    if clear_stale_retired_marker {
                                        self.delete_hnsw_s3_retired_version_marker(
                                            store.as_ref(),
                                            entry.db_id,
                                            *table_id,
                                            *index_id,
                                            version,
                                        )
                                        .await?;
                                        warn!(
                                            keyspace = %entry.keyspace,
                                            db_id = entry.db_id,
                                            table_id,
                                            index_id,
                                            current_version,
                                            version,
                                            "HNSW S3 sweep: removed stale retired marker from speculative future version"
                                        );
                                    }
                                }
                                LiveHnswS3VersionDisposition::HistoricalRetired => {
                                    match retired_marker {
                                        None => {
                                            let marker =
                                                crate::sql::hnsw::storage::HnswS3RetiredVersionGc {
                                                    delete_after_safepoint: Some(seal_safepoint),
                                                };
                                            self.write_hnsw_s3_retired_version_marker(
                                                store.as_ref(),
                                                entry.db_id,
                                                *table_id,
                                                *index_id,
                                                version,
                                                &marker,
                                            )
                                            .await?;
                                        }
                                        Some(mut marker)
                                            if marker.delete_after_safepoint.is_none() =>
                                        {
                                            marker.delete_after_safepoint = Some(seal_safepoint);
                                            self.write_hnsw_s3_retired_version_marker(
                                                store.as_ref(),
                                                entry.db_id,
                                                *table_id,
                                                *index_id,
                                                version,
                                                &marker,
                                            )
                                            .await?;
                                        }
                                        Some(marker)
                                            if gc_safepoint
                                                >= marker
                                                    .delete_after_safepoint
                                                    .unwrap_or(u64::MAX) =>
                                        {
                                            // Delete S3 object, then marker. On S3
                                            // failure, keep the marker (retry next
                                            // sweep) but do NOT abort the entire
                                            // sweep — other indexes must still be
                                            // processed.
                                            match s3
                                                .delete_graph(
                                                    &entry.keyspace,
                                                    entry.db_id,
                                                    *table_id,
                                                    *index_id,
                                                    version,
                                                )
                                                .await
                                            {
                                                Ok(()) => {
                                                    self.delete_hnsw_s3_retired_version_marker(
                                                        store.as_ref(),
                                                        entry.db_id,
                                                        *table_id,
                                                        *index_id,
                                                        version,
                                                    )
                                                    .await?;
                                                    total_deleted += 1;
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        table_id,
                                                        index_id,
                                                        version,
                                                        error = %e,
                                                        "HNSW S3 sweep: retired version delete failed, marker retained for retry"
                                                    );
                                                }
                                            }
                                        }
                                        Some(_) => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if total_deleted > 0 {
            info!(total_deleted, "HNSW S3 sweep complete");
        }

        Ok(())
    }

    /// Read all HNSW metas for a database by enumerating schemas.
    ///
    /// Discovers HNSW indexes from table schemas (O(tables × indexes)),
    /// then point-gets each meta key (~200 bytes). Does NOT scan the
    /// d_{db}_hnsw_* prefix — that prefix contains millions of rowid
    /// mapping and delta keys on large tables.
    async fn read_all_hnsw_metas(
        &self,
        store: &TikvStore,
        db_id: u64,
    ) -> Result<HashMap<(u64, u64), crate::sql::hnsw::storage::HnswMeta>> {
        let mut txn = store.begin().await?;
        let result = async {
            let table_names = store.list_tables(&mut txn, db_id).await?;
            let schemas = store
                .list_table_schemas(&mut txn, db_id, &table_names)
                .await?;

            let mut result = HashMap::new();
            for schema in &schemas {
                for index in &schema.indexes {
                    if !index.is_hnsw() {
                        continue;
                    }
                    let meta_key =
                        crate::sql::hnsw::storage::hnsw_meta_key(db_id, schema.table_id, index.id);
                    if let Some(value) = txn.get(meta_key).await? {
                        if let Ok(meta) =
                            serde_json::from_slice::<crate::sql::hnsw::storage::HnswMeta>(&value)
                        {
                            result.insert((schema.table_id, index.id), meta);
                        }
                    }
                }
            }
            Ok::<_, anyhow::Error>(result)
        }
        .await;

        match result {
            Ok(result) => {
                txn.rollback().await.ok();
                Ok(result)
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e)
            }
        }
    }

    async fn read_hnsw_s3_prefix_gc_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
    ) -> Result<Option<crate::sql::hnsw::storage::HnswS3PrefixGc>> {
        let mut txn = store.begin().await?;
        let result = async {
            let key = crate::sql::hnsw::storage::hnsw_s3_prefix_gc_key(db_id, table_id, index_id);
            let Some(bytes) = txn.get(key).await? else {
                return Ok::<_, anyhow::Error>(None);
            };
            Ok(Some(serde_json::from_slice::<
                crate::sql::hnsw::storage::HnswS3PrefixGc,
            >(&bytes)?))
        }
        .await;
        txn.rollback().await.ok();
        result
    }

    async fn write_hnsw_s3_prefix_gc_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        marker: &crate::sql::hnsw::storage::HnswS3PrefixGc,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let key = crate::sql::hnsw::storage::hnsw_s3_prefix_gc_key(db_id, table_id, index_id);
        crate::txn::txn_put(&mut txn, key, serde_json::to_vec(marker)?).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn delete_hnsw_s3_prefix_gc_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let key = crate::sql::hnsw::storage::hnsw_s3_prefix_gc_key(db_id, table_id, index_id);
        crate::txn::txn_delete(&mut txn, key).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn read_hnsw_s3_retired_version_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
    ) -> Result<Option<crate::sql::hnsw::storage::HnswS3RetiredVersionGc>> {
        let mut txn = store.begin().await?;
        let result = async {
            let key = crate::sql::hnsw::storage::hnsw_s3_retired_version_key(
                db_id, table_id, index_id, version,
            );
            let Some(bytes) = txn.get(key).await? else {
                return Ok::<_, anyhow::Error>(None);
            };
            Ok(Some(serde_json::from_slice::<
                crate::sql::hnsw::storage::HnswS3RetiredVersionGc,
            >(&bytes)?))
        }
        .await;
        txn.rollback().await.ok();
        result
    }

    async fn write_hnsw_s3_retired_version_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
        marker: &crate::sql::hnsw::storage::HnswS3RetiredVersionGc,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let key = crate::sql::hnsw::storage::hnsw_s3_retired_version_key(
            db_id, table_id, index_id, version,
        );
        crate::txn::txn_put(&mut txn, key, serde_json::to_vec(marker)?).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn delete_hnsw_s3_retired_version_marker(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let key = crate::sql::hnsw::storage::hnsw_s3_retired_version_key(
            db_id, table_id, index_id, version,
        );
        crate::txn::txn_delete(&mut txn, key).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn delete_hnsw_s3_retired_version_markers_for_index(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let result = async {
            let range: tikv_client::BoundRange =
                (crate::sql::hnsw::storage::hnsw_s3_retired_version_prefix(
                    db_id, table_id, index_id,
                )
                    ..crate::sql::hnsw::storage::hnsw_s3_retired_version_prefix_end(
                        db_id, table_id, index_id,
                    ))
                    .into();
            let pairs = txn.scan(range, u32::MAX).await?;
            let keys: Vec<Vec<u8>> = pairs
                .map(|pair| {
                    let key: &[u8] = pair.key().as_ref().into();
                    key.to_vec()
                })
                .collect();
            for key in keys {
                crate::txn::txn_delete(&mut txn, key).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                Ok(())
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e)
            }
        }
    }

    async fn delete_hnsw_meta(
        &self,
        store: &TikvStore,
        db_id: u64,
        table_id: u64,
        index_id: u64,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let meta_key = crate::sql::hnsw::storage::hnsw_meta_key(db_id, table_id, index_id);
        crate::txn::txn_delete(&mut txn, meta_key).await?;
        txn.commit().await?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveHnswS3VersionDisposition {
    Current { clear_stale_retired_marker: bool },
    HistoricalRetired,
    FutureSpeculative { clear_stale_retired_marker: bool },
}

fn classify_live_hnsw_s3_version(
    current_version: u64,
    object_version: u64,
    retired_marker_present: bool,
) -> LiveHnswS3VersionDisposition {
    use std::cmp::Ordering;

    match object_version.cmp(&current_version) {
        Ordering::Equal => LiveHnswS3VersionDisposition::Current {
            clear_stale_retired_marker: retired_marker_present,
        },
        Ordering::Less => LiveHnswS3VersionDisposition::HistoricalRetired,
        Ordering::Greater => LiveHnswS3VersionDisposition::FutureSpeculative {
            clear_stale_retired_marker: retired_marker_present,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future;

    // --- GC safepoint computation tests ---

    #[test]
    fn compute_safepoint_basic() {
        // TSO version: physical_ms << 18 | logical
        // Simulate a current_version corresponding to ~1 hour of physical time.
        let physical_ms: u64 = 3_600_000; // 1 hour
        let current_version = physical_ms << 18;
        let life_time_sec = 600; // 10 minutes

        let sp = compute_safepoint_version(current_version, life_time_sec);
        // Expected: (3_600_000 - 600_000) << 18 = 3_000_000 << 18
        let expected = (physical_ms - life_time_sec * 1000) << 18;
        assert_eq!(sp, expected);
    }

    #[test]
    fn compute_safepoint_saturates_to_zero() {
        // current_version is smaller than the life_time offset.
        let current_version = 1000_u64 << 18;
        let life_time_sec = 86400; // 24 hours — way more than 1 second of TSO

        let sp = compute_safepoint_version(current_version, life_time_sec);
        assert_eq!(sp, 0, "should saturate to 0, not underflow");
    }

    #[test]
    fn compute_safepoint_zero_life_time() {
        let current_version = 999_999_u64 << 18;
        let sp = compute_safepoint_version(current_version, 0);
        assert_eq!(sp, current_version, "zero life_time means no offset");
    }

    #[test]
    fn compute_safepoint_preserves_logical_bits() {
        // current_version with some logical bits set
        let physical_ms: u64 = 7_200_000;
        let logical: u64 = 42;
        let current_version = (physical_ms << 18) | logical;
        let life_time_sec = 3600; // 1 hour

        let sp = compute_safepoint_version(current_version, life_time_sec);
        // The subtraction is on the whole version, so logical bits are preserved
        let expected = current_version - ((life_time_sec * 1000) << 18);
        assert_eq!(sp, expected);
    }

    #[test]
    fn cluster_safepoint_ignores_stale_instances() {
        let current_version = 10_000_000u64 << 18;
        let life_time_sec = 600;
        let time_based = compute_safepoint_version(current_version, life_time_sec);
        let stale_version = time_based.saturating_sub(1);

        let states = vec![GcInstanceState {
            instance_id: "stale".to_string(),
            min_start_ts: Some(time_based.saturating_sub(10_000)),
            updated_at_version: stale_version,
            legacy_max_untracked_timeout_sec: None,
        }];

        assert_eq!(
            compute_cluster_gc_safepoint(current_version, life_time_sec, life_time_sec, &states),
            time_based
        );
    }

    #[test]
    fn cluster_safepoint_clamps_to_oldest_live_transaction() {
        let current_version = 10_000_000u64 << 18;
        let life_time_sec = 600;
        let hb_timeout = life_time_sec; // use same value as heartbeat timeout for this test
        let live_updated_at = current_version;

        let states = vec![
            GcInstanceState {
                instance_id: "a".to_string(),
                min_start_ts: Some((9_500_000u64 << 18) + 7),
                updated_at_version: live_updated_at,
                legacy_max_untracked_timeout_sec: None,
            },
            GcInstanceState {
                instance_id: "b".to_string(),
                min_start_ts: Some((9_300_000u64 << 18) + 9),
                updated_at_version: live_updated_at,
                legacy_max_untracked_timeout_sec: None,
            },
        ];

        assert_eq!(
            compute_cluster_gc_safepoint(current_version, life_time_sec, hb_timeout, &states),
            ((9_300_000u64 << 18) + 9).saturating_sub(1)
        );
    }

    #[test]
    fn stale_gc_instance_ids_only_returns_stale_rows() {
        let current_version = 10_000_000u64 << 18;
        let life_time_sec = 600;
        let hb_timeout = life_time_sec;
        let live_updated_at = current_version;
        let stale_updated_at =
            compute_safepoint_version(current_version, hb_timeout).saturating_sub(1);

        let states = vec![
            GcInstanceState {
                instance_id: "live".to_string(),
                min_start_ts: None,
                updated_at_version: live_updated_at,
                legacy_max_untracked_timeout_sec: None,
            },
            GcInstanceState {
                instance_id: "stale".to_string(),
                min_start_ts: Some((9_300_000u64 << 18) + 9),
                updated_at_version: stale_updated_at,
                legacy_max_untracked_timeout_sec: None,
            },
        ];

        assert_eq!(
            stale_gc_instance_ids(current_version, hb_timeout, &states),
            vec!["stale".to_string()]
        );
    }

    #[test]
    fn cluster_gc_life_time_honors_live_legacy_timeout_floor() {
        let current_version = 10_000_000u64 << 18;
        let life_time_sec = 600;
        let hb_timeout = life_time_sec;
        let states = vec![GcInstanceState {
            instance_id: "legacy".to_string(),
            min_start_ts: None,
            updated_at_version: current_version,
            legacy_max_untracked_timeout_sec: Some(3_600),
        }];

        assert_eq!(
            effective_cluster_gc_life_time_sec(current_version, life_time_sec, hb_timeout, &states),
            3_600
        );
        assert_eq!(
            compute_cluster_gc_safepoint(current_version, life_time_sec, hb_timeout, &states),
            compute_safepoint_version(current_version, 3_600)
        );
    }

    #[test]
    fn cluster_gc_life_time_ignores_stale_legacy_timeout_floor() {
        let current_version = 10_000_000u64 << 18;
        let life_time_sec = 600;
        let hb_timeout = life_time_sec;
        let stale_updated_at =
            compute_safepoint_version(current_version, hb_timeout).saturating_sub(1);
        let states = vec![GcInstanceState {
            instance_id: "stale-legacy".to_string(),
            min_start_ts: None,
            updated_at_version: stale_updated_at,
            legacy_max_untracked_timeout_sec: Some(3_600),
        }];

        assert_eq!(
            effective_cluster_gc_life_time_sec(current_version, life_time_sec, hb_timeout, &states),
            life_time_sec
        );
        assert_eq!(
            compute_cluster_gc_safepoint(current_version, life_time_sec, hb_timeout, &states),
            compute_safepoint_version(current_version, life_time_sec)
        );
    }

    #[test]
    fn test_orphan_timeout_calc() {
        let now_ms: i64 = 1_700_000_000_000;
        let orphan_timeout_sec: u64 = 300;
        let timeout_ms = (orphan_timeout_sec as i64).saturating_mul(1000);
        let cutoff = now_ms.saturating_sub(timeout_ms);

        let old_claim = now_ms - 301_000;
        assert!(
            old_claim < cutoff,
            "claim older than timeout should be detected as orphan"
        );

        let recent_claim = now_ms - 299_000;
        assert!(
            recent_claim >= cutoff,
            "claim within timeout should NOT be orphaned"
        );

        let edge_claim = cutoff;
        assert!(
            edge_claim >= cutoff,
            "claim exactly at cutoff boundary is not orphaned"
        );
    }

    #[test]
    fn test_rand_jitter_secs_within_bounds() {
        for max in [1u64, 10, 60, 120, 3600] {
            let jitter = rand_jitter_secs(max);
            assert!(jitter < max, "jitter {} should be < max {}", jitter, max);
        }
    }

    #[test]
    fn test_rand_jitter_secs_max_one() {
        let jitter = rand_jitter_secs(1);
        assert_eq!(jitter, 0, "jitter with max=1 must be 0");
    }

    #[test]
    fn test_effective_cron_orphan_timeout_respects_worker_timeout() {
        let cron_cfg = CronConfig {
            orphan_timeout_sec: 300,
            ..Default::default()
        };

        let worker_cfg = WorkerConfig {
            cron_job_timeout_ms: 1_800_000,
            ..Default::default()
        };

        assert_eq!(
            effective_cron_orphan_timeout_sec(&cron_cfg, &worker_cfg),
            1_800
        );
    }

    #[test]
    fn test_effective_cron_orphan_timeout_keeps_larger_cron_value() {
        let cron_cfg = CronConfig {
            orphan_timeout_sec: 7_200,
            ..Default::default()
        };

        let worker_cfg = WorkerConfig {
            cron_job_timeout_ms: 1_800_000,
            ..Default::default()
        };

        assert_eq!(
            effective_cron_orphan_timeout_sec(&cron_cfg, &worker_cfg),
            7_200
        );
    }

    #[test]
    fn live_hnsw_s3_classification_treats_current_as_live_and_clears_stale_marker() {
        assert_eq!(
            classify_live_hnsw_s3_version(6, 6, false),
            LiveHnswS3VersionDisposition::Current {
                clear_stale_retired_marker: false
            }
        );
        assert_eq!(
            classify_live_hnsw_s3_version(6, 6, true),
            LiveHnswS3VersionDisposition::Current {
                clear_stale_retired_marker: true
            }
        );
    }

    #[test]
    fn live_hnsw_s3_classification_treats_older_versions_as_retired_only_when_behind_current() {
        assert_eq!(
            classify_live_hnsw_s3_version(6, 5, false),
            LiveHnswS3VersionDisposition::HistoricalRetired
        );
        assert_eq!(
            classify_live_hnsw_s3_version(6, 5, true),
            LiveHnswS3VersionDisposition::HistoricalRetired
        );
    }

    #[test]
    fn live_hnsw_s3_classification_never_retires_future_versions() {
        assert_eq!(
            classify_live_hnsw_s3_version(5, 6, false),
            LiveHnswS3VersionDisposition::FutureSpeculative {
                clear_stale_retired_marker: false
            }
        );
        assert_eq!(
            classify_live_hnsw_s3_version(5, 6, true),
            LiveHnswS3VersionDisposition::FutureSpeculative {
                clear_stale_retired_marker: true
            }
        );
        assert_eq!(
            classify_live_hnsw_s3_version(0, 1, false),
            LiveHnswS3VersionDisposition::FutureSpeculative {
                clear_stale_retired_marker: false
            }
        );
    }

    #[test]
    fn gc_orphan_cleanup_uses_claim_deletion_api() {
        // Source-contract: cleanup_orphan_claims_batch must delete via the
        // claim-specific API (delete_worker_claim_by_raw_key), not the
        // queue-entry API (delete_worker_queue_entry).
        //
        // This test FAILS if someone changes the deletion call back to
        // delete_worker_queue_entry in gc.rs.
        let source = include_str!("gc.rs");
        // Split at #[cfg(test)] to inspect only production code, avoiding
        // false positives from strings inside this very test module.
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("gc.rs must contain #[cfg(test)]");
        assert!(
            prod_source.contains("delete_worker_claim_by_raw_key"),
            "gc.rs must call delete_worker_claim_by_raw_key for orphan claim cleanup"
        );
        assert!(
            !prod_source.contains("delete_worker_queue_entry"),
            "gc.rs production code must NOT call delete_worker_queue_entry — \
             claim keys require delete_worker_claim_by_raw_key"
        );
    }

    #[test]
    fn gc_tso_timeout_uses_dedicated_client_timeout_path() {
        let source = include_str!("gc.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("gc.rs must contain #[cfg(test)]");
        assert!(
            prod_source.contains("current_timestamp_with_timeout"),
            "gc.rs must use TransactionClient::current_timestamp_with_timeout so \
             GC TSO probes stay on the dedicated timed path"
        );
        assert!(
            !prod_source.contains("tokio::time::timeout(\n        Duration::from_secs(TSO_TIMEOUT_SEC),\n        client.current_timestamp(),"),
            "gc.rs must not wrap client.current_timestamp() directly; that bypasses \
             the dedicated timed TSO path"
        );
    }

    #[test]
    fn gc_update_safepoint_has_total_timeout_bound() {
        let source = include_str!("gc.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("gc.rs must contain #[cfg(test)]");
        assert!(
            prod_source.contains("SAFEPOINT_UPDATE_TIMEOUT_SEC"),
            "gc.rs must define an explicit total timeout for update_safepoint"
        );
        assert!(
            prod_source.contains("tokio::time::timeout(")
                && prod_source.contains("client.update_safepoint(safepoint)"),
            "gc.rs must bound update_safepoint with an outer timeout"
        );
    }

    #[test]
    fn gc_stale_reaper_rechecks_current_row_before_delete() {
        let source = include_str!("gc.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("gc.rs must contain #[cfg(test)]");
        assert!(
            prod_source.contains("get_gc_instance_state_for_update"),
            "gc.rs stale-row reaping must re-read the current row under lock before delete"
        );
    }

    #[test]
    fn gc_registry_loops_do_not_add_startup_jitter() {
        let source = include_str!("gc.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("gc.rs must contain #[cfg(test)]");
        let publisher_fn = prod_source
            .split("pub async fn run_gc_publisher_loop")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub async fn publish_gc_instance_state_once")
                    .next()
            })
            .expect(
                "gc.rs must define run_gc_publisher_loop before publish_gc_instance_state_once",
            );
        let advancer_fn = prod_source
            .split("pub async fn run_gc_advancer_loop")
            .nth(1)
            .and_then(|rest| {
                rest.split("/// Remove this process's GC registry row")
                    .next()
            })
            .expect("gc.rs must define run_gc_advancer_loop");

        assert!(
            !publisher_fn.contains("rand_jitter_secs"),
            "GC registry publisher must not sleep behind startup jitter; heartbeat cadence is part of the safepoint contract"
        );
        assert!(
            !advancer_fn.contains("rand_jitter_secs"),
            "GC safepoint advancer must not sleep behind startup jitter; initial cadence must stay deterministic"
        );
    }

    #[test]
    fn gc_publisher_loop_publishes_immediately_on_entry() {
        let source = include_str!("gc.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("gc.rs must contain #[cfg(test)]");
        let publisher_fn = prod_source
            .split("pub async fn run_gc_publisher_loop")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub async fn publish_gc_instance_state_once")
                    .next()
            })
            .expect(
                "gc.rs must define run_gc_publisher_loop before publish_gc_instance_state_once",
            );

        assert!(
            publisher_fn.contains(
                "interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);"
            ),
            "GC publisher loop must delay missed ticks instead of bursting multiple heartbeats"
        );

        // Extract the region between `interval` creation and `loop {` — this is
        // where a skip-first-tick call would live if someone re-added it.
        let between_interval_and_loop = publisher_fn
            .split("tokio::time::interval(")
            .nth(1)
            .and_then(|after_interval| after_interval.split("loop {").next())
            .expect(
                "run_gc_publisher_loop must contain a tokio::time::interval() call followed by loop {"
            );

        assert!(
            !between_interval_and_loop.contains("interval.tick().await"),
            "The publisher loop must not skip its first tick — doing so creates a \
             full-interval blind window on panic-restart because the supervisor \
             re-enters the loop without a prior synchronous publish."
        );
    }

    #[tokio::test]
    async fn cleanup_orphan_claims_respects_batch_size() {
        let orphan_count = 7usize;
        let batch_size = 3usize;
        let expected_batches = orphan_count.div_ceil(batch_size);
        let all_claim_keys: Vec<Vec<u8>> = (0..orphan_count).map(|idx| vec![idx as u8]).collect();
        let mut processed_keys: Vec<Vec<u8>> = Vec::new();

        let (cleaned, batches) = run_claim_gc_batches(batch_size, |start_after, requested_size| {
            let start_index = start_after
                .as_ref()
                .and_then(|key| {
                    all_claim_keys
                        .iter()
                        .position(|candidate_key| candidate_key == key)
                })
                .map(|idx| idx + 1)
                .unwrap_or(0);
            let end_index = (start_index + requested_size).min(all_claim_keys.len());
            let page_keys = all_claim_keys[start_index..end_index].to_vec();
            processed_keys.extend(page_keys.iter().cloned());

            let scanned = page_keys.len();
            let last_key = page_keys.last().cloned();
            future::ready(Ok(ClaimGcBatch {
                scanned,
                cleaned: scanned as u32,
                last_key,
            }))
        })
        .await
        .expect("pagination loop should succeed");

        assert_eq!(batches, expected_batches);
        assert_eq!(cleaned, orphan_count as u32);
        assert_eq!(processed_keys, all_claim_keys);
    }

    #[tokio::test]
    async fn run_claim_gc_batches_clamps_batch_size_above_u32_max() {
        let oversized_batch_size = (u32::MAX as usize) + 1;
        let mut call_count = 0usize;

        let (cleaned, batches) =
            run_claim_gc_batches(oversized_batch_size, |start_after, requested_size| {
                assert_eq!(
                    requested_size,
                    u32::MAX as usize,
                    "batch size must be clamped at consumption point",
                );

                let result = match call_count {
                    0 => {
                        assert_eq!(start_after, None);
                        ClaimGcBatch {
                            scanned: u32::MAX as usize,
                            cleaned: 0,
                            last_key: Some(vec![1]),
                        }
                    }
                    1 => {
                        assert_eq!(start_after, Some(vec![1]));
                        ClaimGcBatch {
                            scanned: 1,
                            cleaned: 1,
                            last_key: Some(vec![2]),
                        }
                    }
                    _ => panic!("loop should terminate after second batch"),
                };
                call_count += 1;
                future::ready(Ok(result))
            })
            .await
            .expect("pagination loop should succeed");

        assert_eq!(batches, 2, "must continue after first full capped batch");
        assert_eq!(cleaned, 1);
        assert_eq!(call_count, 2);
    }

    #[test]
    fn clear_gc_instance_state_neutralizes_before_delete() {
        let source = include_str!("gc.rs");
        let clear_fn = source
            .split("pub async fn clear_gc_instance_state")
            .nth(1)
            .and_then(|rest| rest.split("\npub ").next())
            .expect("gc.rs must define clear_gc_instance_state");

        // Phase 1: must publish min_start_ts=None to neutralize the row.
        let neutralize_pos = clear_fn
            .find("put_gc_instance_state")
            .expect("clear_gc_instance_state must publish a neutralizing heartbeat");
        assert!(
            clear_fn.contains("None, // no min_start_ts"),
            "neutralizing publish must pass min_start_ts = None"
        );

        // Phase 2: must delete the row after neutralizing.
        let delete_pos = clear_fn
            .find("delete_gc_instance_state")
            .expect("clear_gc_instance_state must delete the row");

        assert!(
            neutralize_pos < delete_pos,
            "must neutralize (publish None) before deleting the row"
        );
    }

    // ── End-to-end GC safepoint scenario tests ──────────────────
    //
    // These exercise the full chain: ActiveTxnRegistry → min_start_ts →
    // compute_cluster_gc_safepoint, verifying each real-world scenario
    // that the PR set out to protect against.

    use crate::worker::active_txn_registry::ActiveTxnRegistry;

    /// Helper: build a TSO version from millisecond timestamp.
    fn tso(ms: u64) -> u64 {
        ms << 18
    }

    /// Helper: build a live GcInstanceState with the given min_start_ts.
    fn live_state(
        instance_id: &str,
        min_start_ts: Option<u64>,
        updated_at_ms: u64,
    ) -> GcInstanceState {
        GcInstanceState {
            instance_id: instance_id.to_string(),
            min_start_ts,
            updated_at_version: tso(updated_at_ms),
            legacy_max_untracked_timeout_sec: None,
        }
    }

    // ── Scenario 1: Worker txn keeps safepoint clamped ──────────
    //
    // All e2e tests use txn start_ts values OLDER than gc_life_time so
    // the time-based safepoint alone would NOT protect them. This proves
    // the ActiveTxnRegistry is essential for protection.
    //
    // Timeline layout (gc_life_time = 600s = 600_000ms):
    //   now            = tso(10_000_000)
    //   time_based_sp  = tso(10_000_000 - 600_000) = tso(9_400_000)
    //   old txn        = tso(9_000_000) — 1000s old, OUTSIDE gc_life_time
    //
    // Without registry: safepoint = tso(9_400_000) > tso(9_000_000) → txn exposed
    // With registry:    safepoint = tso(9_000_000) - 1 → txn protected

    #[test]
    fn e2e_worker_txn_clamps_safepoint_then_releases_on_commit() {
        let registry = Arc::new(ActiveTxnRegistry::new());
        let life_time_sec = 600; // time_based_sp = tso(9_400_000)
        let now = tso(10_000_000);
        let time_based_sp = compute_safepoint_version(now, life_time_sec);

        // Worker begins a long-lived scan txn 1000s ago (outside gc_life_time).
        let worker_start_ts = tso(9_000_000);
        assert!(
            time_based_sp > worker_start_ts,
            "precondition: time-based safepoint must exceed txn start_ts to prove registry is needed"
        );

        let guard = registry.track_worker_txn(worker_start_ts);
        assert_eq!(registry.min_start_ts(), Some(worker_start_ts));

        // Simulate publisher: publish min_start_ts to cluster.
        let state = live_state("inst-1", registry.min_start_ts(), 10_000_000);
        let safepoint = compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state]);

        // Registry clamps safepoint below the worker txn.
        assert_eq!(safepoint, worker_start_ts - 1);

        // Worker commits — guard drops, unregisters.
        drop(guard);
        assert_eq!(registry.min_start_ts(), None);

        // Next publish: no txns → safepoint advances past old start_ts.
        let state_after = live_state("inst-1", registry.min_start_ts(), 10_000_000);
        let safepoint_after =
            compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state_after]);
        assert!(
            safepoint_after > worker_start_ts,
            "safepoint {safepoint_after} should advance past old worker start_ts {worker_start_ts}"
        );
    }

    // ── Scenario 2: Session txn keeps safepoint clamped ─────────

    #[test]
    fn e2e_session_txn_clamps_safepoint_then_releases_on_commit() {
        let registry = Arc::new(ActiveTxnRegistry::new());
        let life_time_sec = 600;
        let now = tso(10_000_000);

        // Session with an old explicit transaction (outside gc_life_time).
        let session_start_ts = tso(9_000_000);
        registry.register_connection(42, session_start_ts);
        assert_eq!(registry.min_start_ts(), Some(session_start_ts));

        let state = live_state("inst-1", registry.min_start_ts(), 10_000_000);
        let safepoint = compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state]);
        assert_eq!(safepoint, session_start_ts - 1);

        // Session commits — clean unregister.
        registry.unregister_connection(42);
        assert_eq!(registry.min_start_ts(), None);

        let state_after = live_state("inst-1", registry.min_start_ts(), 10_000_000);
        let safepoint_after =
            compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state_after]);
        assert!(safepoint_after > session_start_ts);
    }

    // ── Scenario 3: Worker rollback failure → quarantine protects ─

    #[test]
    fn e2e_worker_rollback_failure_quarantine_keeps_safepoint_clamped() {
        let registry = Arc::new(ActiveTxnRegistry::new());
        let life_time_sec = 600;
        let now = tso(10_000_000);

        let worker_start_ts = tso(9_000_000);
        let mut guard = registry.track_worker_txn(worker_start_ts);

        // Commit fails, rollback also fails → quarantine the guard.
        guard.quarantine();
        drop(guard);

        // Even after guard drop, registry still has the entry (quarantined).
        assert_eq!(registry.min_start_ts(), Some(worker_start_ts));
        assert_eq!(registry.quarantined_len(), 1);

        // Safepoint still clamped by quarantined entry.
        let state = live_state("inst-1", registry.min_start_ts(), 10_000_000);
        let safepoint = compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state]);
        assert_eq!(safepoint, worker_start_ts - 1);

        // After QUARANTINE_TTL, the publisher reaps the entry.
        let reaped = registry.reap_quarantined_with_ttl(std::time::Duration::ZERO);
        assert_eq!(reaped, 1);
        assert_eq!(registry.min_start_ts(), None);

        // Safepoint now advances.
        let state_after = live_state("inst-1", registry.min_start_ts(), 10_000_000);
        let safepoint_after =
            compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state_after]);
        assert!(safepoint_after > worker_start_ts);
    }

    // ── Scenario 4: Session disconnect → quarantine protects ────

    #[test]
    fn e2e_session_disconnect_quarantine_keeps_safepoint_clamped() {
        let registry = Arc::new(ActiveTxnRegistry::new());
        let life_time_sec = 600;
        let now = tso(10_000_000);

        let session_start_ts = tso(9_000_000);
        registry.register_connection(99, session_start_ts);

        // Client disconnects — Session::drop calls quarantine_connection.
        registry.quarantine_connection(99);

        // Registry still holds the entry (quarantined).
        assert_eq!(registry.min_start_ts(), Some(session_start_ts));
        assert_eq!(registry.quarantined_len(), 1);

        let state = live_state("inst-1", registry.min_start_ts(), 10_000_000);
        let safepoint = compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state]);
        assert_eq!(safepoint, session_start_ts - 1);

        // After TTL, publisher reaps.
        registry.reap_quarantined_with_ttl(std::time::Duration::ZERO);
        assert_eq!(registry.min_start_ts(), None);
    }

    // ── Scenario 5: Session commit then drop → quarantine is no-op

    #[test]
    fn e2e_clean_session_commit_then_drop_no_quarantine_leak() {
        let registry = Arc::new(ActiveTxnRegistry::new());

        let session_start_ts = tso(9_000_000);
        registry.register_connection(77, session_start_ts);
        assert_eq!(registry.min_start_ts(), Some(session_start_ts));

        // Session commits successfully — unregister.
        registry.unregister_connection(77);
        assert_eq!(registry.min_start_ts(), None);

        // Session::drop fires — quarantine_connection is a no-op since
        // the entry was already removed by commit.
        registry.quarantine_connection(77);
        assert_eq!(registry.quarantined_len(), 0);
        assert_eq!(registry.min_start_ts(), None);
    }

    // ── Scenario 6: Multi-instance cluster safepoint ────────────

    #[test]
    fn e2e_multi_instance_safepoint_clamps_to_global_minimum() {
        let life_time_sec = 600;
        let now = tso(10_000_000);

        // Instance A: has a long-running txn from 1000s ago (outside gc_life_time).
        let inst_a = live_state("inst-a", Some(tso(9_000_000)), 10_000_000);
        // Instance B: all txns are recent (within gc_life_time).
        let inst_b = live_state("inst-b", Some(tso(9_999_000)), 10_000_000);
        // Instance C: no active txns.
        let inst_c = live_state("inst-c", None, 10_000_000);

        let safepoint = compute_cluster_gc_safepoint(
            now,
            life_time_sec,
            life_time_sec,
            &[inst_a, inst_b, inst_c],
        );

        // Must clamp to instance A's old txn (the global minimum).
        assert_eq!(safepoint, tso(9_000_000) - 1);
    }

    // ── Scenario 7: Stale instance row ignored by advancer ──────

    #[test]
    fn e2e_stale_instance_row_does_not_block_gc_advancement() {
        let life_time_sec = 600;
        let now = tso(10_000_000);

        // Instance A: live, no active txns.
        let inst_a = live_state("inst-a", None, 10_000_000);
        // Instance B: STALE — last heartbeat was far in the past.
        let inst_b = GcInstanceState {
            instance_id: "inst-b".to_string(),
            min_start_ts: Some(tso(1_000_000)), // very old start_ts
            updated_at_version: tso(1_000_000), // very old heartbeat
            legacy_max_untracked_timeout_sec: None,
        };

        let safepoint =
            compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[inst_a, inst_b]);

        // Stale instance B must be ignored — safepoint is purely time-based.
        let time_based_safepoint = compute_safepoint_version(now, life_time_sec);
        assert_eq!(safepoint, time_based_safepoint);
    }

    // ── Scenario 8: Shutdown neutralize → row cannot clamp GC ───

    #[test]
    fn e2e_shutdown_neutralized_row_cannot_clamp_safepoint() {
        let life_time_sec = 600;
        let now = tso(10_000_000);

        // Before shutdown: instance has an active old txn.
        let before = live_state("inst-1", Some(tso(9_000_000)), 10_000_000);
        let safepoint_before =
            compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[before]);
        assert_eq!(safepoint_before, tso(9_000_000) - 1);

        // Shutdown publishes min_start_ts=None (neutralize).
        let neutralized = live_state("inst-1", None, 10_000_000);
        let safepoint_after =
            compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[neutralized]);

        // Neutralized row doesn't clamp — safepoint is purely time-based.
        let time_based = compute_safepoint_version(now, life_time_sec);
        assert_eq!(safepoint_after, time_based);
        assert!(safepoint_after > tso(9_000_000));
    }

    // ── Scenario 9: Mixed connections + workers + quarantine ────

    #[test]
    fn e2e_mixed_connections_workers_quarantine_safepoint_is_global_min() {
        let registry = Arc::new(ActiveTxnRegistry::new());
        let life_time_sec = 600;
        let now = tso(10_000_000);

        // Session connection: start_ts = 9.1M (old, outside gc_life_time)
        registry.register_connection(1, tso(9_100_000));

        // Worker guard: start_ts = 9.2M
        let _guard = registry.track_worker_txn(tso(9_200_000));

        // Quarantined worker: start_ts = 9.0M (oldest)
        {
            let mut old_guard = registry.track_worker_txn(tso(9_000_000));
            old_guard.quarantine();
        }

        // min_start_ts should be the quarantined entry (oldest).
        assert_eq!(registry.min_start_ts(), Some(tso(9_000_000)));

        let state = live_state("inst-1", registry.min_start_ts(), 10_000_000);
        let safepoint = compute_cluster_gc_safepoint(now, life_time_sec, life_time_sec, &[state]);
        assert_eq!(safepoint, tso(9_000_000) - 1);
    }

    // ── Scenario 10: DDL txn rotation refreshes registration ────

    #[test]
    fn e2e_txn_rotation_updates_registry_to_new_start_ts() {
        let registry = Arc::new(ActiveTxnRegistry::new());

        // Session opens txn with old start_ts.
        let old_ts = tso(9_000_000);
        registry.register_connection(10, old_ts);
        assert_eq!(registry.min_start_ts(), Some(old_ts));

        // DDL rotation: commit old, begin new with fresh start_ts.
        // (Simulates begin_replacement_session_owned_txn)
        registry.unregister_connection(10); // clear old
        let new_ts = tso(9_999_000);
        registry.register_connection(10, new_ts); // refresh

        assert_eq!(registry.min_start_ts(), Some(new_ts));
        assert!(new_ts > old_ts, "refreshed start_ts should be newer");
    }

    // ── Scenario 11: Missed heartbeat exposes live txn ──────────

    #[test]
    fn e2e_missed_heartbeat_exposes_live_txn_to_gc() {
        let life_time_sec = 600;
        // Heartbeat timeout = 3 * publish_interval.
        // With default gc_safepoint_interval_sec=300, timeout=900s.
        // For this test we use a smaller value to keep the scenario compact.
        let hb_timeout_sec: u64 = 900; // 3 * 300
        let heartbeat_timeout_ms = hb_timeout_sec * 1000;
        let publish_time_ms: u64 = 10_000_000;
        let txn_start_ts = tso(9_000_000);

        let state = GcInstanceState {
            instance_id: "inst-stuck".to_string(),
            min_start_ts: Some(txn_start_ts),
            updated_at_version: tso(publish_time_ms),
            legacy_max_untracked_timeout_sec: None,
        };

        // Phase 1: t=(timeout - 1s) — just inside heartbeat timeout, txn IS protected.
        let now_inside = tso(publish_time_ms + heartbeat_timeout_ms - 1000);
        assert!(is_live_gc_instance_state(
            now_inside,
            hb_timeout_sec,
            &state
        ));
        let sp_inside = compute_cluster_gc_safepoint(
            now_inside,
            life_time_sec,
            hb_timeout_sec,
            std::slice::from_ref(&state),
        );
        assert_eq!(
            sp_inside,
            txn_start_ts - 1,
            "txn must be protected while live"
        );

        // Phase 2: t=(timeout + 1s) — just outside heartbeat timeout (missed heartbeat).
        let now_outside = tso(publish_time_ms + heartbeat_timeout_ms + 1000);
        assert!(!is_live_gc_instance_state(
            now_outside,
            hb_timeout_sec,
            &state
        ));
        let sp_outside = compute_cluster_gc_safepoint(
            now_outside,
            life_time_sec,
            hb_timeout_sec,
            std::slice::from_ref(&state),
        );
        assert!(
            sp_outside > txn_start_ts,
            "VULNERABILITY: safepoint {sp_outside} exceeds live txn {txn_start_ts} \
             — missed heartbeat exposed the txn to GC"
        );
    }

    // ── Scenario 12: Exact heartbeat timeout boundary (>= edge) ─

    #[test]
    fn e2e_missed_heartbeat_boundary_exact_life_time_edge() {
        let life_time_sec = 600;
        // Heartbeat timeout derived from 3 * gc_safepoint_interval_sec.
        let hb_timeout_sec: u64 = 900;
        let heartbeat_timeout_ms = hb_timeout_sec * 1000;
        let publish_time_ms: u64 = 10_000_000;
        let txn_start_ts = tso(9_000_000);

        let state = GcInstanceState {
            instance_id: "inst-edge".to_string(),
            min_start_ts: Some(txn_start_ts),
            updated_at_version: tso(publish_time_ms),
            legacy_max_untracked_timeout_sec: None,
        };

        // Exact boundary: updated_at + heartbeat_timeout — still live (>= check).
        let now_exact = tso(publish_time_ms + heartbeat_timeout_ms);
        assert!(is_live_gc_instance_state(now_exact, hb_timeout_sec, &state));
        let sp_exact = compute_cluster_gc_safepoint(
            now_exact,
            life_time_sec,
            hb_timeout_sec,
            std::slice::from_ref(&state),
        );
        assert_eq!(sp_exact, txn_start_ts - 1, "protected at exact boundary");

        // One ms past boundary — stale.
        let now_past = tso(publish_time_ms + heartbeat_timeout_ms + 1);
        assert!(!is_live_gc_instance_state(now_past, hb_timeout_sec, &state));
        let sp_past =
            compute_cluster_gc_safepoint(now_past, life_time_sec, hb_timeout_sec, &[state]);
        assert!(sp_past > txn_start_ts, "1ms past boundary: txn exposed");
    }

    // ── Scenario 13: Multi-instance, one stale ──────────────────

    #[test]
    fn e2e_missed_heartbeat_multi_instance_one_stale_exposes_its_txn() {
        let life_time_sec = 600;
        let hb_timeout_sec: u64 = life_time_sec; // use life_time as timeout for this test
        let now = tso(10_601_000); // 601s after inst-a's last heartbeat
        let txn_a = tso(9_000_000);
        let txn_b = tso(9_500_000);

        // inst-a: stale (last heartbeat 601s ago, > hb_timeout_sec)
        let inst_a = GcInstanceState {
            instance_id: "inst-a".to_string(),
            min_start_ts: Some(txn_a),
            updated_at_version: tso(10_000_000),
            legacy_max_untracked_timeout_sec: None,
        };
        // inst-b: live (just heartbeated)
        let inst_b = live_state("inst-b", Some(txn_b), 10_601_000);

        assert!(!is_live_gc_instance_state(now, hb_timeout_sec, &inst_a));
        assert!(is_live_gc_instance_state(now, hb_timeout_sec, &inst_b));

        let sp =
            compute_cluster_gc_safepoint(now, life_time_sec, hb_timeout_sec, &[inst_a, inst_b]);
        assert_eq!(sp, txn_b - 1, "clamped to live inst-b only");
        assert!(sp > txn_a, "inst-a's txn exposed: its row went stale");
        assert!(sp < txn_b, "inst-b's txn still protected");
    }

    // ── Contract: publisher retries on failure ───────────────────

    #[test]
    fn gc_publisher_loop_retries_on_failure_with_backoff() {
        let source = include_str!("gc.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("gc.rs must contain #[cfg(test)]");
        let publisher_fn = prod_source
            .split("pub async fn run_gc_publisher_loop")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub async fn publish_gc_instance_state_once")
                    .next()
            })
            .expect("run_gc_publisher_loop must exist");

        assert!(
            publisher_fn.contains("backoff"),
            "publisher must retry with exponential backoff on failure, not wait a full interval"
        );
        assert!(
            publisher_fn.contains("tokio::time::sleep(backoff)"),
            "publisher retry must use sleep-based backoff between attempts"
        );
    }

    // ── heartbeat_timeout_sec adapts to config ──────────────────

    #[test]
    fn heartbeat_timeout_is_3x_publish_interval() {
        let mut config = WorkerConfig::default();
        assert_eq!(config.gc_safepoint_interval_sec, 300);
        assert_eq!(heartbeat_timeout_sec(&config), 900);

        config.gc_safepoint_interval_sec = 60;
        assert_eq!(heartbeat_timeout_sec(&config), 180);

        config.gc_safepoint_interval_sec = 30; // minimum allowed
        assert_eq!(heartbeat_timeout_sec(&config), 90);
    }

    #[test]
    fn heartbeat_timeout_always_less_than_default_gc_life_time() {
        // The config validator enforces gc_life_time >= 3 * interval,
        // so heartbeat_timeout (= 3 * interval) <= gc_life_time.
        let config = WorkerConfig::default();
        assert!(
            heartbeat_timeout_sec(&config) <= config.gc_life_time_sec,
            "heartbeat_timeout {} must not exceed gc_life_time {}",
            heartbeat_timeout_sec(&config),
            config.gc_life_time_sec,
        );
    }

    #[test]
    fn heartbeat_timeout_exceeds_publish_interval() {
        // Core invariant: timeout > interval, otherwise healthy instances look dead.
        let config = WorkerConfig::default();
        assert!(
            heartbeat_timeout_sec(&config) > config.gc_safepoint_interval_sec,
            "heartbeat_timeout {} must exceed publish interval {}",
            heartbeat_timeout_sec(&config),
            config.gc_safepoint_interval_sec,
        );
    }
}
