use crate::cron::process_list::{get_process_list, RunningCronJob};
use crate::cron::types::{CronRun, CronRunStatus};
use crate::extensions::context::{with_context_opts, ExtensionContextOpts};
use crate::observability;
use crate::pool::TikvClientPool;
use crate::sql::ddl;
use crate::sql::executor::core::retry::is_retryable_tikv_error;
use crate::sql::parse_sql;
use crate::sql::query_context::{self, QueryContext};
use crate::sql::Executor;
use crate::storage::{CronRunClaimStatus, TikvStore, WqIndexRow};
use crate::worker::config::WorkerConfig;
use crate::worker::metrics::WorkerMetrics;

mod helpers;

use crate::worker::now_epoch_ms;
use crate::worker::types::*;
use anyhow::{anyhow, Result};
pub(crate) use helpers::HNSW_GRAPH_MAX_BYTES;
use helpers::{
    background_statement_extension_context, execute_hnsw_merge, parse_backfill_index_command,
    parse_hnsw_merge_command, repair_incomplete_cic_states, should_skip_frozen_merge,
    should_start_cic_backfill,
};
pub(crate) use helpers::{
    is_retryable_region_error, region_error_backoff, REGION_ERROR_MAX_RETRIES,
};
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
const WORKER_BGSQL_MAX_RETRY_ATTEMPTS: usize = 64;

