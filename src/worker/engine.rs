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
use crate::worker::now_epoch_ms;
use crate::worker::types::*;
use anyhow::{anyhow, Result};
use pgwire::tokio::CancellationToken;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tikv_client::TimestampExt;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, warn};

const STATEMENT_TIMEOUT_ERROR: &str = "canceling statement due to statement timeout";
const CANCELLED_BY_ADMIN_ERROR: &str = "cancelled by administrator";

pub struct WorkerEngine {
    config: WorkerConfig,
    system_store: Arc<TikvStore>,
    pool: Arc<TikvClientPool>,
    active_jobs: Arc<AtomicU32>,
    semaphore: Arc<Semaphore>,
    metrics: Arc<WorkerMetrics>,
    notify: Arc<Notify>,
    shutdown: CancellationToken,
}

struct ActiveJobGuard {
    active_jobs: Arc<AtomicU32>,
}

impl ActiveJobGuard {
    fn new(active_jobs: Arc<AtomicU32>) -> Self {
        active_jobs.fetch_add(1, Ordering::Relaxed);
        Self { active_jobs }
    }
}

impl Drop for ActiveJobGuard {
    fn drop(&mut self) {
        self.active_jobs.fetch_sub(1, Ordering::Relaxed);
    }
}

impl WorkerEngine {
    pub fn new(
        config: WorkerConfig,
        system_store: Arc<TikvStore>,
        pool: Arc<TikvClientPool>,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs));
        let notify = Arc::new(Notify::new());
        let shutdown = CancellationToken::new();
        crate::worker::set_worker_notify(notify.clone());
        Self {
            config,
            system_store,
            pool,
            active_jobs: Arc::new(AtomicU32::new(0)),
            semaphore,
            metrics: Arc::new(WorkerMetrics::new()),
            notify,
            shutdown,
        }
    }

    pub fn metrics(&self) -> &Arc<WorkerMetrics> {
        &self.metrics
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
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
        if let Err(e) = self.reconcile_hnsw_merges().await {
            warn!(
                "HNSW merge reconciliation failed (engine will continue): {}",
                e
            );
        }

        if let Err(e) = warm_load_storage_stats(&self.pool, &self.system_store).await {
            warn!(
                "Storage stats warm-load failed (engine will continue): {}",
                e
            );
        }

        if let Err(e) = self.reconcile_storage_scans().await {
            warn!(
                "Storage scan reconciliation failed (engine will continue): {}",
                e
            );
        }

        let mut interval = tokio::time::interval(Duration::from_millis(self.config.poll_ms));
        let mut last_storage_reconcile = tokio::time::Instant::now();
        let storage_scan_interval = Duration::from_secs(self.config.storage_scan_interval_sec);

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    info!("WorkerEngine shutdown requested");
                    break;
                }
                _ = interval.tick() => {}
                _ = self.notify.notified() => {}
            }

            if last_storage_reconcile.elapsed() >= storage_scan_interval {
                if let Err(e) = self.reconcile_storage_scans().await {
                    warn!("Periodic storage scan reconciliation failed: {}", e);
                }
                last_storage_reconcile = tokio::time::Instant::now();
            }

            if let Err(e) = self.tick().await {
                warn!("Worker tick error: {}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let now_ms = now_epoch_ms();

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
            let permit = match self.semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => break,
            };

            let engine_system_store = self.system_store.clone();
            let engine_pool = self.pool.clone();
            let engine_config = self.config.clone();
            let active_jobs = self.active_jobs.clone();
            let engine_metrics = self.metrics.clone();
            let engine_shutdown = self.shutdown.clone();

            join_set.spawn(async move {
                let _permit = permit;
                let _active_jobs_guard = ActiveJobGuard::new(active_jobs);
                let result = Self::claim_and_execute(
                    &engine_system_store,
                    &engine_pool,
                    &engine_config,
                    &engine_metrics,
                    key,
                    entry,
                    engine_shutdown,
                )
                .await;

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
        shutdown_signal: CancellationToken,
    ) -> Result<()> {
        Self::claim_and_execute_core(
            system_store,
            pool,
            config,
            metrics,
            queue_key,
            entry,
            shutdown_signal,
            Self::finalize_cron_run,
        )
        .await
    }

    /// Core implementation of claim_and_execute, parameterized over the finalize
    /// function so tests can inject failures in the real code path.
    async fn claim_and_execute_core<F, Fut>(
        system_store: &Arc<TikvStore>,
        pool: &Arc<TikvClientPool>,
        config: &WorkerConfig,
        metrics: &Arc<WorkerMetrics>,
        queue_key: Vec<u8>,
        entry: TaskQueueEntry,
        shutdown_signal: CancellationToken,
        finalize_fn: F,
    ) -> Result<()>
    where
        F: FnOnce(Arc<TikvStore>, u64, CronRun, CronRunStatus, Option<String>, i64, i64) -> Fut,
        Fut: Future<Output = Result<()>> + Send,
    {
        let queue_fire_time_ms = crate::storage::decode_worker_queue_fire_time(&queue_key)
            .ok_or_else(|| anyhow!("corrupted worker queue key: missing fire_time_ms"))?;
        let scheduled_minute = queue_fire_time_ms.div_euclid(60_000);
        let claim = WorkerClaim::new(config.worker_id.clone(), entry.task_type);

        let mut txn = system_store.begin().await?;
        let claimed = system_store
            .try_claim_worker_task(
                &mut txn,
                &entry.keyspace,
                entry.db_id,
                entry.task_id,
                queue_fire_time_ms,
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
            Self::claim_and_record_cron_run(pool, &entry, scheduled_minute).await?
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
                started_at: now_epoch_ms(),
            });

            let timeout_ms = max_runtime_ms.unwrap_or(config.cron_job_timeout_ms);
            let timeout_dur = if timeout_ms > 0 {
                Some(Duration::from_millis(timeout_ms))
            } else {
                None
            };

            let task_fut = Self::execute_task(
                pool,
                config,
                &entry,
                Some(cancel_signal),
                Some(shutdown_signal.clone()),
            );
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
            Self::execute_task(pool, config, &entry, None, Some(shutdown_signal)).await
        };

        // Capture finalize result instead of propagating with `?` — cleanup
        // must run unconditionally even when finalize fails (#1259).
        let finalize_result =
            if let Some((store, db_id, run, started_at, _max_runtime_ms)) = cron_run {
                let (status, message) = match &exec_result {
                    Ok(completed_commands) => (
                        CronRunStatus::Succeeded,
                        Some(success_message(*completed_commands)),
                    ),
                    Err(e) => (CronRunStatus::Failed, Some(e.to_string())),
                };

                finalize_fn(
                    store,
                    db_id,
                    run,
                    status,
                    message,
                    started_at,
                    now_epoch_ms(),
                )
                .await
            } else {
                Ok(())
            };

        if let Err(ref e) = finalize_result {
            warn!("finalize_cron_run failed: {e}; proceeding with cleanup");
        }

        // Cleanup: delete worker claim, manage queue entry, requeue next cron
        // fire. This block ALWAYS runs regardless of finalize_result.
        let mut txn = system_store.begin().await?;
        system_store
            .delete_worker_claim(
                &mut txn,
                &entry.keyspace,
                entry.db_id,
                entry.task_id,
                queue_fire_time_ms,
                entry.task_type,
            )
            .await?;
        if !keep_queue_entry {
            if entry.task_type == TaskType::HnswMerge {
                // HnswMerge uses deterministic fire_time=0 → concurrent DML can
                // overwrite the same queue key with a new nonce. Read-compare-delete
                // ensures we only remove the entry we actually processed.
                //
                // Only delete the queue entry on SUCCESS. On failure (e.g., S3 not
                // configured on this node), keep the entry so a capable worker can
                // pick it up on the next poll cycle. This prevents a non-S3 worker
                // from repeatedly claiming and failing merges, blocking progress
                // until the 600s periodic sweep re-enqueues.
                if exec_result.is_ok() {
                    if let Some(current_bytes) = txn.get(queue_key.clone()).await? {
                        let current =
                            TaskQueueEntry::deserialize_compat(&current_bytes).map_err(|e| {
                                anyhow::anyhow!("Failed to deserialize worker queue entry: {e}")
                            })?;
                        if current.nonce == entry.nonce {
                            system_store
                                .delete_worker_queue_entry(&mut txn, &queue_key)
                                .await?;
                        }
                        // nonce mismatch → DML overwrote → skip delete, next tick handles it
                    }
                }
            } else {
                // Non-HnswMerge: these tasks never share deterministic keys with DML,
                // so unconditional delete is safe (original behavior preserved).
                system_store
                    .delete_worker_queue_entry(&mut txn, &queue_key)
                    .await?;
            }
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

        // Propagate finalize error AFTER cleanup succeeds.
        finalize_result?;

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
            let started_at = now_epoch_ms();
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
        store: Arc<TikvStore>,
        db_id: u64,
        mut run: CronRun,
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
            store.put_cron_run(&mut txn, db_id, &run).await?;
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
        shutdown_signal: Option<CancellationToken>,
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
        let tx_start_ms = now_epoch_ms();
        let statement_memory_accountant = handle.memory_accountant();

        if entry.task_type == TaskType::StorageSizeScan {
            execute_storage_size_scan(&store, entry.db_id).await?;
            return Ok(1);
        }

        if entry.task_type == TaskType::HnswMerge && entry.command.starts_with("__hnsw_merge ") {
            let (table_id, index_id) = parse_hnsw_merge_command(&entry.command)?;
            execute_hnsw_merge(&store, entry.db_id, table_id, index_id).await?;
            return Ok(1);
        }

        // BgDdl tasks (e.g. CREATE INDEX CONCURRENTLY backfill) are exempt from
        // statement_timeout — they legitimately run for extended periods.
        if entry.task_type == TaskType::BgDdl && entry.command.starts_with("__backfill_index ") {
            let stmt_ts = now_epoch_ms();
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
        let task_timeout = if !is_cron && config.statement_timeout_ms > 0 {
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
            handle.memory_accountant(),
            handle.trigger_cache().clone(),
            handle.rls_policy_cache().clone(),
            handle.stats_cache().clone(),
        );

        let search_path = if entry.task_type == TaskType::Cron {
            vec!["public".to_string(), "cron".to_string()]
        } else {
            vec!["public".to_string()]
        };
        let tikv_client = store.transaction_client();

        let mut txn = store.begin().await?;
        let mut txn_guard = crate::worker::active_txn_registry::global_registry()
            .map(|registry| registry.track_worker_txn(txn.start_timestamp().version()));
        let mut sequence_values = crate::sql::sequences::SequenceSession::new();
        let task_fut = async {
            let statements = parse_sql(&entry.command)?;
            for stmt in &statements {
                let stmt_ts = now_epoch_ms();
                let qctx = QueryContext::new(
                    0,
                    database_name.clone(),
                    current_user.clone(),
                    stmt_ts,
                    tx_start_ms,
                    timezone.clone(),
                );

                // Wrap each statement in its own extension context to reset
                // http_requests counter and isolate statement memory lifecycle.
                let ext_ctx = background_statement_extension_context(
                    is_cron,
                    &entry.keyspace,
                    tikv_client.clone(),
                );
                let fut = crate::pool::run_with_statement_memory_scope(
                    Some(statement_memory_accountant.clone()),
                    with_context_opts(
                        ext_ctx,
                        query_context::with_scoped_query_context(
                            &qctx,
                            exec.execute_statement_on_txn(
                                &mut txn,
                                entry.db_id,
                                &mut sequence_values,
                                &search_path,
                                stmt,
                                None,
                                None,
                            ),
                        ),
                    ),
                );
                let _ = fut.await?;
            }
            txn.commit().await?;
            Ok(statements.len())
        };

        let result = run_with_guards(
            task_fut,
            task_timeout,
            cancel_signal.as_ref(),
            shutdown_signal.as_ref(),
        )
        .await;

        match result {
            Ok(completed_commands) => Ok(completed_commands),
            Err(e) => {
                if txn.rollback().await.is_err() {
                    // Rollback failed — the txn may still be live in TiKV.
                    // Keep the GC registration so the safepoint does not
                    // advance past this potentially live transaction.
                    if let Some(g) = txn_guard.as_mut() {
                        g.quarantine();
                    }
                }
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

    /// Enqueue storage size scan tasks for all known databases.
    async fn reconcile_storage_scans(&self) -> Result<()> {
        let mut txn = self.system_store.begin().await?;
        let registry_entries = self.system_store.list_worker_registry(&mut txn).await?;
        txn.commit().await?;

        let mut total_enqueued = 0u32;
        for entry in &registry_entries {
            let handle = match self.pool.acquire(Some(entry.keyspace.clone())).await {
                Ok(h) => h,
                Err(_) => continue,
            };
            let store = handle.store().clone();
            let mut tenant_txn = store.begin().await?;
            let databases = store.list_databases(&mut tenant_txn).await?;
            tenant_txn.rollback().await.ok();

            for db in databases {
                if let Err(e) =
                    enqueue_storage_scan(&self.system_store, &entry.keyspace, db.id).await
                {
                    warn!(
                        "Failed to enqueue storage scan for keyspace={} db_id={}: {}",
                        entry.keyspace, db.id, e
                    );
                } else {
                    total_enqueued += 1;
                }
            }
        }

        if total_enqueued > 0 {
            info!(total_enqueued, "Storage scan reconciliation complete");
        }
        Ok(())
    }

    async fn reconcile_hnsw_merges(&self) -> Result<()> {
        let mut txn = self.system_store.begin().await?;
        let all_entries = self.system_store.list_worker_registry(&mut txn).await?;
        txn.commit().await?;

        let mut total_enqueued = 0u32;
        let mut total_observed = 0u32;
        // Iterate ALL entries — no has_hnsw_merge() filter.
        // A crash between DML commit and flush_pending_hnsw_merges() leaves
        // deltas without the registry bit being set. We must check every
        // known (keyspace, db_id) to find orphaned deltas.
        for entry in &all_entries {
            match enqueue_pending_hnsw_merges(
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
                }
                Err(e) => warn!(
                    "HNSW reconcile error for keyspace={} db_id={}: {}",
                    entry.keyspace, entry.db_id, e
                ),
            }
        }
        if total_observed > 0 {
            info!(
                total_observed,
                total_enqueued, "HNSW startup reconciliation complete"
            );
        }
        Ok(())
    }
}

/// Result of scanning a (keyspace, db_id) for HNSW indexes with pending deltas.
pub(crate) struct HnswSweepResult {
    /// Number of HNSW indexes observed to have pending deltas.
    pub observed: u32,
    /// Number of merge tasks successfully enqueued to system store.
    pub enqueued: u32,
    /// Number of enqueue attempts that failed (system store errors).
    pub enqueue_errors: u32,
}

/// Scan all tables in (keyspace, db_id) for HNSW indexes with pending deltas.
/// For each found, enqueue a merge task (idempotent via deterministic queue key).
///
/// This function does NOT filter by any registry bit — it directly inspects
/// table schemas and probes for delta keys in the tenant store.
pub(crate) async fn enqueue_pending_hnsw_merges(
    system_store: &TikvStore,
    pool: &TikvClientPool,
    keyspace: &str,
    db_id: u64,
) -> Result<HnswSweepResult> {
    use crate::sql::hnsw::storage::{
        hnsw_delta_prefix, hnsw_delta_prefix_end, hnsw_merge_task_id, hnsw_meta_key, HnswMeta,
    };
    use rand::Rng;
    use tikv_client::BoundRange;

    let handle = pool.acquire(Some(keyspace.to_string())).await?;
    let store = handle.store().clone();
    let mut txn = store.begin().await?;
    // This read-only snapshot scans all tables, schemas, and per-index delta
    // prefixes — proportional to tenant size.  Register with the GC safepoint
    // so GC does not advance past this snapshot while the sweep runs.
    let mut txn_guard = crate::worker::active_txn_registry::global_registry()
        .map(|registry| registry.track_worker_txn(txn.start_timestamp().version()));

    let table_names = store.list_tables(&mut txn, db_id).await?;
    let mut observed = 0u32;
    let mut enqueued = 0u32;
    let mut enqueue_errors = 0u32;

    for table_name in table_names {
        let Some(schema) = store.get_schema(&mut txn, db_id, &table_name).await? else {
            continue;
        };
        for index in &schema.indexes {
            if !index.is_hnsw() {
                continue;
            }

            // Check if index is frozen — skip enqueue entirely.
            let mk = hnsw_meta_key(db_id, schema.table_id, index.id);
            if let Some(meta_bytes) = txn.get(mk).await? {
                if let Ok(meta) = serde_json::from_slice::<HnswMeta>(&meta_bytes) {
                    if should_skip_frozen_merge(&meta) {
                        continue;
                    }
                }
            }

            // Probe for pending deltas (limit=1, just checking existence)
            let prefix = hnsw_delta_prefix(db_id, schema.table_id, index.id);
            let end = hnsw_delta_prefix_end(db_id, schema.table_id, index.id);
            let range: BoundRange = (prefix..end).into();
            let pairs: Vec<_> = txn.scan(range, 1).await?.collect();
            if pairs.is_empty() {
                continue;
            }

            observed += 1;

            // Deltas found → enqueue merge task
            let task_id = match hnsw_merge_task_id(schema.table_id, index.id) {
                Ok(id) => id,
                Err(e) => {
                    warn!(
                        "HNSW sweep: task_id overflow for table_id={} index_id={}: {}",
                        schema.table_id, index.id, e
                    );
                    enqueue_errors += 1;
                    continue;
                }
            };
            let mut entry = TaskQueueEntry::new(
                keyspace.to_string(),
                db_id,
                task_id,
                TaskType::HnswMerge,
                format!("__hnsw_merge {} {}", schema.table_id, index.id),
                "system".to_string(),
                192,
            );
            entry.nonce = rand::thread_rng().gen_range(1..=u64::MAX);

            let enqueue_result: Result<()> = async {
                let mut sys_txn = system_store.begin().await?;
                system_store
                    .put_worker_queue_entry(&mut sys_txn, &entry, 0)
                    .await?;
                sys_txn.commit().await?;
                Ok(())
            }
            .await;

            match enqueue_result {
                Ok(()) => enqueued += 1,
                Err(e) => {
                    enqueue_errors += 1;
                    warn!(
                        "HNSW sweep: failed to enqueue merge for table_id={} index_id={}: {}",
                        schema.table_id, index.id, e
                    );
                }
            }
        }
    }

    if txn.rollback().await.is_err() {
        if let Some(g) = txn_guard.as_mut() {
            g.quarantine();
        }
    }
    Ok(HnswSweepResult {
        observed,
        enqueued,
        enqueue_errors,
    })
}

async fn run_with_guards<F, T>(
    fut: F,
    timeout: Option<Duration>,
    cancel: Option<&Arc<Notify>>,
    shutdown: Option<&CancellationToken>,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let timed_fut = async move {
        match timeout {
            Some(dur) => tokio::time::timeout(dur, fut)
                .await
                .map_err(|_| anyhow!(STATEMENT_TIMEOUT_ERROR))?,
            None => fut.await,
        }
    };

    match (cancel, shutdown) {
        (Some(cancel), Some(shutdown)) => tokio::select! {
            res = timed_fut => res,
            _ = cancel.notified() => Err(anyhow!(CANCELLED_BY_ADMIN_ERROR)),
            _ = shutdown.cancelled() => Err(anyhow!(CANCELLED_BY_ADMIN_ERROR)),
        },
        (Some(cancel), None) => tokio::select! {
            res = timed_fut => res,
            _ = cancel.notified() => Err(anyhow!(CANCELLED_BY_ADMIN_ERROR)),
        },
        (None, Some(shutdown)) => tokio::select! {
            res = timed_fut => res,
            _ = shutdown.cancelled() => Err(anyhow!(CANCELLED_BY_ADMIN_ERROR)),
        },
        (None, None) => timed_fut.await,
    }
}

fn should_start_cic_backfill(state: IndexState) -> bool {
    matches!(state, IndexState::Building)
}

fn repair_incomplete_cic_states(schema: &mut crate::model::TableSchema) -> u32 {
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

fn parse_hnsw_merge_command(command: &str) -> Result<(u64, u64)> {
    let args = command
        .strip_prefix("__hnsw_merge ")
        .ok_or_else(|| anyhow!("invalid hnsw_merge command: {}", command))?;
    let mut parts = args.split_whitespace();
    let table_id: u64 = parts
        .next()
        .ok_or_else(|| anyhow!("missing table_id in hnsw_merge command"))?
        .parse()?;
    let index_id: u64 = parts
        .next()
        .ok_or_else(|| anyhow!("missing index_id in hnsw_merge command"))?
        .parse()?;
    if parts.next().is_some() {
        return Err(anyhow!("invalid hnsw_merge command args: {}", command));
    }
    Ok((table_id, index_id))
}

fn background_statement_extension_context(
    is_cron: bool,
    keyspace: &str,
    tikv_client: Option<Arc<tikv_client::TransactionClient>>,
) -> ExtensionContextOpts {
    if is_cron {
        ExtensionContextOpts::cron(keyspace).with_tikv_client(tikv_client)
    } else {
        ExtensionContextOpts::statement(true, true, keyspace).with_tikv_client(tikv_client)
    }
}

/// Maximum deltas to process in a single merge transaction.
const MERGE_BATCH_SIZE: usize = 5000;

/// Maximum serialized graph size (bytes) before freezing the index.
/// Set below TiKV's default `raft-entry-max-size` (16 MB) with margin.
pub(crate) const HNSW_GRAPH_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Returns `true` if the merge should be skipped because the index is frozen.
/// Used at the top of `execute_hnsw_merge` and testable independently.
///
/// When S3 offload is enabled, frozen indexes can be merged again because
/// the graph blob goes to S3 (no TiKV raft-entry-max-size concern).
pub(crate) fn should_skip_frozen_merge(meta: &crate::sql::hnsw::storage::HnswMeta) -> bool {
    if crate::sql::hnsw::s3::hnsw_s3_client().is_some() {
        return false;
    }
    meta.frozen
}

/// Checks whether a serialized graph exceeds the safe size limit and should
/// trigger a freeze. Returns `Some(frozen_meta_bytes)` if the index must be
/// frozen (caller should persist these bytes and abort the merge), or `None`
/// if the graph is within limits.
pub(crate) fn check_graph_oversize_freeze(
    graph_len: usize,
    meta: &crate::sql::hnsw::storage::HnswMeta,
) -> Option<Vec<u8>> {
    if graph_len <= HNSW_GRAPH_MAX_BYTES {
        return None;
    }
    let mut frozen_meta = meta.clone();
    frozen_meta.frozen = true;
    serde_json::to_vec(&frozen_meta).ok()
}

/// Execute HNSW merge in batches. Each batch is a separate TiKV transaction
/// that processes up to MERGE_BATCH_SIZE deltas, writes the consolidated base
/// graph, deletes consumed deltas, and updates meta — all atomically.
///
/// If more deltas remain after a batch, loops with a new transaction.
/// Crashes between batches lose no data (uncommitted deltas remain in TiKV).
async fn execute_hnsw_merge(
    store: &Arc<TikvStore>,
    db_id: u64,
    table_id: u64,
    index_id: u64,
) -> Result<()> {
    use crate::sql::hnsw::storage::{
        create_empty_hnsw_index, delete_delta_keys, hnsw_delta_prefix, hnsw_delta_prefix_end,
        hnsw_graph_key, hnsw_meta_key, hnsw_s3_retired_version_key, load_base_graph,
        serialize_hnsw_snapshot, HnswDelta, HnswMeta, HnswS3RetiredVersionGc,
    };
    use crate::txn::txn_put;
    use tikv_client::BoundRange;

    let merge_start = std::time::Instant::now();
    let mut total_deltas_merged: usize = 0;

    // Capability pre-check: if this index requires S3 but this worker
    // doesn't have S3 configured, fail early before acquiring any locks.
    // This prevents a non-S3 worker from holding a pessimistic lock on
    // the meta key while it discovers it can't serve the merge.
    if crate::sql::hnsw::s3::hnsw_s3_client().is_none() {
        let mut pre_txn = store.begin().await?;
        let pre_key = hnsw_meta_key(db_id, table_id, index_id);
        if let Some(pre_bytes) = pre_txn.get(pre_key).await? {
            if let Ok(pre_meta) = serde_json::from_slice::<HnswMeta>(&pre_bytes) {
                if pre_meta.storage_version >= 2 || pre_meta.graph_version > 0 {
                    pre_txn.rollback().await.ok();
                    tracing::debug!(
                        table_id,
                        index_id,
                        graph_version = pre_meta.graph_version,
                        "Skipping HNSW merge: index requires S3 but this worker has no S3 client"
                    );
                    return Ok(());
                }
            }
        }
        pre_txn.rollback().await.ok();
    }

    loop {
        let mut txn = store.begin().await?;
        let mut txn_guard = crate::worker::active_txn_registry::global_registry()
            .map(|registry| registry.track_worker_txn(txn.start_timestamp().version()));

        // 1. Read meta with pessimistic lock.
        // get_for_update is the last line of defense against duplicate merge
        // execution. Worker claims are now bound to the exact queue entry, but
        // manual requeue / operator mistakes must still not let two merges
        // compute the same next graph_version from a stale snapshot.
        let meta_key = hnsw_meta_key(db_id, table_id, index_id);
        let Some(meta_bytes) = txn.get_for_update(meta_key.clone()).await? else {
            // Index metadata missing — index was dropped. Abort silently.
            if txn.rollback().await.is_err() {
                if let Some(g) = txn_guard.as_mut() {
                    g.quarantine();
                }
            }
            break;
        };
        let meta: HnswMeta = serde_json::from_slice(&meta_bytes)?;
        if should_skip_frozen_merge(&meta) {
            if txn.rollback().await.is_err() {
                if let Some(g) = txn_guard.as_mut() {
                    g.quarantine();
                }
            }
            info!(table_id, index_id, "HNSW merge skipped: index is frozen");
            return Ok(());
        }
        if meta.storage_version != 1 && meta.storage_version != 2 {
            if txn.rollback().await.is_err() {
                if let Some(g) = txn_guard.as_mut() {
                    g.quarantine();
                }
            }
            return Err(anyhow!(
                "HNSW index has unsupported storage_version={}; only v1/v2 supported",
                meta.storage_version
            ));
        }

        // 2. Scan up to MERGE_BATCH_SIZE delta keys.
        let prefix = hnsw_delta_prefix(db_id, table_id, index_id);
        let end = hnsw_delta_prefix_end(db_id, table_id, index_id);
        let mut batch_keys: Vec<Vec<u8>> = Vec::new();
        let mut batch_deltas: Vec<HnswDelta> = Vec::new();
        let mut scan_start = prefix.clone();

        while batch_deltas.len() < MERGE_BATCH_SIZE {
            let remaining = (MERGE_BATCH_SIZE - batch_deltas.len()) as u32;
            let scan_limit = remaining.min(1024);
            let range: BoundRange = (scan_start.clone()..end.clone()).into();
            let pairs: Vec<tikv_client::KvPair> = txn.scan(range, scan_limit).await?.collect();
            let page_count = pairs.len();
            if page_count == 0 {
                break;
            }

            for pair in pairs {
                let k: &[u8] = pair.key().as_ref().into();
                let key: Vec<u8> = k.to_vec();
                if !key.starts_with(&prefix) {
                    break;
                }
                let delta: HnswDelta = bincode::deserialize(pair.value())?;
                scan_start = key.clone();
                scan_start.push(0x00);
                batch_keys.push(key);
                batch_deltas.push(delta);
                if batch_deltas.len() >= MERGE_BATCH_SIZE {
                    break;
                }
            }
            if (page_count as u32) < scan_limit {
                break;
            }
        }

        if batch_deltas.is_empty() {
            if txn.rollback().await.is_err() {
                if let Some(g) = txn_guard.as_mut() {
                    g.quarantine();
                }
            }
            break; // No more deltas — merge complete.
        }

        let batch_count = batch_deltas.len();

        // 3. Load base graph (or create empty if none exists yet).
        let keyspace = store.keyspace().unwrap_or("default");
        let (index, _): (crate::sql::hnsw::HnswIndexHandle, _) =
            match load_base_graph(&mut txn, db_id, table_id, index_id, &meta, keyspace).await? {
                Some(pair) => pair,
                None => create_empty_hnsw_index(
                    meta.dimensions,
                    &meta.distance_metric,
                    meta.m,
                    meta.ef_construction,
                )?,
            };

        // 4. Reserve capacity + apply deltas.
        let needed = index.size() as u64 + batch_count as u64;
        if needed > index.capacity() as u64 {
            let next_cap = needed.saturating_mul(2).max(1);
            index
                .reserve(next_cap as usize)
                .map_err(|e| anyhow!("HNSW reserve failed: {}", e))?;
        }
        for delta in &batch_deltas {
            index
                .add(delta.label, &delta.vector)
                .map_err(|e| anyhow!("HNSW add failed: {}", e))?;
        }

        // 5. Serialize new base graph.
        let mut updated_meta = meta.clone();
        updated_meta.count = index.size() as u64;
        updated_meta.capacity = index.capacity() as u64;
        let (graph_bytes, _meta_bytes_tikv) =
            serialize_hnsw_snapshot(db_id, table_id, index_id, index.deref(), &updated_meta)?;

        // 5a. S3 vs TiKV write path for the graph blob.
        if let Some(s3) = crate::sql::hnsw::s3::hnsw_s3_client() {
            // S3 path: upload graph to S3, increment graph_version.
            let previous_version = updated_meta.graph_version;
            let new_version = updated_meta.graph_version + 1;
            s3.put_graph(
                keyspace,
                db_id,
                table_id,
                index_id,
                new_version,
                bytes::Bytes::from(graph_bytes),
            )
            .await
            .map_err(|e| anyhow!("HNSW S3 put_graph failed: {}", e))?;
            updated_meta.graph_version = new_version;
            // First S3 write: upgrade storage_version to 2.
            if updated_meta.storage_version == 1 {
                updated_meta.storage_version = 2;
            }
            // Unfreeze if previously frozen (S3 has no size limit concern).
            updated_meta.frozen = false;
            // Re-serialize meta with updated graph_version/storage_version.
            let meta_bytes_s3 = serde_json::to_vec(&updated_meta)?;
            txn_put(&mut txn, meta_key, meta_bytes_s3).await?;
            if previous_version > 0 {
                let marker = HnswS3RetiredVersionGc {
                    delete_after_safepoint: None,
                };
                txn_put(
                    &mut txn,
                    hnsw_s3_retired_version_key(db_id, table_id, index_id, previous_version),
                    serde_json::to_vec(&marker)?,
                )
                .await?;
            }
            // On first migration (old graph was in TiKV), delete the stale
            // TiKV graph blob. Safe: concurrent queries at older snapshots
            // still see it via MVCC; no future query will read it since
            // graph_version > 0 routes to S3.
            if new_version == 1 {
                let graph_key =
                    crate::sql::hnsw::storage::hnsw_graph_key(db_id, table_id, index_id);
                crate::txn::txn_delete(&mut txn, graph_key).await?;
            }
            // Skip TiKV graph write and oversize check — graph is in S3.
        } else {
            // TiKV path: check oversize freeze, then write graph to TiKV.
            if let Some(frozen_meta_bytes) =
                check_graph_oversize_freeze(graph_bytes.len(), &updated_meta)
            {
                warn!(
                    table_id,
                    index_id,
                    graph_bytes = graph_bytes.len(),
                    limit = HNSW_GRAPH_MAX_BYTES,
                    "HNSW graph exceeds size limit — freezing index"
                );
                txn_put(&mut txn, meta_key, frozen_meta_bytes).await?;
                txn.commit().await?;
                return Ok(());
            }

            // 6. Atomic write: new base graph + update meta.
            txn_put(
                &mut txn,
                hnsw_graph_key(db_id, table_id, index_id),
                graph_bytes,
            )
            .await?;
            let meta_bytes_new = serde_json::to_vec(&updated_meta)?;
            txn_put(&mut txn, meta_key, meta_bytes_new).await?;
        }
        delete_delta_keys(&mut txn, &batch_keys).await?;

        // 7. Commit.
        if let Err(e) = txn.commit().await {
            if let Some(g) = txn_guard.as_mut() {
                g.quarantine();
            }
            return Err(e.into());
        }
        total_deltas_merged += batch_count;

        info!(
            table_id,
            index_id,
            batch_count,
            graph_size = index.size(),
            "HNSW merge batch committed"
        );

        // If we got fewer than MERGE_BATCH_SIZE deltas, no more remain.
        if batch_count < MERGE_BATCH_SIZE {
            break;
        }
    }

    if total_deltas_merged > 0 {
        info!(
            table_id,
            index_id,
            total_deltas_merged,
            elapsed_ms = merge_start.elapsed().as_millis() as u64,
            "HNSW merge complete"
        );
    }
    Ok(())
}

/// Page size for storage size scan: keys per TiKV scan request.
const STORAGE_SCAN_PAGE_SIZE: u32 = 4096;

/// Rate-limit sleep between scan pages to avoid interfering with foreground traffic.
const STORAGE_SCAN_RATE_LIMIT_MS: u64 = 5;

/// Execute a storage size scan for a single database.
///
/// Performs a full paginated range scan over `[d_{db_id}_, d_{db_id+1}_)`,
/// classifies each key by prefix, and accumulates logical sizes (key.len + value.len).
/// Results are persisted to TiKV and cached in memory.
async fn execute_storage_size_scan(store: &Arc<TikvStore>, db_id: u64) -> Result<()> {
    use crate::storage::encode_database_data_range;
    use crate::storage_stats::{
        classify_key, global_storage_stats_cache, parse_legacy_hnsw_table_id,
        serialize_storage_stats, DbStorageStats, KeyCategory, TableStorageStats,
    };
    use tikv_client::BoundRange;

    let scan_start = std::time::Instant::now();
    let (range_start, range_end) = encode_database_data_range(db_id);

    let mut data_bytes: u64 = 0;
    let mut index_bytes: u64 = 0;
    let mut metadata_bytes: u64 = 0;
    let mut table_stats: std::collections::HashMap<u64, TableStorageStats> =
        std::collections::HashMap::new();

    let mut txn = store.begin_optimistic().await?;
    // This snapshot spans the full paginated scan (including rate-limit sleeps),
    // so it must participate in GC safepoint protection like other worker txns.
    let _txn_guard = crate::worker::active_txn_registry::global_registry()
        .map(|registry| registry.track_worker_txn(txn.start_timestamp().version()));
    let mut cursor = range_start;

    loop {
        let range: BoundRange = (cursor.clone()..range_end.clone()).into();
        let kv_pairs: Vec<tikv_client::KvPair> =
            txn.scan(range, STORAGE_SCAN_PAGE_SIZE).await?.collect();
        let page_count = kv_pairs.len();

        if page_count == 0 {
            break;
        }

        for pair in &kv_pairs {
            let key: Vec<u8> = pair.key().clone().into();
            let value: &[u8] = pair.value();
            let entry_bytes = (key.len() + value.len()) as u64;

            let classification = classify_key(&key);
            match classification.category {
                KeyCategory::Data => {
                    data_bytes += entry_bytes;
                    if let Some(table_id) = classification.table_id {
                        let ts = table_stats
                            .entry(table_id)
                            .or_insert_with(|| TableStorageStats {
                                table_id,
                                ..Default::default()
                            });
                        ts.data_bytes += entry_bytes;
                    }
                }
                KeyCategory::Index => {
                    index_bytes += entry_bytes;
                    if let Some(table_id) = classification.table_id {
                        let ts = table_stats
                            .entry(table_id)
                            .or_insert_with(|| TableStorageStats {
                                table_id,
                                ..Default::default()
                            });
                        ts.index_bytes += entry_bytes;
                    }
                }
                KeyCategory::Metadata => {
                    metadata_bytes += entry_bytes;
                }
                KeyCategory::Unknown => {
                    metadata_bytes += entry_bytes;
                }
            }
        }

        let last_key: Vec<u8> = kv_pairs.last().unwrap().key().clone().into();
        cursor = last_key;
        cursor.push(0x00);

        if (page_count as u32) < STORAGE_SCAN_PAGE_SIZE {
            break;
        }

        tokio::time::sleep(Duration::from_millis(STORAGE_SCAN_RATE_LIMIT_MS)).await;
    }

    // Legacy: HNSW index KV is encoded as string keys (decimal IDs) outside the
    // v2 database range, so we need an extra scan for those bytes.
    //
    // Example prefix: `d_{db_id}_hnsw_...`
    let hnsw_prefix: Vec<u8> = format!("d_{db_id}_hnsw_").into_bytes();
    let mut hnsw_end = hnsw_prefix.clone();
    if let Some(last) = hnsw_end.last_mut() {
        *last = last.wrapping_add(1);
    }
    let mut hnsw_cursor = hnsw_prefix.clone();

    loop {
        let range: BoundRange = (hnsw_cursor.clone()..hnsw_end.clone()).into();
        let kv_pairs: Vec<tikv_client::KvPair> =
            txn.scan(range, STORAGE_SCAN_PAGE_SIZE).await?.collect();
        let page_count = kv_pairs.len();

        if page_count == 0 {
            break;
        }

        for pair in &kv_pairs {
            let key: Vec<u8> = pair.key().clone().into();
            let value: &[u8] = pair.value();
            let entry_bytes = (key.len() + value.len()) as u64;

            index_bytes += entry_bytes;

            // Best-effort table attribution for legacy HNSW keys.
            if key.len() >= hnsw_prefix.len() && key[..hnsw_prefix.len()] == hnsw_prefix[..] {
                if let Some(table_id) = parse_legacy_hnsw_table_id(&key[hnsw_prefix.len()..]) {
                    let ts = table_stats
                        .entry(table_id)
                        .or_insert_with(|| TableStorageStats {
                            table_id,
                            ..Default::default()
                        });
                    ts.index_bytes += entry_bytes;
                }
            }
        }

        let last_key: Vec<u8> = kv_pairs.last().unwrap().key().clone().into();
        hnsw_cursor = last_key;
        hnsw_cursor.push(0x00);

        if (page_count as u32) < STORAGE_SCAN_PAGE_SIZE {
            break;
        }

        tokio::time::sleep(Duration::from_millis(STORAGE_SCAN_RATE_LIMIT_MS)).await;
    }

    // Read-only transaction — just drop it, no commit needed.
    txn.rollback().await.ok();

    let scan_duration_ms = scan_start.elapsed().as_millis() as i64;
    let scanned_at_ms = now_epoch_ms();

    let stats = DbStorageStats {
        database_id: db_id,
        data_bytes,
        index_bytes,
        metadata_bytes,
        tables: table_stats,
        scanned_at_ms,
        scan_duration_ms,
    };

    let stats_key = crate::storage::encode_storage_stats_key_v2(db_id);
    let stats_value = serialize_storage_stats(&stats);
    let mut persist_txn = store.begin().await?;
    crate::txn::txn_put(&mut persist_txn, stats_key, stats_value).await?;
    persist_txn.commit().await?;

    let keyspace = store.keyspace().unwrap_or("default");
    global_storage_stats_cache().put(keyspace, db_id, stats);

    info!(
        db_id,
        data_bytes, index_bytes, metadata_bytes, scan_duration_ms, "Storage size scan complete"
    );

    Ok(())
}

/// Enqueue a storage size scan task for a specific database.
///
/// Called by `db9_refresh_storage_stats()` and by the periodic reconciler.
pub(crate) async fn enqueue_storage_scan(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
) -> Result<()> {
    let entry = TaskQueueEntry::new(
        keyspace.to_string(),
        db_id,
        db_id as i64,
        TaskType::StorageSizeScan,
        String::new(),
        "system".to_string(),
        200, // low priority — background housekeeping
    );
    let fire_time = now_epoch_ms();
    let mut txn = system_store.begin().await?;
    system_store
        .put_worker_queue_entry(&mut txn, &entry, fire_time)
        .await?;
    txn.commit().await?;
    crate::worker::wake_worker();
    Ok(())
}

/// Warm-load persisted storage stats into the in-memory cache on startup.
pub(crate) async fn warm_load_storage_stats(
    pool: &TikvClientPool,
    system_store: &TikvStore,
) -> Result<()> {
    use crate::storage_stats::{deserialize_storage_stats, global_storage_stats_cache};

    let mut sys_txn = system_store.begin().await?;
    let registry_entries = system_store.list_worker_registry(&mut sys_txn).await?;
    sys_txn.commit().await?;

    let mut loaded = 0u32;
    for entry in registry_entries {
        let handle = match pool.acquire(Some(entry.keyspace.clone())).await {
            Ok(h) => h,
            Err(_) => continue,
        };
        let store = handle.store().clone();
        let mut txn = store.begin().await?;

        let databases = store.list_databases(&mut txn).await?;
        for db in databases {
            let stats_key = crate::storage::encode_storage_stats_key_v2(db.id);
            if let Some(data) = txn.get(stats_key).await? {
                if let Some(stats) = deserialize_storage_stats(&data) {
                    global_storage_stats_cache().put(&entry.keyspace, db.id, stats);
                    loaded += 1;
                }
            }
        }
        txn.rollback().await.ok();
    }

    if loaded > 0 {
        info!(loaded, "Warm-loaded persisted storage stats into cache");
    }
    Ok(())
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
    use crate::model::{DataType, IndexDef, TableSchema};

    fn idx(name: &str, state: IndexState) -> IndexDef {
        IndexDef {
            name: name.to_string(),
            id: 1,
            columns: vec!["c1".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }
    }

    #[test]
    fn background_statement_extension_context_uses_fresh_statement_state_per_call() {
        let first = background_statement_extension_context(true, "tenant_a", None);
        let second = background_statement_extension_context(true, "tenant_a", None);
        assert_eq!(
            first.execution_kind,
            crate::extensions::context::ExecutionKind::Cron
        );
        assert_eq!(
            second.execution_kind,
            crate::extensions::context::ExecutionKind::Cron
        );
        assert!(
            !std::sync::Arc::ptr_eq(&first.statement_state, &second.statement_state),
            "each background statement must start with a fresh statement-scoped extension state"
        );

        let interactive = background_statement_extension_context(false, "tenant_a", None);
        assert_eq!(
            interactive.execution_kind,
            crate::extensions::context::ExecutionKind::Interactive
        );
    }

    #[test]
    fn repair_incomplete_cic_states_repairs_building_and_writeonly() {
        let mut schema = TableSchema {
            name: "public.t".to_string(),
            table_id: 1,
            columns: vec![crate::model::ColumnDef {
                name: "c1".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
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
            rls_enabled: false,
            rls_force: false,
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

    #[test]
    fn active_job_guard_decrements_on_panic() {
        let active_jobs = Arc::new(AtomicU32::new(0));

        let panic_result = std::panic::catch_unwind({
            let active_jobs = active_jobs.clone();
            move || {
                let _guard = ActiveJobGuard::new(active_jobs.clone());
                assert_eq!(1, active_jobs.load(Ordering::Relaxed));
                panic!("intentional panic");
            }
        });

        assert!(panic_result.is_err());
        assert_eq!(0, active_jobs.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn run_with_guards_timeout_and_cancel_returns_timeout_error() {
        let cancel = Arc::new(Notify::new());
        let fut = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<(), anyhow::Error>(())
        };

        let err = run_with_guards(fut, Some(Duration::from_millis(5)), Some(&cancel), None)
            .await
            .expect_err("expected timeout");
        assert_eq!(err.to_string(), STATEMENT_TIMEOUT_ERROR);
    }

    #[tokio::test]
    async fn run_with_guards_timeout_without_cancel_returns_timeout_error() {
        let fut = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<(), anyhow::Error>(())
        };

        let err = run_with_guards(fut, Some(Duration::from_millis(5)), None, None)
            .await
            .expect_err("expected timeout");
        assert_eq!(err.to_string(), STATEMENT_TIMEOUT_ERROR);
    }

    #[tokio::test]
    async fn run_with_guards_without_timeout_cancel_returns_cancel_error() {
        let cancel = Arc::new(Notify::new());
        cancel.notify_one();
        let fut = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<(), anyhow::Error>(())
        };

        let err = run_with_guards(fut, None, Some(&cancel), None)
            .await
            .expect_err("expected cancel");
        assert_eq!(err.to_string(), CANCELLED_BY_ADMIN_ERROR);
    }

    #[tokio::test]
    async fn run_with_guards_without_timeout_or_cancel_returns_inner_result() {
        let fut = async { Ok::<usize, anyhow::Error>(7) };

        let result = run_with_guards(fut, None, None, None)
            .await
            .expect("expected success");
        assert_eq!(result, 7);
    }

    #[tokio::test]
    async fn run_with_guards_shutdown_token_returns_cancel_error() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let fut = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<(), anyhow::Error>(())
        };

        let err = run_with_guards(fut, None, None, Some(&shutdown))
            .await
            .expect_err("expected shutdown cancellation");
        assert_eq!(err.to_string(), CANCELLED_BY_ADMIN_ERROR);
    }

    #[test]
    fn execute_task_applies_timeout_to_whole_worker_transaction() {
        let source = include_str!("engine.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("engine.rs must contain #[cfg(test)]");
        let execute_task_start = prod_source
            .find("async fn execute_task(")
            .expect("execute_task must exist");
        let execute_bg_ddl_start = prod_source[execute_task_start..]
            .find("async fn execute_bg_ddl_backfill(")
            .map(|offset| execute_task_start + offset)
            .expect("execute_bg_ddl_backfill must exist after execute_task");
        let execute_task_source = &prod_source[execute_task_start..execute_bg_ddl_start];

        assert!(
            execute_task_source.contains("let task_timeout ="),
            "execute_task must compute a task-scoped timeout"
        );
        assert!(
            execute_task_source.contains("let _ = fut.await?;"),
            "individual statements must execute without per-statement timeout wrapping"
        );
        assert!(
            execute_task_source.contains("run_with_guards(")
                && execute_task_source.contains("task_timeout,")
                && execute_task_source.contains("cancel_signal.as_ref(),")
                && execute_task_source.contains("shutdown_signal.as_ref(),"),
            "execute_task must wrap the whole task future in run_with_guards"
        );
        assert!(
            !execute_task_source.contains("run_with_guards(fut, stmt_timeout"),
            "execute_task must not apply timeout per statement"
        );
    }

    #[test]
    fn worker_engine_shutdown_is_wired_into_run_loop_and_task_guards() {
        let source = include_str!("engine.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("engine.rs must contain #[cfg(test)]");
        let run_fn = prod_source
            .split("pub async fn run(&self)")
            .nth(1)
            .and_then(|rest| rest.split("async fn tick(&self)").next())
            .expect("engine.rs must define WorkerEngine::run before tick");

        assert!(
            run_fn.contains("_ = self.shutdown.cancelled()"),
            "WorkerEngine::run must stop polling when shutdown is requested"
        );
        assert!(
            prod_source.contains("Some(shutdown_signal.clone())")
                && prod_source.contains("Some(shutdown_signal)).await"),
            "worker task execution must propagate shutdown cancellation to running tasks"
        );
    }

    #[test]
    fn storage_size_scan_tracks_its_long_lived_read_transaction() {
        let source = include_str!("engine.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("engine.rs must contain #[cfg(test)]");
        let scan_start = prod_source
            .find("async fn execute_storage_size_scan(")
            .expect("execute_storage_size_scan must exist");
        let scan_end = prod_source[scan_start..]
            .find("/// Enqueue a storage size scan task")
            .map(|offset| scan_start + offset)
            .expect("execute_storage_size_scan must appear before enqueue helper");
        let scan_source = &prod_source[scan_start..scan_end];

        assert!(
            scan_source.contains("let mut txn = store.begin_optimistic().await?;"),
            "storage size scan must keep its paginated snapshot in a single optimistic transaction"
        );
        assert!(
            scan_source.contains("track_worker_txn(txn.start_timestamp().version())"),
            "storage size scan must publish its long-lived scan transaction in the active txn registry"
        );
    }

    #[test]
    fn hnsw_startup_sweep_tracks_its_read_transaction() {
        let source = include_str!("engine.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("engine.rs must contain #[cfg(test)]");
        let fn_start = prod_source
            .find("pub(crate) async fn enqueue_pending_hnsw_merges(")
            .expect("enqueue_pending_hnsw_merges must exist");
        let fn_end = prod_source[fn_start..]
            .find("\npub")
            .map(|offset| fn_start + offset)
            .unwrap_or(prod_source.len());
        let fn_source = &prod_source[fn_start..fn_end];

        assert!(
            fn_source.contains("track_worker_txn(txn.start_timestamp().version())"),
            "HNSW startup sweep must publish its tenant snapshot in the active txn registry \
             — the scan is proportional to tenant size and can outlive gc_life_time on large tenants"
        );
    }

    #[test]
    fn parse_hnsw_merge_command_accepts_valid_shape() {
        let (table_id, index_id) =
            parse_hnsw_merge_command("__hnsw_merge 123 456").expect("valid command");
        assert_eq!(table_id, 123);
        assert_eq!(index_id, 456);
    }

    #[test]
    fn parse_hnsw_merge_command_rejects_extra_args() {
        let err = parse_hnsw_merge_command("__hnsw_merge 1 2 trailing")
            .expect_err("extra args must be rejected");
        assert!(
            err.to_string().contains("invalid hnsw_merge command args"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_hnsw_merge_command_rejects_missing_args() {
        let err =
            parse_hnsw_merge_command("__hnsw_merge 1").expect_err("missing index_id must fail");
        assert!(
            err.to_string()
                .contains("missing index_id in hnsw_merge command"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_hnsw_merge_command_rejects_non_numeric() {
        let err = parse_hnsw_merge_command("__hnsw_merge abc 2")
            .expect_err("non-numeric table_id must fail");
        assert!(
            err.to_string().contains("invalid digit") || err.to_string().contains("number"),
            "unexpected parse error: {err}"
        );
    }

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn finalize_failure_in_claim_and_execute_releases_claim_and_requeues_cron() {
        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_finalize_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = crate::worker::config::WorkerConfig {
            enabled: true,
            system_keyspace: system_keyspace.clone(),
            cron_job_timeout_ms: 5000,
            ..Default::default()
        };
        let system_store = crate::worker::init_system_store(pd_endpoints.clone(), &cfg)
            .await
            .expect("init system store")
            .expect("store must be present");
        let pool = Arc::new(crate::pool::TikvClientPool::new(pd_endpoints));
        let metrics = Arc::new(crate::worker::metrics::WorkerMetrics::new());

        // Setup: tenant store with cron job
        let keyspace = format!(
            "test_finalize_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let db_id = 1_u64;
        let task_id = 42_i64;

        {
            let handle = pool
                .acquire(Some(keyspace.clone()))
                .await
                .expect("acquire tenant handle");
            let tenant_store = handle.store().clone();
            let mut txn = tenant_store.begin().await.unwrap();
            tenant_store
                .set_cron_enabled(&mut txn, db_id)
                .await
                .unwrap();
            let job = crate::cron::types::CronJob {
                job_id: task_id,
                schedule: "*/5 * * * *".to_string(),
                command: "SELECT 1".to_string(),
                nodename: String::new(),
                nodeport: 0,
                database: "postgres".to_string(),
                username: "admin".to_string(),
                active: true,
                jobname: None,
                max_runtime_ms: None,
            };
            tenant_store
                .put_cron_job(&mut txn, db_id, &job)
                .await
                .unwrap();
            txn.commit().await.unwrap();
        }

        // Setup: queue entry in system store
        let entry = TaskQueueEntry::new(
            keyspace.clone(),
            db_id,
            task_id,
            TaskType::Cron,
            "SELECT 1".to_string(),
            "admin".to_string(),
            100,
        )
        .with_schedule("*/5 * * * *".to_string());

        let fire_time_ms = crate::worker::now_epoch_ms();
        let queue_key = {
            let mut txn = system_store.begin().await.unwrap();
            system_store
                .put_worker_queue_entry(&mut txn, &entry, fire_time_ms)
                .await
                .unwrap();
            txn.commit().await.unwrap();

            // Read back the queue key
            let mut txn2 = system_store.begin().await.unwrap();
            let entries = system_store
                .scan_due_queue_entries(&mut txn2, i64::MAX, 1000)
                .await
                .unwrap();
            let (key, _) = entries
                .into_iter()
                .find(|(_, e)| e.task_id == task_id && e.keyspace == keyspace)
                .expect("queue entry must exist");
            txn2.rollback().await.ok();
            key
        };

        // Call the REAL claim_and_execute code path with injected finalize failure
        let result = WorkerEngine::claim_and_execute_core(
            &system_store,
            &pool,
            &cfg,
            &metrics,
            queue_key,
            entry.clone(),
            CancellationToken::new(),
            |_store, _db_id, _run, _status, _msg, _start, _end| async {
                Err(anyhow!("injected: TiKV write error in finalize_cron_run"))
            },
        )
        .await;

        // Verify: finalize error propagated (claim_and_execute_core returns Err)
        assert!(
            result.is_err(),
            "claim_and_execute_core must propagate finalize error after cleanup"
        );
        assert!(
            result.unwrap_err().to_string().contains("injected"),
            "propagated error must be the finalize error"
        );

        // Verify persisted state: cleanup DID run
        let mut txn = system_store.begin().await.unwrap();

        // Worker claim MUST be released
        let claims = system_store.list_worker_claims(&mut txn).await.unwrap();
        assert!(
            !claims.iter().any(|(_, c)| c.worker_id == cfg.worker_id),
            "INVARIANT VIOLATED: worker claim must be deleted after cleanup"
        );

        // Next cron fire MUST be requeued (new queue entry with future fire time)
        let queue = system_store
            .scan_due_queue_entries(&mut txn, i64::MAX, 1000)
            .await
            .unwrap();
        assert!(
            queue.iter().any(|(_, e)| e.task_id == task_id
                && e.keyspace == keyspace
                && e.task_type == TaskType::Cron),
            "INVARIANT VIOLATED: next cron fire must be requeued after cleanup"
        );

        txn.rollback().await.ok();
    }

    // ── frozen hotfix: engine entry-point helper tests ────────────

    #[test]
    fn should_skip_frozen_merge_returns_true_for_frozen_index() {
        use crate::sql::hnsw::storage::HnswMeta;
        let meta = HnswMeta {
            count: 5000,
            capacity: 10000,
            dimensions: 1536,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: crate::sql::hnsw::storage::HnswLabelMode::Direct,
            frozen: true,
            graph_version: 0,
            dropped_at: None,
        };
        // This is the exact function called in execute_hnsw_merge dispatch.
        // When S3 is not configured, frozen indexes should be skipped.
        // Note: should_skip_frozen_merge now returns false when S3 is enabled,
        // but in unit tests S3 is not initialized, so it returns true.
        assert!(super::should_skip_frozen_merge(&meta));
    }

    #[test]
    fn should_skip_frozen_merge_returns_false_for_normal_index() {
        use crate::sql::hnsw::storage::HnswMeta;
        let meta = HnswMeta {
            count: 100,
            capacity: 200,
            dimensions: 128,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: crate::sql::hnsw::storage::HnswLabelMode::Direct,
            frozen: false,
            graph_version: 0,
            dropped_at: None,
        };
        assert!(!super::should_skip_frozen_merge(&meta));
    }

    #[test]
    fn check_graph_oversize_freeze_returns_none_for_small_graph() {
        use crate::sql::hnsw::storage::HnswMeta;
        let meta = HnswMeta {
            count: 10,
            capacity: 20,
            dimensions: 3,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: crate::sql::hnsw::storage::HnswLabelMode::Direct,
            frozen: false,
            graph_version: 0,
            dropped_at: None,
        };
        // Small graph: no freeze.
        let result = super::check_graph_oversize_freeze(1024, &meta);
        assert!(result.is_none());
    }

    #[test]
    fn check_graph_oversize_freeze_returns_frozen_meta_for_oversized_graph() {
        use crate::sql::hnsw::storage::HnswMeta;
        let meta = HnswMeta {
            count: 5000,
            capacity: 10000,
            dimensions: 1536,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: crate::sql::hnsw::storage::HnswLabelMode::Direct,
            frozen: false,
            graph_version: 0,
            dropped_at: None,
        };
        // Oversized graph: should return frozen meta bytes.
        let result = super::check_graph_oversize_freeze(super::HNSW_GRAPH_MAX_BYTES + 1, &meta);
        assert!(result.is_some());
        // The returned bytes should deserialize to frozen=true.
        let frozen: HnswMeta = serde_json::from_slice(&result.unwrap()).unwrap();
        assert!(frozen.frozen);
        assert_eq!(frozen.count, 5000);
        assert_eq!(frozen.dimensions, 1536);
    }

    #[test]
    fn check_graph_oversize_freeze_at_exact_boundary_does_not_freeze() {
        use crate::sql::hnsw::storage::HnswMeta;
        let meta = HnswMeta {
            count: 100,
            capacity: 200,
            dimensions: 128,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: crate::sql::hnsw::storage::HnswLabelMode::Direct,
            frozen: false,
            graph_version: 0,
            dropped_at: None,
        };
        // Exactly at boundary: not oversize (uses >).
        let result = super::check_graph_oversize_freeze(super::HNSW_GRAPH_MAX_BYTES, &meta);
        assert!(result.is_none());
    }

    #[test]
    fn frozen_skip_in_sweep_uses_should_skip_frozen_merge() {
        use crate::sql::hnsw::storage::HnswMeta;
        // Simulates the sweep loop: for each index, read meta, check frozen.
        let metas = [
            (
                "frozen_idx",
                HnswMeta {
                    count: 5000,
                    capacity: 10000,
                    dimensions: 1536,
                    distance_metric: "l2".to_string(),
                    m: 16,
                    ef_construction: 200,
                    storage_version: 1,
                    label_mode: crate::sql::hnsw::storage::HnswLabelMode::Direct,
                    frozen: true,
                    graph_version: 0,
                    dropped_at: None,
                },
            ),
            (
                "normal_idx",
                HnswMeta {
                    count: 100,
                    capacity: 200,
                    dimensions: 128,
                    distance_metric: "l2".to_string(),
                    m: 16,
                    ef_construction: 200,
                    storage_version: 1,
                    label_mode: crate::sql::hnsw::storage::HnswLabelMode::Direct,
                    frozen: false,
                    graph_version: 0,
                    dropped_at: None,
                },
            ),
        ];
        // The real sweep loop calls should_skip_frozen_merge for each index.
        let enqueued: Vec<_> = metas
            .iter()
            .filter(|(_, meta)| !super::should_skip_frozen_merge(meta))
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(enqueued, vec!["normal_idx"]);
    }

    // ── comprehensive GC safepoint regression guard ────────────

    /// Extract all `fn`/`async fn` bodies from Rust source that contain a
    /// `store.begin()` or `store.begin_optimistic()` call.  Returns
    /// `(fn_signature_line, fn_body)` pairs.
    ///
    /// Heuristic: walk brace depth from the opening `{` of each function.
    /// All indices are BYTE offsets (safe for str slicing on ASCII-dominated Rust source).
    fn extract_fns_with_begin(source: &str) -> Vec<(String, String)> {
        // Match TiKV store begin calls but NOT session.begin() which is a
        // session-level transaction manager with its own GC registration.
        let tikv_begin = |body: &str| -> bool {
            for line in body.lines() {
                let trimmed = line.trim();
                // Skip session.begin() — session-managed GC registration.
                if trimmed.contains("session.begin()") {
                    continue;
                }
                if trimmed.contains(".begin().await")
                    || trimmed.contains(".begin_optimistic().await")
                {
                    return true;
                }
            }
            false
        };

        let bytes = source.as_bytes();
        let len = bytes.len();
        let mut results: Vec<(String, String)> = Vec::new();
        let mut search_from = 0usize;

        while let Some(rel) = source[search_from..].find("fn ") {
            let fn_pos = search_from + rel; // byte offset of "fn "

            // Walk backwards to capture `pub`, `async`, attributes on the same line.
            let sig_start = source[..fn_pos].rfind('\n').map(|p| p + 1).unwrap_or(0);

            // Find the opening brace after `fn `.
            let brace_start = match source[fn_pos..].find('{') {
                Some(offset) => fn_pos + offset,
                None => {
                    search_from = fn_pos + 3;
                    continue;
                }
            };

            let sig_line = source[sig_start..brace_start].trim().to_string();

            // Walk brace depth to find the matching closing brace.
            let mut depth: i32 = 0;
            let mut k = brace_start;
            let mut in_line_comment = false;
            let mut in_string = false;
            let mut in_raw_string = false;
            let mut raw_hashes = 0usize;

            loop {
                if k >= len {
                    break;
                }
                let b = bytes[k];

                if in_line_comment {
                    if b == b'\n' {
                        in_line_comment = false;
                    }
                    k += 1;
                    continue;
                }

                if in_raw_string {
                    // End of raw string: `"` followed by `raw_hashes` `#`s
                    if b == b'"' {
                        let mut h = 0;
                        while k + 1 + h < len && bytes[k + 1 + h] == b'#' && h < raw_hashes {
                            h += 1;
                        }
                        if h == raw_hashes {
                            in_raw_string = false;
                            k += 1 + h;
                            continue;
                        }
                    }
                    k += 1;
                    continue;
                }

                if in_string {
                    if b == b'\\' {
                        k += 2; // skip escaped char
                        continue;
                    }
                    if b == b'"' {
                        in_string = false;
                    }
                    k += 1;
                    continue;
                }

                // Not inside any literal context.
                match b {
                    b'/' if k + 1 < len && bytes[k + 1] == b'/' => {
                        in_line_comment = true;
                        k += 2;
                        continue;
                    }
                    b'r' if k + 1 < len => {
                        // Detect raw string: r#"..."# or r##"..."##, etc.
                        let mut h = 0;
                        while k + 1 + h < len && bytes[k + 1 + h] == b'#' {
                            h += 1;
                        }
                        if h > 0 && k + 1 + h < len && bytes[k + 1 + h] == b'"' {
                            in_raw_string = true;
                            raw_hashes = h;
                            k += 2 + h; // skip r###"
                            continue;
                        }
                    }
                    b'"' => {
                        in_string = true;
                        k += 1;
                        continue;
                    }
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            k += 1; // include closing brace
                            break;
                        }
                    }
                    _ => {}
                }
                k += 1;
            }

            let fn_body = &source[brace_start..k];

            // Check if this function body contains a TiKV store begin() call.
            let has_begin = tikv_begin(fn_body);
            if has_begin {
                results.push((sig_line, fn_body.to_string()));
            }

            search_from = k;
        }

        results
    }

    /// Extract a short function name from a signature line like
    /// `pub async fn foo(` -> `"foo"`.
    fn fn_name_from_sig(sig: &str) -> &str {
        // Find `fn ` and then the identifier
        if let Some(fn_pos) = sig.find("fn ") {
            let after_fn = &sig[fn_pos + 3..];
            let end = after_fn
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(after_fn.len());
            &after_fn[..end]
        } else {
            sig
        }
    }

    #[test]
    fn all_long_lived_worker_txns_must_register_with_gc_safepoint() {
        // ── Source files to scan ────────────────────────────────
        //
        // Each tuple: (file label, source text).
        // We use include_str! so the test tracks the ACTUAL source at compile
        // time — no runtime file I/O, no chance of stale caches.
        let sources: &[(&str, &str)] = &[
            ("worker/engine.rs", include_str!("engine.rs")),
            ("worker/gc.rs", include_str!("gc.rs")),
            ("cron/worker.rs", include_str!("../cron/worker.rs")),
            (
                "sql/ddl/create_index.rs",
                include_str!("../sql/ddl/create_index.rs"),
            ),
            ("sql/ddl/mod.rs", include_str!("../sql/ddl/mod.rs")),
            (
                "sql/executor/bg_sql.rs",
                include_str!("../sql/executor/bg_sql.rs"),
            ),
            (
                "sql/executor/core/mod.rs",
                include_str!("../sql/executor/core/mod.rs"),
            ),
            (
                "sql/executor/core/guc_engine.rs",
                include_str!("../sql/executor/core/guc_engine.rs"),
            ),
            (
                "sql/executor/cron.rs",
                include_str!("../sql/executor/cron.rs"),
            ),
            (
                "sql/executor/dml_analyzed/mod.rs",
                include_str!("../sql/executor/dml_analyzed/mod.rs"),
            ),
            (
                "sql/executor/procedure/materialized_views.rs",
                include_str!("../sql/executor/procedure/materialized_views.rs"),
            ),
            (
                "sql/executor/table_utils/mod.rs",
                include_str!("../sql/executor/table_utils/mod.rs"),
            ),
            ("session_context.rs", include_str!("../session_context.rs")),
            ("main.rs", include_str!("../main.rs")),
            (
                "protocol/handler/dynamic/startup.rs",
                include_str!("../protocol/handler/dynamic/startup.rs"),
            ),
            (
                "protocol/handler/dynamic/query.rs",
                include_str!("../protocol/handler/dynamic/query.rs"),
            ),
            (
                "sql/session/transaction.rs",
                include_str!("../sql/session/transaction.rs"),
            ),
            ("auth/rbac.rs", include_str!("../auth/rbac.rs")),
            (
                "extensions/fs/ws/auth.rs",
                include_str!("../extensions/fs/ws/auth.rs"),
            ),
            (
                "storage/tikv_store/mod.rs",
                include_str!("../storage/tikv_store/mod.rs"),
            ),
            (
                "storage/tikv_store/migrations.rs",
                include_str!("../storage/tikv_store/migrations.rs"),
            ),
            (
                "storage/tikv_store/sequences.rs",
                include_str!("../storage/tikv_store/sequences.rs"),
            ),
        ];

        // ── SHORT-LIVED ALLOWLIST ──────────────────────────────
        //
        // Functions listed here are verified safe WITHOUT track_worker_txn
        // registration.  Each entry MUST have a comment explaining WHY.
        //
        // When you add a new store.begin() call, either:
        //   (a) Add track_worker_txn() if the txn is long-lived, OR
        //   (b) Add the function here with a justification.
        //
        // Categories:
        //   [tick]      — single metadata read + immediate commit/rollback
        //   [claim]     — pessimistic claim attempt, bounded by 1 key write
        //   [enqueue]   — write ≤ handful of queue/registry entries + commit
        //   [reconcile] — bounded metadata scan (registry list, not data)
        //   [finalize]  — single record update + commit
        //   [lookup]    — point read or tiny scan + immediate rollback/commit
        //   [bootstrap] — one-time startup initialization (few key writes)
        //   [migration] — one-time schema migration at startup
        //   [session]   — session-scoped txn (registered via connection GC)
        //   [DDL]       — session-scoped DDL txn (registered via connection GC or session rebind)
        //   [autocommit]— optimistic CAS loop with immediate commit per attempt

        let allowlist: &[&str] = &[
            // ── worker/engine.rs ──

            // [tick] Scans due queue entries (bounded by limit=1000) + immediate commit.
            "tick",
            // [reconcile] Reads registry list (small metadata) + immediate commit.
            "reconcile_cron_jobs",
            // [reconcile] Reads system queue + tenant cron state; bounded metadata operations.
            "reconcile_cron_for_db",
            // [reconcile] Reads registry list (small metadata) + immediate commit.
            "reconcile_incomplete_cic_indexes",
            // [reconcile] Scans table schemas for a single DB; bounded by schema count + commit.
            "reconcile_incomplete_cic_indexes_for_db",
            // [claim] Pessimistic claim attempt: 1 key check + commit.
            "claim_and_execute_core",
            // [lookup] Reads cron state + records run; bounded by single job lookup + commit.
            "claim_and_record_cron_run",
            // [lookup] Point read: checks cron enabled + loads single job; commit.
            "load_next_cron_queue_entry",
            // [finalize] Updates single cron run record + commit.
            "finalize_cron_run",
            // [lookup] Single point read to resolve database name; immediate commit.
            "execute_task",
            // [lookup] Reads schema for one table; immediate commit.
            "execute_bg_ddl_backfill",
            // [reconcile] Reads registry list (small metadata) + immediate commit.
            "reconcile_storage_scans",
            // [reconcile] Reads registry list (small metadata) + immediate commit.
            "reconcile_hnsw_merges",
            // [enqueue] Single put to system store queue + commit.
            "enqueue_storage_scan",
            // [reconcile] Reads registry + iterates DBs to warm cache; read-only + rollback.
            "warm_load_storage_stats",
            // [finalize] Single stats key write after scan completes; immediate commit.
            // (The long-lived scan txn in execute_storage_size_scan IS tracked; this is
            // just the final persist_txn that writes the result.)
            "execute_storage_size_scan",
            // ── worker/gc.rs ──

            // [lookup] Neutralize GC instance state: single key write + commit.
            "clear_gc_instance_state",
            // [lookup] Publish GC instance state: single key write + commit.
            "publish_gc_instance_state",
            // [lookup] Read all GC instance states (small registry) + rollback.
            "advance_gc_safepoint",
            // [reconcile] Read registry list + immediate commit.
            "sweep_hnsw_delta_backlogs",
            // [reconcile] Read registry list + immediate commit.
            "cleanup_cron_runs",
            // [claim] Scans claim batch (bounded by batch_size) + commit/rollback.
            "cleanup_orphan_claims_batch",
            // [lookup] Read GC instance states (small registry) + rollback.
            "reap_stale_gc_instance_states",
            // [lookup] Delete stale GC instance rows (bounded) + commit.
            "reap_stale_gc_instance_states_from_scan",
            // [lookup] Read all HNSW metas for S3 GC sweep; bounded scan + rollback.
            "read_all_hnsw_metas",
            // [lookup] Scan S3 orphan prefixes + delete; bounded + commit.
            "sweep_hnsw_s3_orphans",
            // [lookup] Point read of S3 prefix GC marker; immediate commit/rollback.
            "read_hnsw_s3_prefix_gc_marker",
            // [finalize] Single key write for S3 prefix GC marker; immediate commit.
            "write_hnsw_s3_prefix_gc_marker",
            // [finalize] Single key delete for S3 prefix GC marker; immediate commit.
            "delete_hnsw_s3_prefix_gc_marker",
            // [lookup] Point read of S3 retired version marker; immediate commit/rollback.
            "read_hnsw_s3_retired_version_marker",
            // [finalize] Single key write for S3 retired version marker; immediate commit.
            "write_hnsw_s3_retired_version_marker",
            // [finalize] Single key delete for S3 retired version marker; immediate commit.
            "delete_hnsw_s3_retired_version_marker",
            // [reconcile] Scan + delete all retired version markers for an index; bounded + commit.
            "delete_hnsw_s3_retired_version_markers_for_index",
            // [finalize] Single key delete for HNSW meta; immediate commit.
            "delete_hnsw_meta",
            // ── cron/worker.rs ──
            // (gc_database IS long-lived and MUST have track_worker_txn — not in allowlist)

            // ── sql/ddl/create_index.rs ──

            // [enqueue] Write queue entry + registry update for CIC backfill; immediate commit.
            "execute_create_index",
            // [DDL] Single schema read + state update + commit; bounded by one table.
            "update_index_state",
            // (backfill_index_by_name IS long-lived and has track_active_worker_txn — not in allowlist)
            // (reconcile_index_pass IS long-lived and has track_active_worker_txn — not in allowlist)

            // ── sql/ddl/mod.rs ──

            // (maybe_rotate_backfill_txn calls begin_replacement_session_owned_txn
            //  which re-registers via session context — not in allowlist; see separate test)

            // ── sql/executor/bg_sql.rs ──

            // [enqueue] Writes queue entry + registry update; immediate commit.
            "execute_bg_sql",
            // [enqueue] Launches background task; single put + commit.
            "execute_bg_launch",
            // [lookup] Point read for bg_result + scan for pending; immediate commit.
            "execute_bg_result",
            // ── sql/executor/core/mod.rs ──

            // [enqueue] Writes trigger queue entries; immediate commit.
            "flush_trigger_activations",
            // [enqueue] Writes HNSW merge queue entries; immediate commit.
            "flush_pending_hnsw_merges",
            // ── sql/executor/core/guc_engine.rs ──

            // [lookup] Reads user/role for auth error message; immediate rollback.
            "session_auth_different_user_error",
            // ── sql/executor/cron.rs ──

            // [enqueue] Reschedule/dequeue cron entries in system store; immediate commit.
            "enqueue_cron_to_worker",
            // [enqueue] Delete queue entries for dequeued cron job; immediate commit.
            "dequeue_cron_from_worker",
            // ── sql/executor/dml_analyzed/mod.rs ──

            // [enqueue] Check-and-enqueue auto-analyze task; immediate commit.
            "maybe_enqueue_auto_analyze",
            // ── sql/executor/procedure/materialized_views.rs ──

            // [enqueue] Enqueue background refresh task; immediate commit.
            "execute_refresh_materialized_view",
            // [DDL] Refresh matview: single schema read + task enqueue + commit.
            "execute_refresh_materialized_view_cmd",
            // ── sql/executor/table_utils/mod.rs ──

            // [lookup] Reads trigger queue entries for stats view; bounded scan.
            "execute_async_trigger_stats_query",
            // ── session_context.rs ──

            // [session] Opens replacement session-owned txn; immediately re-registers
            // via refresh_current_session_txn_registration.
            "begin_replacement_session_owned_txn",
            // ── main.rs ──

            // [bootstrap] One-time auth bootstrap at startup; single write + commit.
            "main",
            "async_main",
            // ── protocol/handler/dynamic/startup.rs ──

            // [bootstrap] Per-connection auth bootstrap (idempotent); single write + commit.
            "do_startup",
            // [lookup] Auth check; single read + immediate commit/rollback.
            "authenticate_user",
            // ── protocol/handler/dynamic/query.rs ──

            // [lookup] Temporary read-only txn for prepared statement analysis; immediate rollback.
            "do_describe",
            // [session] COPY FROM uses session txn (registered via connection GC).
            "handle_copy_from_simple_query",
            // [session] Parse step creates temp txn for analysis; immediate rollback.
            "on_parse",
            // ── sql/session/transaction.rs ──

            // [session] Opens session-scoped transaction; registered via connection
            // active_txn_registry (register_connection call immediately follows).
            "begin",
            // ── auth/rbac.rs ──

            // [lookup] Check for superuser existence; immediate rollback.
            "is_initialized",
            // ── extensions/fs/ws/auth.rs ──

            // [bootstrap] WebSocket auth bootstrap + auth check; immediate commit/rollback.
            "authenticate_ws",
            // [lookup] WebSocket auth handler; single read + immediate commit/rollback.
            "handle_auth",
            // ── storage/tikv_store/mod.rs ──

            // [autocommit] Optimistic CAS loop; immediate commit per attempt.
            "autocommit_update_key",
            // [bootstrap] One-time format version check/init at startup; immediate commit.
            "check_format_version",
            // [bootstrap] One-time default database creation; immediate commit.
            "bootstrap_default_database",
            // ── storage/tikv_store/migrations.rs ──

            // [migration] One-time schema migration at startup.
            "ensure_view_relation_bindings_migration",
            // [migration] One-time schema migration at startup.
            "ensure_no_pk_fk_cascade_migration",
            // ── storage/tikv_store/sequences.rs ──

            // [autocommit] Optimistic CAS loop for sequence OID assignment; immediate commit.
            "ensure_sequence_oid",
            // [autocommit] Backfill sequence OID; immediate commit per attempt.
            "autocommit_backfill_sequence_oid",
            // [lookup] Read current sequence allocator value; immediate rollback.
            "migrate_identity_sequence_if_needed",
            // [migration] One-time implicit→standalone sequence migration; immediate commit.
            "maybe_migrate_implicit_sequence_to_standalone",
        ];

        // ── Scan and verify ────────────────────────────────────

        let mut violations: Vec<String> = Vec::new();

        for (file_label, source) in sources {
            // Strip test modules — only scan production code.
            let prod_source = source.split("#[cfg(test)]").next().unwrap_or(source);

            let fns = extract_fns_with_begin(prod_source);

            for (sig, body) in &fns {
                let name = fn_name_from_sig(sig);

                // Skip if on the allowlist.
                if allowlist.contains(&name) {
                    continue;
                }

                // Must contain track_worker_txn (either direct or via helper).
                let has_track =
                    body.contains("track_worker_txn") || body.contains("track_active_worker_txn");

                if !has_track {
                    violations.push(format!(
                        "  {file_label} :: {name}\n    \
                         This function contains store.begin() but does NOT call \
                         track_worker_txn() and is NOT in the short-lived allowlist.\n    \
                         Fix: either add track_worker_txn() if the txn is long-lived,\n    \
                         or add \"{name}\" to the allowlist in this test with a comment \
                         explaining why it's safe."
                    ));
                }
            }
        }

        assert!(
            violations.is_empty(),
            "\n\nGC SAFEPOINT REGRESSION: {} function(s) have unprotected \
             store.begin() calls.\n\nEvery long-lived TiKV transaction must \
             register with ActiveTxnRegistry via track_worker_txn() so the \
             GC safepoint does not advance past live snapshots.\n\n\
             Violations:\n{}\n",
            violations.len(),
            violations.join("\n\n")
        );
    }

    #[test]
    fn recovery_unfreeze_then_skip_check_returns_false() {
        use crate::sql::hnsw::storage::HnswMeta;
        // Frozen index: dispatch skips.
        let frozen_json = r#"{
            "count": 5000, "capacity": 10000, "dimensions": 1536,
            "distance_metric": "l2", "m": 16, "ef_construction": 200,
            "storage_version": 1, "frozen": true
        }"#;
        let meta: HnswMeta = serde_json::from_str(frozen_json).unwrap();
        assert!(super::should_skip_frozen_merge(&meta));

        // Operator unfreeze: same meta with frozen=false.
        let unfrozen_json = r#"{
            "count": 5000, "capacity": 10000, "dimensions": 1536,
            "distance_metric": "l2", "m": 16, "ef_construction": 200,
            "storage_version": 1, "frozen": false
        }"#;
        let meta2: HnswMeta = serde_json::from_str(unfrozen_json).unwrap();
        assert!(!super::should_skip_frozen_merge(&meta2));
    }
}
