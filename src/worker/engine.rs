use crate::cron::process_list::{get_process_list, RunningCronJob};
use crate::cron::types::{CronRun, CronRunStatus};
use crate::extensions::context::{with_context_opts, ExtensionContextOpts};
use crate::observability;
use crate::pool::TikvClientPool;
use crate::sql::ddl;
use crate::sql::parse_sql;
use crate::sql::query_context::{self, QueryContext};
use crate::sql::Executor;
use crate::storage::{CronRunClaimStatus, TikvStore};
use crate::worker::config::WorkerConfig;
use crate::worker::metrics::WorkerMetrics;
use crate::worker::types::*;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, warn};

pub struct WorkerEngine {
    config: WorkerConfig,
    system_store: Arc<TikvStore>,
    pool: Arc<TikvClientPool>,
    active_jobs: Arc<AtomicU32>,
    semaphore: Arc<Semaphore>,
    metrics: Arc<WorkerMetrics>,
    notify: Arc<Notify>,
}

impl WorkerEngine {
    pub fn new(
        config: WorkerConfig,
        system_store: Arc<TikvStore>,
        pool: Arc<TikvClientPool>,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs));
        let notify = Arc::new(Notify::new());
        crate::worker::set_worker_notify(notify.clone());
        Self {
            config,
            system_store,
            pool,
            active_jobs: Arc::new(AtomicU32::new(0)),
            semaphore,
            metrics: Arc::new(WorkerMetrics::new()),
            notify,
        }
    }

    pub async fn run(&self) {
        info!(
            "WorkerEngine starting (poll_ms={}, max_concurrent={})",
            self.config.poll_ms, self.config.max_concurrent_jobs
        );

        if let Err(e) = self.reconcile_cron_jobs().await {
            warn!("Cron reconciliation failed (engine will continue): {}", e);
        }
        if let Err(e) = self.reconcile_incomplete_cic_indexes().await {
            warn!(
                "CIC index-state recovery failed (engine will continue): {}",
                e
            );
        }

        let mut interval = tokio::time::interval(Duration::from_millis(self.config.poll_ms));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.notify.notified() => {}
            }
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

        self.metrics.sample_tick(
            due_entries.len() as u64,
            self.active_jobs.load(Ordering::Relaxed),
        );

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
            let engine_metrics = self.metrics.clone();

            join_set.spawn(async move {
                let _permit = permit;
                active_jobs.fetch_add(1, Ordering::Relaxed);
                let result = Self::claim_and_execute(
                    &engine_system_store,
                    &engine_pool,
                    &engine_config,
                    &engine_metrics,
                    key,
                    entry,
                )
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

    /// Reconcile cron jobs at startup: ensure all active cron jobs have queue entries,
    /// and remove queue entries for jobs that no longer exist or are inactive.
    async fn reconcile_cron_jobs(&self) -> Result<()> {
        info!("Starting cron job reconciliation...");

        let mut txn = self.system_store.begin().await?;
        let registry_entries = self.system_store.list_worker_registry(&mut txn).await?;
        txn.commit().await?;

        let mut total_enqueued = 0u32;
        let mut total_cleaned = 0u32;

        for entry in registry_entries {
            if !entry.has_cron() {
                continue;
            }

            match self
                .reconcile_cron_for_db(&entry.keyspace, entry.db_id)
                .await
            {
                Ok((enqueued, cleaned)) => {
                    total_enqueued += enqueued;
                    total_cleaned += cleaned;
                }
                Err(e) => {
                    warn!(
                        "Cron reconciliation error for keyspace={} db_id={}: {}",
                        entry.keyspace, entry.db_id, e
                    );
                }
            }
        }

        info!(
            "Cron reconciliation complete: enqueued={} cleaned={}",
            total_enqueued, total_cleaned
        );
        Ok(())
    }

    /// Reconcile cron jobs for a single (keyspace, db_id).
    /// Returns (enqueued_count, cleaned_count).
    async fn reconcile_cron_for_db(&self, keyspace: &str, db_id: u64) -> Result<(u32, u32)> {
        // 1. Scan existing cron queue entries from the system store
        let mut txn = self.system_store.begin().await?;
        let existing_queue = self
            .system_store
            .scan_cron_queue_entries_for_db(&mut txn, keyspace, db_id)
            .await?;
        txn.commit().await?;

        let existing_job_ids: HashSet<i64> =
            existing_queue.iter().map(|(_, task_id)| *task_id).collect();

        // 2. Acquire tenant store and check cron state
        let handle = self.pool.acquire(Some(keyspace.to_string())).await?;
        let store = handle.store().clone();

        let mut tenant_txn = store.begin().await?;
        let cron_enabled = store.is_cron_enabled(&mut tenant_txn, db_id).await?;

        if !cron_enabled {
            tenant_txn.commit().await?;
            // Cron disabled but registry has cron bit — clean up all queue entries
            if !existing_queue.is_empty() {
                let mut sys_txn = self.system_store.begin().await?;
                for (key, _) in &existing_queue {
                    self.system_store
                        .delete_worker_queue_entry(&mut sys_txn, key)
                        .await?;
                }
                sys_txn.commit().await?;
            }
            return Ok((0, existing_queue.len() as u32));
        }

        // 3. Load active cron jobs from tenant store
        let cron_jobs = store.list_cron_jobs(&mut tenant_txn, db_id).await?;
        tenant_txn.commit().await?;

        let active_jobs: HashMap<i64, &crate::cron::types::CronJob> = cron_jobs
            .iter()
            .filter(|j| j.active)
            .map(|j| (j.job_id, j))
            .collect();
        let active_job_ids: HashSet<i64> = active_jobs.keys().copied().collect();

        let mut enqueued = 0u32;
        let mut cleaned = 0u32;

        // 4. Enqueue missing: active jobs not in queue
        let missing: Vec<i64> = active_job_ids
            .difference(&existing_job_ids)
            .copied()
            .collect();

        if !missing.is_empty() {
            let mut sys_txn = self.system_store.begin().await?;
            for job_id in missing {
                let job = active_jobs[&job_id];
                let next_fire = match compute_next_fire_time(&job.schedule) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(
                            "Cron reconciliation: invalid schedule for job_id={} schedule='{}': {}",
                            job_id, job.schedule, e
                        );
                        continue;
                    }
                };
                let queue_entry = TaskQueueEntry::new(
                    keyspace.to_string(),
                    db_id,
                    job.job_id,
                    TaskType::Cron,
                    job.command.clone(),
                    job.username.clone(),
                    128,
                )
                .with_schedule(job.schedule.clone());
                self.system_store
                    .put_worker_queue_entry(&mut sys_txn, &queue_entry, next_fire)
                    .await?;
                enqueued += 1;
            }
            sys_txn.commit().await?;
        }

        // 5. Cleanup orphans: queue entries whose job_id is not in active jobs
        let orphan_keys: Vec<Vec<u8>> = existing_queue
            .into_iter()
            .filter(|(_, task_id)| !active_job_ids.contains(task_id))
            .map(|(key, _)| key)
            .collect();

        if !orphan_keys.is_empty() {
            let mut sys_txn = self.system_store.begin().await?;
            for key in &orphan_keys {
                self.system_store
                    .delete_worker_queue_entry(&mut sys_txn, key)
                    .await?;
            }
            sys_txn.commit().await?;
            cleaned = orphan_keys.len() as u32;
        }

        Ok((enqueued, cleaned))
    }

    /// Recover CIC indexes left in transitional states after process restart.
    ///
    /// Transitional states (`Building`/`WriteOnly`) are not durable across worker restarts:
    /// they indicate an interrupted asynchronous build pipeline. We conservatively mark such
    /// indexes `Invalid` so they are never used/read as complete.
    async fn reconcile_incomplete_cic_indexes(&self) -> Result<()> {
        let mut txn = self.system_store.begin().await?;
        let registry_entries = self.system_store.list_worker_registry(&mut txn).await?;
        txn.commit().await?;

        for entry in registry_entries {
            if !entry.has_bg_ddl() {
                continue;
            }
            if let Err(e) = self
                .reconcile_incomplete_cic_indexes_for_db(&entry.keyspace, entry.db_id)
                .await
            {
                warn!(
                    "CIC recovery error for keyspace={} db_id={}: {}",
                    entry.keyspace, entry.db_id, e
                );
            }
        }
        Ok(())
    }

    async fn reconcile_incomplete_cic_indexes_for_db(
        &self,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        let handle = self.pool.acquire(Some(keyspace.to_string())).await?;
        let store = handle.store().clone();
        let mut txn = store.begin().await?;

        let result: Result<u32> = async {
            let mut repaired = 0u32;
            let table_names = store.list_tables(&mut txn, db_id).await?;
            for table_name in table_names {
                let Some(mut schema) = store.get_schema(&mut txn, db_id, &table_name).await? else {
                    continue;
                };
                let repaired_in_schema = repair_incomplete_cic_states(&mut schema);
                if repaired_in_schema > 0 {
                    repaired += repaired_in_schema;
                    store.update_schema(&mut txn, db_id, schema).await?;
                }
            }
            Ok(repaired)
        }
        .await;

        match result {
            Ok(repaired) => {
                txn.commit().await?;
                if repaired > 0 {
                    warn!(
                        "Recovered {} incomplete CIC indexes as Invalid in keyspace={} db_id={}",
                        repaired, keyspace, db_id
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

    async fn claim_and_execute(
        system_store: &Arc<TikvStore>,
        pool: &Arc<TikvClientPool>,
        config: &WorkerConfig,
        metrics: &Arc<WorkerMetrics>,
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

        metrics.record_claim(claimed);

        if !claimed {
            txn.rollback().await.ok();
            return Ok(());
        }
        txn.commit().await?;

        let (cron_run, keep_queue_entry) = if entry.task_type == TaskType::Cron {
            Self::claim_and_record_cron_run(pool, &entry, fire_time_min).await?
        } else {
            (None, false)
        };
        let should_requeue_cron = cron_run.is_some();

        let exec_result = if entry.task_type == TaskType::Cron && cron_run.is_none() {
            Ok(0usize)
        } else if let Some((_, cron_db_id, ref run, _, max_runtime_ms)) = cron_run {
            let cancel_signal = get_process_list().register(RunningCronJob {
                run_id: run.run_id,
                job_id: entry.task_id,
                keyspace: entry.keyspace.clone(),
                db_id: cron_db_id,
                username: entry.username.clone(),
                command: entry.command.clone(),
                started_at: now_ms(),
            });

            let timeout_ms = max_runtime_ms.unwrap_or(config.cron_job_timeout_ms);
            let timeout_dur = if timeout_ms > 0 {
                Some(Duration::from_millis(timeout_ms))
            } else {
                None
            };

            let task_fut = Self::execute_task(pool, config, &entry, Some(cancel_signal));
            let result = match timeout_dur {
                Some(dur) => match tokio::time::timeout(dur, task_fut).await {
                    Ok(r) => r,
                    Err(_) => Err(anyhow!("cron job timed out after {}ms", timeout_ms)),
                },
                None => task_fut.await,
            };

            get_process_list().deregister(run.run_id);
            result
        } else {
            Self::execute_task(pool, config, &entry, None).await
        };

        if let Some((store, db_id, mut run, started_at, _max_runtime_ms)) = cron_run {
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
        if !keep_queue_entry {
            system_store
                .delete_worker_queue_entry(&mut txn, &queue_key)
                .await?;
        }

        if entry.task_type == TaskType::Cron && should_requeue_cron {
            if let Some(next_entry) = Self::load_next_cron_queue_entry(pool, &entry).await? {
                if let Some(schedule) = next_entry.schedule.as_deref() {
                    if let Ok(next_fire) = compute_next_fire_time(schedule) {
                        system_store
                            .put_worker_queue_entry(&mut txn, &next_entry, next_fire)
                            .await?;
                    }
                }
            }
        }

        if entry.task_type == TaskType::BgSql {
            let result_text = match &exec_result {
                Ok(_) => "OK".to_string(),
                Err(e) => format!("ERROR: {}", e),
            };
            system_store
                .put_bg_result(
                    &mut txn,
                    &entry.keyspace,
                    entry.db_id,
                    entry.task_id,
                    &result_text,
                )
                .await?;
        }

        txn.commit().await?;

        match exec_result {
            Ok(_) => {
                metrics.record_task_result(entry.task_type, true);
                info!(
                    "Worker task completed: keyspace={} db_id={} task_id={} type={:?}",
                    entry.keyspace, entry.db_id, entry.task_id, entry.task_type
                );
            }
            Err(ref e) => {
                metrics.record_task_result(entry.task_type, false);
                warn!(
                    "Worker task failed: keyspace={} db_id={} task_id={} type={:?} error={}",
                    entry.keyspace, entry.db_id, entry.task_id, entry.task_type, e
                );
            }
        }

        Ok(())
    }

    async fn claim_and_record_cron_run(
        pool: &Arc<TikvClientPool>,
        entry: &TaskQueueEntry,
        scheduled_minute: i64,
    ) -> Result<(
        Option<(Arc<TikvStore>, u64, CronRun, i64, Option<u64>)>,
        bool,
    )> {
        let handle = pool.acquire(Some(entry.keyspace.clone())).await?;
        let store = handle.store().clone();
        let mut txn = store.begin().await?;

        let claim_result = async {
            if !store.is_cron_enabled(&mut txn, entry.db_id).await? {
                return Ok((None, false));
            }

            let claim_status = store
                .try_claim_cron_run(&mut txn, entry.db_id, entry.task_id, scheduled_minute)
                .await?;
            match claim_status {
                CronRunClaimStatus::Claimed => {}
                CronRunClaimStatus::AlreadyClaimedForMinute
                | CronRunClaimStatus::BlockedByRunningGuard => {
                    // Keep the queue entry so the cron trigger can be retried later.
                    return Ok((None, true));
                }
            }

            let Some(job) = store
                .get_cron_job(&mut txn, entry.db_id, entry.task_id)
                .await?
            else {
                return Ok((None, false));
            };
            if !job.active {
                return Ok((None, false));
            }

            let database = store
                .get_database_by_id(&mut txn, entry.db_id)
                .await?
                .map(|db| db.name)
                .unwrap_or_else(|| "postgres".to_string());

            let max_runtime_ms = job.max_runtime_ms;

            let run_id = store.next_cron_run_id(entry.db_id).await?;
            let started_at = now_ms();
            let run = CronRun {
                run_id,
                job_id: entry.task_id,
                job_pid: None,
                database,
                username: job.username.clone(),
                command: job.command.clone(),
                status: CronRunStatus::Running,
                return_message: None,
                start_time: Some(started_at),
                end_time: None,
            };
            store.put_cron_run(&mut txn, entry.db_id, &run).await?;
            // Set the running guard to prevent overlapping runs
            store
                .set_cron_running_guard(&mut txn, entry.db_id, entry.task_id, run_id)
                .await?;
            Ok((
                Some((store, entry.db_id, run, started_at, max_runtime_ms)),
                false,
            ))
        }
        .await;

        match claim_result {
            Ok((Some(run), keep_queue_entry)) => {
                txn.commit().await?;
                Ok((Some(run), keep_queue_entry))
            }
            Ok((None, keep_queue_entry)) => {
                txn.rollback().await.ok();
                Ok((None, keep_queue_entry))
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e)
            }
        }
    }

    async fn load_next_cron_queue_entry(
        pool: &Arc<TikvClientPool>,
        entry: &TaskQueueEntry,
    ) -> Result<Option<TaskQueueEntry>> {
        let handle = pool.acquire(Some(entry.keyspace.clone())).await?;
        let store = handle.store().clone();
        let mut txn = store.begin().await?;

        let result: Result<Option<TaskQueueEntry>> = async {
            if !store.is_cron_enabled(&mut txn, entry.db_id).await? {
                return Ok(None);
            }

            let Some(job) = store
                .get_cron_job(&mut txn, entry.db_id, entry.task_id)
                .await?
            else {
                return Ok(None);
            };
            if !job.active {
                return Ok(None);
            }
            Ok(Some(
                TaskQueueEntry::new(
                    entry.keyspace.clone(),
                    entry.db_id,
                    entry.task_id,
                    TaskType::Cron,
                    job.command.clone(),
                    job.username.clone(),
                    entry.priority,
                )
                .with_schedule(job.schedule.clone()),
            ))
        }
        .await;

        match result {
            Ok(next_entry) => {
                txn.commit().await?;
                Ok(next_entry)
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
            store.put_cron_run(&mut txn, db_id, run).await?;
            // Clear the running guard now that the run is in a terminal state
            store
                .clear_cron_running_guard(&mut txn, db_id, run.job_id, run.run_id)
                .await?;
            Ok(())
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
        config: &WorkerConfig,
        entry: &TaskQueueEntry,
        cancel_signal: Option<Arc<Notify>>,
    ) -> Result<usize> {
        let handle = pool.acquire(Some(entry.keyspace.clone())).await?;
        let store = handle.store().clone();
        let database_name: Arc<str> = {
            let mut db_txn = store.begin().await?;
            let resolved = store
                .get_database_by_id(&mut db_txn, entry.db_id)
                .await?
                .map(|db| db.name)
                .unwrap_or_else(|| "postgres".to_string());
            db_txn.commit().await?;
            Arc::from(resolved)
        };
        let current_user: Arc<str> = Arc::from(entry.username.clone());
        let timezone: Arc<str> = Arc::from("UTC");
        let tx_start_ms = chrono::Utc::now().timestamp_millis();

        // BgDdl tasks (e.g. CREATE INDEX CONCURRENTLY backfill) are exempt from
        // statement_timeout — they legitimately run for extended periods.
        if entry.task_type == TaskType::BgDdl && entry.command.starts_with("__backfill_index ") {
            let stmt_ts = chrono::Utc::now().timestamp_millis();
            let qctx = QueryContext::new(
                0,
                database_name.clone(),
                current_user.clone(),
                stmt_ts,
                tx_start_ms,
                timezone.clone(),
            );
            query_context::with_scoped_query_context(
                &qctx,
                Self::execute_bg_ddl_backfill(&store, entry),
            )
            .await?;
            return Ok(1);
        }

        let is_cron = entry.task_type == TaskType::Cron;
        let stmt_timeout = if !is_cron && config.statement_timeout_ms > 0 {
            Some(std::time::Duration::from_millis(
                config.statement_timeout_ms,
            ))
        } else {
            None
        };

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
        let ext_ctx = if entry.task_type == TaskType::Cron {
            ExtensionContextOpts::cron(&entry.keyspace)
        } else {
            ExtensionContextOpts {
                is_superuser: true,
                allow_local_fs: false,
                tenant_keyspace: entry.keyspace.clone(),
                execution_kind: crate::extensions::context::ExecutionKind::Interactive,
            }
        };

        let mut txn = store.begin().await?;
        let mut sequence_values: HashMap<String, i64> = HashMap::new();
        let result = async {
            let statements = parse_sql(&entry.command)?;
            for stmt in &statements {
                let stmt_ts = chrono::Utc::now().timestamp_millis();
                let qctx = QueryContext::new(
                    0,
                    database_name.clone(),
                    current_user.clone(),
                    stmt_ts,
                    tx_start_ms,
                    timezone.clone(),
                );

                // Wrap each statement in its own extension context to reset http_requests counter
                let fut = with_context_opts(
                    ext_ctx.clone(),
                    query_context::with_scoped_query_context(
                        &qctx,
                        exec.execute_statement_on_txn(
                            &mut txn,
                            entry.db_id,
                            &mut sequence_values,
                            &search_path,
                            stmt,
                            None,
                        ),
                    ),
                );

                let res = match (&stmt_timeout, &cancel_signal) {
                    (Some(t), Some(sig)) => tokio::select! {
                        r = tokio::time::timeout(*t, fut) => match r {
                            Ok(res) => res,
                            Err(_) => Err(anyhow::anyhow!("canceling statement due to statement timeout")),
                        },
                        _ = sig.notified() => Err(anyhow::anyhow!("cancelled by administrator")),
                    },
                    (Some(t), None) => match tokio::time::timeout(*t, fut).await {
                        Ok(res) => res,
                        Err(_) => Err(anyhow::anyhow!("canceling statement due to statement timeout")),
                    },
                    (None, Some(sig)) => tokio::select! {
                        r = fut => r,
                        _ = sig.notified() => Err(anyhow::anyhow!("cancelled by administrator")),
                    },
                    (None, None) => fut.await,
                };
                let _ = res?;
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
    }

    async fn execute_bg_ddl_backfill(store: &Arc<TikvStore>, entry: &TaskQueueEntry) -> Result<()> {
        let (table_name, index_name) = parse_backfill_index_command(&entry.command)?;
        let db_id = entry.db_id;

        let mark_invalid = |e: anyhow::Error| async {
            if let Err(mark_err) =
                ddl::update_index_state(store, db_id, &table_name, &index_name, IndexState::Invalid)
                    .await
            {
                warn!(
                    "Failed to mark index invalid after backfill failure: table={} index={} error={}",
                    table_name, index_name, mark_err
                );
            }
            e
        };

        let mut schema_txn = store.begin().await?;
        let schema = match store
            .get_schema(&mut schema_txn, db_id, &table_name)
            .await?
        {
            Some(schema) => schema,
            None => {
                schema_txn.rollback().await.ok();
                return Err(anyhow!("Table '{}' not found", table_name));
            }
        };
        schema_txn.commit().await?;
        let index = schema
            .indexes
            .iter()
            .find(|idx| idx.name == index_name)
            .cloned()
            .ok_or_else(|| anyhow!("Index '{}' not found on table '{}'", index_name, table_name))?;
        if !should_start_cic_backfill(index.state) {
            // Guard against duplicate/stale queue entries: CIC phases must start from Building.
            warn!(
                "Skipping CIC backfill task because index is not in Building state: table={} index={} state={:?}",
                table_name, index_name, index.state
            );
            return Ok(());
        }

        // Phase 1 (Building): backfill and atomically flip to WriteOnly.
        if let Err(e) = ddl::backfill_index_by_name(
            store,
            db_id,
            &table_name,
            &index_name,
            Some(IndexState::WriteOnly),
        )
        .await
        {
            return Err(mark_invalid(e).await);
        }

        // Phase 2 (WriteOnly): catch-up scan on a fresh snapshot.
        if let Err(e) =
            ddl::backfill_index_by_name(store, db_id, &table_name, &index_name, None).await
        {
            return Err(mark_invalid(e).await);
        }

        // Phase 3: reconcile stale entries and atomically expose index to planner.
        if let Err(e) = ddl::reconcile_index(
            store,
            db_id,
            &table_name,
            &index_name,
            Some(IndexState::Ready),
        )
        .await
        {
            return Err(mark_invalid(e).await);
        }

        Ok(())
    }
}

fn should_start_cic_backfill(state: IndexState) -> bool {
    matches!(state, IndexState::Building)
}

fn repair_incomplete_cic_states(schema: &mut crate::types::TableSchema) -> u32 {
    let mut repaired = 0u32;
    for idx in &mut schema.indexes {
        if matches!(idx.state, IndexState::Building | IndexState::WriteOnly) {
            idx.state = IndexState::Invalid;
            repaired += 1;
        }
    }
    repaired
}

fn parse_backfill_index_command(command: &str) -> Result<(String, String)> {
    let args = command
        .strip_prefix("__backfill_index ")
        .ok_or_else(|| anyhow!("invalid backfill command: {}", command))?;
    let mut parts = args.split_whitespace();
    let table_name = parts
        .next()
        .ok_or_else(|| anyhow!("missing table name in backfill command"))?;
    let index_name = parts
        .next()
        .ok_or_else(|| anyhow!("missing index name in backfill command"))?;
    if parts.next().is_some() {
        return Err(anyhow!("invalid backfill command args: {}", command));
    }
    Ok((table_name.to_string(), index_name.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DataType, IndexDef, TableSchema};

    fn idx(name: &str, state: IndexState) -> IndexDef {
        IndexDef {
            name: name.to_string(),
            id: 1,
            columns: vec!["c1".to_string()],
            unique: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state,
        }
    }

    #[test]
    fn repair_incomplete_cic_states_repairs_building_and_writeonly() {
        let mut schema = TableSchema {
            name: "public.t".to_string(),
            table_id: 1,
            columns: vec![crate::types::ColumnDef {
                name: "c1".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![
                idx("i_ready", IndexState::Ready),
                idx("i_building", IndexState::Building),
                idx("i_invalid", IndexState::Invalid),
                idx("i_write_only", IndexState::WriteOnly),
            ],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "postgres".to_string(),
            from_alias: None,
        };

        let repaired = repair_incomplete_cic_states(&mut schema);
        assert_eq!(repaired, 2);
        assert_eq!(schema.indexes[0].state, IndexState::Ready);
        assert_eq!(schema.indexes[1].state, IndexState::Invalid);
        assert_eq!(schema.indexes[2].state, IndexState::Invalid);
        assert_eq!(schema.indexes[3].state, IndexState::Invalid);
    }

    #[test]
    fn repair_incomplete_cic_states_noop_when_no_transient_state() {
        let mut schema = TableSchema {
            indexes: vec![
                idx("i_ready", IndexState::Ready),
                idx("i_invalid", IndexState::Invalid),
            ],
            ..TableSchema::default()
        };

        let repaired = repair_incomplete_cic_states(&mut schema);
        assert_eq!(repaired, 0);
        assert_eq!(schema.indexes[0].state, IndexState::Ready);
        assert_eq!(schema.indexes[1].state, IndexState::Invalid);
    }

    #[test]
    fn should_start_cic_backfill_only_when_building() {
        assert!(should_start_cic_backfill(IndexState::Building));
        assert!(!should_start_cic_backfill(IndexState::Ready));
        assert!(!should_start_cic_backfill(IndexState::Invalid));
        assert!(!should_start_cic_backfill(IndexState::WriteOnly));
    }
}