async fn worker_bgsql_backoff(attempt: usize) {
    let base_ms = 5u64.saturating_mul(1u64 << attempt.min(6));
    let jitter_ms = rand::random::<u64>() % (base_ms + 1);
    tokio::time::sleep(Duration::from_millis(base_ms + jitter_ms)).await;
}

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
        if let Err(e) = self.reconcile_ddl_journal().await {
            warn!("DDL journal recovery failed (engine will continue): {}", e);
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
        // V2 descriptors (small values) up to `now`.
        let mut due_entries: Vec<(Vec<u8>, DueItem)> = self
            .system_store
            .scan_due_v2(&mut txn, now_ms, 1000)
            .await?
            .into_iter()
            .map(|(key, descriptor)| (key, DueItem::V2(descriptor)))
            .collect();
        // Migration window only: an OLD binary may still enqueue legacy
        // `_worker_queue_` entries during a rolling deploy. The new binary
        // executes them IN PLACE (never moves them to V2), sharing the same
        // worker-claim identity as the old binary so an entry is processed once
        // and deleted from its single namespace. V1 drains naturally: cron
        // requeues its next fire as V2, one-shots are executed and deleted.
        // Byte-safe (key scan + per-key point-get); gated so it is one empty RPC
        // once V1 is drained. NOTE: V2 is scanned first up to the limit and
        // legacy only fills the remainder, so legacy drains opportunistically
        // (not on a fixed schedule); a sustained backlog of >=limit due V2
        // entries deprioritizes it — acceptable since old-binary writes cease
        // once the deploy completes.
        if due_entries.len() < 1000 && self.system_store.legacy_queue_has_entries(&mut txn).await? {
            let remaining = (1000 - due_entries.len()) as u32;
            let legacy = self
                .system_store
                .scan_due_legacy_bytesafe(&mut txn, now_ms, remaining)
                .await?;
            due_entries.extend(
                legacy
                    .into_iter()
                    .map(|(key, entry)| (key, DueItem::Legacy(entry))),
            );
        }
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
        // 1. Existing cron entries. The V2 index is a bounded prefix scan that
        //    reads only tiny index values; the gated legacy scan covers pre-V2
        //    `_worker_queue_` entries (never indexed into V2) so reconcile does
        //    NOT re-enqueue a V2 copy of a job that still has a legacy entry —
        //    which would double-fire it during a rolling deploy.
        let mut txn = self.system_store.begin().await?;
        let existing_rows = self
            .system_store
            .index_rows_for_db_type(&mut txn, keyspace, db_id, TaskType::Cron)
            .await?;
        let legacy_entries = self
            .system_store
            .legacy_entries_for_db_type(&mut txn, keyspace, db_id, TaskType::Cron)
            .await?;
        txn.commit().await?;

        let existing_job_ids: HashSet<i64> = existing_rows
            .iter()
            .map(|r| r.task_id)
            .chain(legacy_entries.iter().map(|(_, task_id)| *task_id))
            .collect();

        // 2. Acquire tenant store and check cron state
        let handle = self.pool.acquire(Some(keyspace.to_string())).await?;
        let store = handle.store().clone();

        let mut tenant_txn = store.begin().await?;
        let cron_enabled = store.is_cron_enabled(&mut tenant_txn, db_id).await?;

        if !cron_enabled {
            tenant_txn.commit().await?;
            // Cron disabled but registry has cron bit — clean up all queue
            // entries across BOTH layers.
            let total = existing_rows.len() + legacy_entries.len();
            if total > 0 {
                let mut sys_txn = self.system_store.begin().await?;
                for r in &existing_rows {
                    self.system_store
                        .delete_task_v2(
                            &mut sys_txn,
                            &r.due_key,
                            &r.keyspace,
                            r.db_id,
                            r.task_type,
                            r.task_id,
                            r.fire_time_ms,
                        )
                        .await?;
                }
                for (key, _) in &legacy_entries {
                    self.system_store
                        .delete_worker_queue_entry(&mut sys_txn, key)
                        .await?;
                }
                sys_txn.commit().await?;
            }
            return Ok((0, total as u32));
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
                    .put_task_v2(&mut sys_txn, &queue_entry, next_fire)
                    .await?;
                enqueued += 1;
            }
            sys_txn.commit().await?;
        }

        // 5. Cleanup orphans across BOTH layers: queue entries whose job_id is
        //    not in active jobs.
        let orphan_rows: Vec<&WqIndexRow> = existing_rows
            .iter()
            .filter(|r| !active_job_ids.contains(&r.task_id))
            .collect();
        let orphan_legacy: Vec<&(Vec<u8>, i64)> = legacy_entries
            .iter()
            .filter(|(_, task_id)| !active_job_ids.contains(task_id))
            .collect();

        if !orphan_rows.is_empty() || !orphan_legacy.is_empty() {
            let mut sys_txn = self.system_store.begin().await?;
            for r in &orphan_rows {
                self.system_store
                    .delete_task_v2(
                        &mut sys_txn,
                        &r.due_key,
                        &r.keyspace,
                        r.db_id,
                        r.task_type,
                        r.task_id,
                        r.fire_time_ms,
                    )
                    .await?;
            }
            for (key, _) in &orphan_legacy {
                self.system_store
                    .delete_worker_queue_entry(&mut sys_txn, key)
                    .await?;
            }
            sys_txn.commit().await?;
            cleaned = (orphan_rows.len() + orphan_legacy.len()) as u32;
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

    /// Recover from incomplete DDL operations by scanning the DDL journal.
    ///
    /// For each journal entry left behind by a crash:
    /// - `CreateIndex`: delete the orphaned index key range, then remove the journal entry.
    /// - `CreateTableAsSelect`: drop the partially-created table, then remove the journal entry.
    ///
    /// DDL journal writes register `TASK_TYPE_DDL_JOURNAL` in the worker
    /// registry, so the standard registry enumeration discovers all databases
    /// that may have journal entries.
    async fn reconcile_ddl_journal(&self) -> Result<()> {
        let mut sys_txn = self.system_store.begin().await?;
        let registry_entries = self.system_store.list_worker_registry(&mut sys_txn).await?;
        sys_txn.commit().await?;

        for entry in registry_entries {
            if let Err(e) = self
                .reconcile_ddl_journal_for_db(&entry.keyspace, entry.db_id)
                .await
            {
                warn!(
                    "DDL journal recovery error for keyspace={} db_id={}: {}",
                    entry.keyspace, entry.db_id, e
                );
            }
        }
        Ok(())
    }

    async fn reconcile_ddl_journal_for_db(&self, keyspace: &str, db_id: u64) -> Result<()> {
        use crate::storage::DdlOperation;

        let handle = self.pool.acquire(Some(keyspace.to_string())).await?;
        let store = handle.store().clone();

        // Scan journal entries in a read transaction.
        let mut scan_txn = store.begin().await?;
        let journal_entries = store.scan_ddl_journal(&mut scan_txn, db_id).await?;
        scan_txn.commit().await?;

        if journal_entries.is_empty() {
            // The TASK_TYPE_DDL_JOURNAL bit may be stale (set by a prior
            // successful DDL whose producer intentionally did not clear it).
            // We intentionally do NOT clear it here: the journal scan and
            // registry update are on different stores (data vs system) with
            // no cross-store atomicity, so a concurrent DDL producer could
            // set the bit + write a journal entry between our scan and our
            // clear, leaving the new entry undiscoverable after a crash.
            // The cost of the stale bit is one cheap empty journal scan per
            // startup per affected database — acceptable.
            return Ok(());
        }

        // Process each entry in its own transaction to avoid exceeding TiKV
        // transaction size limits when multiple large orphans exist.
        for jentry in &journal_entries {
            let mut txn = store.begin().await?;
            match &jentry.operation {
                DdlOperation::CreateIndex {
                    table_id,
                    index_id,
                    index_name,
                    index_range_start,
                    index_range_end,
                } => {
                    // Delete orphaned index keys in batches, rotating the
                    // transaction between batches to stay within TiKV limits.
                    let mut cursor = index_range_start.clone();
                    loop {
                        let next = store
                            .delete_key_range_batch(&mut txn, cursor, index_range_end)
                            .await?;
                        match next {
                            Some(next_cursor) => {
                                txn.commit().await?;
                                txn = store.begin().await?;
                                cursor = next_cursor;
                            }
                            None => break,
                        }
                    }
                    // Release the index name reservation so the name can be reused.
                    // Note: txn_delete is a no-op for missing keys, so this only
                    // fails on real TiKV errors — propagate to preserve the
                    // journal entry for retry rather than leaving an orphaned
                    // sys_relname_ key.
                    store
                        .release_relation_name(&mut txn, db_id, index_name)
                        .await?;
                    store.delete_ddl_journal(&mut txn, db_id, jentry.id).await?;
                    txn.commit().await?;
                    info!(
                        "DDL journal: cleaned up orphaned index data (table_id={}, index_id={}, name={}) in db_id={}",
                        table_id, index_id, index_name, db_id
                    );
                }
                DdlOperation::CreateTableAsSelect {
                    table_id,
                    table_name,
                } => {
                    // Delete data rows in batches with transaction rotation to
                    // stay within TiKV mutation limits (CTAS can produce
                    // arbitrarily many committed rows before crash).
                    use crate::storage::encode_table_data_range_v2;
                    let (data_start, data_end) = encode_table_data_range_v2(db_id, *table_id);
                    let mut cursor = data_start;
                    loop {
                        let next = store
                            .delete_key_range_batch(&mut txn, cursor, &data_end)
                            .await?;
                        match next {
                            Some(next_cursor) => {
                                txn.commit().await?;
                                txn = store.begin().await?;
                                cursor = next_cursor;
                            }
                            None => break,
                        }
                    }
                    // Drop owned sequences (e.g. _rowid_seq) before dropping
                    // the table — drop_table() does not clean these up.
                    // Errors propagate so the journal is preserved for retry.
                    let seqs = store.list_sequences(&mut txn, db_id).await?;
                    for def in seqs {
                        if let Some((owned_table, _)) = &def.owned_by {
                            if owned_table == table_name {
                                let seq_name = def.full_name();
                                store.drop_sequence(&mut txn, db_id, &seq_name).await?;
                                store
                                    .release_relation_name(&mut txn, db_id, &seq_name)
                                    .await?;
                            }
                        }
                    }
                    // Clean up table metadata (schema, relation name, etc.)
                    // in a final small transaction.  This uses drop_table which
                    // handles schema deletion, index cleanup, relname release,
                    // comments, and statistics.  With all data rows already
                    // deleted above, the remaining metadata fits in one txn.
                    if let Err(e) = store.drop_table(&mut txn, db_id, table_name).await {
                        warn!(
                            "DDL journal: failed to drop CTAS table metadata '{}' in db_id={}: {}. \
                             Journal entry preserved for retry on next startup.",
                            table_name, db_id, e
                        );
                        txn.rollback().await.ok();
                        continue;
                    }
                    store.delete_ddl_journal(&mut txn, db_id, jentry.id).await?;
                    txn.commit().await?;
                    info!(
                        "DDL journal: cleaned up orphaned CTAS table '{}' in db_id={}",
                        table_name, db_id
                    );
                }
            }
        }

        // Note: we intentionally do NOT clear the TASK_TYPE_DDL_JOURNAL
        // registry bit here.  The journal (data store) and registry (system
        // store) are in different transactional domains — there is no way to
        // atomically verify the journal is empty and clear the bit.  A
        // concurrent DDL producer could set the bit and write a new journal
        // entry between our last journal delete and our registry update,
        // leaving the new entry undiscoverable after a crash.
        //
        // The stale bit causes only a cheap empty journal scan per startup.
        // If any entries failed cleanup (!all_cleaned), the bit must stay
        // set regardless so the next startup retries those entries.

        info!(
            "DDL journal: recovered {} incomplete operations in keyspace={} db_id={}",
            journal_entries.len(),
            keyspace,
            db_id
        );
        Ok(())
    }
    async fn claim_and_execute(
        system_store: &Arc<TikvStore>,
        pool: &Arc<TikvClientPool>,
        config: &WorkerConfig,
        metrics: &Arc<WorkerMetrics>,
        queue_key: Vec<u8>,
        due: DueItem,
        shutdown_signal: CancellationToken,
    ) -> Result<()> {
        Self::claim_and_execute_core(
            system_store,
            pool,
            config,
            metrics,
            queue_key,
            due,
            shutdown_signal,
            Self::finalize_cron_run,
        )
        .await
    }

    /// Delete a due entry using the correct layout: V2 removes the descriptor,
    /// index, and (split-type) payload together; legacy removes just the
    /// `_worker_queue_` key.
    async fn delete_due_entry(
        system_store: &Arc<TikvStore>,
        txn: &mut tikv_client::Transaction,
        is_v2: bool,
        due_key: &[u8],
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
    ) -> Result<()> {
        if is_v2 {
            system_store
                .delete_task_v2(
                    txn,
                    due_key,
                    &entry.keyspace,
                    entry.db_id,
                    entry.task_type.to_bitmask(),
                    entry.task_id,
                    fire_time_ms,
                )
                .await
        } else {
            system_store.delete_worker_queue_entry(txn, due_key).await
        }
    }

    /// Release a just-won worker claim (used when we decline to execute after
    /// claiming — e.g. the entry was concurrently deleted).
    async fn release_claim(
        system_store: &Arc<TikvStore>,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
        task_type: TaskType,
    ) -> Result<()> {
        let mut txn = system_store.begin().await?;
        system_store
            .delete_worker_claim(&mut txn, keyspace, db_id, task_id, fire_time_ms, task_type)
            .await?;
        txn.commit().await?;
        Ok(())
    }

    /// Core implementation of claim_and_execute, parameterized over the finalize
    /// function so tests can inject failures in the real code path.
    ///
    /// Handles both V2 descriptors and (during the migration window) legacy
    /// `_worker_queue_` entries. A legacy entry is executed IN PLACE and deleted
    /// from V1 — never moved to V2 — so an entry exists in a single namespace and
    /// the shared worker claim makes it execute-once even across an old+new
    /// binary deploy. A post-claim existence re-check closes the
    /// read-before/claim-after-release window: if the entry was deleted by
    /// another replica (or unschedule / DROP DATABASE reap) since the tick
    /// scanned it, we release the claim and skip rather than re-execute.
    async fn claim_and_execute_core<F, Fut>(
        system_store: &Arc<TikvStore>,
        pool: &Arc<TikvClientPool>,
        config: &WorkerConfig,
        metrics: &Arc<WorkerMetrics>,
        queue_key: Vec<u8>,
        due: DueItem,
        shutdown_signal: CancellationToken,
        finalize_fn: F,
    ) -> Result<()>
    where
        F: FnOnce(Arc<TikvStore>, u64, CronRun, CronRunStatus, Option<String>, i64, i64) -> Fut,
        Fut: Future<Output = Result<()>> + Send,
    {
        // The due key carries the prefix (V2 `_wq_due_v2_` vs legacy
        // `_worker_queue_`); decode fire_time and clean up accordingly.
        let is_v2 = crate::storage::is_wq_due_v2_key(&queue_key);
        let queue_fire_time_ms = if is_v2 {
            crate::storage::decode_wq_due_v2_fire_time(&queue_key)
        } else {
            crate::storage::decode_worker_queue_fire_time(&queue_key)
        }
        .ok_or_else(|| anyhow!("corrupted worker queue key: missing fire_time_ms"))?;
        let scheduled_minute = queue_fire_time_ms.div_euclid(60_000);
        let task_type = due.task_type();
        let (claim_keyspace, claim_db_id, claim_task_id) =
            (due.keyspace().to_string(), due.db_id(), due.task_id());
        let claim = WorkerClaim::new(config.worker_id.clone(), task_type);

        let mut txn = system_store.begin().await?;
        let claimed = system_store
            .try_claim_worker_task(
                &mut txn,
                &claim_keyspace,
                claim_db_id,
                claim_task_id,
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

        // Post-claim existence re-check: if the due key was deleted since the
        // tick scanned it — by another replica that executed it first, by
        // unschedule, or by a DROP DATABASE reap — do NOT execute. This makes
        // execution at-most-once across concurrent replicas (the shared claim
        // alone only blocks *simultaneous* execution, not read-before /
        // claim-after-release re-execution).
        let still_present = {
            let mut rtxn = system_store.begin().await?;
            let present = rtxn.get(queue_key.clone()).await?.is_some();
            rtxn.rollback().await.ok();
            present
        };
        if !still_present {
            Self::release_claim(
                system_store,
                &claim_keyspace,
                claim_db_id,
                claim_task_id,
                queue_fire_time_ms,
                task_type,
            )
            .await?;
            return Ok(());
        }

        // Hydrate the full entry now that we own the claim and confirmed it
        // exists. Legacy entries carry the command inline; split V2 types fetch
        // the command/username/schedule out-of-line by exact identity.
        let entry: TaskQueueEntry = match due {
            DueItem::Legacy(e) => e,
            DueItem::V2(descriptor) => {
                let payload = if descriptor.needs_payload() {
                    let mut ptxn = system_store.begin().await?;
                    let p = system_store
                        .get_task_payload_v2(
                            &mut ptxn,
                            task_type.to_bitmask(),
                            &claim_keyspace,
                            claim_db_id,
                            claim_task_id,
                            queue_fire_time_ms,
                        )
                        .await?;
                    ptxn.rollback().await.ok();
                    p
                } else {
                    None
                };
                match descriptor.into_entry(payload) {
                    Some(e) => e,
                    None => {
                        // Descriptor present but payload missing. put_task_v2 /
                        // delete_task_v2 write/remove descriptor+index+payload
                        // atomically, so the normal cause is a concurrent delete
                        // that already removed the descriptor too (no-op below).
                        // If instead the descriptor genuinely persists without a
                        // payload (corruption), tear down the orphaned
                        // descriptor+index here so we do NOT re-claim it every
                        // tick forever; then release the claim and skip.
                        let mut ctxn = system_store.begin().await?;
                        system_store
                            .delete_task_v2(
                                &mut ctxn,
                                &queue_key,
                                &claim_keyspace,
                                claim_db_id,
                                task_type.to_bitmask(),
                                claim_task_id,
                                queue_fire_time_ms,
                            )
                            .await?;
                        system_store
                            .delete_worker_claim(
                                &mut ctxn,
                                &claim_keyspace,
                                claim_db_id,
                                claim_task_id,
                                queue_fire_time_ms,
                                task_type,
                            )
                            .await?;
                        ctxn.commit().await?;
                        warn!(
                            "V2 task payload missing after claim; cleaned orphaned descriptor and skipped: \
                             keyspace={} db_id={} task_id={} type={:?} fire_time={}",
                            claim_keyspace, claim_db_id, claim_task_id, task_type, queue_fire_time_ms
                        );
                        return Ok(());
                    }
                }
            }
        };

        let (cron_run, keep_queue_entry) = if entry.task_type == TaskType::Cron {
            Self::claim_and_record_cron_run(pool, &entry, scheduled_minute).await?
        } else {
            (None, false)
        };
        let should_requeue_cron = cron_run.is_some();

        let exec_result = if entry.task_type == TaskType::Cron && cron_run.is_none() {
            Ok(0usize)
        } else if let Some((_, cron_db_id, ref run, _started_at, max_runtime_ms)) = cron_run {
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
            let deadline = if timeout_ms > 0 {
                Some(tokio::time::Instant::now() + Duration::from_millis(timeout_ms))
            } else {
                None
            };

            let result = Self::execute_task(
                pool,
                config,
                &entry,
                Some(cancel_signal),
                Some(shutdown_signal.clone()),
                deadline,
            )
            .await;

            get_process_list().deregister(run.run_id);
            result
        } else {
            Self::execute_task(pool, config, &entry, None, Some(shutdown_signal), None).await
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
                // overwrite the same descriptor with a new nonce. Read-compare-delete
                // ensures we only remove the entry we actually processed.
                //
                // Only delete the queue entry on SUCCESS. On failure (e.g., S3 not
                // configured on this node), keep the entry so a capable worker can
                // pick it up on the next poll cycle. This prevents a non-S3 worker
                // from repeatedly claiming and failing merges, blocking progress
                // until the 600s periodic sweep re-enqueues.
                if exec_result.is_ok() {
                    if let Some(current_bytes) = txn.get(queue_key.clone()).await? {
                        // V2 value is a descriptor carrying the nonce inline; a
                        // legacy value is a full entry. Compare from whichever.
                        let current_nonce = if is_v2 {
                            TaskDescriptorV2::decode(&current_bytes)
                                .map(|d| d.nonce)
                                .map_err(|e| anyhow!("Failed to deserialize V2 descriptor: {e}"))?
                        } else {
                            TaskQueueEntry::deserialize_compat(&current_bytes)
                                .map(|e| e.nonce)
                                .map_err(|e| {
                                    anyhow!("Failed to deserialize worker queue entry: {e}")
                                })?
                        };
                        if current_nonce == entry.nonce {
                            Self::delete_due_entry(
                                system_store,
                                &mut txn,
                                is_v2,
                                &queue_key,
                                &entry,
                                queue_fire_time_ms,
                            )
                            .await?;
                        }
                        // nonce mismatch → DML overwrote → skip delete, next tick handles it
                    }
                }
            } else {
                // Non-HnswMerge: these tasks never share deterministic keys with DML,
                // so unconditional delete is safe.
                Self::delete_due_entry(
                    system_store,
                    &mut txn,
                    is_v2,
                    &queue_key,
                    &entry,
                    queue_fire_time_ms,
                )
                .await?;
            }
        }

        if entry.task_type == TaskType::Cron && should_requeue_cron {
            if let Some(next_entry) = Self::load_next_cron_queue_entry(pool, &entry).await? {
                if let Some(schedule) = next_entry.schedule.as_deref() {
                    if let Ok(next_fire) = compute_next_fire_time(schedule) {
                        system_store
                            .put_task_v2(&mut txn, &next_entry, next_fire)
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

            let Some(db_def) = store.get_database_by_id(&mut txn, entry.db_id).await? else {
                tracing::warn!(
                    db_id = entry.db_id,
                    job_id = entry.task_id,
                    "skipping cron job: database no longer exists (possibly dropped)"
                );
                return Ok((None, false));
            };
            let database = db_def.name;

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
        cron_deadline: Option<tokio::time::Instant>,
    ) -> Result<usize> {
        let handle = pool.acquire(Some(entry.keyspace.clone())).await?;
        let store = handle.store().clone();
        let database_name: Arc<str> = {
            let mut db_txn = store.begin().await?;
            let resolved = store.get_database_by_id(&mut db_txn, entry.db_id).await?;
            db_txn.commit().await?;
            match resolved {
                Some(db) => Arc::from(db.name),
                None => {
                    tracing::warn!(
                        db_id = entry.db_id,
                        task_type = ?entry.task_type,
                        "skipping worker task: database no longer exists (possibly dropped)"
                    );
                    return Ok(0);
                }
            }
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
        // Absolute deadline for run_with_guards.  Using timeout_at (not
        // relative Duration) ensures the timeout fires at the correct
        // wall-clock instant regardless of how long the preamble took,
        // and avoids a race where an outer timeout could drop the future
        // before the inner rollback path runs.
        let task_deadline = if is_cron {
            cron_deadline
        } else if config.statement_timeout_ms > 0 {
            Some(
                tokio::time::Instant::now()
                    + std::time::Duration::from_millis(config.statement_timeout_ms),
            )
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

        let max_attempts = if entry.task_type == TaskType::BgSql {
            WORKER_BGSQL_MAX_RETRY_ATTEMPTS
        } else {
            1
        };

        for attempt in 0..max_attempts {
            let mut txn = store.begin().await?;
            let start_ts_version = txn.start_timestamp().version();
            let mut txn_guard = crate::worker::active_txn_registry::global_registry()
                .map(|registry| registry.track_worker_txn(start_ts_version));
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
                        &entry.username,
                        tikv_client.clone(),
                    );
                    let fut = crate::pool::run_with_statement_memory_scope(
                        Some(statement_memory_accountant.clone()),
                        0, // background task: no client connection
                        async {
                            // Attach SQL + start_ts so a background statement that goes
                            // expensive shows up in the expensive_query log. `entry.command`
                            // is the raw SQL; start_ts is the worker txn's begin version.
                            // Per-statement SQL (not the whole `entry.command`) so a
                            // multi-statement background job attributes the log to the
                            // statement that actually went expensive.
                            crate::pool::set_current_statement_sql(&stmt.to_string());
                            crate::pool::set_current_statement_start_ts(start_ts_version);
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
                            )
                            .await
                        },
                    );
                    let _ = fut.await?;
                }
                txn.commit().await?;
                Ok(statements.len())
            };

            let result = run_with_guards(
                task_fut,
                task_deadline,
                cancel_signal.as_ref(),
                shutdown_signal.as_ref(),
            )
            .await;

            match result {
                Ok(completed_commands) => return Ok(completed_commands),
                Err(e) => {
                    if txn.rollback().await.is_err() {
                        // Rollback failed — the txn may still be live in TiKV.
                        // Keep the GC registration so the safepoint does not
                        // advance past this potentially live transaction.
                        if let Some(g) = txn_guard.as_mut() {
                            g.quarantine();
                        }
                    }

                    let should_retry = entry.task_type == TaskType::BgSql
                        && attempt + 1 < max_attempts
                        && is_retryable_tikv_error(&e);
                    if should_retry {
                        tracing::info!(
                            attempt = attempt + 1,
                            max_attempts,
                            task_id = entry.task_id,
                            "bg_sql write conflict or deadlock, retrying task"
                        );
                        worker_bgsql_backoff(attempt).await;
                        continue;
                    }

                    return Err(e);
                }
            }
        }

        unreachable!("worker retry loop must return")
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

    // Retry the entire scan+enqueue with a fresh transaction on region errors
    // (RegionNotFound, EpochNotMatch, etc.) that occur after TiKV region
    // split/merge. Each retry starts a new snapshot so the tikv-client region
    // cache is refreshed. Fixes #2271.
    for attempt in 0..=REGION_ERROR_MAX_RETRIES {
        let result: Result<HnswSweepResult> = async {
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
                            .put_task_v2(&mut sys_txn, &entry, 0)
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
        .await;

        match result {
            Ok(r) => return Ok(r),
            Err(e) if is_retryable_region_error(&e) && attempt < REGION_ERROR_MAX_RETRIES => {
                warn!(
                    keyspace,
                    db_id,
                    attempt = attempt + 1,
                    max_retries = REGION_ERROR_MAX_RETRIES,
                    "HNSW sweep: region error, retrying with fresh transaction: {e}"
                );
                region_error_backoff(attempt).await;
            }
            Err(e) => return Err(e),
        }
    }

    // Unreachable: the loop either returns Ok or Err on the last attempt.
    unreachable!()
}

async fn run_with_guards<F, T>(
    fut: F,
    deadline: Option<tokio::time::Instant>,
    cancel: Option<&Arc<Notify>>,
    shutdown: Option<&CancellationToken>,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let timed_fut = async move {
        match deadline {
            Some(dl) => tokio::time::timeout_at(dl, fut)
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
        .put_task_v2(&mut txn, &entry, fire_time)
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
use helpers::check_graph_oversize_freeze;
#[cfg(test)]
mod tests;
