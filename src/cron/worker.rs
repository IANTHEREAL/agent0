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
    let orphan_cutoff = now.saturating_sub(
        i64::try_from(config.orphan_timeout_sec.saturating_mul(1000)).unwrap_or(i64::MAX),
    );
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

        let mut recovered = 0usize;
        let mut deleted = 0usize;
        let mut job_ids = HashSet::new();
        let mut keep_by_job: HashMap<i64, Vec<crate::cron::types::CronRun>> = HashMap::new();

        for mut run in runs {
            job_ids.insert(run.job_id);

            if run.status == CronRunStatus::Running {
                if let Some(start_ms) = run.start_time {
                    if start_ms < orphan_cutoff {
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

pub(crate) fn success_message(completed_commands: usize) -> String {
    if completed_commands == 1 {
        "1 command completed".to_string()
    } else {
        format!("{} commands completed", completed_commands)
    }
}
