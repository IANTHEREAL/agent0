use crate::cron::config::CronConfig;
use crate::cron::types::CronRunStatus;
use crate::sql::executor::core::retry::is_retryable_tikv_error;
use crate::storage::TikvStore;
use crate::worker::engine::{
    is_retryable_region_error, region_error_backoff, REGION_ERROR_MAX_RETRIES,
};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::TimestampExt;
use tracing::{info, warn};

/// GC batch size — number of cron runs to process per transaction.
/// Keeps pessimistic lock hold time bounded regardless of total run count.
const GC_BATCH_SIZE: usize = 500;

/// Maximum retries per batch for transient TiKV errors (region routing +
/// write conflict / deadlock).
const GC_BATCH_MAX_RETRIES: u32 = REGION_ERROR_MAX_RETRIES;

pub(crate) async fn gc_database(
    store: &Arc<TikvStore>,
    db_id: u64,
    config: &CronConfig,
) -> Result<()> {
    let now = now_ms();
    let global_orphan_timeout_ms =
        i64::try_from(config.orphan_timeout_sec.saturating_mul(1000)).unwrap_or(i64::MAX);
    let retention_cutoff = now.saturating_sub(
        i64::try_from(
            config
                .run_retention_days
                .saturating_mul(24)
                .saturating_mul(3600)
                .saturating_mul(1000),
        )
        .unwrap_or(i64::MAX),
    );

    // Pre-check: if cron is disabled for this database, skip immediately.
    {
        let mut check_txn = store.begin().await?;
        let enabled = store.is_cron_enabled(&mut check_txn, db_id).await?;
        check_txn.rollback().await.ok();
        if !enabled {
            return Ok(());
        }
    }

    // Load per-job max_runtime_ms overrides once (read-only, separate txn).
    let job_max_runtime: HashMap<i64, Option<u64>> = {
        let mut jobs_txn = store.begin().await?;
        let jobs = store.list_cron_jobs(&mut jobs_txn, db_id).await?;
        jobs_txn.rollback().await.ok();
        jobs.into_iter()
            .map(|j| (j.job_id, j.max_runtime_ms))
            .collect()
    };

    let mut total_recovered = 0usize;
    let mut total_deleted = 0usize;
    let mut cursor: Option<Vec<u8>> = None;

    loop {
        let batch_result = gc_database_batch(
            store,
            db_id,
            now,
            global_orphan_timeout_ms,
            retention_cutoff,
            &job_max_runtime,
            cursor.as_deref(),
        )
        .await;

        match batch_result {
            Ok(batch) => {
                total_recovered = total_recovered.saturating_add(batch.recovered);
                total_deleted = total_deleted.saturating_add(batch.deleted);
                if batch.is_final {
                    break;
                }
                cursor = batch.last_key;
            }
            Err(e) => {
                // Non-retryable error — propagate.
                return Err(e);
            }
        }
    }

    if total_recovered > 0 || total_deleted > 0 {
        info!(
            db_id,
            recovered_orphans = total_recovered,
            deleted_runs = total_deleted,
            "cron GC complete"
        );
    }
    Ok(())
}

struct GcBatchResult {
    recovered: usize,
    deleted: usize,
    last_key: Option<Vec<u8>>,
    is_final: bool,
}

