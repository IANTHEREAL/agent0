use crate::cron::config::CronConfig;
use crate::cron::worker::gc_database;
use crate::pool::TikvClientPool;
use crate::storage::TikvStore;
use crate::worker::config::WorkerConfig;
use crate::worker::metrics::WorkerMetrics;
use crate::worker::now_epoch_ms;
use anyhow::Result;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tikv_client::{Timestamp, TimestampExt};
use tracing::{debug, info, warn};

/// Timeout for PD TSO requests in GC loops. We route this through the
/// tikv-client timeout API so GC gets a bounded wait on a dedicated TSO stream
/// without disturbing the shared timestamp stream used by foreground traffic.
const TSO_TIMEOUT_SEC: u64 = 30;

pub struct WorkerGc {
    system_store: Arc<TikvStore>,
    pool: Arc<TikvClientPool>,
    config: WorkerConfig,
    metrics: Arc<WorkerMetrics>,
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
    pub fn spawn_worker_gc_only(self: Arc<Self>) {
        let gc_self = self.clone();
        tokio::spawn(async move { gc_self.run_gc_loop().await });
        tokio::spawn(async move { self.run_hnsw_sweep_loop().await });
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
    let jitter = rand_jitter_secs(60);
    tokio::time::sleep(Duration::from_secs(jitter)).await;

    info!(
        interval_sec = config.gc_safepoint_interval_sec,
        "GC registry publisher started (unconditional)"
    );

    let mut interval = tokio::time::interval(Duration::from_secs(config.gc_safepoint_interval_sec));
    loop {
        interval.tick().await;
        if let Err(e) = publish_gc_instance_state(store, config).await {
            warn!("GC registry publish failed: {}", e);
        }
    }
}

/// GC safepoint advancer loop — OPTIONAL, controlled by gc_safepoint_enabled.
/// Reads all instances' states, computes global min, advances PD safepoint.
pub async fn run_gc_advancer_loop(
    store: &TikvStore,
    config: &WorkerConfig,
    metrics: &WorkerMetrics,
) {
    let jitter = rand_jitter_secs(60);
    tokio::time::sleep(Duration::from_secs(jitter)).await;

    info!(
        interval_sec = config.gc_safepoint_interval_sec,
        life_time_sec = config.gc_life_time_sec,
        "GC safepoint advancer started"
    );

    let mut interval = tokio::time::interval(Duration::from_secs(config.gc_safepoint_interval_sec));
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

async fn publish_gc_instance_state(store: &TikvStore, config: &WorkerConfig) -> Result<()> {
    let client = store
        .transaction_client()
        .ok_or_else(|| anyhow::anyhow!("no TransactionClient available"))?;

    let current_ts = client
        .current_timestamp_with_timeout(Duration::from_secs(TSO_TIMEOUT_SEC))
        .await
        .map_err(|e| anyhow::anyhow!("failed to get current timestamp from PD: {}", e))?;

    let local_min =
        crate::worker::active_txn_registry::global_registry().and_then(|r| r.min_start_ts());

    // Publish this instance's max untracked transaction timeout so advancers
    // on other nodes can use it as a gc_life_time floor.
    let max_untracked_timeout_sec = if config.enabled {
        let cron_sec = config.cron_job_timeout_ms.saturating_add(999) / 1000;
        let stmt_sec = config.statement_timeout_ms.saturating_add(999) / 1000;
        cron_sec.max(stmt_sec)
    } else {
        0 // Non-worker nodes have no untracked transactions
    };

    let mut txn = store.begin().await?;
    store
        .put_gc_instance_state(
            &mut txn,
            &config.worker_id,
            local_min,
            current_ts.version(),
            max_untracked_timeout_sec,
        )
        .await?;
    txn.commit().await?;
    Ok(())
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
    let mut global_max_timeout_sec = 0u64;
    let stale_threshold_version =
        compute_safepoint_version(current_version, config.gc_life_time_sec);

    let all_states = {
        let mut txn = store.begin().await?;
        let states = store.scan_gc_instance_states(&mut txn).await?;
        txn.rollback().await.ok();
        states
    };

    // First pass: compute effective gc_life_time from cluster-wide max timeout.
    for state in &all_states {
        if state.updated_at_version >= stale_threshold_version {
            global_max_timeout_sec = global_max_timeout_sec.max(state.max_untracked_timeout_sec);
        }
    }

    // Effective gc_life_time: at least cover the longest untracked transaction
    // timeout across ALL live instances in the cluster.
    let effective_life_time_sec = config.gc_life_time_sec.max(global_max_timeout_sec);
    let mut safepoint_version = compute_safepoint_version(current_version, effective_life_time_sec);

    if effective_life_time_sec > config.gc_life_time_sec {
        info!(
            local_life_time = config.gc_life_time_sec,
            cluster_max_timeout = global_max_timeout_sec,
            effective_life_time = effective_life_time_sec,
            "GC life_time raised to cover cluster-wide untracked transaction timeout"
        );
    }

    // Second pass: clamp safepoint to protect active tracked transactions.
    for state in &all_states {
        if state.updated_at_version < stale_threshold_version {
            debug!(
                instance_id = state.instance_id,
                updated_at = state.updated_at_version,
                "Ignoring stale GC instance state"
            );
            continue;
        }
        if let Some(ts) = state.min_start_ts {
            let txn_floor = ts.saturating_sub(1);
            if txn_floor < safepoint_version {
                info!(
                    instance_id = state.instance_id,
                    min_active_start_ts = ts,
                    time_based = safepoint_version,
                    clamped_to = txn_floor,
                    "GC safepoint clamped by active transaction on instance"
                );
                safepoint_version = txn_floor;
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
    match client.update_safepoint(safepoint).await {
        Ok(accepted) => {
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
        Err(e) => Err(anyhow::anyhow!("update_safepoint failed: {}", e)),
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
}
