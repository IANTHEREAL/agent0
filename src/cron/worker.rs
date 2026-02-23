use crate::cron::config::CronConfig;
use crate::cron::types::CronRunStatus;
use crate::storage::TikvStore;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::info;

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

    let mut txn = store.begin().await?;
    let gc_result = async {
        if !store.is_cron_enabled(&mut txn, db_id).await? {
            return Ok((0usize, 0usize));
        }

        let runs = store
            .list_all_cron_runs(&mut txn, db_id, usize::MAX)
            .await?;
        if runs.is_empty() {
            return Ok((0usize, 0usize));
        }

        // Load per-job max_runtime_ms overrides so we don't orphan
        // legitimately long-running jobs whose timeout exceeds the global default.
        let jobs = store.list_cron_jobs(&mut txn, db_id).await?;
        let job_max_runtime: HashMap<i64, Option<u64>> = jobs
            .into_iter()
            .map(|j| (j.job_id, j.max_runtime_ms))
            .collect();

        let mut recovered = 0usize;
        let mut deleted = 0usize;
        let mut job_ids = HashSet::new();
        let mut keep_by_job: HashMap<i64, Vec<crate::cron::types::CronRun>> = HashMap::new();

        for mut run in runs {
            job_ids.insert(run.job_id);

            if run.status == CronRunStatus::Running {
                if let Some(start_ms) = run.start_time {
                    // Per-job cutoff: if the job has max_runtime_ms that exceeds the
                    // global orphan timeout, use the job's value so we don't mark a
                    // legitimately long-running job as orphaned.
                    let job_runtime_ms = job_max_runtime.get(&run.job_id).copied().flatten();
                    let effective_cutoff =
                        orphan_cutoff_for_run(now, global_orphan_timeout_ms, job_runtime_ms);
                    if start_ms < effective_cutoff {
                        run.status = CronRunStatus::Failed;
                        run.return_message =
                            Some("orphan recovery: execution timed out".to_string());
                        run.end_time = Some(now);
                        recovered = recovered.saturating_add(1);
                    }
                }
            }

            let record_ts = run.end_time.or(run.start_time).unwrap_or(i64::MAX);
            if record_ts < retention_cutoff {
                deleted = deleted.saturating_add(1);
                continue;
            }

            keep_by_job.entry(run.job_id).or_default().push(run);
        }

        if recovered == 0 && deleted == 0 {
            return Ok((0usize, 0usize));
        }

        for job_id in job_ids {
            store
                .delete_cron_runs_for_job(&mut txn, db_id, job_id)
                .await?;
        }

        for kept_runs in keep_by_job.into_values() {
            for run in kept_runs {
                store.put_cron_run(&mut txn, db_id, &run).await?;
            }
        }

        Ok((recovered, deleted))
    }
    .await;

    match gc_result {
        Ok((recovered, deleted)) => {
            txn.commit().await?;
            if recovered > 0 || deleted > 0 {
                info!(
                    "cron GC db_id={} recovered_orphans={} deleted_runs={}",
                    db_id, recovered, deleted
                );
            }
            Ok(())
        }
        Err(e) => {
            txn.rollback().await.ok();
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
}