/// Process one batch of cron runs with retry on transient TiKV errors.
async fn gc_database_batch(
    store: &Arc<TikvStore>,
    db_id: u64,
    now: i64,
    global_orphan_timeout_ms: i64,
    retention_cutoff: i64,
    job_max_runtime: &HashMap<i64, Option<u64>>,
    start_after: Option<&[u8]>,
) -> Result<GcBatchResult> {
    for attempt in 0..=GC_BATCH_MAX_RETRIES {
        let result = gc_database_batch_inner(
            store,
            db_id,
            now,
            global_orphan_timeout_ms,
            retention_cutoff,
            job_max_runtime,
            start_after,
        )
        .await;

        match result {
            Ok(batch) => return Ok(batch),
            Err(e)
                if attempt < GC_BATCH_MAX_RETRIES
                    && (is_retryable_region_error(&e) || is_retryable_tikv_error(&e)) =>
            {
                warn!(
                    db_id,
                    attempt = attempt + 1,
                    max_retries = GC_BATCH_MAX_RETRIES,
                    "cron GC batch: transient error, retrying with fresh txn"
                );
                tracing::debug!(db_id, "cron GC batch error detail: {e}");
                region_error_backoff(attempt).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Inner batch logic: scan one page, point-delete expired runs, update
/// orphan-recovered runs. Uses a single short-lived pessimistic transaction.
async fn gc_database_batch_inner(
    store: &Arc<TikvStore>,
    db_id: u64,
    now: i64,
    global_orphan_timeout_ms: i64,
    retention_cutoff: i64,
    job_max_runtime: &HashMap<i64, Option<u64>>,
    start_after: Option<&[u8]>,
) -> Result<GcBatchResult> {
    let mut txn = store.begin().await?;
    let mut txn_guard = crate::worker::active_txn_registry::global_registry()
        .map(|registry| registry.track_worker_txn(txn.start_timestamp().version()));

    let batch_result = async {
        let (runs, raw_keys) = store
            .list_cron_runs_batch(&mut txn, db_id, start_after, GC_BATCH_SIZE)
            .await?;

        if runs.is_empty() {
            return Ok(GcBatchResult {
                recovered: 0,
                deleted: 0,
                last_key: None,
                is_final: true,
            });
        }

        let is_final = runs.len() < GC_BATCH_SIZE;
        let last_key = raw_keys.last().cloned();

        let mut recovered = 0usize;
        let mut deleted = 0usize;

        for (run, raw_key) in runs.into_iter().zip(raw_keys) {
            let mut needs_update = false;
            let mut should_delete = false;
            let mut updated_run = run;

            // Orphan recovery: Running → Failed
            if updated_run.status == CronRunStatus::Running {
                if let Some(start_ms) = updated_run.start_time {
                    let job_runtime_ms =
                        job_max_runtime.get(&updated_run.job_id).copied().flatten();
                    let effective_cutoff =
                        orphan_cutoff_for_run(now, global_orphan_timeout_ms, job_runtime_ms);
                    if start_ms < effective_cutoff {
                        updated_run.status = CronRunStatus::Failed;
                        updated_run.return_message =
                            Some("orphan recovery: execution timed out".to_string());
                        updated_run.end_time = Some(now);
                        recovered = recovered.saturating_add(1);
                        needs_update = true;
                    }
                }
            }

            // Retention check
            let record_ts = updated_run
                .end_time
                .or(updated_run.start_time)
                .unwrap_or(i64::MAX);
            if record_ts < retention_cutoff {
                should_delete = true;
                deleted = deleted.saturating_add(1);
            }

            // Apply mutations: point-delete expired, update orphan-recovered.
            if should_delete {
                store.delete_cron_run_by_key(&mut txn, raw_key).await?;
            } else if needs_update {
                store.put_cron_run(&mut txn, db_id, &updated_run).await?;
            }
            // Runs that are neither expired nor orphan-recovered are untouched —
            // no lock acquired on them.
        }

        Ok(GcBatchResult {
            recovered,
            deleted,
            last_key,
            is_final,
        })
    }
    .await;

    match batch_result {
        Ok(batch) => {
            if batch.recovered > 0 || batch.deleted > 0 {
                store
                    .assert_database_alive_for_update(&mut txn, db_id)
                    .await?;
                txn.commit().await?;
            } else {
                txn.rollback().await.ok();
            }
            Ok(batch)
        }
        Err(e) => {
            if txn.rollback().await.is_err() {
                if let Some(g) = txn_guard.as_mut() {
                    g.quarantine();
                }
            }
            Err(e)
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Compute the orphan cutoff for a specific run, accounting for per-job max_runtime_ms.
/// Returns the timestamp below which a running run should be considered orphaned.
pub(crate) fn orphan_cutoff_for_run(
    now: i64,
    global_orphan_timeout_ms: i64,
    job_max_runtime_ms: Option<u64>,
) -> i64 {
    match job_max_runtime_ms {
        Some(job_ms) => {
            let job_ms_i64 = i64::try_from(job_ms).unwrap_or(i64::MAX);
            let effective_ms = global_orphan_timeout_ms.max(job_ms_i64);
            now.saturating_sub(effective_ms)
        }
        None => now.saturating_sub(global_orphan_timeout_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orphan_cutoff_uses_global_when_no_per_job_override() {
        let now = 1_700_000_000_000i64;
        let global_ms = 300_000i64; // 5 min

        let cutoff = orphan_cutoff_for_run(now, global_ms, None);
        assert_eq!(cutoff, now - 300_000);
    }

    #[test]
    fn test_orphan_cutoff_uses_per_job_when_larger_than_global() {
        let now = 1_700_000_000_000i64;
        let global_ms = 300_000i64; // 5 min
        let job_ms = 600_000u64; // 10 min — exceeds global

        let cutoff = orphan_cutoff_for_run(now, global_ms, Some(job_ms));
        // Should use the job's 10 min, not the global 5 min
        assert_eq!(cutoff, now - 600_000);
    }

    #[test]
    fn test_orphan_cutoff_uses_global_when_larger_than_per_job() {
        let now = 1_700_000_000_000i64;
        let global_ms = 600_000i64; // 10 min
        let job_ms = 120_000u64; // 2 min — smaller than global

        let cutoff = orphan_cutoff_for_run(now, global_ms, Some(job_ms));
        // Should still use the global 10 min (more lenient)
        assert_eq!(cutoff, now - 600_000);
    }

    #[test]
    fn test_per_job_override_prevents_false_orphan() {
        let now = 1_700_000_000_000i64;
        let global_ms = 300_000i64; // 5 min orphan timeout
        let job_ms = 600_000u64; // 10 min per-job max_runtime

        let cutoff = orphan_cutoff_for_run(now, global_ms, Some(job_ms));

        // A run started 7 min ago — past global timeout but within per-job timeout
        let start_7min_ago = now - 420_000;
        assert!(
            start_7min_ago >= cutoff,
            "run within per-job max_runtime_ms must NOT be orphaned"
        );

        // A run started 11 min ago — past both timeouts
        let start_11min_ago = now - 660_000;
        assert!(
            start_11min_ago < cutoff,
            "run exceeding per-job max_runtime_ms should be orphaned"
        );
    }

    #[test]
    fn test_orphan_cutoff_u64_overflow_clamps_to_max() {
        let now = 1_700_000_000_000i64;
        let global_ms = 300_000i64;
        // u64 value exceeding i64::MAX must not wrap negative
        let huge_ms = u64::MAX;

        let cutoff = orphan_cutoff_for_run(now, global_ms, Some(huge_ms));
        // i64::MAX is far larger than now, so saturating_sub yields i64::MIN (effectively never orphan)
        assert!(
            cutoff <= 0,
            "overflow u64 must clamp to i64::MAX, producing a cutoff no run can exceed"
        );
    }

    #[test]
    fn gc_database_batch_tracks_its_transaction() {
        let source = include_str!("worker.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("cron/worker.rs must contain #[cfg(test)]");
        let batch_fn = prod_source
            .split("async fn gc_database_batch_inner(")
            .nth(1)
            .expect("gc_database_batch_inner must exist");

        assert!(
            batch_fn.contains("track_worker_txn(txn.start_timestamp().version())"),
            "gc_database_batch_inner must register with the GC safepoint to \
             prevent GC overrun while the batch transaction is active"
        );
    }

    #[test]
    fn gc_uses_point_deletes_not_delete_all_reinsert() {
        // Guard: gc_database_batch_inner must use point-delete (delete_cron_run_by_key)
        // and NOT the old delete-all-reinsert pattern (delete_cron_runs_for_job).
        let source = include_str!("worker.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("cron/worker.rs must contain #[cfg(test)]");
        let batch_fn = prod_source
            .split("async fn gc_database_batch_inner(")
            .nth(1)
            .expect("gc_database_batch_inner must exist");

        assert!(
            batch_fn.contains("delete_cron_run_by_key"),
            "gc_database_batch_inner must use point-deletes (delete_cron_run_by_key) \
             to minimize pessimistic lock acquisitions"
        );
        assert!(
            !batch_fn.contains("delete_cron_runs_for_job"),
            "gc_database_batch_inner must NOT use delete_cron_runs_for_job — \
             that pattern locks all keys including unchanged survivors"
        );
    }
}
