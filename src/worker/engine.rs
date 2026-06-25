use crate::cron::config::CronConfig;
use crate::cron::process_list::{get_process_list, RunningCronJob};
use crate::cron::types::{CronJob, CronRun, CronRunState, CronRunStatus};
use crate::cron::worker::gc_database;
use crate::extensions::context::{with_context_opts, ExtensionContextOpts};
use crate::observability;
use crate::pool::TikvClientPool;
use crate::sql::ddl;
use crate::sql::executor::core::retry::is_retryable_tikv_error;
use crate::sql::parse_sql;
use crate::sql::query_context::{self, QueryContext};
use crate::sql::Executor;
use crate::storage::{CronClaimOutcome, TikvStore, WqIndexRow};
use crate::worker::config::WorkerConfig;
use crate::worker::metrics::WorkerMetrics;

mod helpers;

use crate::worker::now_epoch_ms;
use crate::worker::types::*;
use anyhow::{anyhow, Result};
pub(crate) use helpers::HNSW_GRAPH_MAX_BYTES;
use helpers::{
    background_statement_extension_context, execute_hnsw_merge, parse_backfill_index_command,
    parse_hnsw_merge_command, should_skip_frozen_merge,
};
pub(crate) use helpers::{
    cleanup_hnsw_s3_graph_upload_after_failed_txn, hnsw_s3_graph_version_for_txn,
    put_hnsw_s3_graph_with_intent,
};
pub(crate) use helpers::{
    is_retryable_region_error, region_error_backoff, REGION_ERROR_MAX_RETRIES,
};
use pgwire::tokio::CancellationToken;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tikv_client::TimestampExt;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

const STATEMENT_TIMEOUT_ERROR: &str = "canceling statement due to statement timeout";
/// Single source of truth for the claim-cancelled / shutdown error message,
/// shared with the specialized long-running paths via `LeaseCancel`.
use crate::worker::CLAIM_CANCELLED_ERROR as CANCELLED_BY_ADMIN_ERROR;
const WORKER_BGSQL_MAX_RETRY_ATTEMPTS: usize = 64;
const WORKER_BGDDL_MAX_RETRY_ATTEMPTS: usize = 64;
const REGISTRY_SWEEP_POLL_INTERVAL_SEC: u64 = 1;
const REGISTRY_SWEEP_CATCHUP_PAGE_INTERVAL_SEC: u64 = 2;
/// Convergent legacy `_worker_queue_` drain (design §II.8 M5). While stragglers
/// are still being migrated, drain a bounded batch every this many seconds.
const LEGACY_DRAIN_ACTIVE_INTERVAL_SEC: u64 = 5;
/// Once the legacy queue has been observed EMPTY for `LEGACY_DRAIN_GRACE_EMPTY_SWEEPS`
/// consecutive probes, fall back to this slower cadence. The drain NEVER stops —
/// an OLD binary may still write V1 rows during a rolling deploy — but the
/// steady-state cost is only a single 1-key empty-range probe at this interval.
const LEGACY_DRAIN_IDLE_INTERVAL_SEC: u64 = 300;
/// Grace window: number of consecutive empty probes before downshifting to the
/// idle cadence. A straggler V1 row resets the streak and re-arms active drain.
const LEGACY_DRAIN_GRACE_EMPTY_SWEEPS: u32 = 3;
/// Per-tick batch budget so one drain tick never holds the maintenance loop on
/// an unbounded backlog: at most this many bounded batches are migrated per tick;
/// the remainder is picked up on the next tick (still convergent).
const LEGACY_DRAIN_MAX_BATCHES_PER_TICK: u32 = 8;
const REGISTRY_SWEEP_RECOVERY_BACKOFF_SEC: u64 = 30;
const SWEEP_BACKOFF_BASE_INTERVALS: u32 = 1;
const SWEEP_BACKOFF_MAX_SHIFT: u32 = 5;
const DISABLED_CHECK_THRESHOLD: u32 = 5;
const CIC_REPAIR_TABLE_PAGE_SIZE: usize = 256;
const HNSW_DIRTY_MARKER_PAGE_SIZE: usize = 256;

/// Pacing for the convergent legacy `_worker_queue_` drain (design §II.8 M5).
/// Active while stragglers are still being seen; downshifts to a cheap periodic
/// empty-range probe once the queue has been empty for the grace window. The
/// drain NEVER stops — a downshift only widens the interval — so an old binary
/// that resumes writing V1 rows during a rolling deploy is always caught.
fn legacy_drain_interval(empty_streak: u32) -> Duration {
    if empty_streak >= LEGACY_DRAIN_GRACE_EMPTY_SWEEPS {
        Duration::from_secs(LEGACY_DRAIN_IDLE_INTERVAL_SEC)
    } else {
        Duration::from_secs(LEGACY_DRAIN_ACTIVE_INTERVAL_SEC)
    }
}

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
    backlog_wakeup: Arc<AtomicBool>,
    metrics: Arc<WorkerMetrics>,
    notify: Arc<Notify>,
    shutdown: CancellationToken,
    registry_sweep_state: Mutex<RegistrySweepState>,
}

struct RegistrySweepState {
    cursor: Option<Vec<u8>>,
    catch_up: bool,
    last_page_at: Option<Instant>,
    last_cycle_completed: Option<Instant>,
    hnsw_observed_this_cycle: u64,
    entry_backoff: HashMap<(String, u64), RegistrySweepBackoff>,
    kind_backoff: HashMap<RegistrySweepKindKey, RegistrySweepBackoff>,
    missing_database_seen: HashSet<(String, u64)>,
    cic_table_cursors: HashMap<(String, u64), Vec<u8>>,
    hnsw_dirty_cursors: HashMap<(String, u64), Vec<u8>>,
    /// Convergent legacy `_worker_queue_` drain state (design §II.8 M5).
    /// `legacy_drain_last_at` paces the drain; `legacy_drain_empty_streak`
    /// counts consecutive empty probes for the grace-window downshift.
    legacy_drain_last_at: Option<Instant>,
    legacy_drain_empty_streak: u32,
}

impl Default for RegistrySweepState {
    fn default() -> Self {
        Self {
            cursor: None,
            catch_up: true,
            last_page_at: None,
            last_cycle_completed: None,
            hnsw_observed_this_cycle: 0,
            entry_backoff: HashMap::new(),
            kind_backoff: HashMap::new(),
            missing_database_seen: HashSet::new(),
            cic_table_cursors: HashMap::new(),
            hnsw_dirty_cursors: HashMap::new(),
            legacy_drain_last_at: None,
            legacy_drain_empty_streak: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RegistrySweepKind {
    Cron,
    Cic,
    DdlJournal,
    HnswDelta,
    HnswS3,
    StorageScan,
}

impl RegistrySweepKind {
    fn label(self) -> &'static str {
        match self {
            Self::Cron => "cron",
            Self::Cic => "cic",
            Self::DdlJournal => "ddl_journal",
            Self::HnswDelta => "hnsw_delta",
            Self::HnswS3 => "hnsw_s3",
            Self::StorageScan => "storage_scan",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RegistrySweepKindKey {
    keyspace: String,
    db_id: u64,
    kind: RegistrySweepKind,
}

impl RegistrySweepKindKey {
    fn new(keyspace: &str, db_id: u64, kind: RegistrySweepKind) -> Self {
        Self {
            keyspace: keyspace.to_string(),
            db_id,
            kind,
        }
    }
}

struct RegistrySweepBackoff {
    consecutive_failures: u32,
    retry_after: Instant,
}

impl RegistrySweepBackoff {
    fn new_failed(interval_sec: u64) -> Self {
        Self {
            consecutive_failures: 1,
            retry_after: Instant::now()
                + Duration::from_secs(interval_sec * SWEEP_BACKOFF_BASE_INTERVALS as u64),
        }
    }

    fn record_failure(&mut self, interval_sec: u64) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let shift = self
            .consecutive_failures
            .saturating_sub(1)
            .min(SWEEP_BACKOFF_MAX_SHIFT);
        let multiplier = SWEEP_BACKOFF_BASE_INTERVALS as u64 * (1u64 << shift);
        self.retry_after = Instant::now() + Duration::from_secs(interval_sec * multiplier);
    }

    fn should_skip(&self) -> bool {
        Instant::now() < self.retry_after
    }
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

/// Lifetime guard for a claim's lease-renewal loop. Dropping it aborts the loop
/// (execution has finished, so the lease no longer needs renewing).
struct LeaseRenewerGuard {
    handle: tokio::task::JoinHandle<()>,
}

/// Result of a single lease-renewal attempt. The loop maps this onto its
/// `last_committed_lease_until` state and the cancel/continue decision.
///
/// Extracted as an explicit value (rather than inlined in the spawned closure)
/// so the loop wiring — seed at spawn, advance ONLY on a committed renewal, and
/// compare a renewal error against the COMMITTED deadline — is driven through
/// one real code path that tests can exercise against TiKV. A pure-inequality
/// unit test cannot guard that wiring (post-mortem class F: the original bug was
/// the wiring, not the math).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaseRenewOutcome {
    /// Renewal committed; the stored lease now extends to this deadline. The
    /// loop advances `last_committed_lease_until` to it and continues.
    Renewed { committed_lease_until_ms: i64 },
    /// The claim is gone or owned by another worker (renew returned `false`).
    /// The lease is lost: cancel and stop renewing.
    ClaimLost,
    /// Transient renewal error. `cancel` was decided by comparing `now` against
    /// the COMMITTED deadline (never the prospective one): cancel if the stored
    /// lease has already lapsed, otherwise retry next tick.
    Errored { cancel: bool },
}

impl Drop for LeaseRenewerGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn cron_queue_entry_matches_job(entry: &TaskQueueEntry, job: &CronJob) -> bool {
    entry.task_type == TaskType::Cron
        && entry.task_id == job.job_id
        && entry.command == job.command
        && entry.username == job.username
        && entry.schedule.as_deref() == Some(job.schedule.as_str())
}

/// Map a cron claim outcome to the queue-row disposition: `None` = we own a run,
/// proceed; `Some(false)` = drop the due row (this exact fire already completed —
/// requeue is the *next* fire's job); `Some(true)` = keep the row and retry next
/// tick (a live run holds the job, no-overlap).
fn keep_queue_entry_for_claim_status(outcome: &CronClaimOutcome) -> Option<bool> {
    match outcome {
        CronClaimOutcome::Claimed { .. } | CronClaimOutcome::TookOver { .. } => None,
        CronClaimOutcome::AlreadyTerminalForMinute => Some(false),
        // Both block outcomes keep the queue row and retry next tick. The `Folded`
        // variant additionally carries durable fold writes the caller must COMMIT
        // (see `must_commit_blocked`); the queue disposition is identical.
        CronClaimOutcome::BlockedByLiveActive | CronClaimOutcome::BlockedByLiveActiveFolded => {
            Some(true)
        }
    }
}

/// Whether the cleanup must enqueue the job's NEXT fire, given the claim outcome.
///
/// The guaranteed-schedule-progress contract (design 35 §Contract) requires the
/// next fire be enqueued whenever the claimed minute is DONE — not only when THIS
/// worker ran it. A minute is DONE in two cases:
///   - we claimed/took over and executed it (`Claimed`/`TookOver`), or
///   - it is ALREADY TERMINAL (`AlreadyTerminalForMinute`): some other path
///     completed it — including the crash-recovery case where this worker claimed
///     M, crashed before requeue, and the reaper terminalized CONTROL(M) while the
///     due row for M still existed. A later worker reclaiming that due row sees
///     terminal CONTROL and returns `AlreadyTerminalForMinute`; if cleanup then
///     dropped the row WITHOUT requeue, the schedule would silently stall until a
///     much-later registry sweep (the residual silent-dropped-fire of DEFECT 2).
///
/// A still-running block (`BlockedByLiveActive`/`Folded`) is NOT done: the live
/// run owns the fire and will itself requeue the next fire when it finalizes —
/// the due row is KEPT for a later retry and no new fire is enqueued here (a
/// double-enqueue would be folded by the singleton next-fire put, but keeping the
/// row is the correct disposition).
///
/// The next-fire enqueue this gates is a fenced, idempotent SINGLETON put
/// (`enqueue_task_v2_unless_db_dropped` over the deterministic next-due key), so
/// multiple workers reclaiming the same terminal minute enqueue M+1 at most once,
/// and a dropped DB suppresses it — the same guarantees the ran-it path relies on.
fn cron_outcome_requeues_next_fire(outcome: &CronClaimOutcome) -> bool {
    match outcome {
        // Ran it, or it was already done — the minute is terminal: requeue M+1.
        CronClaimOutcome::Claimed { .. }
        | CronClaimOutcome::TookOver { .. }
        | CronClaimOutcome::AlreadyTerminalForMinute => true,
        // A live run still holds the job — keep the row, do NOT enqueue a new fire.
        CronClaimOutcome::BlockedByLiveActive | CronClaimOutcome::BlockedByLiveActiveFolded => {
            false
        }
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
            backlog_wakeup: Arc::new(AtomicBool::new(false)),
            metrics: Arc::new(WorkerMetrics::new()),
            notify,
            shutdown,
            registry_sweep_state: Mutex::new(RegistrySweepState::default()),
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

        let mut interval = tokio::time::interval(Duration::from_millis(self.config.poll_ms));

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    info!("WorkerEngine shutdown requested");
                    break;
                }
                _ = interval.tick() => {
                    if let Err(e) = self.tick().await {
                        warn!("Worker tick error: {}", e);
                    }
                }
                _ = self.notify.notified() => {
                    if let Err(e) = self.tick().await {
                        warn!("Worker tick error: {}", e);
                    }
                }
            }
        }
    }

    pub async fn run_maintenance(&self) {
        info!(
            "WorkerEngine maintenance loop starting (poll_sec={})",
            REGISTRY_SWEEP_POLL_INTERVAL_SEC
        );

        let mut sweep_interval =
            tokio::time::interval(Duration::from_secs(REGISTRY_SWEEP_POLL_INTERVAL_SEC));

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    info!("WorkerEngine maintenance shutdown requested");
                    break;
                }
                _ = sweep_interval.tick() => {
                    if let Err(e) = self.registry_sweep_tick().await {
                        warn!("Worker registry sweep error: {}", e);
                    }
                    if let Err(e) = self.legacy_queue_drain_tick().await {
                        warn!("Worker legacy queue drain error: {}", e);
                    }
                }
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let now_ms = now_epoch_ms();

        let mut txn = self.system_store.begin().await?;
        // V2 descriptors (small values) up to `now`.
        let due_entries: Vec<(Vec<u8>, DueItem)> = self
            .system_store
            .scan_due_v2(&mut txn, now_ms, 1000)
            .await?
            .into_iter()
            .map(|(key, descriptor)| (key, DueItem::V2(descriptor)))
            .collect();
        txn.commit().await?;

        self.metrics.sample_tick(
            due_entries.len() as u64,
            self.active_jobs.load(Ordering::Relaxed),
        );
        let mut async_trigger_keyspaces: HashSet<String> = due_entries
            .iter()
            .filter(|(_, entry)| entry.task_type() == TaskType::AsyncTrigger)
            .map(|(_, entry)| entry.keyspace().to_string())
            .collect();
        async_trigger_keyspaces
            .extend(crate::worker::async_trigger_queue_depth_dirty_keyspaces_due());
        for keyspace in async_trigger_keyspaces {
            if let Err(err) =
                crate::worker::sample_async_trigger_queue_depth(&self.system_store, &keyspace).await
            {
                warn!(
                    "Failed to sample async trigger queue depth for {}: {}",
                    keyspace, err
                );
            }
        }

        if due_entries.is_empty() {
            return Ok(());
        }

        let due_count = due_entries.len();
        let mut dispatched = 0usize;
        for (key, entry) in due_entries {
            let permit = match self.semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    self.backlog_wakeup.store(true, Ordering::Relaxed);
                    break;
                }
            };
            dispatched += 1;

