use crate::extensions::context::{with_context_opts, ExtensionContextOpts};
use crate::observability;
use crate::pool::TikvClientPool;
use crate::cron::types::{CronRun, CronRunStatus};
use crate::sql::parse_sql;
use crate::sql::Executor;
use crate::storage::TikvStore;
use crate::worker::config::WorkerConfig;
use crate::worker::types::*;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, warn};

pub struct WorkerEngine {
    config: WorkerConfig,
    system_store: Arc<TikvStore>,
    pool: Arc<TikvClientPool>,
    active_jobs: Arc<AtomicU32>,
    semaphore: Arc<Semaphore>,
}

impl WorkerEngine {
    pub fn new(config: WorkerConfig, system_store: Arc<TikvStore>, pool: Arc<TikvClientPool>) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs));
        Self {
            config,
            system_store,
            pool,
            active_jobs: Arc::new(AtomicU32::new(0)),
            semaphore,
        }
    }

    pub async fn run(&self) {
        info!(
            "WorkerEngine starting (poll_ms={}, max_concurrent={})",
            self.config.poll_ms, self.config.max_concurrent_jobs
        );

        let mut interval = tokio::time::interval(Duration::from_millis(self.config.poll_ms));
        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                warn!("Worker tick error: {}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let now_ms = chrono::Utc::now().timestamp_millis();

        let mut txn = self.system_store.begin().await?;
        let due_entries = self
            .system_store
            .scan_due_queue_entries(&mut txn, now_ms, 1000)
            .await?;
        txn.commit().await?;

        if due_entries.is_empty() {
            return Ok(());
        }

        let mut join_set = JoinSet::new();
        for (key, entry) in due_entries {
            if self.active_jobs.load(Ordering::Relaxed) >= self.config.max_concurrent_jobs as u32 {
                break;
            }

            let permit = match self.semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => break,
            };

            let engine_system_store = self.system_store.clone();
            let engine_pool = self.pool.clone();
            let engine_config = self.config.clone();
            let active_jobs = self.active_jobs.clone();

            join_set.spawn(async move {
                let _permit = permit;
                active_jobs.fetch_add(1, Ordering::Relaxed);
                let result =
                    Self::claim_and_execute(&engine_system_store, &engine_pool, &engine_config, key, entry)
                        .await;
                active_jobs.fetch_sub(1, Ordering::Relaxed);

                if let Err(ref e) = result {
                    warn!("Worker task execution error: {}", e);
                }

                result
            });
        }

        while let Some(result) = join_set.join_next().await {
            if let Err(e) = result {
                warn!("Worker task join error: {}", e);
            }
        }

        Ok(())
    }

    async fn claim_and_execute(
        system_store: &Arc<TikvStore>,
        pool: &Arc<TikvClientPool>,
        config: &WorkerConfig,
        queue_key: Vec<u8>,
        entry: TaskQueueEntry,
    ) -> Result<()> {
        let fire_time_min = chrono::Utc::now().timestamp() / 60;
        let claim = WorkerClaim::new(config.worker_id.clone(), entry.task_type);

        let mut txn = system_store.begin().await?;
        let claimed = system_store
            .try_claim_worker_task(
                &mut txn,
                &entry.keyspace,
                entry.db_id,
                entry.task_id,
                fire_time_min,
                &claim,
            )
            .await?;

        if !claimed {
            txn.rollback().await.ok();
            return Ok(());
        }
        txn.commit().await?;

        let cron_run = if entry.task_type == TaskType::Cron {
            Self::claim_and_record_cron_run(pool, &entry, fire_time_min).await?
        } else {
            None
        };

        let exec_result = if entry.task_type == TaskType::Cron && cron_run.is_none() {
            Ok(0usize)
        } else {
            Self::execute_task(pool, config, &entry).await
        };

        if let Some((store, db_id, mut run, started_at)) = cron_run {
            let (status, message) = match &exec_result {
                Ok(completed_commands) => (
                    CronRunStatus::Succeeded,
                    Some(success_message(*completed_commands)),
                ),
                Err(e) => (CronRunStatus::Failed, Some(e.to_string())),
            };

            Self::finalize_cron_run(
                &store,
                db_id,
                &mut run,
                status,
                message,
                started_at,
                now_ms(),
            )
            .await?;
        }

        let mut txn = system_store.begin().await?;
        system_store
            .delete_worker_claim(
                &mut txn,
                &entry.keyspace,
                entry.db_id,
                entry.task_id,
                fire_time_min,
            )
            .await?;
        system_store
            .delete_worker_queue_entry(&mut txn, &queue_key)
            .await?;

        if entry.task_type == TaskType::Cron {
            if let Some(ref schedule) = entry.schedule {
                if let Ok(next_fire) = compute_next_fire_time(schedule) {
                    system_store
                        .put_worker_queue_entry(&mut txn, &entry, next_fire)
                        .await?;
                }
            }
        }
        txn.commit().await?;

        match exec_result {
            Ok(_) => info!(
                "Worker task completed: keyspace={} db_id={} task_id={} type={:?}",
                entry.keyspace, entry.db_id, entry.task_id, entry.task_type
            ),
            Err(ref e) => warn!(
                "Worker task failed: keyspace={} db_id={} task_id={} type={:?} error={}",
                entry.keyspace, entry.db_id, entry.task_id, entry.task_type, e
            ),
        }

        Ok(())
    }

    async fn claim_and_record_cron_run(
        pool: &Arc<TikvClientPool>,
        entry: &TaskQueueEntry,
        scheduled_minute: i64,
    ) -> Result<Option<(Arc<TikvStore>, u64, CronRun, i64)>> {
        let handle = pool.acquire(Some(entry.keyspace.clone())).await?;
        let store = handle.store().clone();
        let mut txn = store.begin().await?;

        let claim_result = async {
            if !store.is_cron_enabled(&mut txn, entry.db_id).await? {
                return Ok(None);
            }

            let claimed = store
                .try_claim_cron_run(&mut txn, entry.db_id, entry.task_id, scheduled_minute)
                .await?;
            if !claimed {
                return Ok(None);
            }

            let database = store
                .get_database_by_id(&mut txn, entry.db_id)
                .await?
                .map(|db| db.name)
                .unwrap_or_else(|| "postgres".to_string());

            let run_id = store.next_cron_run_id(entry.db_id).await?;
            let started_at = now_ms();
            let run = CronRun {
                run_id,
                job_id: entry.task_id,
                job_pid: None,
                database,
                username: entry.username.clone(),
                command: entry.command.clone(),
                status: CronRunStatus::Running,
                return_message: None,
                start_time: Some(started_at),
                end_time: None,
            };
            store.put_cron_run(&mut txn, entry.db_id, &run).await?;
            Ok(Some((store, entry.db_id, run, started_at)))
        }
        .await;

        match claim_result {
            Ok(Some(run)) => {
                txn.commit().await?;
                Ok(Some(run))
            }
            Ok(None) => {
                txn.rollback().await.ok();
                Ok(None)
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e)
            }
        }
    }

    async fn finalize_cron_run(
        store: &Arc<TikvStore>,
        db_id: u64,
        run: &mut CronRun,
        status: CronRunStatus,
        return_message: Option<String>,
        start_time: i64,
        end_time: i64,
    ) -> Result<()> {
        let mut txn = store.begin().await?;
        let finalize_result = async {
            run.status = status;
            run.return_message = return_message;
            run.start_time = Some(start_time);
            run.end_time = Some(end_time);
            store.put_cron_run(&mut txn, db_id, run).await
        }
        .await;

        match finalize_result {
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

    async fn execute_task(
        pool: &Arc<TikvClientPool>,
        _config: &WorkerConfig,
        entry: &TaskQueueEntry,
    ) -> Result<usize> {
        let handle = pool.acquire(Some(entry.keyspace.clone())).await?;
        let store = handle.store().clone();
        let exec = Executor::new(
            store.clone(),
            entry.keyspace.clone(),
            observability::registry().tenant(&entry.keyspace),
            handle.trigger_cache().clone(),
            handle.stats_cache().clone(),
        );

        let search_path = if entry.task_type == TaskType::Cron {
            vec!["public".to_string(), "cron".to_string()]
        } else {
            vec!["public".to_string()]
        };
        let ext_ctx = ExtensionContextOpts {
            is_superuser: true,
            allow_local_fs: false,
        };

        with_context_opts(ext_ctx, async {
            let mut txn = store.begin().await?;
            let mut sequence_values: HashMap<String, i64> = HashMap::new();
            let result = async {
                let statements = parse_sql(&entry.command)?;
                for stmt in &statements {
                    let _ = exec
                        .execute_statement_on_txn(
                            &mut txn,
                            entry.db_id,
                            &mut sequence_values,
                            &search_path,
                            stmt,
                            None,
                        )
                        .await?;
                }
                txn.commit().await?;
                Ok(statements.len())
            }
            .await;

            match result {
                Ok(completed_commands) => Ok(completed_commands),
                Err(e) => {
                    let _ = txn.rollback().await;
                    Err(e)
                }
            }
        })
        .await
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn success_message(completed_commands: usize) -> String {
    if completed_commands == 1 {
        "1 command completed".to_string()
    } else {
        format!("{} commands completed", completed_commands)
    }
}

fn compute_next_fire_time(schedule: &str) -> Result<i64> {
    use crate::cron::parser::{next_occurrence, parse_cron_expression};

    let cron_schedule = parse_cron_expression(schedule)?;
    let now = chrono::Utc::now();
    let next = next_occurrence(&cron_schedule, now)
        .ok_or_else(|| anyhow!("no next occurrence for cron schedule: {}", schedule))?;
    Ok(next.timestamp_millis())
}