            let engine_system_store = self.system_store.clone();
            let engine_pool = self.pool.clone();
            let engine_config = self.config.clone();
            let active_jobs = self.active_jobs.clone();
            let backlog_wakeup = self.backlog_wakeup.clone();
            let engine_metrics = self.metrics.clone();
            let engine_shutdown = self.shutdown.clone();

            tokio::spawn(async move {
                {
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
                }
                if backlog_wakeup.swap(false, Ordering::Relaxed) {
                    crate::worker::wake_worker();
                }
            });
        }
        if dispatched < due_count {
            self.backlog_wakeup.store(true, Ordering::Relaxed);
        }

        Ok(())
    }

    async fn registry_sweep_tick(&self) -> Result<()> {
        let now = Instant::now();
        let (start_after, batch_size) = {
            let state = self.registry_sweep_state.lock().await;
            let page_interval = if state.catch_up {
                Duration::from_secs(REGISTRY_SWEEP_CATCHUP_PAGE_INTERVAL_SEC)
            } else {
                Duration::from_secs(self.config.sweep_page_interval_sec.max(1))
            };
            if state
                .last_page_at
                .is_some_and(|last| now.duration_since(last) < page_interval)
            {
                return Ok(());
            }
            if state.cursor.is_none() && !state.catch_up {
                let cycle_interval =
                    Duration::from_secs(self.config.registry_sweep_interval_sec.max(1));
                if state
                    .last_cycle_completed
                    .is_some_and(|last| now.duration_since(last) < cycle_interval)
                {
                    return Ok(());
                }
            }
            (
                state.cursor.clone(),
                self.config.registry_reconcile_batch_size.max(1),
            )
        };

        let (entries, next_cursor) = {
            let mut txn = self.system_store.begin().await?;
            let page = self
                .system_store
                .scan_worker_registry_page(&mut txn, start_after.as_deref(), batch_size)
                .await?;
            txn.rollback().await.ok();
            page
        };

        if entries.is_empty() {
            self.finish_registry_sweep_cycle(now, 0).await;
            return Ok(());
        }

        let mut cron_config = CronConfig::from_env();
        cron_config.orphan_timeout_sec =
            crate::worker::gc::effective_cron_orphan_timeout_sec(&cron_config, &self.config);

        let mut page_hnsw_observed = 0u64;
        let mut page_hnsw_enqueued = 0u64;
        let mut page_hnsw_enqueue_errors = 0u64;
        let mut processed = 0usize;
        let mut skipped = 0usize;
        let mut touched_keyspaces = HashSet::new();

        for entry in &entries {
            let backoff_key = (entry.keyspace.clone(), entry.db_id);
            if self.registry_sweep_should_skip(entry, &backoff_key).await? {
                skipped += 1;
                continue;
            }

            let handle = match self.pool.acquire(Some(entry.keyspace.clone())).await {
                Ok(handle) => {
                    touched_keyspaces.insert(entry.keyspace.clone());
                    handle
                }
                Err(e) => {
                    self.registry_sweep_record_failure(&backoff_key).await;
                    warn!(
                        keyspace = %entry.keyspace,
                        db_id = entry.db_id,
                        "Worker registry sweep tenant acquire failed: {}",
                        e
                    );
                    continue;
                }
            };
            let store = handle.store().clone();

            match self
                .process_registry_sweep_entry(entry, &store, &cron_config)
                .await
            {
                Ok(outcome) => {
                    processed += 1;
                    page_hnsw_observed += outcome.hnsw_observed as u64;
                    page_hnsw_enqueued += outcome.hnsw_enqueued as u64;
                    page_hnsw_enqueue_errors += outcome.hnsw_enqueue_errors as u64;
                    self.registry_sweep_record_success(&backoff_key).await;
                }
                Err(e) => {
                    self.registry_sweep_record_failure(&backoff_key).await;
                    warn!(
                        keyspace = %entry.keyspace,
                        db_id = entry.db_id,
                        "Worker registry sweep entry failed: {}",
                        e
                    );
                }
            }
            drop(handle);
        }

        for keyspace in touched_keyspaces {
            if self.pool.evict_if_idle(&keyspace).await {
                tracing::debug!(keyspace, "Worker registry sweep evicted idle tenant client");
            }
        }

        if page_hnsw_enqueued > 0 {
            self.metrics
                .hnsw_sweep_enqueued
                .fetch_add(page_hnsw_enqueued, Ordering::Relaxed);
            metrics::counter!("db9_server_hnsw_sweep_enqueued_total").increment(page_hnsw_enqueued);
        }
        if page_hnsw_enqueue_errors > 0 {
            self.metrics
                .hnsw_sweep_enqueue_errors
                .fetch_add(page_hnsw_enqueue_errors, Ordering::Relaxed);
            metrics::counter!("db9_server_hnsw_sweep_enqueue_errors_total")
                .increment(page_hnsw_enqueue_errors);
        }

        let cycle_completed = next_cursor.is_none();
        {
            let mut state = self.registry_sweep_state.lock().await;
            state.cursor = next_cursor;
            state.last_page_at = Some(now);
            state.hnsw_observed_this_cycle = state
                .hnsw_observed_this_cycle
                .saturating_add(page_hnsw_observed);
            if cycle_completed {
                let observed = state.hnsw_observed_this_cycle;
                state.hnsw_observed_this_cycle = 0;
                state.catch_up = false;
                state.last_cycle_completed = Some(now);
                self.metrics
                    .hnsw_pending_indexes_observed
                    .store(observed, Ordering::Relaxed);
                metrics::gauge!("db9_server_hnsw_pending_indexes_observed").set(observed as f64);
                metrics::counter!("db9_server_worker_sweep_cycles_completed_total").increment(1);
            }
        }

        metrics::counter!("db9_server_worker_sweep_entries_total").increment(processed as u64);
        if skipped > 0 {
            metrics::counter!("db9_server_worker_sweep_entries_skipped_total")
                .increment(skipped as u64);
        }
        info!(
            processed,
            skipped,
            page_entries = entries.len(),
            cycle_completed,
            hnsw_observed = page_hnsw_observed,
            hnsw_enqueued = page_hnsw_enqueued,
            hnsw_enqueue_errors = page_hnsw_enqueue_errors,
            "Worker registry sweep page complete"
        );

        Ok(())
    }

    async fn finish_registry_sweep_cycle(&self, now: Instant, observed_delta: u64) {
        let mut state = self.registry_sweep_state.lock().await;
        state.cursor = None;
        state.last_page_at = Some(now);
        state.catch_up = false;
        state.last_cycle_completed = Some(now);
        state.hnsw_observed_this_cycle = state
            .hnsw_observed_this_cycle
            .saturating_add(observed_delta);
        let observed = state.hnsw_observed_this_cycle;
        state.hnsw_observed_this_cycle = 0;
        self.metrics
            .hnsw_pending_indexes_observed
            .store(observed, Ordering::Relaxed);
        metrics::gauge!("db9_server_hnsw_pending_indexes_observed").set(observed as f64);
        metrics::counter!("db9_server_worker_sweep_cycles_completed_total").increment(1);
    }

    /// Convergent background drain of legacy `_worker_queue_` rows (design
    /// §II.8 M5).
    ///
    /// The startup migration only converts V1 rows that exist when this node
    /// latches `_wq_schema_version = 2`. During a rolling deploy an OLD
    /// (pre-V2) binary keeps writing V1 rows AFTER that point; the V2-only tick
    /// would never dequeue or reap them, stranding cron fires / bg DDL / bg SQL
    /// / auto-analyze forever. This tick keeps draining stragglers in BOUNDED
    /// batches until the legacy queue is observed empty across a grace window,
    /// after which it costs only a cheap empty-range probe — and it NEVER stops,
    /// because an old binary may write a fresh V1 row at any time in the window.
    ///
    /// Bounded by construction (issue #2576 invariant): each active sweep does
    /// at most `LEGACY_DRAIN_MAX_BATCHES_PER_TICK` page-sized batch migrations;
    /// the converged path is a single 1-key probe. This is NOT a per-operation
    /// or per-tick global scan — only this maintenance-loop step touches the
    /// legacy layer, and the V2-only enqueue/dequeue hot paths never do.
    async fn legacy_queue_drain_tick(&self) -> Result<()> {
        let now = Instant::now();
        {
            let state = self.registry_sweep_state.lock().await;
            let interval = legacy_drain_interval(state.legacy_drain_empty_streak);
            if state
                .legacy_drain_last_at
                .is_some_and(|last| now.duration_since(last) < interval)
            {
                return Ok(());
            }
        }

        // Drain bounded batches until the legacy queue is empty or the per-tick
        // budget is spent. Each batch migrates V1 -> V2 (due/index/payload) and
        // deletes the V1 keys in the SAME transaction.
        let mut migrated_total = 0usize;
        let mut batches = 0u32;
        let mut drained_to_empty = false;
        while batches < LEGACY_DRAIN_MAX_BATCHES_PER_TICK {
            let migrated = self.system_store.drain_legacy_worker_queue_batch().await?;
            batches += 1;
            migrated_total += migrated;
            if migrated == 0 {
                drained_to_empty = true;
                break;
            }
        }

        // If the budget was spent without emptying, confirm whether more remains
        // so the grace streak is not advanced prematurely.
        if !drained_to_empty {
            drained_to_empty = self.system_store.legacy_worker_queue_is_empty().await?;
        }

        {
            let mut state = self.registry_sweep_state.lock().await;
            state.legacy_drain_last_at = Some(now);
            if drained_to_empty {
                state.legacy_drain_empty_streak = state.legacy_drain_empty_streak.saturating_add(1);
            } else {
                // A straggler appeared: re-arm aggressive draining.
                state.legacy_drain_empty_streak = 0;
            }
        }

        if migrated_total > 0 {
            self.metrics
                .legacy_queue_drained
                .fetch_add(migrated_total as u64, Ordering::Relaxed);
            metrics::counter!("db9_server_worker_legacy_queue_drained_total")
                .increment(migrated_total as u64);
            info!(
                migrated = migrated_total,
                batches, "Worker drained legacy V1 worker-queue stragglers into V2"
            );
            // New V2 due rows are now visible to the tick; nudge it.
            crate::worker::wake_worker();
        }

        Ok(())
    }

    async fn registry_sweep_should_skip(
        &self,
        entry: &TaskRegistryEntry,
        key: &(String, u64),
    ) -> Result<bool> {
        let should_check_disabled = {
            let state = self.registry_sweep_state.lock().await;
            let Some(backoff) = state.entry_backoff.get(key) else {
                return Ok(false);
            };
            if !backoff.should_skip() {
                return Ok(false);
            }
            backoff.consecutive_failures >= DISABLED_CHECK_THRESHOLD
        };

        if should_check_disabled {
            let state =
                crate::worker::check_keyspace_state(self.pool.pd_endpoints(), &entry.keyspace)
                    .await;
            if state.as_deref() == Some("DISABLED") {
                warn!(
                    keyspace = %entry.keyspace,
                    db_id = entry.db_id,
                    "Worker registry sweep retained DISABLED keyspace entry"
                );
                return Ok(true);
            }
        }

        Ok(true)
    }

    async fn registry_sweep_record_success(&self, key: &(String, u64)) {
        let mut state = self.registry_sweep_state.lock().await;
        state.entry_backoff.remove(key);
    }

    async fn registry_sweep_record_failure(&self, key: &(String, u64)) {
        let mut state = self.registry_sweep_state.lock().await;
        let interval = self.config.registry_sweep_interval_sec.max(1);
        match state.entry_backoff.entry(key.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().record_failure(interval);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(RegistrySweepBackoff::new_failed(interval));
            }
        }
    }

    async fn registry_sweep_kind_should_skip(
        &self,
        keyspace: &str,
        db_id: u64,
        kind: RegistrySweepKind,
    ) -> bool {
        let key = RegistrySweepKindKey::new(keyspace, db_id, kind);
        let state = self.registry_sweep_state.lock().await;
        state
            .kind_backoff
            .get(&key)
            .is_some_and(RegistrySweepBackoff::should_skip)
    }

    async fn registry_sweep_record_kind_success(
        &self,
        keyspace: &str,
        db_id: u64,
        kind: RegistrySweepKind,
    ) {
        let key = RegistrySweepKindKey::new(keyspace, db_id, kind);
        let mut state = self.registry_sweep_state.lock().await;
        state.kind_backoff.remove(&key);
    }

    async fn registry_sweep_record_kind_failure(
        &self,
        keyspace: &str,
        db_id: u64,
        kind: RegistrySweepKind,
    ) {
        let key = RegistrySweepKindKey::new(keyspace, db_id, kind);
        let interval = self.registry_sweep_kind_backoff_interval(kind);
        let mut state = self.registry_sweep_state.lock().await;
        match state.kind_backoff.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().record_failure(interval);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(RegistrySweepBackoff::new_failed(interval));
            }
        }
    }

    fn registry_sweep_kind_backoff_interval(&self, kind: RegistrySweepKind) -> u64 {
        match kind {
            RegistrySweepKind::Cron | RegistrySweepKind::Cic | RegistrySweepKind::DdlJournal => {
                REGISTRY_SWEEP_RECOVERY_BACKOFF_SEC
            }
            RegistrySweepKind::HnswDelta | RegistrySweepKind::HnswS3 => {
                self.config.hnsw_sweep_interval_sec.max(1)
            }
            RegistrySweepKind::StorageScan => self.config.storage_scan_interval_sec.max(1),
        }
    }

    async fn registry_sweep_db_missing_should_delete(&self, key: &(String, u64)) -> bool {
        let mut state = self.registry_sweep_state.lock().await;
        !state.missing_database_seen.insert(key.clone())
    }

    async fn registry_sweep_record_db_exists(&self, key: &(String, u64)) {
        let mut state = self.registry_sweep_state.lock().await;
        state.missing_database_seen.remove(key);
    }

    async fn registry_sweep_record_registry_deleted(&self, key: &(String, u64)) {
        let mut state = self.registry_sweep_state.lock().await;
        state.entry_backoff.remove(key);
        state.missing_database_seen.remove(key);
        state.cic_table_cursors.remove(key);
        state.hnsw_dirty_cursors.remove(key);
        state.kind_backoff.retain(|kind_key, _| {
            kind_key.keyspace.as_str() != key.0.as_str() || kind_key.db_id != key.1
        });
    }

    async fn cic_table_cursor(&self, keyspace: &str, db_id: u64) -> Option<Vec<u8>> {
        let state = self.registry_sweep_state.lock().await;
        state
            .cic_table_cursors
            .get(&(keyspace.to_string(), db_id))
            .cloned()
    }

    async fn record_cic_table_cursor(
        &self,
        keyspace: &str,
        db_id: u64,
        next_cursor: Option<Vec<u8>>,
    ) {
        let mut state = self.registry_sweep_state.lock().await;
        let key = (keyspace.to_string(), db_id);
        if let Some(cursor) = next_cursor {
            state.cic_table_cursors.insert(key, cursor);
        } else {
            state.cic_table_cursors.remove(&key);
        }
    }

    async fn hnsw_dirty_cursor(&self, keyspace: &str, db_id: u64) -> Option<Vec<u8>> {
        let state = self.registry_sweep_state.lock().await;
        state
            .hnsw_dirty_cursors
            .get(&(keyspace.to_string(), db_id))
            .cloned()
    }

    async fn record_hnsw_dirty_cursor(
        &self,
        keyspace: &str,
        db_id: u64,
        next_cursor: Option<Vec<u8>>,
    ) {
        let mut state = self.registry_sweep_state.lock().await;
        let key = (keyspace.to_string(), db_id);
        if let Some(cursor) = next_cursor {
            state.hnsw_dirty_cursors.insert(key, cursor);
        } else {
            state.hnsw_dirty_cursors.remove(&key);
        }
    }

    async fn process_registry_sweep_entry(
        &self,
        entry: &TaskRegistryEntry,
        store: &Arc<TikvStore>,
        cron_config: &CronConfig,
    ) -> Result<RegistrySweepEntryOutcome> {
        let registry_key = (entry.keyspace.clone(), entry.db_id);
        let db_exists = {
            let mut txn = store.begin().await?;
            let exists = store
                .get_database_by_id(&mut txn, entry.db_id)
                .await?
                .is_some();
            txn.rollback().await.ok();
            exists
        };
        if !db_exists {
            if !self
                .registry_sweep_db_missing_should_delete(&registry_key)
                .await
            {
                warn!(
                    keyspace = %entry.keyspace,
                    db_id = entry.db_id,
                    "Worker registry sweep saw database missing; retaining registry row until next cycle"
                );
                return Ok(RegistrySweepEntryOutcome::default());
            }
            let reaped = self
                .system_store
                .reap_db_queue_entries_then_delete_worker_registry(&entry.keyspace, entry.db_id)
                .await?;
            if reaped > 0 {
                info!(
                    keyspace = %entry.keyspace,
                    db_id = entry.db_id,
                    reaped,
                    "Worker registry sweep reaped queue entries for missing database"
                );
            }
            self.registry_sweep_record_registry_deleted(&registry_key)
                .await;
            return Ok(RegistrySweepEntryOutcome::default());
        }
        self.registry_sweep_record_db_exists(&registry_key).await;

        let mut outcome = RegistrySweepEntryOutcome::default();

        // Registry task bits are inventory hints, not recovery truth. Producers
        // can race on the bitmask row, so recovery probes run for every live
        // registry entry and rely on each subsystem's durable tenant state.
        let kind = RegistrySweepKind::Cron;
        if !self
            .registry_sweep_kind_should_skip(&entry.keyspace, entry.db_id, kind)
            .await
        {
            let result = async {
                self.reconcile_cron_for_db(store, &entry.keyspace, entry.db_id)
                    .await?;
                gc_database(store, entry.db_id, cron_config).await?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            self.record_registry_sweep_kind_result(entry, kind, result)
                .await;
        }

        let kind = RegistrySweepKind::Cic;
        if !self
            .registry_sweep_kind_should_skip(&entry.keyspace, entry.db_id, kind)
            .await
        {
            let table_cursor = self.cic_table_cursor(&entry.keyspace, entry.db_id).await;
            match self
                .reconcile_incomplete_cic_indexes_for_db_safe(
                    store,
                    &entry.keyspace,
                    entry.db_id,
                    table_cursor.as_deref(),
                    CIC_REPAIR_TABLE_PAGE_SIZE,
                )
                .await
            {
                Ok(next_cursor) => {
                    self.record_cic_table_cursor(&entry.keyspace, entry.db_id, next_cursor)
                        .await;
                    self.registry_sweep_record_kind_success(&entry.keyspace, entry.db_id, kind)
                        .await;
                }
                Err(e) => {
                    self.registry_sweep_record_kind_failure(&entry.keyspace, entry.db_id, kind)
                        .await;
                    warn!(
                        keyspace = %entry.keyspace,
                        db_id = entry.db_id,
                        kind = kind.label(),
                        "Worker registry sweep task failed: {}",
                        e
                    );
                }
            }
        }

        let kind = RegistrySweepKind::DdlJournal;
        if !self
            .registry_sweep_kind_should_skip(&entry.keyspace, entry.db_id, kind)
            .await
        {
            let result = self
                .reconcile_ddl_journal_for_db(store, &entry.keyspace, entry.db_id)
                .await;
            self.record_registry_sweep_kind_result(entry, kind, result)
                .await;
        }

        let kind = RegistrySweepKind::HnswDelta;
        if !self
            .registry_sweep_kind_should_skip(&entry.keyspace, entry.db_id, kind)
            .await
        {
            let dirty_cursor = self.hnsw_dirty_cursor(&entry.keyspace, entry.db_id).await;
            match enqueue_pending_hnsw_merges(
                &self.system_store,
                store,
                &entry.keyspace,
                entry.db_id,
                dirty_cursor.as_deref(),
                HNSW_DIRTY_MARKER_PAGE_SIZE,
                self.config.hnsw_sweep_interval_sec,
            )
            .await
            {
                Ok(hnsw) => {
                    outcome.hnsw_observed = hnsw.observed;
                    outcome.hnsw_enqueued = hnsw.enqueued;
                    outcome.hnsw_enqueue_errors = hnsw.enqueue_errors;
                    self.record_hnsw_dirty_cursor(
                        &entry.keyspace,
                        entry.db_id,
                        hnsw.next_dirty_cursor,
                    )
                    .await;
                    self.registry_sweep_record_kind_success(&entry.keyspace, entry.db_id, kind)
                        .await;
                }
                Err(e) => {
                    self.registry_sweep_record_kind_failure(&entry.keyspace, entry.db_id, kind)
                        .await;
                    warn!(
                        keyspace = %entry.keyspace,
                        db_id = entry.db_id,
                        kind = kind.label(),
                        "Worker registry sweep task failed: {}",
                        e
                    );
                }
            }
        }

        if crate::sql::hnsw::s3::hnsw_s3_client().is_some() {
            let kind = RegistrySweepKind::HnswS3;
            if !self
                .registry_sweep_kind_should_skip(&entry.keyspace, entry.db_id, kind)
                .await
            {
                let gc = crate::worker::gc::WorkerGc::new(
                    self.system_store.clone(),
                    self.pool.clone(),
                    self.config.clone(),
                );
                let result = gc.sweep_hnsw_s3_orphans_for_entry(entry, store).await;
                self.record_registry_sweep_kind_result(entry, kind, result)
                    .await;
            }
        }

        let kind = RegistrySweepKind::StorageScan;
        if !self
            .registry_sweep_kind_should_skip(&entry.keyspace, entry.db_id, kind)
            .await
        {
            let result = async {
                if self.storage_scan_due(store, entry.db_id).await? {
                    enqueue_storage_scan_with_jitter(
                        &self.system_store,
                        &entry.keyspace,
                        entry.db_id,
                        self.config.storage_scan_jitter_sec,
                    )
                    .await?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            self.record_registry_sweep_kind_result(entry, kind, result)
                .await;
        }

        Ok(outcome)
    }

    async fn record_registry_sweep_kind_result(
        &self,
        entry: &TaskRegistryEntry,
        kind: RegistrySweepKind,
        result: Result<()>,
    ) {
        match result {
            Ok(()) => {
                self.registry_sweep_record_kind_success(&entry.keyspace, entry.db_id, kind)
                    .await;
            }
            Err(e) => {
                self.registry_sweep_record_kind_failure(&entry.keyspace, entry.db_id, kind)
                    .await;
                warn!(
                    keyspace = %entry.keyspace,
                    db_id = entry.db_id,
                    kind = kind.label(),
                    "Worker registry sweep task failed: {}",
                    e
                );
            }
        }
    }

    async fn reconcile_incomplete_cic_indexes_for_db_safe(
        &self,
        store: &Arc<TikvStore>,
        keyspace: &str,
        db_id: u64,
        table_start_after: Option<&[u8]>,
        table_page_size: usize,
    ) -> Result<Option<Vec<u8>>> {
        let mut txn = store.begin().await?;
        let result: Result<(u32, Option<Vec<u8>>)> = async {
            let mut repaired = 0u32;
            let (table_names, next_cursor) = store
                .scan_tables_page(&mut txn, db_id, table_start_after, table_page_size)
                .await?;
            for table_name in table_names {
                let Some(mut schema) = store.get_schema(&mut txn, db_id, &table_name).await? else {
                    continue;
                };
                let mut schema_repaired = 0u32;
                for idx in &mut schema.indexes {
                    if !matches!(idx.state, IndexState::Building | IndexState::WriteOnly) {
                        continue;
                    }
                    let Ok(task_id) = i64::try_from(idx.id) else {
                        warn!(
                            table = %table_name,
                            index = %idx.name,
                            index_id = idx.id,
                            "Skipping CIC repair pending check because index_id does not fit i64"
                        );
                        continue;
                    };
                    let pending = {
                        let mut sys_txn = self.system_store.begin().await?;
                        let pending = self
                            .system_store
                            .task_has_pending(
                                &mut sys_txn,
                                keyspace,
                                db_id,
                                task_id,
                                TaskType::BgDdl,
                            )
                            .await?;
                        sys_txn.rollback().await.ok();
                        pending
                    };
                    if pending {
                        continue;
                    }
                    idx.state = IndexState::Invalid;
                    schema_repaired += 1;
                }
                if schema_repaired > 0 {
                    repaired += schema_repaired;
                    store.update_schema(&mut txn, db_id, schema).await?;
                }
            }
            Ok((repaired, next_cursor))
        }
        .await;

        match result {
            Ok((repaired, next_cursor)) => {
                if repaired > 0 {
                    store
                        .assert_database_alive_for_update(&mut txn, db_id)
                        .await?;
                    txn.commit().await?;
                } else {
                    txn.rollback().await.ok();
                }
                if repaired > 0 {
                    warn!(
                        "Recovered {} incomplete CIC indexes as Invalid in keyspace={} db_id={}",
                        repaired, keyspace, db_id
                    );
                }
                Ok(next_cursor)
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e)
            }
        }
    }

    async fn storage_scan_due(&self, store: &Arc<TikvStore>, db_id: u64) -> Result<bool> {
        use crate::storage_stats::deserialize_storage_stats;

        let mut txn = store.begin().await?;
        // Cross-store liveness fence (same class as reconcile_cron_for_db): the
        // due-decision reads tenant stats but enqueue_storage_scan writes into
        // the global system_store queue. Take get_for_update on the tenant DB
        // row so a DROP DATABASE that already removed the metadata makes us
        // report "not due", suppressing the enqueue. Any orphan from the
        // irreducible cross-store window is self-healing via the worker tick.
        let alive = store.database_alive_for_update(&mut txn, db_id).await?;
        if !alive {
            txn.rollback().await.ok();
            return Ok(false);
        }
        let stats_key = crate::storage::encode_storage_stats_key_v2(db_id);
        let data = txn.get(stats_key).await?;
        txn.rollback().await.ok();

        let Some(data) = data else {
            return Ok(true);
        };
        let Some(stats) = deserialize_storage_stats(&data) else {
            return Ok(true);
        };
        let interval_ms = i64::try_from(self.config.storage_scan_interval_sec)
            .unwrap_or(i64::MAX / 1000)
            .saturating_mul(1000);
        Ok(now_epoch_ms().saturating_sub(stats.scanned_at_ms) >= interval_ms)
    }

    /// Reconcile cron jobs for a single (keyspace, db_id).
    /// Returns (enqueued_count, cleaned_count).
    async fn reconcile_cron_for_db(
        &self,
        store: &Arc<TikvStore>,
        keyspace: &str,
        db_id: u64,
    ) -> Result<(u32, u32)> {
        // 1. Existing cron entries (V2 identity index only). Reconciliation is
        //    bounded: it reads the per-(db, type) V2 index, never the global
        //    due queue. Legacy `_worker_queue_` rows do not need draining here
        //    because the one-shot V1->V2 migration runs in
        //    `init_gc_registry_store` at startup, before the worker tick loop
        //    or this sweep ever run. After migration the queue is V2-only.
        let mut txn = self.system_store.begin().await?;
        let existing_rows = self
            .system_store
            .index_rows_for_db_type(&mut txn, keyspace, db_id, TaskType::Cron)
            .await?;
        txn.commit().await?;

        let existing_job_ids: HashSet<i64> = existing_rows.iter().map(|r| r.task_id).collect();

        // 2. Check tenant cron state using the sweep-owned tenant handle.
        //
        // Cross-store liveness fence: the cron jobs read here live in the TENANT
        // keyspace, but the next-fire rows are enqueued into the global
        // `_sys_worker` queue (system_store) in step 4 — two stores, so a
        // single-txn fence (as load_next/finalize use) is impossible. We take
        // get_for_update on the tenant DB metadata row in the same tenant
        // snapshot that decides the missing jobs, so a DROP DATABASE that has
        // already removed the metadata row makes us bail before enqueuing.
        //
        // The former cross-store residual — a DROP that commits its
        // metadata-delete between this tenant commit and the system enqueue
        // commit — is now PREVENTED, not merely self-healing (issue #2628 item
        // 2). DROP's reap writes a durable dropped-DB TOMBSTONE in the SYSTEM
        // store, and step 4's enqueue takes get_for_update on that tombstone in
        // the SAME system txn as put_task_v2
        // (`enqueue_task_v2_unless_db_dropped`). The reap's tombstone put and the
        // enqueue's tombstone read then conflict under pessimistic txns: at most
        // one commits, and on enqueue retry the tombstone is present → suppress.
        // No stale `_sys_worker` next-fire row can remain for the dropped db_id.
        // The self-healing nets (execute_task's get_database_by_id → None skip,
        // and the queue reap) are retained as defense-in-depth.
        let mut tenant_txn = store.begin().await?;
        if !store
            .database_alive_for_update(&mut tenant_txn, db_id)
            .await?
        {
            tenant_txn.rollback().await.ok();
            return Ok((0, 0));
        }
        let cron_enabled = store.is_cron_enabled(&mut tenant_txn, db_id).await?;

        if !cron_enabled {
            tenant_txn.commit().await?;
            // Cron disabled but registry has cron bit — clean up bounded V2
            // entries via the per-(db, type) index. No legacy V1 rows remain:
            // the startup V1->V2 migration already converted them.
            let total = existing_rows.len();
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
                // Cross-store fence: tombstone get_for_update + put_task_v2 in
                // ONE system txn (issue #2628 item 2). A concurrent DROP-reap
                // conflicts on the tombstone key.
                if self
                    .system_store
                    .enqueue_task_v2_unless_db_dropped(&mut sys_txn, &queue_entry, next_fire)
                    .await?
                {
                    enqueued += 1;
                }
            }
            sys_txn.commit().await?;
        }

        // 5. Cleanup bounded V2 orphans: queue entries whose job_id is not in
        //    active jobs. The queue is V2-only after the startup migration, so
        //    the V2 identity index is the complete orphan set — no global
        //    `_worker_queue_` scan is needed.
        let orphan_rows: Vec<&WqIndexRow> = existing_rows
            .iter()
            .filter(|r| !active_job_ids.contains(&r.task_id))
            .collect();

        if !orphan_rows.is_empty() {
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
            sys_txn.commit().await?;
            cleaned = orphan_rows.len() as u32;
        }

        Ok((enqueued, cleaned))
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
    async fn reconcile_ddl_journal_for_db(
        &self,
        store: &Arc<TikvStore>,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        use crate::storage::DdlOperation;

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
                                store
                                    .assert_database_alive_for_update(&mut txn, db_id)
                                    .await?;
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
                    store
                        .assert_database_alive_for_update(&mut txn, db_id)
                        .await?;
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
                                store
                                    .assert_database_alive_for_update(&mut txn, db_id)
                                    .await?;
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
                             Journal entry preserved for retry on the next sweep cycle.",
                            table_name, db_id, e
                        );
                        txn.rollback().await.ok();
                        continue;
                    }
                    store.delete_ddl_journal(&mut txn, db_id, jentry.id).await?;
                    store
                        .assert_database_alive_for_update(&mut txn, db_id)
                        .await?;
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
        // The stale bit causes only a cheap empty journal scan per sweep cycle.
        // If any entries failed cleanup (!all_cleaned), the bit must stay
        // set regardless so a later sweep cycle retries those entries.

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

    /// Delete a V2 due entry plus its identity index and split payload row.
    async fn delete_due_entry(
        system_store: &Arc<TikvStore>,
        txn: &mut tikv_client::Transaction,
        due_key: &[u8],
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
    ) -> Result<()> {
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

    /// Pure classifier mapping a renewal-txn result onto a `LeaseRenewOutcome`.
    ///
    /// This holds the three pieces of wiring the original bug got wrong, in ONE
    /// testable place (post-mortem class F: the bug was the wiring, not the math):
    ///
    /// 1. `Ok(true)` advances the committed deadline to the PROSPECTIVE value that
    ///    just committed (`new_lease_until_ms`) — and ONLY this arm advances.
    /// 2. `Ok(false)` is `ClaimLost` (claim gone / foreign owner) → loop cancels.
    /// 3. `Err` compares `now` against the COMMITTED deadline
    ///    (`last_committed_lease_until_ms`), NOT the prospective one. This is the
    ///    exact regression surface: passing `new_lease_until_ms` here (always in
    ///    the future) would make `cancel` always false and re-open
    ///    double-execution. Unit tests drive `Err` directly to guard this.
    fn classify_renew_result(
        renewed: Result<bool>,
        new_lease_until_ms: i64,
        now_ms: i64,
        last_committed_lease_until_ms: i64,
    ) -> LeaseRenewOutcome {
        match renewed {
            // Commit landed — the stored lease now extends to `new_lease_until_ms`.
            // ONLY this arm advances the loop's committed deadline.
            Ok(true) => LeaseRenewOutcome::Renewed {
                committed_lease_until_ms: new_lease_until_ms,
            },
            // Claim is gone or owned by another worker — our lease is lost.
            Ok(false) => LeaseRenewOutcome::ClaimLost,
            // Transient error. Decide against the COMMITTED deadline, never the
            // prospective `new_lease_until_ms`: once the stored lease has lapsed
            // another worker can win the expired-lease CAS and double-execute.
            Err(_) => LeaseRenewOutcome::Errored {
                cancel: now_ms >= last_committed_lease_until_ms,
            },
        }
    }

    /// Perform ONE lease-renewal attempt and classify the outcome.
    ///
    /// `last_committed_lease_until_ms` is the deadline the loop has actually
    /// committed so far (seeded from the claim's initial lease). The txn I/O and
    /// the outcome classification are split: this method does the I/O, then
    /// delegates the decision to the pure `classify_renew_result`, so the
    /// "committed, not prospective" comparison is unit-testable without TiKV
    /// while still being the exact decision a TiKV-backed loop test drives.
    ///
    /// Returns `Renewed` carrying the NEW committed deadline only when the renew
    /// txn committed (`Ok(true)`); the caller advances its state from that value.
    #[allow(clippy::too_many_arguments)]
    async fn renew_lease_once(
        system_store: &Arc<TikvStore>,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
        worker_id: &str,
        task_type: TaskType,
        lease_ms: i64,
        last_committed_lease_until_ms: i64,
    ) -> LeaseRenewOutcome {
        let new_lease_until = crate::worker::now_epoch_ms().saturating_add(lease_ms);
        let renewed = async {
            let mut txn = system_store.begin().await?;
            let ok = system_store
                .renew_worker_claim(
                    &mut txn,
                    keyspace,
                    db_id,
                    task_id,
                    fire_time_ms,
                    worker_id,
                    task_type,
                    new_lease_until,
                )
                .await?;
            if ok {
                txn.commit().await?;
            } else {
                txn.rollback().await.ok();
            }
            Ok::<bool, anyhow::Error>(ok)
        }
        .await;

        // Log the abnormal outcomes here (where keyspace/task context is in
        // scope), then delegate the decision to the pure classifier.
        match &renewed {
            Ok(true) => {}
            Ok(false) => warn!(
                "Worker claim lost (renewal found no/foreign claim); cancelling task: \
                 keyspace={} db_id={} task_id={} type={:?}",
                keyspace, db_id, task_id, task_type
            ),
            Err(e) => warn!(
                "Worker claim renewal error (will retry within lease): \
                 keyspace={} db_id={} task_id={} type={:?}: {e}",
                keyspace, db_id, task_id, task_type
            ),
        }

        Self::classify_renew_result(
            renewed,
            new_lease_until,
            crate::worker::now_epoch_ms(),
            last_committed_lease_until_ms,
        )
    }

    /// Spawn the lease-renewal loop for a claimed task. While the task runs, the
    /// loop renews the claim's lease at ~lease/3. If a renewal is LOST (claim
    /// deleted or stolen) or repeatedly errors past the lease, it cancels
    /// `exec_shutdown` so the executor aborts before its next tenant commit.
    /// The returned guard aborts the loop when dropped (execution finished).
    #[allow(clippy::too_many_arguments)]
    fn spawn_claim_lease_renewer(
        system_store: Arc<TikvStore>,
        config: WorkerConfig,
        keyspace: String,
        db_id: u64,
        task_id: i64,
        fire_time_ms: i64,
        task_type: TaskType,
        // The lease deadline committed at claim time (claimed_at + claim_lease_ms).
        // Seeds the "last successfully committed lease" the error path compares
        // against, so a renewal-error storm cancels once the STORED lease lapses.
        initial_lease_until_ms: i64,
        exec_shutdown: CancellationToken,
    ) -> LeaseRenewerGuard {
        let lease_ms = (config.claim_lease_ms as i64).max(1);
        // Renew at ~lease/3, floored so we never busy-spin.
        let renew_interval_ms = (lease_ms / 3).max(1_000) as u64;
        let worker_id = config.worker_id.clone();

        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(renew_interval_ms));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // First tick fires immediately; skip it so the first renewal lands
            // ~renew_interval after the claim was taken.
            ticker.tick().await;
            // The last lease deadline we actually COMMITTED. Seeded from the
            // claim's initial lease, advanced ONLY by a Renewed outcome (an
            // Ok(true) commit); the error path compares against THIS, never the
            // prospective `new_lease_until` (which is always in the future).
            let mut last_committed_lease_until = initial_lease_until_ms;
            loop {
                tokio::select! {
                    _ = exec_shutdown.cancelled() => return,
                    _ = ticker.tick() => {}
                }

                match Self::renew_lease_once(
                    &system_store,
                    &keyspace,
                    db_id,
                    task_id,
                    fire_time_ms,
                    &worker_id,
                    task_type,
                    lease_ms,
                    last_committed_lease_until,
                )
                .await
                {
                    LeaseRenewOutcome::Renewed {
                        committed_lease_until_ms,
                    } => {
                        // Advance ONLY on a committed renewal.
                        last_committed_lease_until = committed_lease_until_ms;
                    }
                    LeaseRenewOutcome::ClaimLost => {
                        // Lease lost — abort before the run commits more tenant work.
                        exec_shutdown.cancel();
                        return;
                    }
                    LeaseRenewOutcome::Errored { cancel } => {
                        // `cancel` was decided against the COMMITTED deadline.
                        if cancel {
                            exec_shutdown.cancel();
                            return;
                        }
                        // Otherwise retry next tick within the remaining window.
                    }
                }
            }
        });

        LeaseRenewerGuard { handle }
    }

    /// Core implementation of claim_and_execute, parameterized over the finalize
    /// function so tests can inject failures in the real code path.
    ///
    /// Handles V2 descriptors after startup migration has converted legacy
    /// `_worker_queue_` rows. A post-claim existence re-check closes the
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
        F: FnOnce(
            Arc<TikvStore>,
            u64,
            CronRun,
            CronRunStatus,
            Option<String>,
            i64,
            i64,
            i64,
        ) -> Fut,
        Fut: Future<Output = Result<()>> + Send,
    {
        let queue_fire_time_ms = crate::storage::decode_wq_due_v2_fire_time(&queue_key)
            .ok_or_else(|| anyhow!("corrupted worker queue key: missing fire_time_ms"))?;
        let scheduled_minute = queue_fire_time_ms.div_euclid(60_000);
        let task_type = due.task_type();
        let (claim_keyspace, claim_db_id, claim_task_id) =
            (due.keyspace().to_string(), due.db_id(), due.task_id());
        // Worker-queue SYSTEM-claim orphan timeout: a fallback for legacy claims
        // without an explicit lease. The live run renews its claim via the lease
        // keeper, so this stays the raw control timeout (it gates the queue claim,
        // not the cron control deadline).
        let legacy_orphan_timeout_ms = (config.orphan_timeout_sec as i64).saturating_mul(1000);
        // Cron CONTROL/ACTIVE floor: the FROZEN orphan deadline must cover the
        // full legitimate EXECUTION window. With no per-job `max_runtime_ms` that
        // window is `cron_job_timeout_ms`, not the bare `orphan_timeout_sec`, so
        // the floor is the effective `max(orphan_timeout, cron_job_timeout)` — the
        // SAME single-source helper the GC reaper view uses (no second copy that
        // can drift). The per-claim path then takes a further `max` with
        // `job.max_runtime_ms` inside `cron_effective_orphan_deadline_ms`.
        let cron_control_floor_ms = crate::worker::gc::effective_cron_orphan_floor_ms(
            config.orphan_timeout_sec,
            config.cron_job_timeout_ms,
        );
        let claim_lease_ms = config.claim_lease_ms as i64;
        let claim = WorkerClaim::with_lease(config.worker_id.clone(), task_type, claim_lease_ms);

        let mut txn = system_store.begin().await?;
        let claimed = system_store
            .try_claim_worker_task(
                &mut txn,
                &claim_keyspace,
                claim_db_id,
                claim_task_id,
                queue_fire_time_ms,
                &claim,
                legacy_orphan_timeout_ms,
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
        // exists. Split V2 types fetch the command/username/schedule out-of-line
        // by exact identity.
        let entry: TaskQueueEntry = match due {
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

        let (cron_run, keep_queue_entry, should_requeue_cron) = if entry.task_type == TaskType::Cron
        {
            Self::claim_and_record_cron_run(
                pool,
                &entry,
                scheduled_minute,
                cron_control_floor_ms,
                config.cron_job_timeout_ms,
            )
            .await?
        } else {
            (None, false, false)
        };
        // `should_requeue_cron` is decoupled from `cron_run.is_some()`: it is true
        // whenever the claimed minute is DONE (ran-it OR `AlreadyTerminalForMinute`),
        // so a reaper-recovered crash whose minute the next worker observes already
        // terminal still enqueues the next fire and preserves guaranteed schedule
        // progress (design 35 §Contract / DEFECT 2 residual). It is false for a
        // still-live block and for not-actionable outcomes (cron disabled, job
        // gone/inactive, stale payload, DB dropped).

        // Lease keeper: while this task executes, periodically renew our claim
        // (at ~lease/3) so GC's expired-lease reaper never reaps a still-running
        // task and lets a second worker double-execute it. A lost renewal
        // (claim deleted or stolen) cancels `exec_shutdown`, aborting the run
        // before its next tenant commit — at-most-once while the lease is live
        // (design §K4). `exec_shutdown` is a child of the engine shutdown token,
        // so an engine shutdown still propagates.
        let exec_shutdown = shutdown_signal.child_token();
        let _lease_guard = Self::spawn_claim_lease_renewer(
            system_store.clone(),
            config.clone(),
            claim_keyspace.clone(),
            claim_db_id,
            claim_task_id,
            queue_fire_time_ms,
            task_type,
            // Seed with the lease deadline committed by the winning claim above.
            claim.lease_until_ms,
            exec_shutdown.clone(),
        );

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

            // The executor timeout and the frozen orphan deadline are the SAME
            // "legitimate execution window" — derive both from the one source so a
            // still-executing run can never be classed expired and taken over
            // (`cron_execution_window_ms`; design 35 §Effective floor).
            let timeout_ms = crate::storage::cron::cron_execution_window_ms(
                max_runtime_ms,
                config.cron_job_timeout_ms,
            );
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
                Some(exec_shutdown.clone()),
                deadline,
            )
            .await;

            get_process_list().deregister(run.run_id);
            result
        } else {
            Self::execute_task(
                pool,
                config,
                &entry,
                None,
                Some(exec_shutdown.clone()),
                None,
            )
            .await
        };

        // Execution finished — stop renewing the lease. The cleanup below holds
        // the claim until it deletes it explicitly, and runs on the same node,
        // so it does not depend on the lease.
        drop(_lease_guard);

        // Commit-adjacent ownership fence (claim-lease lifecycle, design §K4).
        // finalize_cron_run commits TERMINAL tenant cron state (terminal CronRun
        // + cleared running guard + released per-minute claim). Once the renewer
        // is stopped, our lease may already have been stolen — a takeover only
        // happens on an EXPIRED lease, by which point the renewer has cancelled
        // exec_shutdown and aborted us. A worker that no longer owns the claim
        // must NOT commit terminal cron state or run any cleanup/requeue: the
        // takeover worker is the sole authority for this run's outcome. DB
        // liveness alone (finalize's existing fence) does not detect takeover —
        // the DB is still alive, only ownership changed. So re-verify ownership
        // against the SAME system_store claim (the identity check
        // delete_worker_claim_if_owned does, hoisted ahead of finalize) and, if
        // lost, skip finalize AND the whole cleanup/requeue/bg-result block,
        // leaving the row for the new owner. This is a same-store fence and is
        // feasible (unlike the documented cross-store cron next-fire residual).
        let still_owned = {
            let mut own_txn = system_store.begin().await?;
            let owned = system_store
                .is_worker_claim_owned_by(
                    &mut own_txn,
                    &entry.keyspace,
                    entry.db_id,
                    entry.task_id,
                    queue_fire_time_ms,
                    entry.task_type,
                    &config.worker_id,
                )
                .await?;
            own_txn.rollback().await.ok();
            owned
        };

        if !still_owned {
            warn!(
                "Worker no longer owns claim before finalize (lease lost / taken over); \
                 skipping finalize + cleanup and leaving queue row for the new owner: \
                 keyspace={} db_id={} task_id={} type={:?}",
                entry.keyspace, entry.db_id, entry.task_id, entry.task_type
            );
            // The takeover worker owns the run's terminal state and cleanup; the
            // running guard stays Running so the new owner is not blocked.
            return Ok(());
        }

        // Capture finalize result instead of propagating with `?` — cleanup
        // must run unconditionally even when finalize fails (#1259). Finalize is
        // now gated on the ownership fence above, so a non-owner never reaches it.
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
                    scheduled_minute,
                )
                .await
            } else {
                Ok(())
            };

        if let Err(ref e) = finalize_result {
            warn!("finalize_cron_run failed: {e}; proceeding with cleanup");
        }

        // Cleanup: delete OUR worker claim, manage queue entry, requeue next
        // cron fire. This block ALWAYS runs regardless of finalize_result.
        //
        // Ownership-checked claim delete: the fence above already established we
        // still own the claim, but re-check atomically here under the SAME txn
        // that performs the delete (and the queue-row / requeue / bg-result
        // writes), so a takeover landing between the fence and this commit still
        // cannot make us delete the new owner's claim or its work.
        // `still_owned == false` means "we lost the lease in that window"; we
        // then leave the entire row untouched.
        let mut txn = system_store.begin().await?;
        let still_owned = system_store
            .delete_worker_claim_if_owned(
                &mut txn,
                &entry.keyspace,
                entry.db_id,
                entry.task_id,
                queue_fire_time_ms,
                entry.task_type,
                &config.worker_id,
            )
            .await?;

        if !still_owned {
            txn.rollback().await.ok();
            warn!(
                "Worker no longer owns claim at cleanup (lease lost / taken over); \
                 leaving queue row for the new owner: keyspace={} db_id={} task_id={} type={:?}",
                entry.keyspace, entry.db_id, entry.task_id, entry.task_type
            );
            // Do not propagate a finalize error here: the takeover worker is the
            // authority for this run's outcome.
            return Ok(());
        }

        if !keep_queue_entry {
            if entry.task_type.uses_deterministic_queue_key() {
                // Deterministic-key tasks can be overwritten by a later enqueue
                // while a worker still holds the old claim. Read-compare-delete
                // ensures cleanup only removes the descriptor it processed.
                //
                // HNSW merge additionally keeps the row on failure so a worker
                // with the right capability can retry. Other deterministic tasks
                // delete the exact processed descriptor even after failure.
                if exec_result.is_ok()
                    || !entry.task_type.keeps_deterministic_queue_entry_on_failure()
                {
                    if let Some(current_bytes) = txn.get(queue_key.clone()).await? {
                        let current_nonce = TaskDescriptorV2::decode(&current_bytes)
                            .map(|d| d.nonce)
                            .map_err(|e| anyhow!("Failed to deserialize V2 descriptor: {e}"))?;
                        if current_nonce == entry.nonce {
                            Self::delete_due_entry(
                                system_store,
                                &mut txn,
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
                // Non-deterministic queue keys are unique per logical due row, so
                // cleanup can delete the exact scanned key unconditionally.
                Self::delete_due_entry(
                    system_store,
                    &mut txn,
                    &queue_key,
                    &entry,
                    queue_fire_time_ms,
                )
                .await?;
            }
        }

        // Requeue the next cron fire whenever the claimed minute is DONE
        // (`should_requeue_cron`): we ran/took-over the fire, OR the claim observed
        // it ALREADY TERMINAL (`AlreadyTerminalForMinute` — e.g. this worker claimed
        // M, crashed before requeue, and the reaper terminalized CONTROL(M) while
        // the due row for M still existed; a later worker reclaims it, sees terminal
        // CONTROL, and must NOT drop the row without scheduling M+1). Both cases use
        // the SAME fenced idempotent singleton enqueue below — a reaper-recovered
        // crash thus preserves guaranteed schedule progress instead of stalling
        // until a much-later registry sweep (design 35 §Contract / DEFECT 2
        // residual). A still-live block keeps the row and does NOT requeue here.
        //
        // Gated on `finalize_result.is_ok()`: a failed finalize means the run's
        // terminal state / DB liveness is in doubt (e.g. DROP DATABASE removed
        // metadata mid-run), so scheduling the next fire could write a fresh entry
        // into the global queue for a dropped DB. The `AlreadyTerminalForMinute`
        // path carries no `cron_run`, so `finalize_result` is `Ok(())` (the run was
        // finalized by whoever completed M) — its DB-liveness is instead enforced by
        // `load_next_cron_queue_entry`'s own tenant fence + the dropped-DB tombstone
        // fence on the enqueue below.
        if entry.task_type == TaskType::Cron && should_requeue_cron && finalize_result.is_ok() {
            // The next-fire decision must cross the SAME DB-liveness fence that
            // finalize uses. The database metadata row lives in the TENANT
            // keyspace (not this system_store txn), so the fence is taken inside
            // load_next_cron_queue_entry's own tenant txn via
            // assert_database_alive_for_update: a dropped/!alive DB makes it
            // return None, so no next entry is written into the global queue.
            //
            // The cross-store residual (a DROP committing between that tenant
            // fence and this system commit) is now PREVENTED, not merely
            // self-healing (issue #2628 item 2): the enqueue routes through
            // enqueue_task_v2_unless_db_dropped, which takes get_for_update on
            // the durable dropped-DB tombstone (written by DROP's reap in the
            // SYSTEM store) in THIS SAME system txn as put_task_v2. The reap's
            // tombstone put and this enqueue then conflict under pessimistic
            // txns — at most one commits, and a retry sees the tombstone and
            // suppresses — so no stale next-fire row survives for a dropped db.
            if let Some(next_entry) = Self::load_next_cron_queue_entry(pool, &entry).await? {
                if let Some(schedule) = next_entry.schedule.as_deref() {
                    if let Ok(next_fire) = compute_next_fire_time(schedule) {
                        // Cross-store fence: tombstone get_for_update +
                        // put_task_v2 in this SAME system txn (issue #2628 item
                        // 2). A DROP-reap that committed the tombstone (or races
                        // this commit) conflicts on the tombstone key, so no
                        // stale next-fire row survives for a dropped db_id.
                        system_store
                            .enqueue_task_v2_unless_db_dropped(&mut txn, &next_entry, next_fire)
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
                if entry.task_type == crate::worker::types::TaskType::AsyncTrigger {
                    crate::metrics::record_trigger_event(&entry.keyspace, "completed");
                    if let Err(err) = crate::worker::sample_async_trigger_queue_depth(
                        system_store,
                        &entry.keyspace,
                    )
                    .await
                    {
                        warn!(
                            "Failed to sample async trigger queue depth for {}: {}",
                            entry.keyspace, err
                        );
                    }
                }
                info!(
                    "Worker task completed: keyspace={} db_id={} task_id={} type={:?}",
                    entry.keyspace, entry.db_id, entry.task_id, entry.task_type
                );
            }
            Err(ref e) => {
                metrics.record_task_result(entry.task_type, false);
                if entry.task_type == crate::worker::types::TaskType::AsyncTrigger {
                    crate::metrics::record_trigger_event(&entry.keyspace, "failed");
                    if let Err(err) = crate::worker::sample_async_trigger_queue_depth(
                        system_store,
                        &entry.keyspace,
                    )
                    .await
                    {
                        warn!(
                            "Failed to sample async trigger queue depth for {}: {}",
                            entry.keyspace, err
                        );
                    }
                }
                warn!(
                    "Worker task failed: keyspace={} db_id={} task_id={} type={:?} error={}",
                    entry.keyspace, entry.db_id, entry.task_id, entry.task_type, e
                );
            }
        }

        Ok(())
    }

    /// Returns `(cron_run, keep_queue_entry, requeue_next_fire)`:
    /// - `cron_run`: `Some` iff this worker owns a run to execute + finalize.
    /// - `keep_queue_entry`: keep the due row (a live run holds the job) vs drop it.
    /// - `requeue_next_fire`: enqueue the job's NEXT fire because the claimed minute
    ///   is DONE (ran it, or already terminal). FALSE for blocked/not-actionable
    ///   outcomes (cron disabled, job gone/inactive, stale payload, DB dropped, or a
    ///   still-live block) — see `cron_outcome_requeues_next_fire`. Decoupled from
    ///   `cron_run.is_some()` so the `AlreadyTerminalForMinute` crash-recovery case
    ///   (reaper terminalized the minute; this worker did not run it) still preserves
    ///   guaranteed schedule progress (design 35 §Contract / DEFECT 2 residual).
    async fn claim_and_record_cron_run(
        pool: &Arc<TikvClientPool>,
        entry: &TaskQueueEntry,
        scheduled_minute: i64,
        global_orphan_timeout_ms: i64,
        cron_job_timeout_ms: u64,
    ) -> Result<(
        Option<(Arc<TikvStore>, u64, CronRun, i64, Option<u64>)>,
        bool,
        bool,
    )> {
        let handle = pool.acquire(Some(entry.keyspace.clone())).await?;
        let store = handle.store().clone();

        // Fail-closed migration gate (design 35): before claiming on the new
        // control/active path, ensure any legacy guard/claim keys for this db are
        // migrated, so a rolling deploy never runs the old and new claim schemes
        // against the same fire. Idempotent — a no-op marker read once migrated.
        //
        // The migrated guard inherits a LIVE orphan deadline so it keeps the
        // legacy guard's no-overlap authority for the rolling window (a born-stale
        // `deadline_ms = 0` would be classed as expired by both the no-overlap gate
        // and the reaper, reopening the double-exec window). The migration resolves
        // EACH guard's own job `max_runtime_ms` from the catalog inside its txn and
        // stamps the deadline via the SAME shared helper a fresh claim uses
        // (`now + max(global, job.max_runtime)`), so a long-running job's migrated
        // authority does not expire at `now + global` while the per-claim straggler
        // fold (which already uses the longer effective deadline) would not —
        // closing the mixed-version job-level overlap that divergence opened. We
        // pass the global timeout as the floor; the per-job runtime is read inside
        // the migration, not here.
        store
            .ensure_cron_control_migrated(
                entry.db_id,
                now_epoch_ms(),
                global_orphan_timeout_ms,
                cron_job_timeout_ms,
            )
            .await?;

        let mut txn = store.begin().await?;

        // The inner block yields `(run, keep_queue_entry, must_commit, requeue)`:
        // `must_commit` is `true` ONLY when the claim performed a durable straggler
        // fold but is blocked (it wrote ACTIVE/CONTROL the reaper must later be able
        // to reap). Every plain "no run" path leaves it `false` so the txn rolls
        // back — no needless commit (design 35 §Post-marker straggler fold).
        // `requeue` is `true` iff the claimed minute is DONE and the caller must
        // enqueue the job's next fire (ran-it OR `AlreadyTerminalForMinute`); the
        // not-actionable early returns below leave it `false` (no job to schedule
        // from, or the DB/cron is gone) — see `cron_outcome_requeues_next_fire`.
        let claim_result = async {
            // Cheap fast-path snapshot read: skip the claim work if cron is already
            // visibly disabled. This is NOT the authoritative gate — a concurrent
            // `DROP EXTENSION pg_cron` can still commit `remove_cron_enabled` AFTER
            // this snapshot. The control-plane writes below are fenced by the
            // `is_cron_enabled_for_update` pessimistic read taken in the SAME txn
            // before commit (see the commit arms), which conflicts with that drop.
            if !store.is_cron_enabled(&mut txn, entry.db_id).await? {
                return Ok((None, false, false, false));
            }

            let Some(job) = store
                .get_cron_job(&mut txn, entry.db_id, entry.task_id)
                .await?
            else {
                return Ok((None, false, false, false));
            };
            if !job.active {
                return Ok((None, false, false, false));
            }
            if !cron_queue_entry_matches_job(entry, &job) {
                warn!(
                    keyspace = %entry.keyspace,
                    db_id = entry.db_id,
                    job_id = entry.task_id,
                    "skipping stale cron queue entry whose payload no longer matches catalog job"
                );
                return Ok((None, false, false, false));
            }

            // The database name is needed to build the run record, so resolve it
            // (and the DROP-DATABASE liveness signal) BEFORE the claim CAS.
            let Some(db_def) = store.get_database_by_id(&mut txn, entry.db_id).await? else {
                tracing::warn!(
                    db_id = entry.db_id,
                    job_id = entry.task_id,
                    "skipping cron job: database no longer exists (possibly dropped)"
                );
                return Ok((None, false, false, false));
            };
            let database = db_def.name;
            let max_runtime_ms = job.max_runtime_ms;

            // Orphan deadline: now + max(global orphan timeout, this job's
            // max_runtime), FROZEN onto the CONTROL/ACTIVE records at claim time.
            // The fence-CAS reaper (`reap_stale_active_runs`, cron/worker.rs) is
            // the single orphan-recovery path and drives off this frozen deadline,
            // so a later `cron.alter_job` that lowers max_runtime cannot retroact
            // a still-live run to Failed. It is >> the system claim lease, so a
            // normally lease-renewing run is never superseded mid-flight.
            let now = now_epoch_ms();
            // Single source of truth for the frozen orphan deadline — the SAME
            // helper the bulk migration and the per-claim straggler fold use, so
            // `now + max(global, job.max_runtime)` is computed in exactly one place
            // (a second copy once let the bulk migration stamp `now + global` and
            // expire a long job's migrated authority early — design 35 §Migration).
            let deadline_ms = crate::storage::cron::cron_effective_orphan_deadline_ms(
                now,
                global_orphan_timeout_ms,
                max_runtime_ms,
                cron_job_timeout_ms,
            );

            // Single-lifecycle, fence-token claim (design 35). One pessimistic CAS
            // over the CONTROL + ACTIVE keys: dedup, no-overlap, fence mint, and
            // the Running history projection all happen here.
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    entry.db_id,
                    entry.task_id,
                    scheduled_minute,
                    now,
                    deadline_ms,
                    &job,
                    database,
                )
                .await?;
            if let Some(keep_queue_entry) = keep_queue_entry_for_claim_status(&outcome) {
                // A blocked claim yields no run. If it folded a straggler guard it
                // wrote durable ACTIVE/CONTROL the reaper must later reap, so the txn
                // MUST commit; a plain block wrote nothing and rolls back.
                //
                // `AlreadyTerminalForMinute` lands here too (no run, drop the row),
                // but its minute is DONE — the caller MUST still enqueue the next
                // fire (`cron_outcome_requeues_next_fire`), or a reaper-recovered
                // crash silently loses one fire (DEFECT 2 residual). A still-live
                // block is NOT done → no requeue.
                let requeue = cron_outcome_requeues_next_fire(&outcome);
                return Ok((
                    None,
                    keep_queue_entry,
                    outcome.must_commit_blocked(),
                    requeue,
                ));
            }
            let requeue = cron_outcome_requeues_next_fire(&outcome);
            let (CronClaimOutcome::Claimed { run } | CronClaimOutcome::TookOver { run }) = outcome
            else {
                unreachable!(
                    "keep_queue_entry_for_claim_status returns None only for Claimed/TookOver"
                );
            };
            let started_at = run.start_time.unwrap_or(now);
            Ok((
                Some((store.clone(), entry.db_id, run, started_at, max_runtime_ms)),
                false,
                true,
                requeue,
            ))
        }
        .await;

        match claim_result {
            Ok((Some(run), keep_queue_entry, _must_commit, requeue)) => {
                // Authoritative cron-disabled fence (P1). This is the SAME txn that
                // wrote CONTROL/ACTIVE/CronRun via `claim_or_takeover_cron_run`.
                // Take `get_for_update` on the cron-enabled marker so a concurrent
                // `DROP EXTENSION pg_cron` (`remove_cron_enabled` + `delete_all_cron_data`)
                // either makes this read see cron disabled (→ abort, no orphaned
                // control-plane state) or write-write-conflicts the marker and aborts
                // one side. The plain fast-path read above does not serialize with
                // the drop; this fence does. Mirrors the DB-liveness fence beside it
                // — the disabled-DB GC skip never reaps such orphaned state, so the
                // write must be prevented, not self-healed.
                if !store
                    .is_cron_enabled_for_update(&mut txn, entry.db_id)
                    .await?
                {
                    // Aborted: nothing committed and cron is disabled — no next fire.
                    txn.rollback().await.ok();
                    return Ok((None, keep_queue_entry, false));
                }
                store
                    .assert_database_alive_for_update(&mut txn, entry.db_id)
                    .await?;
                txn.commit().await?;
                Ok((Some(run), keep_queue_entry, requeue))
            }
            // Blocked-but-folded: commit the durable fold (ACTIVE/CONTROL the reaper
            // can later supersede) under the same DB-liveness fence, even though no
            // run was claimed this tick. Without this the fold is rolled back every
            // tick and the orphaned straggler never becomes reapable.
            Ok((None, keep_queue_entry, true, requeue)) => {
                // Same authoritative cron-disabled fence: a straggler fold TRANSLATES
                // legacy guard state into new ACTIVE/CONTROL authority, so a fold
                // committed after a concurrent `DROP EXTENSION pg_cron` would leave
                // orphaned (un-reaped) control-plane state. Fence it identically to
                // the claimed-run arm.
                if !store
                    .is_cron_enabled_for_update(&mut txn, entry.db_id)
                    .await?
                {
                    txn.rollback().await.ok();
                    return Ok((None, keep_queue_entry, false));
                }
                store
                    .assert_database_alive_for_update(&mut txn, entry.db_id)
                    .await?;
                txn.commit().await?;
                // A fold is a still-live block (`BlockedByLiveActiveFolded`) → not
                // done → `requeue` is false; propagate it for a single clean path.
                Ok((None, keep_queue_entry, requeue))
            }
            Ok((None, keep_queue_entry, false, requeue)) => {
                txn.rollback().await.ok();
                Ok((None, keep_queue_entry, requeue))
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
            // Liveness fence: take get_for_update on the tenant DB metadata row
            // BEFORE deciding the next fire, so a concurrent DROP DATABASE makes
            // this read see the DB gone (None) rather than scheduling a next
            // cron fire into the global queue for a dropped DB. This is the same
            // fence finalize_cron_run uses; here a dropped DB simply means "no
            // next entry" so cleanup can still delete the claim/queue row.
            if !store
                .database_alive_for_update(&mut txn, entry.db_id)
                .await?
            {
                return Ok(None);
            }

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
        scheduled_min: i64,
    ) -> Result<()> {
        let terminal_state = match &status {
            CronRunStatus::Succeeded => CronRunState::Succeeded,
            CronRunStatus::Cancelled => CronRunState::Cancelled,
            _ => CronRunState::Failed,
        };
        run.status = status;
        run.return_message = return_message;
        run.start_time = Some(start_time);
        run.end_time = Some(end_time);

        let mut txn = store.begin().await?;
        // Fence-gated terminal CAS (design 35). The store rejects the write if a
        // takeover has minted a higher fence — so a worker that lost its lease
        // cannot commit terminal cron state (DEFECT 1). On accept the SAME txn
        // clears the no-overlap pointer and projects the terminal CronRun, so a
        // terminal transition can never strand the per-minute dedup record
        // (DEFECT 2). `run.run_id` IS the fence (minted == run_id at claim).
        let finalize_result: Result<bool> = async {
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    run.job_id,
                    scheduled_min,
                    run.run_id,
                    terminal_state,
                    &run,
                )
                .await?;
            if !accepted {
                return Ok(false);
            }
            store
                .assert_database_alive_for_update(&mut txn, db_id)
                .await?;
            Ok(true)
        }
        .await;

        match finalize_result {
            Ok(true) => {
                txn.commit().await?;
                Ok(())
            }
            Ok(false) => {
                txn.rollback().await.ok();
                warn!(
                    db_id,
                    job_id = run.job_id,
                    scheduled_min,
                    "cron finalize rejected by fence (lease lost / taken over); \
                     leaving terminal state to the new owner"
                );
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

        // The specialized long-running paths below return BEFORE `run_with_guards`
        // (the only place that otherwise threads `shutdown_signal`), yet each one
        // commits TENANT writes after extended work. They must observe the same
        // lease-cancellation token so a lost/stolen claim aborts the run before its
        // next commit — at-most-once while the lease is live. `LeaseCancel` carries
        // that token to every per-batch / per-phase commit choke point.
        let lease_cancel = crate::worker::LeaseCancel::new(shutdown_signal.clone());

        if entry.task_type == TaskType::StorageSizeScan {
            execute_storage_size_scan(
                &store,
                &entry.keyspace,
                entry.db_id,
                pool.pd_endpoints(),
                config.storage_scan_pd_rate_limit_ms,
                &lease_cancel,
            )
            .await?;
            return Ok(1);
        }

        if entry.task_type == TaskType::HnswMerge && entry.command.starts_with("__hnsw_merge ") {
            let (table_id, index_id) = parse_hnsw_merge_command(&entry.command)?;
            execute_hnsw_merge(&store, entry.db_id, table_id, index_id, &lease_cancel).await?;
            return Ok(1);
        }

        // BgDdl tasks (e.g. CREATE INDEX CONCURRENTLY backfill) are exempt from
        // statement_timeout — they legitimately run for extended periods.
        if entry.task_type == TaskType::BgDdl && entry.command.starts_with("__backfill_index ") {
            for attempt in 0..WORKER_BGDDL_MAX_RETRY_ATTEMPTS {
                let stmt_ts = now_epoch_ms();
                let qctx = QueryContext::new(
                    0,
                    database_name.clone(),
                    current_user.clone(),
                    stmt_ts,
                    tx_start_ms,
                    timezone.clone(),
                );
                let mark_retryable_invalid = attempt + 1 == WORKER_BGDDL_MAX_RETRY_ATTEMPTS;
                let result = query_context::with_scoped_query_context(
                    &qctx,
                    Self::execute_bg_ddl_backfill(
                        &store,
                        entry,
                        &lease_cancel,
                        mark_retryable_invalid,
                    ),
                )
                .await;

                match result {
                    Ok(()) => return Ok(1),
                    Err(e)
                        if attempt + 1 < WORKER_BGDDL_MAX_RETRY_ATTEMPTS
                            && is_retryable_tikv_error(&e) =>
                    {
                        tracing::info!(
                            attempt = attempt + 1,
                            max_attempts = WORKER_BGDDL_MAX_RETRY_ATTEMPTS,
                            task_id = entry.task_id,
                            "bg_ddl backfill hit transient storage conflict, retrying task"
                        );
                        worker_bgsql_backoff(attempt).await;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }

            unreachable!("bg_ddl retry loop must return")
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
                let mut cron_activity_modified = false;
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
                    let statement_dirty_table_ids =
                        Arc::new(parking_lot::Mutex::new(HashSet::new()));
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
                                    crate::session_context::with_statement_dirty_table_ids(
                                        statement_dirty_table_ids.clone(),
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
                            )
                            .await
                        },
                    );
                    let result = fut.await?;
                    if is_cron {
                        crate::database_activity::record_sql_activity(
                            &entry.keyspace,
                            entry.db_id,
                            crate::database_activity::DatabaseActivityKind::Active,
                        );
                        if result.modifies_database()
                            || !statement_dirty_table_ids.lock().is_empty()
                        {
                            cron_activity_modified = true;
                        }
                    }
                }
                store
                    .assert_database_alive_for_update(&mut txn, entry.db_id)
                    .await?;
                txn.commit().await?;
                if is_cron && cron_activity_modified {
                    crate::database_activity::record_sql_activity(
                        &entry.keyspace,
                        entry.db_id,
                        crate::database_activity::DatabaseActivityKind::Modified,
                    );
                }
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

    async fn execute_bg_ddl_backfill(
        store: &Arc<TikvStore>,
        entry: &TaskQueueEntry,
        lease_cancel: &crate::worker::LeaseCancel,
        mark_retryable_invalid: bool,
    ) -> Result<()> {
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
        match index.state {
            IndexState::Building | IndexState::WriteOnly => {}
            IndexState::Ready | IndexState::Invalid => {
                // Duplicate/stale queue entries are harmless once the CIC has
                // reached a terminal state. WriteOnly is not terminal: a retry
                // after phase 1 must resume from phase 2 instead of pretending
                // the task is complete.
                warn!(
                    "Skipping CIC backfill task because index is already terminal: table={} index={} state={:?}",
                    table_name, index_name, index.state
                );
                return Ok(());
            }
        }

        // Lease fence at the start of every phase AND inside each phase's
        // per-batch txn rotation (`maybe_rotate_backfill_txn`, threaded via
        // `lease_cancel`). A multi-phase CIC backfill is a long loop where the
        // claim lease can lapse mid-run; on cancellation we abort WITHOUT
        // committing the next batch / phase and leave the task for the new owner.
        //
        // A pre-phase cancellation surfaces as a plain error (not mark_invalid):
        // the index is still in a valid intermediate state for the new owner to
        // resume, so it must NOT be flipped to Invalid.
        lease_cancel.bail_if_cancelled()?;

        // Phase 1 (Building): backfill and atomically flip to WriteOnly. A
        // retry may re-enter here after phase 1 already committed; in that
        // case the current state is WriteOnly and phase 1 must be skipped.
        if index.state == IndexState::Building {
            match ddl::backfill_index_by_name(
                store,
                db_id,
                &table_name,
                &index_name,
                Some(IndexState::WriteOnly),
                lease_cancel,
            )
            .await
            {
                Ok(()) => {}
                Err(e) if is_claim_cancelled_error(&e) => return Err(e),
                Err(e) if is_retryable_tikv_error(&e) && !mark_retryable_invalid => return Err(e),
                Err(e) => return Err(mark_invalid(e).await),
            }
        }

        lease_cancel.bail_if_cancelled()?;

        // Phase 2 (WriteOnly): catch-up scan on a fresh snapshot.
        match ddl::backfill_index_by_name(
            store,
            db_id,
            &table_name,
            &index_name,
            None,
            lease_cancel,
        )
        .await
        {
            Ok(()) => {}
            Err(e) if is_claim_cancelled_error(&e) => return Err(e),
            Err(e) if is_retryable_tikv_error(&e) && !mark_retryable_invalid => return Err(e),
            Err(e) => return Err(mark_invalid(e).await),
        }

        lease_cancel.bail_if_cancelled()?;

        // Phase 3: reconcile stale entries and atomically expose index to planner.
        match ddl::reconcile_index(
            store,
            db_id,
            &table_name,
            &index_name,
            Some(IndexState::Ready),
            lease_cancel,
        )
        .await
        {
            Ok(()) => {}
            Err(e) if is_claim_cancelled_error(&e) => return Err(e),
            Err(e) if is_retryable_tikv_error(&e) && !mark_retryable_invalid => return Err(e),
            Err(e) => return Err(mark_invalid(e).await),
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
    /// Raw dirty-marker key cursor for the next bounded page.
    pub next_dirty_cursor: Option<Vec<u8>>,
}

#[derive(Default)]
struct RegistrySweepEntryOutcome {
    hnsw_observed: u32,
    hnsw_enqueued: u32,
    hnsw_enqueue_errors: u32,
}

fn encode_hnsw_dirty_backfill_due_ms(next_due_ms: i64) -> Vec<u8> {
    next_due_ms.to_be_bytes().to_vec()
}

fn decode_hnsw_dirty_backfill_due_ms(value: &[u8]) -> Option<i64> {
    let bytes: [u8; 8] = value.try_into().ok()?;
    Some(i64::from_be_bytes(bytes))
}

fn hnsw_dirty_backfill_due(done_value: Option<&[u8]>, now_ms: i64) -> bool {
    match done_value.and_then(decode_hnsw_dirty_backfill_due_ms) {
        Some(next_due_ms) => next_due_ms <= now_ms,
        // Missing or legacy/invalid completion values are treated as due so an
        // earlier PR build that wrote `b"1"` cannot permanently suppress repair.
        None => true,
    }
}

fn next_hnsw_dirty_backfill_due_ms(fallback_interval_sec: u64) -> i64 {
    let interval_sec = fallback_interval_sec.max(1).min((i64::MAX / 1000) as u64);
    now_epoch_ms().saturating_add((interval_sec as i64).saturating_mul(1000))
}

/// One bounded page of deploy-time compatibility backfill for HNSW dirty
/// markers.
///
/// Dirty-marker sweeps are authoritative for new writes, but old binaries wrote
/// delta keys before dirty markers existed. This tenant-local cursor lets any
/// stateless replica converge those pre-marker deltas without returning the hot
/// merge sweep to a permanent full-schema walk. Completion is a low-frequency
/// cooldown, not a permanent latch, so markerless deltas written by old binaries
/// during a rolling deploy are still discovered by a later fallback pass.
async fn backfill_hnsw_dirty_markers_page(
    store: &Arc<TikvStore>,
    txn: &mut tikv_client::Transaction,
    db_id: u64,
    table_page_size: usize,
    fallback_interval_sec: u64,
) -> Result<bool> {
    use crate::sql::hnsw::storage::{
        hnsw_delta_prefix, hnsw_delta_prefix_end, hnsw_dirty_backfill_cursor_key,
        hnsw_dirty_backfill_done_key, hnsw_dirty_key,
    };
    use crate::txn::{txn_delete, txn_put};
    use tikv_client::BoundRange;

    let done_key = hnsw_dirty_backfill_done_key(db_id);
    let cursor_key = hnsw_dirty_backfill_cursor_key(db_id);
    let cursor = txn.get_for_update(cursor_key.clone()).await?;
    let done_value = txn.get(done_key.clone()).await?;
    let now_ms = now_epoch_ms();
    // Re-check after locking the shared cursor so concurrent stateless workers
    // do not re-scan from the beginning after another worker commits a fresh
    // cooldown marker. A cursor means a compatibility pass is already in
    // progress and must continue even while the last completion marker exists.
    if cursor.is_none() && !hnsw_dirty_backfill_due(done_value.as_deref(), now_ms) {
        return Ok(false);
    }
    let (table_names, next_cursor) = store
        .scan_tables_page(txn, db_id, cursor.as_deref(), table_page_size.max(1))
        .await?;

    for table_name in table_names {
        let Some(schema) = store.get_schema(txn, db_id, &table_name).await? else {
            continue;
        };
        for index in &schema.indexes {
            if !index.is_hnsw() {
                continue;
            }

            let prefix = hnsw_delta_prefix(db_id, schema.table_id, index.id);
            let end = hnsw_delta_prefix_end(db_id, schema.table_id, index.id);
            let range: BoundRange = (prefix.clone()..end).into();
            let pairs: Vec<_> = txn.scan(range, 1).await?.collect();
            let Some(delta_key) = pairs.iter().find_map(|pair| {
                let key: &[u8] = pair.key().as_ref().into();
                key.starts_with(&prefix).then(|| key.to_vec())
            }) else {
                continue;
            };

            let dirty_key = hnsw_dirty_key(db_id, schema.table_id, index.id);
            if txn.get_for_update(dirty_key.clone()).await?.is_none() {
                txn_put(txn, dirty_key, delta_key).await?;
            }
        }
    }

    if let Some(cursor) = next_cursor {
        txn_put(txn, cursor_key, cursor).await?;
        if done_value.is_some() {
            txn_delete(txn, done_key).await?;
        }
    } else {
        let next_due_ms = next_hnsw_dirty_backfill_due_ms(fallback_interval_sec);
        txn_put(
            txn,
            done_key,
            encode_hnsw_dirty_backfill_due_ms(next_due_ms),
        )
        .await?;
        txn_delete(txn, cursor_key).await?;
    }

    Ok(true)
}

/// Scan dirty HNSW indexes in (keyspace, db_id), then enqueue merge tasks for
/// markers that still have pending deltas. This function does NOT filter by any
/// registry bit; callers decide whether the database has HNSW work registered.
pub(crate) async fn enqueue_pending_hnsw_merges(
    system_store: &TikvStore,
    store: &Arc<TikvStore>,
    keyspace: &str,
    db_id: u64,
    dirty_start_after: Option<&[u8]>,
    dirty_page_size: usize,
    compatibility_backfill_interval_sec: u64,
) -> Result<HnswSweepResult> {
    use crate::sql::hnsw::storage::{
        hnsw_delta_prefix, hnsw_delta_prefix_end, hnsw_dirty_prefix, hnsw_dirty_prefix_end,
        hnsw_merge_task_id, hnsw_meta_key, parse_hnsw_dirty_key, HnswMeta,
    };
    use crate::txn::txn_delete;
    use rand::Rng;
    use tikv_client::BoundRange;

    // Retry the entire scan+enqueue with a fresh transaction on region errors
    // (RegionNotFound, EpochNotMatch, etc.) that occur after TiKV region
    // split/merge. Each retry starts a new snapshot so the tikv-client region
    // cache is refreshed. Fixes #2271.
    for attempt in 0..=REGION_ERROR_MAX_RETRIES {
        let result: Result<HnswSweepResult> = async {
            let mut txn = store.begin().await?;
            // This snapshot scans the compact dirty-marker set for one database.
            // Register it with the GC safepoint so GC does not advance past the
            // snapshot while the sweep runs.
            let mut txn_guard = crate::worker::active_txn_registry::global_registry()
                .map(|registry| registry.track_worker_txn(txn.start_timestamp().version()));

            // Cross-store liveness fence (same rationale as reconcile_cron_for_db):
            // merge tasks read here from the TENANT store but are enqueued into
            // the global system_store queue, so we cannot fence the enqueue in a
            // single txn. Take get_for_update on the tenant DB row in this scan
            // snapshot so a DROP DATABASE that already removed the metadata makes
            // the sweep bail with no merges enqueued. Any orphan from the
            // irreducible cross-store window is self-healing: execute_task skips
            // a merge for a dropped DB and cleanup deletes the queue row.
            if !store.database_alive_for_update(&mut txn, db_id).await? {
                if txn.rollback().await.is_err() {
                    if let Some(g) = txn_guard.as_mut() {
                        g.quarantine();
                    }
                }
                return Ok(HnswSweepResult {
                    observed: 0,
                    enqueued: 0,
                    enqueue_errors: 0,
                    next_dirty_cursor: None,
                });
            }

            let dirty_prefix = hnsw_dirty_prefix(db_id);
            let dirty_end = hnsw_dirty_prefix_end(db_id);
            let dirty_start = match dirty_start_after {
                Some(last_key) => {
                    let mut next = last_key.to_vec();
                    next.push(0);
                    next
                }
                None => dirty_prefix.clone(),
            };
            let dirty_page_limit = dirty_page_size.max(1).min(u32::MAX as usize) as u32;
            let dirty_range: BoundRange = (dirty_start..dirty_end).into();
            let dirty_pairs: Vec<_> = txn.scan(dirty_range, dirty_page_limit).await?.collect();
            let dirty_page_len = dirty_pairs.len();
            let mut last_dirty_key = None;
            let mut mutated = false;
            let mut observed = 0u32;
            let mut enqueued = 0u32;
            let mut enqueue_errors = 0u32;

            for pair in dirty_pairs {
                let key: &[u8] = pair.key().as_ref().into();
                let dirty_key = key.to_vec();
                last_dirty_key = Some(dirty_key.clone());
                if !dirty_key.starts_with(&dirty_prefix) {
                    continue;
                }
                let dirty_marker_value = pair.value().to_vec();
                let Some((table_id, index_id)) = parse_hnsw_dirty_key(db_id, &dirty_key) else {
                    continue;
                };

                let mut delete_stale_dirty_marker = false;
                let mk = hnsw_meta_key(db_id, table_id, index_id);
                match txn.get(mk).await? {
                    Some(meta_bytes) => match serde_json::from_slice::<HnswMeta>(&meta_bytes) {
                        Ok(meta) => {
                            if should_skip_frozen_merge(&meta) {
                                continue;
                            }
                            if meta.dropped_at.is_some() {
                                delete_stale_dirty_marker = true;
                            }
                        }
                        Err(e) => {
                            warn!(
                                table_id,
                                index_id,
                                "HNSW sweep: skipping dirty marker with undecodable meta: {}",
                                e
                            );
                            continue;
                        }
                    },
                    None => {
                        delete_stale_dirty_marker = true;
                    }
                }

                if !delete_stale_dirty_marker {
                    let prefix = hnsw_delta_prefix(db_id, table_id, index_id);
                    let end = hnsw_delta_prefix_end(db_id, table_id, index_id);
                    let range: BoundRange = (prefix.clone()..end).into();
                    let pairs: Vec<_> = txn.scan(range, 1).await?.collect();
                    let has_delta = pairs.iter().any(|pair| {
                        let key: &[u8] = pair.key().as_ref().into();
                        key.starts_with(&prefix)
                    });
                    if !has_delta {
                        delete_stale_dirty_marker = true;
                    }
                }

                if delete_stale_dirty_marker {
                    if txn.get_for_update(dirty_key.clone()).await? == Some(dirty_marker_value) {
                        txn_delete(&mut txn, dirty_key).await?;
                        mutated = true;
                    }
                    continue;
                }

                observed += 1;

                // Deltas found -> enqueue merge task.
                let task_id = match hnsw_merge_task_id(table_id, index_id) {
                    Ok(id) => id,
                    Err(e) => {
                        warn!(
                            "HNSW sweep: task_id overflow for table_id={} index_id={}: {}",
                            table_id, index_id, e
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
                    format!("__hnsw_merge {} {}", table_id, index_id),
                    "system".to_string(),
                    192,
                );
                entry.nonce = rand::thread_rng().gen_range(1..=u64::MAX);

                let enqueue_result: Result<bool> = async {
                    let mut sys_txn = system_store.begin().await?;
                    let enqueued = system_store
                        .enqueue_singleton_task_v2_unless_db_dropped(&mut sys_txn, &entry, 0)
                        .await?;
                    sys_txn.commit().await?;
                    Ok(enqueued)
                }
                .await;

                match enqueue_result {
                    Ok(true) => enqueued += 1,
                    Ok(false) => {
                        debug!(
                            "HNSW sweep: merge already pending or database dropped for table_id={} index_id={}",
                            table_id, index_id
                        );
                    }
                    Err(e) => {
                        enqueue_errors += 1;
                        warn!(
                            "HNSW sweep: failed to enqueue merge for table_id={} index_id={}: {}",
                            table_id, index_id, e
                        );
                    }
                }
            }

            mutated |= backfill_hnsw_dirty_markers_page(
                store,
                &mut txn,
                db_id,
                dirty_page_size,
                compatibility_backfill_interval_sec,
            )
            .await?;

            let next_dirty_cursor =
                if dirty_page_len < dirty_page_limit as usize || last_dirty_key.is_none() {
                    None
                } else {
                    last_dirty_key
                };

            if mutated {
                if txn.commit().await.is_err() {
                    if let Some(g) = txn_guard.as_mut() {
                        g.quarantine();
                    }
                    return Err(anyhow!(
                        "HNSW sweep: failed to commit stale dirty marker cleanup"
                    ));
                }
            } else if txn.rollback().await.is_err() {
                if let Some(g) = txn_guard.as_mut() {
                    g.quarantine();
                }
            }
            Ok(HnswSweepResult {
                observed,
                enqueued,
                enqueue_errors,
                next_dirty_cursor,
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

/// True if `e` is the claim-lease cancellation / shutdown error (raised by
/// `LeaseCancel::bail_if_cancelled` and `run_with_guards`). A CIC backfill that
/// aborts because it LOST its claim must NOT be marked Invalid — the index stays
/// in a valid intermediate state for the new owner to resume.
fn is_claim_cancelled_error(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|cause| cause.to_string().contains(CANCELLED_BY_ADMIN_ERROR))
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

/// Execute a storage size scan for a single database.
///
/// Uses PD Region stats over DB9's encoded database key range. PD returns a
/// whole-Region physical/MVCC estimate in MiB; this path intentionally does not
/// provide exact data/index/table breakdowns and does not fall back to a tenant
/// KV scan on PD failure.
async fn execute_storage_size_scan(
    store: &Arc<TikvStore>,
    keyspace: &str,
    db_id: u64,
    pd_endpoints: &[String],
    pd_rate_limit_ms: u64,
    lease_cancel: &crate::worker::LeaseCancel,
) -> Result<()> {
    use crate::storage_stats::{
        global_storage_stats_cache, serialize_storage_stats, DbStorageStats,
    };

    let scan_start = std::time::Instant::now();
    crate::worker::pd_region_stats::enforce_pd_stats_rate_limit(pd_rate_limit_ms).await;
    let pd = match crate::worker::pd_region_stats::fetch_database_region_stats(
        pd_endpoints,
        keyspace,
        db_id,
    )
    .await
    {
        Ok(pd) => pd,
        Err(e) => {
            metrics::counter!(
                "db9_server_worker_storage_pd_region_stats_total",
                "result" => "err",
            )
            .increment(1);
            warn!(
                keyspace,
                db_id,
                "PD Region storage stats failed; leaving previous storage stats intact: {}",
                e
            );
            return Err(e.context("PD Region storage stats failed"));
        }
    };

    let scan_duration_ms = scan_start.elapsed().as_millis() as i64;
    let scanned_at_ms = now_epoch_ms();

    let stats = DbStorageStats::pd_region_estimate(
        db_id,
        pd.stats.total_bytes_estimate(),
        pd.stats.count,
        pd.stats.empty_count,
        pd.stats.storage_keys,
        scanned_at_ms,
        scan_duration_ms,
    );

    // Lease fence before the only tenant write in this path: if the claim lease
    // was lost/stolen during the PD lookup / rate-limit wait, abandon
    // the stats persist so the new owner can re-run it. Returning here leaves the
    // task for the new owner exactly as the generic run_with_guards path bails.
    lease_cancel.bail_if_cancelled()?;

    let stats_key = crate::storage::encode_storage_stats_key_v2(db_id);
    let stats_value = serialize_storage_stats(&stats);
    let mut persist_txn = store.begin().await?;
    crate::txn::txn_put(&mut persist_txn, stats_key, stats_value).await?;
    store
        .assert_database_alive_for_update(&mut persist_txn, db_id)
        .await?;
    persist_txn.commit().await?;

    global_storage_stats_cache().put(keyspace, db_id, stats);
    metrics::counter!(
        "db9_server_worker_storage_pd_region_stats_total",
        "result" => "ok",
    )
    .increment(1);

    info!(
        db_id,
        keyspace,
        keyspace_id = pd.keyspace_id,
        region_count = pd.stats.count,
        empty_region_count = pd.stats.empty_count,
        storage_size_mib = pd.stats.storage_size_mib,
        storage_keys = pd.stats.storage_keys,
        total_bytes_estimate = pd.stats.total_bytes_estimate(),
        scan_duration_ms,
        "Storage size PD Region estimate complete"
    );

    Ok(())
}

/// Enqueue a storage size scan task for a specific database.
///
/// Called by `db9_refresh_storage_stats()` and by the bounded registry sweep.
pub(crate) async fn enqueue_storage_scan(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
) -> Result<()> {
    enqueue_storage_scan_at(system_store, keyspace, db_id, 0).await
}

async fn enqueue_storage_scan_with_jitter(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    jitter_sec: u64,
) -> Result<()> {
    use rand::Rng;

    let delay_ms = if jitter_sec == 0 {
        0
    } else {
        rand::thread_rng().gen_range(0..=jitter_sec.saturating_mul(1000))
    };
    let fire_time = now_epoch_ms().saturating_add(i64::try_from(delay_ms).unwrap_or(i64::MAX));
    enqueue_storage_scan_at(system_store, keyspace, db_id, fire_time).await
}

async fn enqueue_storage_scan_at(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    fire_time: i64,
) -> Result<()> {
    use rand::Rng;

    let mut entry = TaskQueueEntry::new(
        keyspace.to_string(),
        db_id,
        db_id as i64,
        TaskType::StorageSizeScan,
        String::new(),
        "system".to_string(),
        200, // low priority — background housekeeping
    );
    entry.nonce = rand::thread_rng().gen_range(1..=u64::MAX);
    let mut txn = system_store.begin().await?;
    if !system_store
        .enqueue_singleton_task_v2_unless_db_dropped(&mut txn, &entry, fire_time)
        .await?
    {
        txn.rollback().await.ok();
        debug!(
            keyspace,
            db_id, "Storage size scan already pending or database dropped; skipping enqueue"
        );
        return Ok(());
    }
    txn.commit().await?;
    crate::worker::wake_worker();
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
