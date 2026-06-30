use super::*;

/// Pre-create a TENANT keyspace in PD before the pool's `with_keyspace` connect.
/// The vendored tikv-client does NOT auto-create keyspaces, so `pool.acquire`
/// for a fresh keyspace fails with "keyspace does not exist" unless it is
/// provisioned first. Mirrors what `init_gc_registry_store` does for the system
/// keyspace. Used by every TiKV-backed engine test that acquires a tenant store.
#[cfg(test)]
async fn ensure_tenant_keyspace_for_test(pd_endpoints: &[String], keyspace: &str) {
    crate::worker::ensure_system_keyspace(pd_endpoints, keyspace)
        .await
        .expect("pre-create tenant keyspace in PD");
}

fn test_cron_job() -> crate::cron::types::CronJob {
    crate::cron::types::CronJob {
        job_id: 42,
        schedule: "0 0 1 1 *".to_string(),
        command: "SELECT 1".to_string(),
        nodename: String::new(),
        nodeport: 0,
        database: "postgres".to_string(),
        username: "admin".to_string(),
        active: true,
        jobname: Some("codex".to_string()),
        max_runtime_ms: None,
    }
}

fn test_cron_queue_entry(job: &crate::cron::types::CronJob) -> TaskQueueEntry {
    TaskQueueEntry::new(
        "tenant_a".to_string(),
        1,
        job.job_id,
        TaskType::Cron,
        job.command.clone(),
        job.username.clone(),
        128,
    )
    .with_schedule(job.schedule.clone())
}

#[test]
fn cron_queue_entry_must_match_current_catalog_job() {
    let job = test_cron_job();
    let entry = test_cron_queue_entry(&job);
    assert!(super::cron_queue_entry_matches_job(&entry, &job));

    let mut changed_command = job.clone();
    changed_command.command = "SELECT 2".to_string();
    assert!(!super::cron_queue_entry_matches_job(
        &entry,
        &changed_command
    ));

    let mut changed_schedule = job.clone();
    changed_schedule.schedule = "0 0 2 1 *".to_string();
    assert!(!super::cron_queue_entry_matches_job(
        &entry,
        &changed_schedule
    ));

    let mut changed_owner = job.clone();
    changed_owner.username = "other_user".to_string();
    assert!(!super::cron_queue_entry_matches_job(&entry, &changed_owner));
}

#[test]
fn already_claimed_cron_minute_deletes_duplicate_due_row() {
    use crate::cron::types::{CronRun, CronRunStatus};
    use crate::storage::CronClaimOutcome;
    let dummy_run = || CronRun {
        run_id: 1,
        job_id: 1,
        job_pid: None,
        database: "db".to_string(),
        username: "u".to_string(),
        command: "SELECT 1".to_string(),
        status: CronRunStatus::Running,
        return_message: None,
        start_time: Some(0),
        end_time: None,
    };
    assert_eq!(
        super::keep_queue_entry_for_claim_status(&CronClaimOutcome::Claimed { run: dummy_run() }),
        None
    );
    assert_eq!(
        super::keep_queue_entry_for_claim_status(&CronClaimOutcome::TookOver { run: dummy_run() }),
        None,
        "a takeover owns the run and must proceed, not drop/keep the row"
    );
    assert_eq!(
        super::keep_queue_entry_for_claim_status(&CronClaimOutcome::AlreadyTerminalForMinute),
        Some(false),
        "a fire that already reached terminal state must drop the due row, not retry forever"
    );
    assert_eq!(
        super::keep_queue_entry_for_claim_status(&CronClaimOutcome::BlockedByLiveActive),
        Some(true),
        "a live run (no-overlap) should keep the due row so it can retry after the active run"
    );
    // A blocked-but-folded claim has the SAME queue disposition (keep + retry) as a
    // plain block — only the txn commit decision differs (it carries durable fold
    // writes the reaper must reap, signalled separately via `must_commit_blocked`).
    assert_eq!(
        super::keep_queue_entry_for_claim_status(&CronClaimOutcome::BlockedByLiveActiveFolded),
        Some(true),
        "a blocked-but-folded straggler keeps the due row to retry, same as a plain block"
    );
}

/// The fold-commit signal (P1 fold-commit fix): only the `BlockedByLiveActiveFolded`
/// outcome — the straggler-fold block path that wrote durable ACTIVE/CONTROL — asks
/// the caller to COMMIT its txn. Every other outcome (plain block, terminal dedup,
/// or a yielded run handled on the run-commit path) must NOT request a blocked
/// commit, so a plain block with no writes still rolls back.
#[test]
fn only_folded_block_requests_commit() {
    use crate::cron::types::{CronRun, CronRunStatus};
    let dummy_run = || CronRun {
        run_id: 1,
        job_id: 1,
        job_pid: None,
        database: "db".to_string(),
        username: "u".to_string(),
        command: "SELECT 1".to_string(),
        status: CronRunStatus::Running,
        return_message: None,
        start_time: Some(0),
        end_time: None,
    };
    assert!(
        CronClaimOutcome::BlockedByLiveActiveFolded.must_commit_blocked(),
        "a folded straggler block must commit so the orphan becomes reapable"
    );
    assert!(
        !CronClaimOutcome::BlockedByLiveActive.must_commit_blocked(),
        "a plain block wrote nothing and must roll back"
    );
    assert!(
        !CronClaimOutcome::AlreadyTerminalForMinute.must_commit_blocked(),
        "a terminal dedup outcome wrote nothing and must roll back"
    );
    assert!(
        !CronClaimOutcome::Claimed { run: dummy_run() }.must_commit_blocked(),
        "a claimed run is committed on the run path, not the blocked-commit path"
    );
    assert!(
        !CronClaimOutcome::TookOver { run: dummy_run() }.must_commit_blocked(),
        "a takeover run is committed on the run path, not the blocked-commit path"
    );
}

#[test]
fn cron_claim_stale_check_precedes_same_minute_claim() {
    let source = include_str!("../engine.rs");
    let fn_body = source
        .split("async fn claim_and_record_cron_run(")
        .nth(1)
        .and_then(|rest| rest.split("async fn load_next_cron_queue_entry(").next())
        .expect("claim_and_record_cron_run must exist before load_next_cron_queue_entry");

    let job_lookup = fn_body
        .find(".get_cron_job(")
        .expect("cron claim path must load the catalog job");
    let stale_check = fn_body
        .find("cron_queue_entry_matches_job")
        .expect("cron claim path must reject stale queue payloads");
    let claim = fn_body
        .find(".claim_or_takeover_cron_run(")
        .expect("cron claim path must claim the scheduled minute via the fence CAS");

    assert!(
        job_lookup < claim && stale_check < claim,
        "stale cron entries must be rejected before the claim CAS runs"
    );
}

#[test]
fn background_statement_extension_context_uses_fresh_statement_state_per_call() {
    let first = background_statement_extension_context(true, "tenant_a", "admin", None);
    let second = background_statement_extension_context(true, "tenant_a", "admin", None);
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

    let interactive = background_statement_extension_context(false, "tenant_a", "alice", None);
    assert_eq!(
        interactive.execution_kind,
        crate::extensions::context::ExecutionKind::Interactive
    );
    // Cron path stays role-less (can't reach JuiceFS by design), interactive
    // workers (AsyncTrigger / BgSql) carry the originating username so
    // `init_juicefs_backend` can derive scp.
    assert_eq!(first.authenticated_role, None);
    assert_eq!(interactive.authenticated_role.as_deref(), Some("alice"));
}

#[test]
fn should_start_cic_backfill_only_when_building() {
    assert!(should_start_cic_backfill(IndexState::Building));
    assert!(!should_start_cic_backfill(IndexState::Ready));
    assert!(!should_start_cic_backfill(IndexState::Invalid));
    assert!(!should_start_cic_backfill(IndexState::WriteOnly));
}

#[test]
fn cic_repair_checks_pending_bgddl_before_invalidating() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let repair_fn = prod_source
        .split("async fn reconcile_incomplete_cic_indexes_for_db_safe(")
        .nth(1)
        .and_then(|rest| rest.split("async fn storage_scan_due").next())
        .expect("queue-aware CIC repair must exist before storage_scan_due");

    assert!(
        repair_fn.contains("task_has_pending"),
        "CIC repair must not invalidate Building/WriteOnly indexes with a pending BgDdl task"
    );
    assert!(
        repair_fn.contains("TaskType::BgDdl"),
        "CIC pending check must target BgDdl queue entries"
    );
    assert!(
        repair_fn.contains(".scan_tables_page(") && !repair_fn.contains(".list_tables("),
        "CIC repair must use bounded table pages, not a full database table scan"
    );
    assert!(
        repair_fn.contains("table_start_after") && repair_fn.contains("table_page_size"),
        "CIC repair must accept a raw-key cursor and page size from sweep state"
    );
}

#[test]
fn registry_sweep_backoff_new_failed_sets_retry_in_future() {
    let b = RegistrySweepBackoff::new_failed(600);
    assert_eq!(b.consecutive_failures, 1);
    assert!(
        b.should_skip(),
        "should skip immediately after first failure"
    );
}

#[test]
fn registry_sweep_backoff_escalates_and_caps() {
    let mut b = RegistrySweepBackoff::new_failed(10);
    assert_eq!(b.consecutive_failures, 1);

    b.record_failure(10);
    assert_eq!(b.consecutive_failures, 2);
    b.record_failure(10);
    assert_eq!(b.consecutive_failures, 3);

    for _ in 0..20 {
        b.record_failure(10);
    }
    assert_eq!(b.consecutive_failures, 23);
    assert!(b.should_skip());
}

#[test]
fn registry_sweep_backoff_should_skip_returns_false_after_delay() {
    let b = RegistrySweepBackoff {
        consecutive_failures: 1,
        retry_after: Instant::now() - Duration::from_secs(1),
    };
    assert!(
        !b.should_skip(),
        "should not skip when retry_after is in the past"
    );
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

    let deadline = tokio::time::Instant::now() + Duration::from_millis(5);
    let err = run_with_guards(fut, Some(deadline), Some(&cancel), None)
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

    let deadline = tokio::time::Instant::now() + Duration::from_millis(5);
    let err = run_with_guards(fut, Some(deadline), None, None)
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
    let source = include_str!("../engine.rs");
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
        execute_task_source.contains("let task_deadline ="),
        "execute_task must compute a task-scoped deadline"
    );
    assert!(
        execute_task_source.contains("let result = fut.await?;"),
        "individual statements must execute without per-statement timeout wrapping while preserving the statement result"
    );
    assert!(
        execute_task_source.contains("run_with_guards(")
            && execute_task_source.contains("task_deadline,")
            && execute_task_source.contains("cancel_signal.as_ref(),")
            && execute_task_source.contains("shutdown_signal.as_ref(),"),
        "execute_task must wrap the whole task future in run_with_guards"
    );
    assert!(
        !execute_task_source.contains("run_with_guards(fut, stmt_timeout"),
        "execute_task must not apply timeout per statement"
    );
}

/// Class-level guard for the claim-lease cancellation fix: EVERY specialized
/// task path that returns BEFORE `run_with_guards` (and therefore bypasses the
/// only other place that threads `shutdown_signal`) must still thread the
/// lease-cancellation token into its body. If a new early-return specialized
/// path is added without `lease_cancel`, this fails — keeping the whole class
/// covered, not just the three current paths.
#[test]
fn specialized_task_paths_thread_lease_cancel() {
    let source = include_str!("../engine.rs");
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
    let dispatch = &prod_source[execute_task_start..execute_bg_ddl_start];

    // The shared token is constructed from shutdown_signal once.
    assert!(
        dispatch.contains(
            "let lease_cancel = crate::worker::LeaseCancel::new(shutdown_signal.clone());"
        ),
        "execute_task must construct a LeaseCancel from shutdown_signal for the specialized paths"
    );
    // Each of the three specialized dispatch calls must forward it.
    assert!(
        dispatch.contains("execute_storage_size_scan(")
            && dispatch.contains("&store")
            && dispatch.contains("pool.pd_endpoints()")
            && dispatch.contains("config.storage_scan_pd_rate_limit_ms")
            && dispatch.contains("&lease_cancel"),
        "StorageSizeScan path must thread lease_cancel"
    );
    assert!(
        dispatch
            .contains("execute_hnsw_merge(&store, entry.db_id, table_id, index_id, &lease_cancel)"),
        "HnswMerge path must thread lease_cancel"
    );
    assert!(
        dispatch.contains("Self::execute_bg_ddl_backfill(&store, entry, &lease_cancel)"),
        "BgDdl backfill path must thread lease_cancel"
    );
}

/// A CIC backfill that aborts because it LOST its claim lease must NOT be marked
/// Invalid — the index stays in a valid intermediate state for the new owner to
/// resume. This locks that branch: every phase routes a cancellation error past
/// `mark_invalid` via `is_claim_cancelled_error`.
#[test]
fn cancelled_backfill_is_not_marked_invalid() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let start = prod_source
        .find("async fn execute_bg_ddl_backfill(")
        .expect("execute_bg_ddl_backfill must exist");
    // Bound the slice at the end of the impl block region (next free fn / struct).
    let body = &prod_source[start..];

    // All three phases must short-circuit a cancellation error before mark_invalid.
    let guards = body
        .matches("Err(e) if is_claim_cancelled_error(&e) => return Err(e),")
        .count();
    assert!(
        guards >= 3,
        "each CIC phase must bypass mark_invalid on a claim-cancelled error (found {guards})"
    );
    // And the helper must exist with the right intent.
    assert!(
        prod_source.contains("fn is_claim_cancelled_error("),
        "engine.rs must define is_claim_cancelled_error"
    );
}

#[test]
fn cron_timeout_uses_absolute_deadline_through_to_run_with_guards() {
    // The cron deadline (Instant) must flow from the caller all the way
    // into run_with_guards, which uses timeout_at (absolute) instead of
    // timeout (relative Duration).  This ensures:
    //  1. The timeout fires at the correct wall-clock instant regardless
    //     of preamble duration (pool.acquire, DB lookup, store.begin).
    //  2. There is no outer timeout_at that could race and drop the future
    //     before the inner rollback path runs.
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");

    // execute_task must accept a deadline (Instant), not a Duration
    assert!(
        prod_source.contains("cron_deadline: Option<tokio::time::Instant>"),
        "execute_task must accept cron_deadline as Option<tokio::time::Instant>"
    );

    // run_with_guards must accept a deadline (Instant) and use timeout_at
    assert!(
        prod_source.contains("deadline: Option<tokio::time::Instant>"),
        "run_with_guards must accept deadline as Option<tokio::time::Instant>"
    );
    assert!(
        prod_source.contains("tokio::time::timeout_at(dl, fut)"),
        "run_with_guards must use timeout_at (absolute), not timeout (relative)"
    );

    // No outer timeout_at wrapping execute_task — that would race and
    // drop the future before rollback can run
    let claim_fn = prod_source
        .split("claim_and_execute_core")
        .nth(2) // skip the call site, get the definition body
        .and_then(|rest| rest.split("async fn claim_and_record_cron_run(").next())
        .expect("claim_and_execute_core must exist");
    assert!(
        !claim_fn.contains("timeout_at("),
        "claim_and_execute_core must NOT wrap execute_task in timeout_at — \
         the deadline flows into run_with_guards which handles timeout + rollback"
    );
}

#[test]
fn worker_engine_shutdown_is_wired_into_run_loop_and_task_guards() {
    let source = include_str!("../engine.rs");
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
    // Worker task execution must propagate shutdown cancellation to running
    // tasks. The engine shutdown token is threaded into a per-task child token
    // (`exec_shutdown = shutdown_signal.child_token()`) so that BOTH an engine
    // shutdown AND a lost-lease cancellation abort the running task; the child
    // token is what execute_task receives.
    assert!(
        prod_source.contains("let exec_shutdown = shutdown_signal.child_token();"),
        "per-task shutdown must be a child of the engine shutdown token so engine \
         shutdown still propagates to running tasks"
    );
    // Both the cron and the non-cron execute_task branches must receive the
    // lease-aware exec_shutdown token. Assert on the token threading (count of
    // occurrences) rather than exact argument whitespace: rustfmt re-wraps the
    // execute_task call across lines depending on the length of nearby code, so
    // a single-line substring like `Some(exec_shutdown.clone()), None)` is
    // brittle and breaks on unrelated edits.
    assert!(
        prod_source.matches("Some(exec_shutdown.clone())").count() >= 2,
        "worker task execution must run under the lease-aware exec_shutdown token \
         in both the cron and non-cron execute_task branches"
    );
}

#[test]
fn cron_sweep_reconciliation_does_not_scan_legacy_worker_queue() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let reconcile_fn = prod_source
        .split("async fn reconcile_cron_for_db")
        .nth(1)
        .and_then(|rest| rest.split("async fn reconcile_ddl_journal_for_db").next())
        .expect("reconcile_cron_for_db must exist before DDL journal reconciliation");

    assert!(
        reconcile_fn.contains(".index_rows_for_db_type("),
        "cron sweep reconciliation must use the bounded V2 identity index"
    );
    assert!(
        !reconcile_fn.contains(".legacy_entries_for_db_type("),
        "cron sweep reconciliation must not scan legacy _worker_queue_"
    );
    assert!(
        !reconcile_fn.contains(".delete_worker_queue_entry("),
        "cron sweep reconciliation must not delete legacy entries by scanning _worker_queue_"
    );
}

#[test]
fn worker_tick_dequeues_v2_only() {
    let source = include_str!("../engine.rs");
    let tick_fn = source
        .split("async fn tick(&self)")
        .nth(1)
        .and_then(|rest| rest.split("async fn worker_db_disabled").next())
        .expect("tick must exist before worker_db_disabled");

    assert!(
        tick_fn.contains(".scan_due_v2("),
        "worker tick must scan the V2 due queue"
    );
    assert!(
        !tick_fn.contains("scan_due_legacy_bytesafe")
            && !tick_fn.contains("legacy_queue_has_entries")
            && !tick_fn.contains("DueItem::Legacy"),
        "worker tick must not dual-read legacy _worker_queue_ rows"
    );
}

#[test]
fn worker_tick_does_not_wait_for_dispatched_tasks() {
    let source = include_str!("../engine.rs");
    let tick_fn = source
        .split("async fn tick(&self)")
        .nth(1)
        .and_then(|rest| rest.split("async fn registry_sweep_tick").next())
        .expect("tick must exist before registry_sweep_tick");

    assert!(
        tick_fn.contains("tokio::spawn(async move"),
        "worker tick must dispatch due tasks asynchronously"
    );
    assert!(
        !tick_fn.contains("join_next") && !tick_fn.contains("JoinSet"),
        "worker tick must not await task completion; long HNSW/DDL jobs must not block BgSql wakeups"
    );
    assert!(
        tick_fn.contains("crate::worker::wake_worker();"),
        "completed tasks must wake the worker so queued overflow does not wait for the poll interval"
    );
    assert!(
        tick_fn.contains("backlog_wakeup.swap(false, Ordering::Relaxed)"),
        "worker tick must gate completion wakeups on observed queue backlog"
    );
    assert!(
        tick_fn.contains("self.backlog_wakeup.store(true, Ordering::Relaxed)"),
        "worker tick must mark backlog only when concurrency slots are exhausted"
    );
}

#[test]
fn storage_size_scan_uses_pd_region_stats_not_tenant_kv_scan() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    // The PD fetch + estimate live in the shared `compute_pd_region_stats`
    // helper (called by both the legacy and derived executors), so anchor there.
    let scan_start = prod_source
        .find("async fn compute_pd_region_stats(")
        .expect("compute_pd_region_stats must exist");
    let scan_end = prod_source[scan_start..]
        .find("fn log_storage_scan_complete(")
        .map(|offset| scan_start + offset)
        .expect("compute_pd_region_stats must appear before the completion-log helper");
    let scan_source = &prod_source[scan_start..scan_end];

    assert!(
        scan_source.contains("fetch_database_region_stats("),
        "storage size scan must fetch PD Region stats"
    );
    assert!(
        scan_source.contains("DbStorageStats::pd_region_estimate("),
        "storage size scan must build an explicit PD estimate"
    );
    assert!(
        !scan_source.contains(".scan(")
            && !scan_source.contains("begin_optimistic()")
            && !scan_source.contains("track_worker_txn("),
        "PD storage stats path must not scan tenant KV pages or hold a long-lived snapshot"
    );
}

#[test]
fn storage_scan_sweep_uses_interval_due_with_jitter_not_dirty_marker() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let entry_fn = prod_source
        .split("async fn process_registry_sweep_entry(")
        .nth(1)
        .and_then(|rest| {
            rest.split("async fn record_registry_sweep_kind_result")
                .next()
        })
        .expect("process_registry_sweep_entry must exist before result recorder");

    let due_pos = entry_fn
        .find("storage_scan_due")
        .expect("storage sweep must keep interval fallback");
    let enqueue_pos = entry_fn
        .find("enqueue_storage_scan_with_jitter")
        .expect("storage sweep must enqueue due scans with jitter");
    assert!(
        due_pos < enqueue_pos,
        "storage sweep must decide due before enqueueing with jitter"
    );
    assert!(
        !entry_fn.contains("get_storage_size_dirty_marker"),
        "storage sweep must not read dirty markers"
    );
}

#[test]
fn storage_scan_failure_backoff_is_recovery_not_refresh_cadence() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let backoff = prod_source
        .split("fn registry_sweep_kind_backoff_interval")
        .nth(1)
        .and_then(|rest| {
            rest.split("async fn registry_sweep_db_missing_should_delete")
                .next()
        })
        .expect("registry_sweep_kind_backoff_interval must exist before DB-missing helper");

    assert!(
        backoff.contains("RegistrySweepKind::StorageScan => REGISTRY_SWEEP_RECOVERY_BACKOFF_SEC"),
        "StorageScan failure backoff must be a recovery retry, not the normal refresh cadence"
    );
    assert!(
        !backoff.contains("RegistrySweepKind::StorageScan => self.config.storage_scan_interval_sec"),
        "storage_scan_interval_sec is the refresh cadence and must not be reused as failure retry backoff"
    );
}

#[test]
fn storage_scan_active_sweep_claims_derived_state_not_v2_queue() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let entry_fn = prod_source
        .split("async fn process_registry_sweep_entry(")
        .nth(1)
        .and_then(|rest| {
            rest.split("async fn record_registry_sweep_kind_result")
                .next()
        })
        .expect("process_registry_sweep_entry must exist before result recorder");

    let active = entry_fn
        .find("self.config.storage_scan_derived_active")
        .expect("storage scan sweep must gate derived mode with the active flag");
    let derived = entry_fn
        .find("run_storage_scan_derived_if_due")
        .expect("active storage scan sweep must claim derived state");
    let legacy = entry_fn
        .find("enqueue_storage_scan_with_jitter")
        .expect("inactive storage scan sweep must retain legacy V2 fallback");

    assert!(
        active < derived && derived < legacy,
        "active derived StorageSizeScan must return before legacy V2 enqueue fallback"
    );
    assert!(
        entry_fn.contains("return Ok::<(), anyhow::Error>(());"),
        "active derived StorageSizeScan branch must not fall through to V2 enqueue"
    );
}

#[test]
fn legacy_v2_storage_scan_converts_to_derived_refresh_when_active() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let branch = prod_source
        .split("if entry.task_type == TaskType::StorageSizeScan {")
        .nth(1)
        .and_then(|rest| {
            rest.split("if entry.task_type == TaskType::HnswMerge")
                .next()
        })
        .expect("StorageSizeScan execute_task branch must exist before HNSW branch");

    let active = branch
        .find("config.storage_scan_derived_active")
        .expect("legacy StorageSizeScan execution must check derived active flag");
    let refresh = branch
        .find("request_storage_scan_refresh")
        .expect("active legacy StorageSizeScan must nudge derived state");
    let legacy = branch
        .find("execute_storage_size_scan(")
        .expect("inactive StorageSizeScan must keep legacy V2 executor");
    assert!(
        active < refresh && refresh < legacy,
        "active legacy V2 StorageSizeScan rows must be collapsed into derived state, not executed directly"
    );
}

#[test]
fn storage_scan_derived_claim_uses_and_repairs_capacity_tokens() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let claim = prod_source
        .split("async fn claim_storage_scan_derived_run(")
        .nth(1)
        .and_then(|rest| {
            rest.split("async fn finish_storage_scan_derived_run")
                .next()
        })
        .expect("claim_storage_scan_derived_run must exist before finish");
    let capacity = prod_source
        .split("async fn claim_storage_scan_capacity_token(")
        .nth(1)
        .and_then(|rest| rest.split("async fn claim_storage_scan_derived_run").next())
        .expect("capacity-token claim helper must exist before state claim");

    assert!(
        claim.contains("claim_storage_scan_capacity_token")
            && claim.contains("capacity_token_id: Some(token_id)"),
        "derived StorageSizeScan must reserve capacity before moving state to Running"
    );
    assert!(
        capacity.contains("storage_scan_capacity_token_is_live")
            && capacity.contains("put_storage_scan_capacity_token"),
        "capacity tokens must be repaired from authoritative state, not treated as permanent leases"
    );
}

#[test]
fn storage_scan_finish_releases_capacity_token_on_every_terminal_path() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let finish = prod_source
        .split("async fn finish_storage_scan_derived_run(")
        .nth(1)
        .and_then(|rest| rest.split("async fn process_registry_sweep_entry").next())
        .expect("finish_storage_scan_derived_run must exist before process_registry_sweep_entry");
    let release = finish
        .find("delete_storage_scan_capacity_token")
        .expect("finish must release the capacity token");
    let outcome_match = finish
        .find("match effect")
        .expect("finish must branch on effect outcome");

    assert!(
        release < outcome_match && finish.contains("capacity_token_id: None"),
        "finish must release capacity before success, stale-target, and retry state transitions"
    );
}

#[test]
fn storage_scan_derived_failure_retry_is_state_owned() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let finish = prod_source
        .split("async fn finish_storage_scan_derived_run(")
        .nth(1)
        .and_then(|rest| rest.split("async fn process_registry_sweep_entry").next())
        .expect("finish_storage_scan_derived_run must exist before process_registry_sweep_entry");
    let err_branch = finish
        .split("Err(e) => {")
        .nth(1)
        .expect("finish must handle effect errors");

    assert!(
        err_branch.contains("run_after_ms: storage_scan_retry_after_ms(now_ms, attempt)")
            && err_branch.contains("put_storage_scan_bg_state")
            && err_branch.contains("txn.commit().await?")
            && err_branch.contains("Ok(())"),
        "derived effect failures must persist retry state and return success to the registry sweep"
    );
}

#[test]
fn storage_scan_derived_pending_retry_blocks_auto_due_until_run_after() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let claim = prod_source
        .split("async fn claim_storage_scan_derived_run(")
        .nth(1)
        .and_then(|rest| {
            rest.split("async fn finish_storage_scan_derived_run")
                .next()
        })
        .expect("claim_storage_scan_derived_run must exist before finish");
    let retry_gate = claim
        .find("state.work_id > 0 && state.run_after_ms > now_ms")
        .expect("pending retry state must gate claim attempts");
    let stats_due_gate = claim
        .find("if !stats_due && !due_by_state")
        .expect("claim must still consider ordinary auto due state");

    assert!(
        retry_gate < stats_due_gate,
        "pending derived retry/manual state must be respected before automatic stats_due can claim"
    );
}

#[test]
fn storage_scan_derived_effect_validates_incarnation_and_applied_marker() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let effect = prod_source
        .split("async fn execute_storage_size_scan_derived(")
        .nth(1)
        .and_then(|rest| {
            rest.split("pub(crate) async fn request_storage_scan_refresh")
                .next()
        })
        .expect("derived StorageSizeScan effect must exist before refresh helper");

    assert!(
        effect
            .matches("get_tenant_incarnation_stamp_for_update")
            .count()
            >= 2,
        "derived effect must validate tenant incarnation before PD work and again at commit time"
    );
    assert!(
        effect.contains("get_storage_scan_applied_marker_for_update")
            && effect.contains("put_storage_scan_applied_marker"),
        "derived effect must use tenant-local applied markers for idempotent progress"
    );
    assert!(
        effect.contains("lock_storage_scan_owner_for_commit")
            && effect.contains("persist_txn.commit().await"),
        "derived effect must re-check scheduler ownership immediately before committing tenant effects"
    );
}

#[test]
fn storage_scan_capacity_token_liveness_requires_matching_running_state() {
    let token = StorageScanCapacityToken {
        keyspace: "ks".to_string(),
        db_id: 7,
        tenant_incarnation: 11,
        work_id: 21,
        attempt: 2,
        lease_until_ms: 100,
    };
    let state = StorageScanBgState {
        keyspace: "ks".to_string(),
        db_id: 7,
        tenant_incarnation: 11,
        status: StorageScanBgStateStatus::Running,
        work_id: 21,
        run_after_ms: 1,
        lease_until_ms: 100,
        attempt: 2,
        last_done_work_id: 20,
        capacity_token_id: Some(0),
    };

    assert!(storage_scan_capacity_token_matches_state(
        0, &token, &state, 99
    ));
    assert!(
        !storage_scan_capacity_token_matches_state(0, &token, &state, 100),
        "expired state leases must not keep capacity busy"
    );

    let mut wrong_token = token.clone();
    wrong_token.work_id = 22;
    assert!(
        !storage_scan_capacity_token_matches_state(0, &wrong_token, &state, 99),
        "stale capacity-token payload must be repairable from authoritative state"
    );

    let mut idle_state = state.clone();
    idle_state.status = StorageScanBgStateStatus::Idle;
    assert!(
        !storage_scan_capacity_token_matches_state(0, &token, &idle_state, 99),
        "idle state rows must not hold capacity"
    );
}

#[test]
fn storage_scan_success_advances_progress_after_completed_work() {
    let state = StorageScanBgState {
        keyspace: "ks".to_string(),
        db_id: 7,
        tenant_incarnation: 11,
        status: StorageScanBgStateStatus::Running,
        work_id: 21,
        run_after_ms: 1,
        lease_until_ms: 100,
        attempt: 3,
        last_done_work_id: 20,
        capacity_token_id: Some(0),
    };

    let idle = storage_scan_idle_after_success(state, 21, 1_000, 60);
    assert_eq!(idle.status, StorageScanBgStateStatus::Idle);
    assert_eq!(idle.work_id, 0);
    assert_eq!(idle.last_done_work_id, 21);
    assert_eq!(idle.run_after_ms, 61_000);
    assert_eq!(idle.attempt, 0);
    assert_eq!(idle.capacity_token_id, None);
}

#[test]
fn storage_size_scan_leaves_old_stats_intact_on_pd_failure() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    // PD fetch + its failure handling live in the shared `compute_pd_region_stats`
    // helper; persistence happens only in the callers AFTER it returns Ok, so a PD
    // failure returns Err before any serialize/persist can run.
    let scan_start = prod_source
        .find("async fn compute_pd_region_stats(")
        .expect("compute_pd_region_stats must exist");
    let scan_end = prod_source[scan_start..]
        .find("fn log_storage_scan_complete(")
        .map(|offset| scan_start + offset)
        .expect("compute_pd_region_stats must appear before the completion-log helper");
    let scan_source = &prod_source[scan_start..scan_end];

    assert!(
        scan_source
            .contains("PD Region storage stats failed; leaving previous storage stats intact"),
        "PD failure must be logged as leaving previous stats intact"
    );
    assert!(
        scan_source.contains("return Err") && !scan_source.contains("serialize_storage_stats("),
        "PD fetch/estimate stage must return the error before any stats serialization/persist \
         (persistence is the caller's job after a successful estimate)"
    );
}

#[test]
fn registry_sweep_uses_raw_key_cursor_pages() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let sweep_source = prod_source
        .split("async fn registry_sweep_tick(&self)")
        .nth(1)
        .and_then(|rest| rest.split("async fn finish_registry_sweep_cycle").next())
        .expect("registry_sweep_tick must exist before finish_registry_sweep_cycle");

    assert!(
        sweep_source.contains("scan_worker_registry_page"),
        "registry sweep must use storage-level raw-key cursor paging"
    );
    assert!(
        !prod_source.contains("registry_batch_from_cursor"),
        "registry sweep must not slice a freshly materialized full registry Vec"
    );
}

#[test]
fn registry_sweep_cycle_interval_is_independent_from_hnsw_interval() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let sweep_source = prod_source
        .split("async fn registry_sweep_tick(&self)")
        .nth(1)
        .and_then(|rest| rest.split("async fn finish_registry_sweep_cycle").next())
        .expect("registry_sweep_tick must exist before finish_registry_sweep_cycle");

    assert!(
        sweep_source.contains("self.config.registry_sweep_interval_sec"),
        "registry sweep cycle cadence must use its own config"
    );
    assert!(
        !sweep_source.contains("self.config.hnsw_sweep_interval_sec"),
        "registry sweep cycle cadence must not be coupled to HNSW maintenance"
    );
}

#[test]
fn worker_startup_does_not_run_unbounded_registry_fanout() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let startup_source = prod_source
        .split("pub async fn run(&self)")
        .nth(1)
        .and_then(|rest| rest.split("let mut interval =").next())
        .expect("WorkerEngine::run startup block must exist");

    assert!(
        !startup_source.contains("reconcile_ddl_journal"),
        "startup must not synchronously scan every registry entry for DDL journal recovery"
    );
    assert!(
        !startup_source.contains("reconcile_hnsw_merges"),
        "startup must not synchronously scan every registry entry for HNSW deltas"
    );
    assert!(
        !startup_source.contains("reconcile_storage_scans"),
        "startup must not synchronously enqueue storage scans for every registry entry"
    );
    assert!(
        !startup_source.contains("warm_load_storage_stats"),
        "startup must not warm-load storage stats by acquiring every tenant store"
    );
}

#[test]
fn registry_sweep_runs_outside_queue_poll_loop() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let run_fn = prod_source
        .split("pub async fn run(&self)")
        .nth(1)
        .and_then(|rest| rest.split("pub async fn run_maintenance(&self)").next())
        .expect("WorkerEngine::run must exist before run_maintenance");
    let maintenance_fn = prod_source
        .split("pub async fn run_maintenance(&self)")
        .nth(1)
        .and_then(|rest| rest.split("async fn tick(&self)").next())
        .expect("WorkerEngine::run_maintenance must exist before tick");

    assert!(
        !run_fn.contains("registry_sweep_tick"),
        "registry recovery sweep must not share the queue polling loop"
    );
    assert!(
        maintenance_fn.contains("registry_sweep_tick().await"),
        "registry recovery sweep must run from the dedicated maintenance loop"
    );
    assert!(
        !maintenance_fn.contains("legacy_queue_drain_tick")
            && !maintenance_fn.contains("legacy_worker_queue_is_empty"),
        "maintenance loop must not read or project retired legacy V1 queue rows"
    );
}

#[test]
fn worker_execution_loops_are_gated_by_executor_lease() {
    let engine_source = include_str!("../engine.rs");
    let engine_prod_source = engine_source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let maintenance_fn = engine_prod_source
        .split("pub async fn run_maintenance(&self)")
        .nth(1)
        .and_then(|rest| rest.split("async fn tick(&self)").next())
        .expect("WorkerEngine::run_maintenance must exist before tick");
    let tick_fn = engine_prod_source
        .split("async fn tick(&self)")
        .nth(1)
        .and_then(|rest| rest.split("let now_ms = now_epoch_ms();").next())
        .expect("WorkerEngine::tick must start before reading due rows");

    let maintenance_gate_pos = maintenance_fn
        .find("ensure_current_executor().await")
        .expect("maintenance loop must check the executor lease");
    let registry_sweep_pos = maintenance_fn
        .find("registry_sweep_tick().await")
        .expect("maintenance loop must run registry sweep");
    assert!(
        maintenance_gate_pos < registry_sweep_pos,
        "maintenance must acquire the executor lease before running registry sweep"
    );
    assert!(
        tick_fn.contains("ensure_current_executor().await"),
        "queue tick must acquire the executor lease before scanning due rows"
    );

    let gc_source = include_str!("../gc.rs");
    let gc_tick_fn = gc_source
        .split("async fn gc_tick(&self)")
        .nth(1)
        .and_then(|rest| {
            rest.split("self.cleanup_hnsw_s3_external_object_intents")
                .next()
        })
        .expect("WorkerGc::gc_tick must exist before cleanup calls");
    assert!(
        gc_tick_fn.contains("ensure_current_executor().await"),
        "worker-only GC must acquire the executor lease before cleaning worker state"
    );

    let main_source = include_str!("../../main.rs");
    let worker_runtime = main_source
        .split("let worker_runtime = if worker_execution_enabled")
        .nth(1)
        .and_then(|rest| rest.split("// Export snapshot janitor").next())
        .expect("main.rs must wire worker runtime");
    assert!(
        worker_runtime.contains("WorkerExecutorLeaseCoordinator::new")
            && worker_runtime.contains("WorkerEngine::new_with_executor_lease")
            && worker_runtime.contains("engine.worker_gc()"),
        "worker queue, maintenance, and worker-only GC must share the engine's executor lease coordinator"
    );
}

#[test]
fn registry_sweep_entry_reuses_outer_tenant_store() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let entry_fn = prod_source
        .split("async fn process_registry_sweep_entry(")
        .nth(1)
        .and_then(|rest| {
            rest.split("async fn record_registry_sweep_kind_result")
                .next()
        })
        .expect("process_registry_sweep_entry must exist before kind result helper");

    assert!(
        entry_fn.contains("store: &Arc<TikvStore>"),
        "sweep entry processing must receive the tenant store acquired by the page loop"
    );
    assert!(
        !entry_fn.contains("pool.acquire"),
        "sweep entry processing must not acquire the same tenant again internally"
    );
    assert!(
        entry_fn.contains("reconcile_cron_for_db(store")
            && entry_fn.contains("reconcile_ddl_journal_for_db(store")
            && entry_fn.contains("enqueue_pending_hnsw_merges(")
            && entry_fn.contains("sweep_hnsw_s3_orphans_for_entry(entry, store)"),
        "entry-level recovery tasks must reuse the sweep-owned tenant store"
    );
}

#[test]
fn registry_sweep_reaps_missing_database_queue_before_registry_delete() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let entry_fn = prod_source
        .split("async fn process_registry_sweep_entry(")
        .nth(1)
        .and_then(|rest| {
            rest.split("async fn record_registry_sweep_kind_result")
                .next()
        })
        .expect("process_registry_sweep_entry must exist before kind result helper");
    let missing_db_branch = entry_fn
        .split("if !db_exists {")
        .nth(1)
        .and_then(|rest| rest.split("self.registry_sweep_record_db_exists").next())
        .expect("missing database branch must exist before db-exists handling");

    assert!(
        missing_db_branch.contains("reap_db_queue_entries_then_delete_worker_registry"),
        "missing database cleanup must use the ordered queue-reap + registry-delete helper"
    );
    assert!(
        !missing_db_branch.contains("delete_worker_registry(&mut txn"),
        "missing database cleanup must not bypass the ordered cleanup helper"
    );
}

#[test]
fn registry_sweep_disabled_keyspace_retains_registry_row() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let skip_fn = prod_source
        .split("async fn registry_sweep_should_skip(")
        .nth(1)
        .and_then(|rest| rest.split("async fn registry_sweep_record_success").next())
        .expect("registry_sweep_should_skip must exist before record-success helper");
    let disabled_branch = skip_fn
        .split("Some(\"DISABLED\")")
        .nth(1)
        .and_then(|rest| rest.split("return Ok(true)").next())
        .expect("DISABLED keyspace branch must skip the registry entry");

    assert!(
        disabled_branch.contains("retained DISABLED keyspace entry"),
        "DISABLED keyspace rows must be retained because registry rows are recovery inventory"
    );
    assert!(
        !disabled_branch.contains("delete_worker_registry")
            && !disabled_branch.contains("reap_db_queue_entries_then_delete_worker_registry"),
        "DISABLED keyspace handling must not delete registry rows or recovery bits"
    );
}

#[test]
fn registry_sweep_recovery_probes_do_not_depend_on_task_type_bits() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let entry_fn = prod_source
        .split("async fn process_registry_sweep_entry(")
        .nth(1)
        .and_then(|rest| {
            rest.split("async fn reconcile_incomplete_cic_indexes_for_db_safe")
                .next()
        })
        .expect("process_registry_sweep_entry must exist before CIC helper");

    assert!(
        !entry_fn.contains("entry.has_cron()")
            && !entry_fn.contains("entry.has_bg_ddl()")
            && !entry_fn.contains("entry.has_ddl_journal()"),
        "registry bits are producer hints only; cron/CIC/DDL-journal recovery must probe durable truth unconditionally"
    );
}

#[test]
fn deterministic_queue_cleanup_uses_task_type_contract() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let cleanup = prod_source
        .split("if !keep_queue_entry {")
        .nth(1)
        .and_then(|rest| rest.split("if entry.task_type == TaskType::Cron").next())
        .expect("queue cleanup block must exist before cron requeue");

    assert!(
        cleanup.contains("entry.task_type.uses_deterministic_queue_key()"),
        "cleanup must use the TaskType deterministic-key contract, not a one-off HNSW special case"
    );
    assert!(
        cleanup.contains("keeps_deterministic_queue_entry_on_failure()"),
        "deterministic-key failure retention must be explicit per task type"
    );
    assert!(
        cleanup.contains("current_nonce == entry.nonce"),
        "deterministic-key cleanup must compare nonce before deleting the due row"
    );
}

#[test]
fn storage_scan_enqueue_uses_kernel_singleton_helper() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let enqueue_fn = prod_source
        .split("pub(crate) async fn enqueue_storage_scan(")
        .nth(1)
        .expect("enqueue_storage_scan must exist");

    assert!(
        enqueue_fn.contains("TaskType::StorageSizeScan"),
        "storage scan enqueue must create a StorageSizeScan task"
    );
    assert!(
        enqueue_fn.contains("entry.nonce = rand::thread_rng().gen_range(1..=u64::MAX)"),
        "StorageSizeScan uses a deterministic key and must assign a fresh non-zero nonce"
    );
    assert!(
        enqueue_fn.contains(".enqueue_singleton_task_v2_unless_db_dropped("),
        "StorageSizeScan is a singleton maintenance task and must fence on dropped-DB tombstones before enqueue"
    );
}

#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn enqueue_storage_scan_skips_existing_pending_task_without_nonce_churn() {
    let pd_endpoints = std::env::var("PD_ENDPOINTS")
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let system_keyspace = format!(
        "_sys_storage_singleton_test_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let cfg = crate::worker::config::WorkerConfig {
        enabled: true,
        system_keyspace,
        ..Default::default()
    };
    let system_store = crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
        .await
        .expect("init system store");

    let keyspace = format!(
        "storage_singleton_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let db_id = 77_u64;

    enqueue_storage_scan(&system_store, &keyspace, db_id)
        .await
        .expect("first enqueue");
    let first_nonce = storage_scan_nonce(&system_store, &keyspace, db_id).await;

    enqueue_storage_scan(&system_store, &keyspace, db_id)
        .await
        .expect("duplicate enqueue should be a no-op");
    let second_nonce = storage_scan_nonce(&system_store, &keyspace, db_id).await;

    assert_eq!(
        first_nonce, second_nonce,
        "duplicate storage scan enqueue must not overwrite the pending singleton row"
    );
}

async fn storage_scan_nonce(system_store: &TikvStore, keyspace: &str, db_id: u64) -> u64 {
    let mut txn = system_store.begin().await.expect("begin txn");
    let due = system_store
        .scan_due_v2(&mut txn, i64::MAX, 1000)
        .await
        .expect("scan due queue");
    let storage_scans = due
        .into_iter()
        .filter(|(_, descriptor)| {
            descriptor.keyspace == keyspace
                && descriptor.db_id == db_id
                && descriptor.task_id == db_id as i64
                && descriptor.task_type == TaskType::StorageSizeScan
        })
        .collect::<Vec<_>>();
    assert_eq!(
        storage_scans.len(),
        1,
        "there must be exactly one pending StorageSizeScan descriptor"
    );
    let rows = system_store
        .index_rows_for_task(
            &mut txn,
            keyspace,
            db_id,
            db_id as i64,
            TaskType::StorageSizeScan,
        )
        .await
        .expect("read queue index rows");
    assert_eq!(rows.len(), 1, "there must be exactly one queue index row");
    txn.rollback().await.ok();
    storage_scans[0].1.nonce
}

#[test]
fn hnsw_delta_probe_tracks_its_read_transaction() {
    let source = include_str!("../engine.rs");
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
        "HNSW delta probe must publish its tenant snapshot in the active txn registry"
    );
    assert!(
        fn_source.contains("hnsw_dirty_prefix(db_id)")
            && fn_source.contains("hnsw_dirty_prefix_end(db_id)")
            && !fn_source.contains(".scan_tables_page(")
            && !fn_source.contains(".list_tables("),
        "HNSW delta probe must use bounded dirty-marker pages, not a table/schema scan"
    );
    assert!(
        fn_source.contains(".enqueue_singleton_task_v2_unless_db_dropped("),
        "HNSW delta probe must enqueue merge work through the dropped-DB fenced singleton helper"
    );
}

#[test]
fn hnsw_dirty_backfill_completion_marker_is_not_permanent() {
    let now = 10_000;
    let future = encode_hnsw_dirty_backfill_due_ms(now + 1);
    let due = encode_hnsw_dirty_backfill_due_ms(now);
    let past = encode_hnsw_dirty_backfill_due_ms(now - 1);

    assert!(
        !hnsw_dirty_backfill_due(Some(&future), now),
        "future completion marker should throttle the compatibility fallback"
    );
    assert!(
        hnsw_dirty_backfill_due(Some(&due), now),
        "completion marker becomes due at its timestamp"
    );
    assert!(
        hnsw_dirty_backfill_due(Some(&past), now),
        "past completion marker must allow another compatibility pass"
    );
    assert!(
        hnsw_dirty_backfill_due(Some(b"1"), now),
        "legacy permanent done values must be treated as due, not permanent"
    );
    assert!(
        hnsw_dirty_backfill_due(None, now),
        "missing completion marker should allow the initial compatibility pass"
    );
}

#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn hnsw_sweep_backfills_pre_marker_deltas_before_enqueueing() {
    use crate::model::{ColumnDef, DataType, IndexDef, TableSchema};
    use crate::sql::hnsw::storage::{
        create_empty_hnsw_index, hnsw_delta_key, hnsw_dirty_backfill_cursor_key,
        hnsw_dirty_backfill_done_key, hnsw_dirty_key, hnsw_merge_task_id, hnsw_meta_key, HnswDelta,
    };
    use crate::txn::txn_put;

    let pd_endpoints = std::env::var("PD_ENDPOINTS")
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let unique = format!(
        "{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let system_keyspace = format!("_sys_hnswdirty_{unique}");
    let cfg = crate::worker::config::WorkerConfig {
        enabled: true,
        system_keyspace: system_keyspace.clone(),
        ..Default::default()
    };
    let system_store = crate::worker::init_gc_registry_store(pd_endpoints.clone(), &cfg)
        .await
        .expect("init system store");

    let keyspace = format!("test_hnswdirty_{unique}");
    ensure_tenant_keyspace_for_test(&pd_endpoints, &keyspace).await;
    let pool = Arc::new(crate::pool::TikvClientPool::new(pd_endpoints));
    let handle = pool
        .acquire(Some(keyspace.clone()))
        .await
        .expect("acquire tenant handle");
    let tenant_store = handle.store().clone();

    let table_id = 101_u64;
    let index_id = 7_u64;
    let post_done_index_id = 8_u64;
    let db_id = {
        let mut txn = tenant_store.begin().await.unwrap();
        let db = tenant_store
            .create_database(&mut txn, &format!("hnsw_dirty_db_{unique}"), "admin", false)
            .await
            .expect("create database")
            .expect("database should be new");
        let db_id = db.id;
        let table_name = format!("public.hnsw_dirty_backfill_{unique}");
        let mut schema = TableSchema::new(
            table_name,
            table_id,
            vec![
                ColumnDef::new("id", DataType::Int32, false).primary_key(),
                ColumnDef::new("v", DataType::Vector(3), true),
            ],
            vec![0],
        );
        schema.indexes.push(IndexDef {
            name: format!("idx_hnsw_dirty_backfill_{unique}"),
            id: index_id,
            columns: vec!["v".to_string()],
            unique: false,
            is_constraint: false,
            method: Some("hnsw".to_string()),
            predicate: None,
            expressions: Vec::new(),
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: Some(16),
            hnsw_ef_construction: Some(200),
            hnsw_distance_metric: Some("l2".to_string()),
            opclasses: vec![Some("vector_l2_ops".to_string())],
            deferrable: false,
            initially_deferred: false,
        });
        schema.indexes.push(IndexDef {
            name: format!("idx_hnsw_dirty_post_done_{unique}"),
            id: post_done_index_id,
            columns: vec!["v".to_string()],
            unique: false,
            is_constraint: false,
            method: Some("hnsw".to_string()),
            predicate: None,
            expressions: Vec::new(),
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: Some(16),
            hnsw_ef_construction: Some(200),
            hnsw_distance_metric: Some("l2".to_string()),
            opclasses: vec![Some("vector_l2_ops".to_string())],
            deferrable: false,
            initially_deferred: false,
        });
        tenant_store
            .create_table(&mut txn, db_id, schema)
            .await
            .expect("create table");

        let (_, meta) = create_empty_hnsw_index(3, "l2", 16, 200).expect("create hnsw meta");
        let meta_bytes = serde_json::to_vec(&meta).expect("serialize meta");
        txn_put(
            &mut txn,
            hnsw_meta_key(db_id, table_id, index_id),
            meta_bytes.clone(),
        )
        .await
        .expect("put hnsw meta");
        txn_put(
            &mut txn,
            hnsw_meta_key(db_id, table_id, post_done_index_id),
            meta_bytes,
        )
        .await
        .expect("put hnsw meta");
        let delta_key = hnsw_delta_key(db_id, table_id, index_id, 1);
        let delta = HnswDelta {
            label: 1,
            vector: vec![0.1, 0.2, 0.3],
        };
        txn_put(
            &mut txn,
            delta_key,
            bincode::serialize(&delta).expect("serialize delta"),
        )
        .await
        .expect("put old-format delta without dirty marker");
        assert!(
            txn.get(hnsw_dirty_key(db_id, table_id, index_id))
                .await
                .unwrap()
                .is_none(),
            "precondition: simulated old delta must not have a dirty marker"
        );
        assert!(
            txn.get(hnsw_dirty_key(db_id, table_id, post_done_index_id))
                .await
                .unwrap()
                .is_none(),
            "precondition: second index has no dirty marker"
        );
        txn.commit().await.unwrap();
        db_id
    };

    let first = enqueue_pending_hnsw_merges(
        &system_store,
        &tenant_store,
        &keyspace,
        db_id,
        None,
        HNSW_DIRTY_MARKER_PAGE_SIZE,
        600,
    )
    .await
    .expect("first sweep should backfill marker");
    assert_eq!(
        (first.observed, first.enqueued, first.enqueue_errors),
        (0, 0, 0),
        "first sweep sees no marker page yet; it only writes the compatibility marker"
    );

    {
        let mut txn = tenant_store.begin().await.unwrap();
        assert!(
            txn.get(hnsw_dirty_key(db_id, table_id, index_id))
                .await
                .unwrap()
                .is_some(),
            "backfill must create the missing dirty marker"
        );
        assert!(
            txn.get(hnsw_dirty_backfill_done_key(db_id))
                .await
                .unwrap()
                .is_some(),
            "single-page backfill should write the compatibility cooldown marker"
        );
        let done_value = txn
            .get(hnsw_dirty_backfill_done_key(db_id))
            .await
            .unwrap()
            .expect("cooldown marker");
        assert!(
            !hnsw_dirty_backfill_due(Some(&done_value), crate::worker::now_epoch_ms()),
            "fresh cooldown marker must not be due immediately"
        );
        assert!(
            txn.get(hnsw_dirty_backfill_cursor_key(db_id))
                .await
                .unwrap()
                .is_none(),
            "completed backfill should clear the cursor key"
        );
        txn.rollback().await.ok();
    }
    {
        let task_id = hnsw_merge_task_id(table_id, index_id).expect("task id");
        let mut txn = system_store.begin().await.unwrap();
        assert!(
            !system_store
                .task_has_pending(&mut txn, &keyspace, db_id, task_id, TaskType::HnswMerge)
                .await
                .expect("check pending hnsw task"),
            "backfill-only sweep must not enqueue until the next marker page"
        );
        txn.rollback().await.ok();
    }

    let second = enqueue_pending_hnsw_merges(
        &system_store,
        &tenant_store,
        &keyspace,
        db_id,
        None,
        HNSW_DIRTY_MARKER_PAGE_SIZE,
        600,
    )
    .await
    .expect("second sweep should enqueue from backfilled marker");
    assert_eq!(
        (second.observed, second.enqueued, second.enqueue_errors),
        (1, 1, 0),
        "second sweep must enqueue the pre-marker delta discovered by backfill"
    );

    let task_id = hnsw_merge_task_id(table_id, index_id).expect("task id");
    let mut txn = system_store.begin().await.unwrap();
    assert!(
        system_store
            .task_has_pending(&mut txn, &keyspace, db_id, task_id, TaskType::HnswMerge)
            .await
            .expect("check pending hnsw task"),
        "system queue must contain the HNSW merge task after the marker sweep"
    );
    txn.rollback().await.ok();

    {
        let mut txn = tenant_store.begin().await.unwrap();
        let delta_key = hnsw_delta_key(db_id, table_id, post_done_index_id, 2);
        let delta = HnswDelta {
            label: 2,
            vector: vec![0.4, 0.5, 0.6],
        };
        txn_put(
            &mut txn,
            delta_key,
            bincode::serialize(&delta).expect("serialize post-done delta"),
        )
        .await
        .expect("put old-format delta after backfill cooldown");
        assert!(
            txn.get(hnsw_dirty_key(db_id, table_id, post_done_index_id))
                .await
                .unwrap()
                .is_none(),
            "simulated old writer after backfill must still have no dirty marker"
        );
        txn_put(
            &mut txn,
            hnsw_dirty_backfill_done_key(db_id),
            encode_hnsw_dirty_backfill_due_ms(crate::worker::now_epoch_ms() - 1),
        )
        .await
        .expect("force compatibility fallback due");
        txn.commit().await.unwrap();
    }

    let third = enqueue_pending_hnsw_merges(
        &system_store,
        &tenant_store,
        &keyspace,
        db_id,
        None,
        HNSW_DIRTY_MARKER_PAGE_SIZE,
        600,
    )
    .await
    .expect("due compatibility fallback should repair post-done markerless delta");
    assert_eq!(
        third.enqueue_errors, 0,
        "post-done compatibility fallback must not hit enqueue errors"
    );
    {
        let mut txn = tenant_store.begin().await.unwrap();
        assert!(
            txn.get(hnsw_dirty_key(db_id, table_id, post_done_index_id))
                .await
                .unwrap()
                .is_some(),
            "due compatibility fallback must create a marker for post-done old-writer delta"
        );
        txn.rollback().await.ok();
    }

    let fourth = enqueue_pending_hnsw_merges(
        &system_store,
        &tenant_store,
        &keyspace,
        db_id,
        None,
        HNSW_DIRTY_MARKER_PAGE_SIZE,
        600,
    )
    .await
    .expect("marker sweep should enqueue post-done old-writer delta");
    assert_eq!(
        fourth.enqueue_errors, 0,
        "post-done marker sweep must not hit enqueue errors"
    );

    let post_done_task_id = hnsw_merge_task_id(table_id, post_done_index_id).expect("task id");
    let mut txn = system_store.begin().await.unwrap();
    assert!(
        system_store
            .task_has_pending(
                &mut txn,
                &keyspace,
                db_id,
                post_done_task_id,
                TaskType::HnswMerge,
            )
            .await
            .expect("check pending post-done hnsw task"),
        "system queue must contain the post-done HNSW merge task after fallback repair"
    );
    txn.rollback().await.ok();
}

#[test]
fn background_tenant_write_commits_take_database_liveness_fence() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");

    let execute_task = prod_source
        .split("async fn execute_task(")
        .nth(1)
        .and_then(|rest| rest.split("async fn execute_bg_ddl_backfill").next())
        .expect("execute_task must exist before bg ddl backfill");
    assert!(
        execute_task.contains("assert_database_alive_for_update(&mut txn, entry.db_id)"),
        "generic background SQL must lock/read the DB liveness row before commit"
    );

    let storage_scan = prod_source
        .split("async fn execute_storage_size_scan(")
        .nth(1)
        .and_then(|rest| rest.split("/// Enqueue a storage size scan task").next())
        .expect("execute_storage_size_scan must exist before enqueue helper");
    assert!(
        storage_scan.contains("assert_database_alive_for_update(&mut persist_txn, db_id)"),
        "storage stats persist must lock/read the DB liveness row before commit"
    );

    let cic_repair = prod_source
        .split("async fn reconcile_incomplete_cic_indexes_for_db_safe(")
        .nth(1)
        .and_then(|rest| rest.split("async fn storage_scan_due").next())
        .expect("CIC repair helper must exist before storage_scan_due");
    assert!(
        cic_repair.contains("assert_database_alive_for_update(&mut txn, db_id)"),
        "CIC repair must fence before committing schema repair"
    );

    let ddl_journal = prod_source
        .split("async fn reconcile_ddl_journal_for_db(")
        .nth(1)
        .and_then(|rest| rest.split("async fn claim_and_execute").next())
        .expect("DDL journal helper must exist before claim_and_execute");
    assert!(
        ddl_journal
            .matches("assert_database_alive_for_update")
            .count()
            >= 4,
        "DDL journal repair must fence each rotated/final tenant write transaction"
    );

    let cron_claim = prod_source
        .split("async fn claim_and_record_cron_run(")
        .nth(1)
        .and_then(|rest| rest.split("async fn load_next_cron_queue_entry").next())
        .expect("cron claim helper must exist before next-entry load");
    assert!(
        cron_claim.contains("assert_database_alive_for_update(&mut txn, entry.db_id)"),
        "cron run claim must fence before committing tenant cron state"
    );

    let cron_finalize = prod_source
        .split("async fn finalize_cron_run(")
        .nth(1)
        .and_then(|rest| rest.split("async fn execute_task").next())
        .expect("cron finalize helper must exist before execute_task");
    assert!(
        cron_finalize.contains("assert_database_alive_for_update(&mut txn, db_id)"),
        "cron run finalize must fence before committing tenant cron state"
    );
    // Finalize is a fence-gated terminal CAS (design 35): it rejects a worker
    // that lost its lease (old fence) and, on accept, clears the no-overlap
    // pointer + projects the terminal run in one txn — so a terminal transition
    // can never strand a dedup flag (DEFECT 2) nor be committed by a non-owner
    // (DEFECT 1).
    assert!(
        cron_finalize.contains("finalize_cron_run_cas("),
        "cron run finalize must go through the fence CAS, not ad-hoc guard/claim clears"
    );
}

/// Class guard for the commit-adjacent OWNERSHIP fence on the cron long-task
/// path. finalize_cron_run commits TERMINAL tenant cron state; a worker whose
/// lease was stolen must not commit it. DB-liveness (finalize's own fence) does
/// not detect takeover, so claim_and_execute_core must re-verify SAME-STORE
/// claim ownership (is_worker_claim_owned_by) BEFORE running finalize, and skip
/// finalize + cleanup when the claim is gone / taken over. This mirrors the CIC
/// backfill path's commit-adjacent lease fence, just expressed as an ownership
/// re-check rather than a cancel-token bail.
#[test]
fn finalize_is_gated_on_a_preceding_ownership_fence() {
    let prod_source = include_str!("../engine.rs");
    let core = prod_source
        .split("async fn claim_and_execute_core")
        .nth(1)
        .and_then(|rest| rest.split("async fn claim_and_record_cron_run(").next())
        .expect("claim_and_execute_core must exist");

    // The ownership re-check must appear in the body...
    let fence_pos = core
        .find("is_worker_claim_owned_by")
        .expect("finalize must be gated on an is_worker_claim_owned_by ownership fence");
    // ...and it must precede the finalize_fn invocation: a non-owner must never
    // reach finalize.
    let finalize_pos = core
        .find("finalize_fn(")
        .expect("claim_and_execute_core must invoke finalize_fn");
    assert!(
        fence_pos < finalize_pos,
        "ownership fence (is_worker_claim_owned_by) must be hoisted AHEAD of finalize_fn"
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
    let err = parse_hnsw_merge_command("__hnsw_merge 1").expect_err("missing index_id must fail");
    assert!(
        err.to_string()
            .contains("missing index_id in hnsw_merge command"),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_hnsw_merge_command_rejects_non_numeric() {
    let err =
        parse_hnsw_merge_command("__hnsw_merge abc 2").expect_err("non-numeric table_id must fail");
    assert!(
        err.to_string().contains("invalid digit") || err.to_string().contains("number"),
        "unexpected parse error: {err}"
    );
}

/// Shared setup for the cron finalize cleanup tests: stands up a system store
/// and a tenant store with one active cron job, enqueues a single V2 due entry,
/// and returns the handles plus the scanned (queue_key, descriptor) to feed into
/// `claim_and_execute_core`.
async fn setup_cron_finalize_fixture(
    tag: &str,
) -> (
    Arc<TikvStore>,
    Arc<crate::pool::TikvClientPool>,
    crate::worker::config::WorkerConfig,
    Arc<crate::worker::metrics::WorkerMetrics>,
    String,
    i64,
    Vec<u8>,
    TaskDescriptorV2,
) {
    setup_cron_finalize_fixture_cmd(tag, "SELECT 1").await
}

/// Like `setup_cron_finalize_fixture` but with a caller-chosen cron command, so
/// a test can install a long-running command (e.g. `SELECT pg_sleep(..)`) and
/// race a takeover against the execution window.
async fn setup_cron_finalize_fixture_cmd(
    tag: &str,
    command: &str,
) -> (
    Arc<TikvStore>,
    Arc<crate::pool::TikvClientPool>,
    crate::worker::config::WorkerConfig,
    Arc<crate::worker::metrics::WorkerMetrics>,
    String,
    i64,
    Vec<u8>,
    TaskDescriptorV2,
) {
    let pd_endpoints = std::env::var("PD_ENDPOINTS")
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let system_keyspace = format!(
        "_sys_finalize_{tag}_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let cfg = crate::worker::config::WorkerConfig {
        enabled: true,
        system_keyspace: system_keyspace.clone(),
        cron_job_timeout_ms: 5000,
        ..Default::default()
    };
    let system_store = crate::worker::init_gc_registry_store(pd_endpoints.clone(), &cfg)
        .await
        .expect("init system store");
    let pool = Arc::new(crate::pool::TikvClientPool::new(pd_endpoints.clone()));
    let metrics = Arc::new(crate::worker::metrics::WorkerMetrics::new());

    let keyspace = format!(
        "test_finalize_{tag}_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    ensure_tenant_keyspace_for_test(&pd_endpoints, &keyspace).await;
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
            command: command.to_string(),
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

    let entry = TaskQueueEntry::new(
        keyspace.clone(),
        db_id,
        task_id,
        TaskType::Cron,
        command.to_string(),
        "admin".to_string(),
        100,
    )
    .with_schedule("*/5 * * * *".to_string());

    let fire_time_ms = crate::worker::now_epoch_ms();
    let (queue_key, descriptor) = {
        let mut txn = system_store.begin().await.unwrap();
        system_store
            .enqueue_task_v2_unless_db_dropped(&mut txn, &entry, fire_time_ms)
            .await
            .unwrap();
        txn.commit().await.unwrap();

        let mut txn2 = system_store.begin().await.unwrap();
        let entries = system_store
            .scan_due_v2(&mut txn2, i64::MAX, 1000)
            .await
            .unwrap();
        let (key, descriptor) = entries
            .into_iter()
            .find(|(_, d)| d.task_id == task_id && d.keyspace == keyspace)
            .expect("queue entry must exist");
        txn2.rollback().await.ok();
        (key, descriptor)
    };

    (
        system_store,
        pool,
        cfg,
        metrics,
        keyspace,
        task_id,
        queue_key,
        descriptor,
    )
}

/// NEGATIVE side of the `finalize_result.is_ok()` requeue guard (#2): when
/// finalize ERRORS, cleanup must still release our worker claim, but the next
/// cron fire MUST NOT be requeued — the run's terminal state / DB liveness is in
/// doubt (e.g. DROP DATABASE removed metadata mid-run), so writing a fresh entry
/// into the global queue would resurrect work for a possibly-dropped DB.
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn finalize_failure_in_claim_and_execute_releases_claim_and_does_not_requeue_cron() {
    let (system_store, pool, cfg, metrics, keyspace, task_id, queue_key, descriptor) =
        setup_cron_finalize_fixture("fail").await;

    // Call the REAL claim_and_execute code path with injected finalize failure.
    let result = WorkerEngine::claim_and_execute_core(
        &system_store,
        &pool,
        &cfg,
        &metrics,
        queue_key,
        DueItem::V2(descriptor),
        CancellationToken::new(),
        |_store, _db_id, _run, _status, _msg, _start, _end, _sched_min| async {
            Err(anyhow!("injected: TiKV write error in finalize_cron_run"))
        },
    )
    .await;

    // finalize error propagates (claim_and_execute_core returns Err) AFTER cleanup.
    assert!(
        result.is_err(),
        "claim_and_execute_core must propagate finalize error after cleanup"
    );
    assert!(
        result.unwrap_err().to_string().contains("injected"),
        "propagated error must be the finalize error"
    );

    let mut txn = system_store.begin().await.unwrap();

    // Worker claim MUST be released (cleanup runs regardless of finalize result).
    let claims = system_store.list_worker_claims(&mut txn).await.unwrap();
    assert!(
        !claims.iter().any(|(_, c)| c.worker_id == cfg.worker_id),
        "INVARIANT VIOLATED: worker claim must be deleted after cleanup"
    );

    // The NEW contract: next cron fire MUST NOT be requeued when finalize failed.
    // No future descriptor for this task may exist in the system queue. (The
    // processed due entry was deleted by cleanup, so any remaining descriptor
    // would be an illegitimate re-schedule.)
    let queue = system_store
        .scan_due_v2(&mut txn, i64::MAX, 1000)
        .await
        .unwrap();
    assert!(
        !queue.iter().any(|(_, d)| d.task_id == task_id
            && d.keyspace == keyspace
            && d.task_type == TaskType::Cron),
        "INVARIANT VIOLATED: next cron fire must NOT be requeued after a finalize failure"
    );

    txn.rollback().await.ok();
}

/// POSITIVE side of the same guard: when finalize SUCCEEDS, cleanup releases the
/// claim AND requeues the next cron fire. Together with the failure test above,
/// this drives BOTH transitions of `finalize_result.is_ok()`.
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn finalize_success_in_claim_and_execute_releases_claim_and_requeues_cron() {
    let (system_store, pool, cfg, metrics, keyspace, task_id, queue_key, descriptor) =
        setup_cron_finalize_fixture("ok").await;

    // Real path with a finalize that SUCCEEDS (mirrors finalize_cron_run's commit
    // by clearing the running guard / writing the run, then committing).
    let result = WorkerEngine::claim_and_execute_core(
        &system_store,
        &pool,
        &cfg,
        &metrics,
        queue_key,
        DueItem::V2(descriptor),
        CancellationToken::new(),
        |store, fin_db_id, run, status, msg, start, end, sched_min| async move {
            // Exercise the REAL finalize (fence-gated terminal CAS), not a
            // hand-rolled stand-in.
            WorkerEngine::finalize_cron_run(
                store, fin_db_id, run, status, msg, start, end, sched_min,
            )
            .await
        },
    )
    .await;

    assert!(
        result.is_ok(),
        "claim_and_execute_core must succeed when finalize succeeds: {:?}",
        result.err()
    );

    let mut txn = system_store.begin().await.unwrap();

    let claims = system_store.list_worker_claims(&mut txn).await.unwrap();
    assert!(
        !claims.iter().any(|(_, c)| c.worker_id == cfg.worker_id),
        "worker claim must be deleted after a successful run"
    );

    // Next cron fire MUST be requeued (new V2 descriptor for this task).
    let queue = system_store
        .scan_due_v2(&mut txn, i64::MAX, 1000)
        .await
        .unwrap();
    assert!(
        queue.iter().any(|(_, d)| d.task_id == task_id
            && d.keyspace == keyspace
            && d.task_type == TaskType::Cron),
        "next cron fire must be requeued after a successful finalize"
    );

    txn.rollback().await.ok();
}

/// Guaranteed-schedule-progress on the REAPER-RECOVERED-CRASH path (design 35
/// §Contract / DEFECT 2 residual). A worker claims cron minute M (CONTROL Running
/// + ACTIVE) and crashes before requeue; the cron GC reaper recovers it by
/// terminalizing CONTROL(M) and clearing ACTIVE — but the system due row for M
/// still exists (the crashed worker never deleted it). A later worker reclaims
/// that due row: the claim observes terminal CONTROL and returns
/// `AlreadyTerminalForMinute`, so this worker did NOT run the fire (no `cron_run`,
/// `finalize_fn` must NEVER fire). The minute is nonetheless DONE, so cleanup MUST
/// still enqueue the next fire M+1 — otherwise the schedule silently stalls until
/// a much-later registry sweep.
///
/// This drives the REAL `claim_and_execute_core` path and asserts:
///   (i)   the reclaim deduplicates (finalize_fn is never invoked — the only way
///         claim_and_execute_core reaches finalize is with a claimed run, which
///         `AlreadyTerminalForMinute` is not);
///   (ii)  the due row for M is dropped AND the next fire M+1 IS enqueued;
///   (iii) the next-fire enqueue is the SAME fenced idempotent singleton put the
///         ran-it path uses — two reclaims of the same terminal minute enqueue
///         M+1 exactly once (the cron due key is deterministic on
///         (priority, fire_time, type, keyspace, db_id, task_id), so a duplicate
///         next-fire write overwrites rather than duplicating).
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn cluster_already_terminal_minute_still_requeues_next_fire() {
    let (system_store, pool, cfg, metrics, keyspace, task_id, queue_key, descriptor) =
        setup_cron_finalize_fixture("term").await;

    let db_id = 1_u64;
    let fire_time_ms = crate::storage::decode_wq_due_v2_fire_time(&queue_key)
        .expect("fire_time_ms must decode from the queue key");
    // The engine derives the scheduled minute the same way (engine.rs).
    let scheduled_min = fire_time_ms.div_euclid(60_000);

    // Resolve the exact catalog job the fixture installed (so the planted run
    // matches command/username and the claim's no-overlap/dedup keys line up).
    let job = {
        let handle = pool.acquire(Some(keyspace.clone())).await.unwrap();
        let store = handle.store().clone();
        let mut txn = store.begin().await.unwrap();
        let job = store
            .get_cron_job(&mut txn, db_id, task_id)
            .await
            .unwrap()
            .expect("fixture cron job must exist");
        txn.rollback().await.ok();
        job
    };

    // Simulate "worker claims M, crashes, reaper terminalizes": drive the REAL
    // CAS helpers on the TENANT store to leave CONTROL(M) terminal + ACTIVE
    // cleared, exactly the state reap_stale_active_runs commits. The system due
    // row for M is left in place (the crashed worker never deleted it).
    {
        let handle = pool.acquire(Some(keyspace.clone())).await.unwrap();
        let store = handle.store().clone();

        // Claim M -> mints the fence and writes CONTROL(Running) + ACTIVE.
        let fence = {
            let mut txn = store.begin().await.unwrap();
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    task_id,
                    scheduled_min,
                    crate::worker::now_epoch_ms(),
                    crate::worker::now_epoch_ms() + 10_000_000,
                    &job,
                    "postgres".to_string(),
                )
                .await
                .expect("claim M");
            txn.commit().await.expect("commit claim M");
            match outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("first claim of M must be Claimed, got {other:?}"),
            }
        };

        // Reaper terminalization: terminal CONTROL(M) + ACTIVE deleted.
        {
            let mut txn = store.begin().await.unwrap();
            let run = CronRun {
                run_id: fence,
                job_id: task_id,
                job_pid: None,
                database: "postgres".to_string(),
                username: job.username.clone(),
                command: job.command.clone(),
                status: CronRunStatus::Failed,
                return_message: Some("recovered by reaper".to_string()),
                start_time: Some(0),
                end_time: Some(1),
            };
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    task_id,
                    scheduled_min,
                    fence,
                    CronRunState::Failed,
                    &run,
                )
                .await
                .expect("reaper finalize M");
            assert!(accepted, "reaper terminalization of M must be accepted");
            txn.commit().await.expect("commit reaper finalize M");
        }
    }

    // `compute_next_fire_time()` is intentionally wall-clock based. Keep the
    // two synthetic reclaims below away from a minute boundary so this test
    // exercises idempotency for the same computed next fire, not clock rollover.
    let millis_into_minute = chrono::Utc::now().timestamp_millis().rem_euclid(60_000);
    if millis_into_minute > 45_000 {
        tokio::time::sleep(std::time::Duration::from_millis(
            (60_000 - millis_into_minute + 1_000) as u64,
        ))
        .await;
    }

    // finalize_fn must NEVER run for an AlreadyTerminalForMinute reclaim: the
    // minute is already done, so this worker owns no run to finalize.
    let no_finalize = |_store, _db_id, _run, _status, _msg, _start, _end, _sched_min| async {
        panic!(
            "INVARIANT VIOLATED: finalize ran for an AlreadyTerminalForMinute reclaim — \
                 this worker did not run the fire and owns no run to finalize"
        );
        #[allow(unreachable_code)]
        Ok(())
    };

    // First reclaim of the still-present due row for M.
    let result = WorkerEngine::claim_and_execute_core(
        &system_store,
        &pool,
        &cfg,
        &metrics,
        queue_key.clone(),
        DueItem::V2(descriptor.clone()),
        CancellationToken::new(),
        no_finalize,
    )
    .await;
    assert!(
        result.is_ok(),
        "claim_and_execute_core must succeed on an already-terminal reclaim: {:?}",
        result.err()
    );

    // The next fire M+1 must be enqueued; assert it sits at a strictly later fire
    // time than M (so we are observing the requeue, not a leftover M row).
    let next_fire_count = |rows: &[(Vec<u8>, TaskDescriptorV2)]| -> usize {
        rows.iter()
            .filter(|(k, d)| {
                d.task_id == task_id
                    && d.keyspace == keyspace
                    && d.task_type == TaskType::Cron
                    && crate::storage::decode_wq_due_v2_fire_time(k)
                        .map(|ft| ft > fire_time_ms)
                        .unwrap_or(false)
            })
            .count()
    };

    {
        let mut txn = system_store.begin().await.unwrap();

        // (i) the original M due row is gone (cleanup dropped it).
        let m_present = {
            let rows = system_store
                .scan_due_v2(&mut txn, i64::MAX, 1000)
                .await
                .unwrap();
            rows.iter().any(|(k, d)| {
                d.task_id == task_id
                    && d.keyspace == keyspace
                    && d.task_type == TaskType::Cron
                    && crate::storage::decode_wq_due_v2_fire_time(k) == Some(fire_time_ms)
            })
        };
        assert!(!m_present, "the processed minute-M due row must be dropped");

        // (ii) the next fire M+1 IS enqueued.
        let rows = system_store
            .scan_due_v2(&mut txn, i64::MAX, 1000)
            .await
            .unwrap();
        assert_eq!(
            next_fire_count(&rows),
            1,
            "AlreadyTerminalForMinute must requeue exactly one next cron fire (M+1)"
        );
        txn.rollback().await.ok();
    }

    // (iii) Idempotency: re-plant the M due row and reclaim a SECOND time. The
    // minute is still terminal, so the reclaim again dedups and re-enqueues M+1
    // through the SAME deterministic-key singleton put — there must still be
    // exactly one M+1 row (the duplicate next-fire write overwrites in place).
    {
        let entry = TaskQueueEntry::new(
            keyspace.clone(),
            db_id,
            task_id,
            TaskType::Cron,
            job.command.clone(),
            job.username.clone(),
            descriptor.priority,
        )
        .with_schedule(job.schedule.clone());
        let mut txn = system_store.begin().await.unwrap();
        system_store
            .enqueue_task_v2_unless_db_dropped(&mut txn, &entry, fire_time_ms)
            .await
            .unwrap();
        txn.commit().await.unwrap();

        let (key2, descriptor2) = {
            let mut txn = system_store.begin().await.unwrap();
            let rows = system_store
                .scan_due_v2(&mut txn, i64::MAX, 1000)
                .await
                .unwrap();
            let found = rows
                .into_iter()
                .find(|(k, d)| {
                    d.task_id == task_id
                        && d.keyspace == keyspace
                        && d.task_type == TaskType::Cron
                        && crate::storage::decode_wq_due_v2_fire_time(k) == Some(fire_time_ms)
                })
                .expect("re-planted minute-M due row must exist");
            txn.rollback().await.ok();
            found
        };

        let result2 = WorkerEngine::claim_and_execute_core(
            &system_store,
            &pool,
            &cfg,
            &metrics,
            key2,
            DueItem::V2(descriptor2),
            CancellationToken::new(),
            no_finalize,
        )
        .await;
        assert!(
            result2.is_ok(),
            "second already-terminal reclaim must succeed: {:?}",
            result2.err()
        );
    }

    {
        let mut txn = system_store.begin().await.unwrap();
        let rows = system_store
            .scan_due_v2(&mut txn, i64::MAX, 1000)
            .await
            .unwrap();
        assert_eq!(
            next_fire_count(&rows),
            1,
            "two terminal-minute reclaims must enqueue M+1 EXACTLY once (idempotent singleton)"
        );
        txn.rollback().await.ok();
    }
}

/// Behavioral coverage for the commit-adjacent OWNERSHIP fence on the cron
/// long-task path (claim-lease lifecycle, design §K4).
///
/// Scenario: worker A claims a cron run and starts executing a slow command. Its
/// lease lapses mid-run and worker B takes over (re-claims the SAME identity key
/// under B's worker_id — exactly what try_claim_worker_task allows once the lease
/// is expired). When A's execution returns, A must NOT:
///   1. run finalize_fn (commit terminal CronRun / clear the running guard /
///      release the per-minute claim) — the takeover worker owns the outcome;
///   2. delete B's worker claim;
///   3. delete or requeue the due row (which B is now processing).
///
/// Before this fix, A unconditionally ran finalize (fenced only by DB-liveness,
/// which does NOT detect takeover) and committed terminal tenant cron state, then
/// the ownership-checked cleanup ran one step too late. Combined with the
/// never-released per-minute claim, the takeover then hit AlreadyClaimedForMinute
/// and the due row was dropped without requeue — one scheduled fire silently
/// lost. This test drives the REAL claim_and_execute_core path and asserts the
/// stale worker neither commits terminal cron state nor strands the requeue.
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn takeover_before_finalize_skips_stale_finalize_and_preserves_due_row() {
    // Slow command so the test has a window to install the takeover claim while
    // A is still executing (before A reaches the ownership fence).
    let (system_store, pool, cfg, metrics, keyspace, task_id, queue_key, descriptor) =
        setup_cron_finalize_fixture_cmd("takeover", "SELECT pg_sleep(2)").await;

    let fire_time_ms = crate::storage::decode_wq_due_v2_fire_time(&queue_key)
        .expect("fire_time_ms must decode from the queue key");
    let takeover_worker_id = format!("takeover-B-{}", std::process::id());

    // Drive A's real claim+execute path on a background task. finalize_fn must
    // NEVER run for A: the ownership fence ahead of finalize must short-circuit
    // once B owns the claim. A panic here fails the test through the join below.
    let sys_for_a = system_store.clone();
    let pool_for_a = pool.clone();
    let cfg_for_a = cfg.clone();
    let metrics_for_a = metrics.clone();
    let a_handle = tokio::spawn(async move {
        WorkerEngine::claim_and_execute_core(
            &sys_for_a,
            &pool_for_a,
            &cfg_for_a,
            &metrics_for_a,
            queue_key,
            DueItem::V2(descriptor),
            CancellationToken::new(),
            |_store, _db_id, _run, _status, _msg, _start, _end, _sched_min| async {
                panic!(
                    "INVARIANT VIOLATED: stale worker A ran finalize after a takeover — \
                     terminal cron state must NOT be committed by a non-owner"
                );
                #[allow(unreachable_code)]
                Ok(())
            },
        )
        .await
    });

    // Poll until A has installed its claim, then simulate a takeover: replace the
    // claim with B's identity (this is exactly the state try_claim_worker_task
    // leaves after an expired-lease takeover by a second worker).
    let mut installed = false;
    for _ in 0..200 {
        let mut txn = system_store.begin().await.unwrap();
        let owns_a = system_store
            .is_worker_claim_owned_by(
                &mut txn,
                &keyspace,
                1,
                task_id,
                fire_time_ms,
                TaskType::Cron,
                &cfg.worker_id,
            )
            .await
            .unwrap();
        if owns_a {
            // Overwrite A's claim with B's: delete then re-claim as B in one txn.
            system_store
                .delete_worker_claim(
                    &mut txn,
                    &keyspace,
                    1,
                    task_id,
                    fire_time_ms,
                    TaskType::Cron,
                )
                .await
                .unwrap();
            let b_claim = WorkerClaim::with_lease(
                takeover_worker_id.clone(),
                TaskType::Cron,
                cfg.claim_lease_ms as i64,
            );
            let took = system_store
                .try_claim_worker_task(
                    &mut txn,
                    &keyspace,
                    1,
                    task_id,
                    fire_time_ms,
                    &b_claim,
                    (cfg.orphan_timeout_sec as i64).saturating_mul(1000),
                )
                .await
                .unwrap();
            assert!(took, "B must be able to install its takeover claim");
            txn.commit().await.unwrap();
            installed = true;
            break;
        }
        txn.rollback().await.ok();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        installed,
        "A never installed its claim; cannot simulate takeover"
    );

    // A must finish cleanly (Ok) without running finalize and without touching
    // B's claim or the due row.
    let a_result = a_handle.await.expect("A task must not panic");
    assert!(
        a_result.is_ok(),
        "stale worker A must return Ok after skipping finalize+cleanup: {:?}",
        a_result.err()
    );

    let mut txn = system_store.begin().await.unwrap();

    // 1. B's claim MUST still be present (A must not delete the new owner's claim).
    let claims = system_store.list_worker_claims(&mut txn).await.unwrap();
    assert!(
        claims
            .iter()
            .any(|(_, c)| c.worker_id == takeover_worker_id),
        "INVARIANT VIOLATED: takeover worker B's claim must survive A's cleanup"
    );
    assert!(
        !claims.iter().any(|(_, c)| c.worker_id == cfg.worker_id),
        "stale A's own claim must not linger (it was replaced by B's)"
    );

    // 2. The due row MUST be preserved (B is processing it; A must not delete it).
    let queue = system_store
        .scan_due_v2(&mut txn, i64::MAX, 1000)
        .await
        .unwrap();
    let same_fire = queue.iter().any(|(_, d)| {
        d.task_id == task_id && d.keyspace == keyspace && d.task_type == TaskType::Cron
    });
    assert!(
        same_fire,
        "INVARIANT VIOLATED: the due cron row must be left for the takeover worker, not deleted/stranded"
    );
    txn.rollback().await.ok();

    // 3. Terminal tenant cron state MUST NOT have been committed by A: the run
    //    recorded for this job is still in a non-terminal (Running) status, so B
    //    is free to finalize it. A finalize by stale A would have flipped it to
    //    Succeeded/Failed.
    let tenant_store = pool
        .acquire(Some(keyspace.clone()))
        .await
        .expect("acquire tenant handle")
        .store()
        .clone();
    let mut ttxn = tenant_store.begin().await.unwrap();
    let (runs, _) = tenant_store
        .list_cron_runs_batch(&mut ttxn, 1, None, 1000)
        .await
        .unwrap();
    ttxn.rollback().await.ok();
    let job_runs: Vec<_> = runs.iter().filter(|r| r.job_id == task_id).collect();
    assert!(
        !job_runs.is_empty(),
        "A must have recorded a Running cron run when it claimed the fire"
    );
    use crate::cron::types::CronRunStatus;
    assert!(
        job_runs
            .iter()
            .all(|r| matches!(r.status, CronRunStatus::Starting | CronRunStatus::Running)),
        "INVARIANT VIOLATED: stale A committed a terminal CronRun ({:?}) after losing the lease",
        job_runs
            .iter()
            .map(|r| r.status.clone())
            .collect::<Vec<_>>()
    );
}

/// Behavioral coverage for the dropped-DB enqueue fence (#2) across ALL FOUR
/// cross-store re-enqueue call sites. A dropped DB is modeled by a db_id whose
/// metadata row is absent (DROP DATABASE deletes that row before destroying the
/// range). Each site must take the liveness fence, hit its false branch, return
/// its early-return sentinel, and write NOTHING into the global system queue.
///
/// This drives the false branch — not just the source text — so it would catch a
/// missing rollback, an inverted condition, or the wrong sentinel (e.g. Ok(true)
/// / Ok((>0,..)) / enqueued>0 / Some(..)).
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn dropped_db_suppresses_enqueue_across_all_cross_store_sites() {
    let pd_endpoints = std::env::var("PD_ENDPOINTS")
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let system_keyspace = format!(
        "_sys_droppedfence_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let cfg = crate::worker::config::WorkerConfig {
        enabled: true,
        system_keyspace: system_keyspace.clone(),
        ..Default::default()
    };
    let system_store = crate::worker::init_gc_registry_store(pd_endpoints.clone(), &cfg)
        .await
        .expect("init system store");
    let pool = Arc::new(crate::pool::TikvClientPool::new(pd_endpoints.clone()));

    let keyspace = format!(
        "test_droppedfence_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    ensure_tenant_keyspace_for_test(&pd_endpoints, &keyspace).await;
    // bootstrap creates only db_id=1 (postgres). Use a db_id that has NO metadata
    // row: identical state to a DROP DATABASE that already removed the row.
    let missing_db_id = 9_999_u64;
    let task_id = 42_i64;
    let tenant_store = {
        let handle = pool
            .acquire(Some(keyspace.clone()))
            .await
            .expect("acquire tenant handle");
        handle.store().clone()
    };

    // Sanity: the storage primitive itself sees the missing DB as not-alive.
    {
        let mut txn = tenant_store.begin().await.unwrap();
        let alive = tenant_store
            .database_alive_for_update(&mut txn, missing_db_id)
            .await
            .expect("liveness check must not error for a missing DB");
        assert!(!alive, "missing DB must report alive == false");
        txn.rollback().await.ok();
    }

    let engine = WorkerEngine::new(cfg.clone(), system_store.clone(), pool.clone());

    // Snapshot the global queue size before exercising the sites.
    let queue_len_before = {
        let mut txn = system_store.begin().await.unwrap();
        let q = system_store
            .scan_due_v2(&mut txn, i64::MAX, 100_000)
            .await
            .unwrap();
        txn.rollback().await.ok();
        q.len()
    };

    // 1. storage_scan_due → Ok(false) (report "not due", suppress enqueue).
    let due = engine
        .storage_scan_due(&tenant_store, missing_db_id)
        .await
        .expect("storage_scan_due must not error on a dropped DB");
    assert!(!due, "storage_scan_due must report false for a dropped DB");

    // 2. reconcile_cron_for_db → Ok((0, 0)) (nothing enqueued, nothing cleaned).
    let (enqueued, cleaned) = engine
        .reconcile_cron_for_db(&tenant_store, &keyspace, missing_db_id)
        .await
        .expect("reconcile_cron_for_db must not error on a dropped DB");
    assert_eq!(
        (enqueued, cleaned),
        (0, 0),
        "reconcile_cron_for_db must enqueue/clean nothing for a dropped DB"
    );

    // 3. enqueue_pending_hnsw_merges → enqueued == 0 (sweep bails, no merges).
    let sweep = enqueue_pending_hnsw_merges(
        &system_store,
        &tenant_store,
        &keyspace,
        missing_db_id,
        None,
        128,
        600,
    )
    .await
    .expect("enqueue_pending_hnsw_merges must not error on a dropped DB");
    assert_eq!(
        (sweep.observed, sweep.enqueued, sweep.enqueue_errors),
        (0, 0, 0),
        "HNSW sweep must observe/enqueue nothing for a dropped DB"
    );
    assert!(
        sweep.next_dirty_cursor.is_none(),
        "HNSW sweep must not advance a dirty cursor for a dropped DB"
    );

    // 4. load_next_cron_queue_entry → None (no next cron fire scheduled).
    let entry = TaskQueueEntry::new(
        keyspace.clone(),
        missing_db_id,
        task_id,
        TaskType::Cron,
        "SELECT 1".to_string(),
        "admin".to_string(),
        100,
    )
    .with_schedule("*/5 * * * *".to_string());
    let next = WorkerEngine::load_next_cron_queue_entry(&pool, &entry)
        .await
        .expect("load_next_cron_queue_entry must not error on a dropped DB");
    assert!(
        next.is_none(),
        "load_next_cron_queue_entry must yield no next entry for a dropped DB"
    );

    // The decisive behavioral assertion: NONE of the four sites wrote any new
    // descriptor into the global system queue for the dropped DB.
    let mut txn = system_store.begin().await.unwrap();
    let queue = system_store
        .scan_due_v2(&mut txn, i64::MAX, 100_000)
        .await
        .unwrap();
    assert!(
        !queue.iter().any(|(_, d)| d.db_id == missing_db_id),
        "INVARIANT VIOLATED: a cross-store site enqueued work for a dropped DB"
    );
    assert_eq!(
        queue.len(),
        queue_len_before,
        "no global queue rows may be added for a dropped DB"
    );
    txn.rollback().await.ok();
}

/// Cron cross-store TOCTOU invariant: a stale cron next-fire row left in the
/// GLOBAL `_sys_worker` queue for a DB that has since been DROPped is benign.
///
/// The window: a cron next-fire descriptor is enqueued into the system store,
/// then DROP DATABASE removes the tenant's `database_id` metadata row (the same
/// row `database_alive_for_update` / `get_database_by_id` read). The descriptor
/// is now orphaned — it references a db_id that no longer resolves. The two
/// load-bearing properties we must hold are:
///
///   (a) SKIP-BEFORE-SQL: when a worker later claims the row and runs the REAL
///       `claim_and_execute_core` path, `claim_and_record_cron_run` resolves the
///       DB via `get_database_by_id`, sees `None`, and returns `(None, false)`.
///       That makes `cron_run == None`, so `claim_and_execute_core` takes the
///       `cron_run.is_none()` branch (`exec_result = Ok(0)`) and NEVER calls
///       `execute_task` — no tenant SQL is ever parsed/executed against the
///       destroyed/wrong DB. We prove "no SQL ran" by passing a `finalize_fn`
///       that panics if invoked: finalize only runs when a real run was claimed,
///       so it must stay untouched on the skip path.
///
///   (b) REAPED, NOT LEAKED: cleanup runs unconditionally with
///       `keep_queue_entry == false`. Cron uses a non-deterministic queue key,
///       so cleanup deletes the exact processed due row (descriptor + index +
///       payload) via `delete_due_entry`. After processing, the global queue has
///       ZERO surviving `_sys_worker` rows for that db_id — both `scan_due_v2`
///       (descriptor layer) and `index_rows_for_db` (identity index layer) come
///       back empty. The orphan does not survive to be re-claimed forever.
///
/// Assertions are BEHAVIORAL (real store state after the real worker path), not
/// source-string greps. This is the cron-side complement to
/// `dropped_db_suppresses_enqueue_across_all_cross_store_sites`, which proves the
/// PRODUCER never enqueues for a dropped DB; this proves the CONSUMER safely
/// drains an orphan that was already enqueued before the drop.
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn orphan_cron_nextfire_for_dropped_db_is_skipped_and_reaped() {
    let pd_endpoints = std::env::var("PD_ENDPOINTS")
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let system_keyspace = format!(
        "_sys_orphancron_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let cfg = crate::worker::config::WorkerConfig {
        enabled: true,
        system_keyspace: system_keyspace.clone(),
        worker_id: format!(
            "orphancron-worker-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ),
        cron_job_timeout_ms: 5000,
        ..Default::default()
    };
    let system_store = crate::worker::init_gc_registry_store(pd_endpoints.clone(), &cfg)
        .await
        .expect("init system store");
    let pool = Arc::new(crate::pool::TikvClientPool::new(pd_endpoints.clone()));
    let metrics = Arc::new(crate::worker::metrics::WorkerMetrics::new());

    let keyspace = format!(
        "test_orphancron_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    ensure_tenant_keyspace_for_test(&pd_endpoints, &keyspace).await;
    // A db_id with NO `database_id` metadata row — identical durable state to a
    // DROP DATABASE that already removed the row. We DO seed the tenant cron
    // bits (enabled flag + active job) to model a cron next-fire that was
    // enqueued while the DB still existed, then orphaned by the drop. We do NOT
    // call create_database for this id, so `get_database_by_id` sees it as gone.
    let dropped_db_id = 4_242_u64;
    let task_id = 77_i64;

    // Seed tenant-side cron state (job + enabled), but never a database row.
    {
        let handle = pool
            .acquire(Some(keyspace.clone()))
            .await
            .expect("acquire tenant handle");
        let tenant_store = handle.store().clone();
        let mut txn = tenant_store.begin().await.unwrap();
        tenant_store
            .set_cron_enabled(&mut txn, dropped_db_id)
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
            .put_cron_job(&mut txn, dropped_db_id, &job)
            .await
            .unwrap();
        txn.commit().await.unwrap();

        // Confirm the precondition: the DB metadata row is genuinely absent, so
        // this is a true post-DROP orphan rather than a live DB.
        let mut check = tenant_store.begin().await.unwrap();
        let resolved = tenant_store
            .get_database_by_id(&mut check, dropped_db_id)
            .await
            .expect("get_database_by_id must not error for a missing DB");
        assert!(
            resolved.is_none(),
            "precondition: dropped DB must have no metadata row"
        );
        check.rollback().await.ok();
    }

    // Seed the stale cron next-fire descriptor into the GLOBAL system queue.
    let entry = TaskQueueEntry::new(
        keyspace.clone(),
        dropped_db_id,
        task_id,
        TaskType::Cron,
        "SELECT 1".to_string(),
        "admin".to_string(),
        100,
    )
    .with_schedule("*/5 * * * *".to_string());

    let fire_time_ms = crate::worker::now_epoch_ms();
    let (queue_key, descriptor) = {
        let mut txn = system_store.begin().await.unwrap();
        system_store
            .enqueue_task_v2_unless_db_dropped(&mut txn, &entry, fire_time_ms)
            .await
            .unwrap();
        txn.commit().await.unwrap();

        let mut txn2 = system_store.begin().await.unwrap();
        let entries = system_store
            .scan_due_v2(&mut txn2, i64::MAX, 1000)
            .await
            .unwrap();
        let found = entries
            .into_iter()
            .find(|(_, d)| {
                d.task_id == task_id && d.keyspace == keyspace && d.db_id == dropped_db_id
            })
            .expect("seeded orphan cron descriptor must be present in the global queue");
        txn2.rollback().await.ok();
        found
    };

    // Drive the REAL claim+execute path. finalize_fn must NEVER run: it only
    // fires when a cron run was actually claimed, and a dropped DB skips before
    // any run is claimed (the `get_database_by_id -> None` branch).
    let result = WorkerEngine::claim_and_execute_core(
        &system_store,
        &pool,
        &cfg,
        &metrics,
        queue_key,
        DueItem::V2(descriptor),
        CancellationToken::new(),
        |_store, _db_id, _run, _status, _msg, _start, _end, _sched_min| async {
            panic!(
                "INVARIANT VIOLATED: finalize_fn ran for a dropped DB — a cron run \
                 was claimed and SQL executed against a destroyed/wrong database"
            );
            #[allow(unreachable_code)]
            Ok(())
        },
    )
    .await;

    // (a) SKIP-BEFORE-SQL: the path completes cleanly (no run claimed, no SQL).
    assert!(
        result.is_ok(),
        "claim_and_execute_core must cleanly skip an orphan cron row for a dropped DB: {:?}",
        result.err()
    );

    let mut txn = system_store.begin().await.unwrap();

    // Our worker claim must be released (cleanup always runs).
    let claims = system_store.list_worker_claims(&mut txn).await.unwrap();
    assert!(
        !claims.iter().any(|(_, c)| c.worker_id == cfg.worker_id),
        "worker claim must be deleted after skipping an orphan cron row"
    );

    // (b) REAPED: zero surviving descriptor-layer rows for this db_id.
    let due = system_store
        .scan_due_v2(&mut txn, i64::MAX, 100_000)
        .await
        .unwrap();
    assert!(
        !due.iter().any(|(_, d)| d.db_id == dropped_db_id),
        "INVARIANT VIOLATED: orphan cron descriptor survived for a dropped DB (not reaped)"
    );

    // (b) REAPED: zero surviving identity-index rows for this db_id, so the
    // orphan cannot be re-discovered/re-claimed on a later tick. This is the
    // decisive "no permanent leak" assertion across the whole `_sys_worker`
    // identity index for the dropped DB.
    let index_rows = system_store
        .index_rows_for_db(&mut txn, &keyspace, dropped_db_id)
        .await
        .unwrap();
    assert!(
        index_rows.is_empty(),
        "INVARIANT VIOLATED: orphan cron index rows survived for a dropped DB (would be re-claimed forever)"
    );

    // And specifically: the next cron fire was NOT requeued (requeue is gated on
    // a successful run, which never happened).
    assert!(
        !due.iter().any(|(_, d)| d.task_id == task_id
            && d.keyspace == keyspace
            && d.task_type == TaskType::Cron),
        "a dropped DB's cron must not be requeued"
    );

    txn.rollback().await.ok();
}

/// PROACTIVE cross-store fence (#2628 item 2): a DROP-reap that writes the
/// durable dropped-DB TOMBSTONE and a concurrent cron next-fire enqueue that
/// reads that tombstone via `get_for_update` in the SAME system txn as
/// `put_task_v2` (`enqueue_task_v2_unless_db_dropped`) CANNOT both commit — they
/// serialize on the tombstone key under pessimistic txns. Exactly one wins, and
/// when DROP wins the enqueue is aborted/retried-to-suppressed so NO stale
/// `_sys_worker` next-fire row remains for the dropped db_id.
///
/// This makes the former "self-healing residual" orphan IMPOSSIBLE, not merely
/// recoverable. It drives the real conflict against a live store, not a source
/// grep: two concurrent pessimistic txns touching the same tombstone key, then
/// the post-conflict store state.
///
/// Scenarios exercised against the same db_id family:
///   (A) RACE: reap-txn (tombstone put) and enqueue-txn (tombstone
///       get_for_update + put_task_v2) run concurrently, then both attempt to
///       commit. At most one commits. When the reap wins, the global queue holds
///       ZERO next-fire rows for the dropped db_id.
///   (B) SEQUENCED: once a tombstone is durably committed, a later enqueue's
///       `enqueue_task_v2_unless_db_dropped` returns false and writes nothing —
///       the clean suppress path the production reap-then-enqueue ordering hits.
///   (C) REGISTRY FENCE: the SQL cron-enqueue path
///       (`enqueue_cron_registry_and_task_unless_db_dropped`) writes BOTH the
///       `_sys_worker` registry inventory bit AND the next-fire queue row under
///       ONE tombstone fence, so a strictly-sequential DROP-then-`cron.schedule`
///       re-creates NEITHER a stale registry row NOR a stale queue row; a live
///       (un-tombstoned) db_id still gets both.
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn dropped_db_tombstone_makes_cross_store_nextfire_orphan_impossible() {
    let pd_endpoints = std::env::var("PD_ENDPOINTS")
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let system_keyspace = format!(
        "_sys_tombstone_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let cfg = crate::worker::config::WorkerConfig {
        enabled: true,
        system_keyspace: system_keyspace.clone(),
        ..Default::default()
    };
    let system_store = crate::worker::init_gc_registry_store(pd_endpoints.clone(), &cfg)
        .await
        .expect("init system store");

    let keyspace = format!(
        "test_tombstone_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );

    let make_entry = |db_id: u64, task_id: i64| {
        TaskQueueEntry::new(
            keyspace.clone(),
            db_id,
            task_id,
            TaskType::Cron,
            "SELECT 1".to_string(),
            "admin".to_string(),
            100,
        )
        .with_schedule("*/5 * * * *".to_string())
    };

    // ── Scenario A: RACE — reap (tombstone put) vs enqueue (fenced put). ──
    //
    // Both txns are pessimistic. The enqueue takes `get_for_update` on the
    // tombstone key, which is exactly the key the reap writes — so the two
    // serialize on that key and cannot both commit. We drive the race a few
    // times to cover both commit interleavings, asserting the invariant on each.
    //
    // Each round must model a DISTINCT dropped database: a tombstone is durable
    // and never cleared (db_id is monotonic / non-recycled), and this scenario
    // guarantees a tombstone for the round's db_id is committed before the round
    // ends (the reap wins, or the enqueue-won leg commits one explicitly). So a
    // FRESH db_id per round is required for the round's "no prior tombstone"
    // precondition to hold — reusing one db_id would make every round after the
    // first observe the committed tombstone and (correctly) suppress.
    for round in 0..4i64 {
        // Disjoint from every other db_id used below (7_002..=7_005) so a
        // tombstone committed for a race round never fences a later scenario.
        let race_db_id = 7_100_u64 + round as u64;
        let task_id = 500 + round;
        let fire_time_ms = crate::worker::now_epoch_ms();
        let entry = make_entry(race_db_id, task_id);

        // Open both txns BEFORE either commits, so they genuinely contend.
        let mut reap_txn = system_store.begin().await.unwrap();
        let mut enq_txn = system_store.begin().await.unwrap();

        // Enqueue stages its fenced put (tombstone get_for_update sees no
        // tombstone yet → stages put_task_v2). It now holds a pessimistic lock
        // on the tombstone key from get_for_update.
        let staged = system_store
            .enqueue_task_v2_unless_db_dropped(&mut enq_txn, &entry, fire_time_ms)
            .await
            .expect("fenced enqueue stage must not error");
        assert!(
            staged,
            "with no prior tombstone the fenced enqueue must stage the put"
        );

        // Reap stages the tombstone put on the SAME key the enqueue locked.
        let reap_stage = system_store
            .put_dropped_db_tombstone(&mut reap_txn, &keyspace, race_db_id)
            .await;

        // Commit both; at most one may succeed — they conflict on the tombstone.
        let enq_ok = enq_txn.commit().await.is_ok();
        let reap_ok = match reap_stage {
            Ok(()) => reap_txn.commit().await.is_ok(),
            Err(_) => {
                // Reap staging was blocked by the enqueue's pessimistic lock —
                // i.e. the enqueue won contention; the reap made no progress.
                reap_txn.rollback().await.ok();
                false
            }
        };
        assert!(
            !(enq_ok && reap_ok),
            "INVARIANT VIOLATED: reap (tombstone) and fenced enqueue BOTH committed \
             for the same db_id — the cross-store orphan window is open (round {round})"
        );
        assert!(
            enq_ok || reap_ok,
            "at least one of {{reap, enqueue}} should make progress (round {round})"
        );

        // When DROP (the reap) won, the global queue must hold NO next-fire row
        // for this dropped db_id: the enqueue lost and wrote nothing.
        if reap_ok && !enq_ok {
            let mut check = system_store.begin().await.unwrap();
            let due = system_store
                .scan_due_v2(&mut check, i64::MAX, 100_000)
                .await
                .unwrap();
            assert!(
                !due.iter()
                    .any(|(_, d)| d.db_id == race_db_id && d.task_id == task_id),
                "INVARIANT VIOLATED: a stale next-fire row survived after DROP won \
                 the race (round {round})"
            );
            check.rollback().await.ok();
        }

        // If the enqueue happened to win this interleaving, retry the enqueue
        // against the now-known-dropped DB AFTER committing the tombstone: it
        // must now suppress (this is the retry-to-suppressed leg the production
        // path takes when its first commit conflicts).
        if enq_ok && !reap_ok {
            let mut t = system_store.begin().await.unwrap();
            system_store
                .put_dropped_db_tombstone(&mut t, &keyspace, race_db_id)
                .await
                .unwrap();
            t.commit().await.unwrap();

            let mut retry = system_store.begin().await.unwrap();
            let staged_again = system_store
                .enqueue_task_v2_unless_db_dropped(&mut retry, &entry, fire_time_ms)
                .await
                .unwrap();
            retry.rollback().await.ok();
            assert!(
                !staged_again,
                "after the tombstone is committed, a retried enqueue MUST suppress \
                 (round {round})"
            );
        }
    }

    // ── Scenario B: SEQUENCED — tombstone committed first, enqueue suppressed. ──
    //
    // This is the production DROP ordering: the reap commits the tombstone, then
    // any later cross-store enqueue cleanly returns false and writes nothing.
    let seq_db_id = 7_002_u64;
    let seq_task_id = 900_i64;
    {
        let mut t = system_store.begin().await.unwrap();
        system_store
            .put_dropped_db_tombstone(&mut t, &keyspace, seq_db_id)
            .await
            .unwrap();
        t.commit().await.unwrap();
    }
    {
        let mut t = system_store.begin().await.unwrap();
        let exists = system_store
            .dropped_db_tombstone_exists_for_update(&mut t, &keyspace, seq_db_id)
            .await
            .unwrap();
        assert!(exists, "committed tombstone must be readable for_update");
        t.rollback().await.ok();
    }
    {
        let entry = make_entry(seq_db_id, seq_task_id);
        let mut t = system_store.begin().await.unwrap();
        let staged = system_store
            .enqueue_task_v2_unless_db_dropped(&mut t, &entry, crate::worker::now_epoch_ms())
            .await
            .unwrap();
        assert!(
            !staged,
            "fenced enqueue must suppress for a tombstoned (dropped) DB"
        );
        t.commit().await.unwrap();
    }
    // Decisive end-state: zero next-fire rows for the sequenced dropped db_id.
    {
        let mut check = system_store.begin().await.unwrap();
        let due = system_store
            .scan_due_v2(&mut check, i64::MAX, 100_000)
            .await
            .unwrap();
        assert!(
            !due.iter().any(|(_, d)| d.db_id == seq_db_id),
            "INVARIANT VIOLATED: a next-fire row exists for a tombstoned DB"
        );
        check.rollback().await.ok();
    }

    // A tombstone for one db_id must NEVER fence a DIFFERENT db_id (db_id is
    // monotonic / non-recycled, so tombstones are 1:1 with a dropped database).
    {
        let live_db_id = 7_003_u64;
        let entry = make_entry(live_db_id, 1_001);
        let mut t = system_store.begin().await.unwrap();
        let staged = system_store
            .enqueue_task_v2_unless_db_dropped(&mut t, &entry, crate::worker::now_epoch_ms())
            .await
            .unwrap();
        assert!(
            staged,
            "a tombstone for another db_id must not fence a live db_id's enqueue"
        );
        t.rollback().await.ok();
    }

    // ── Scenario C: REGISTRY FENCE — the SQL cron-enqueue path writes BOTH the
    // `_sys_worker` registry inventory bit AND the next-fire queue row, and BOTH
    // must be fenced by the SAME tombstone. This is the strictly-sequential gap:
    // DROP DATABASE has fully committed (tombstone + registry delete + queue reap)
    // and THEN a `cron.schedule` for that db_id arrives. The registry write must
    // be suppressed too — otherwise it re-creates a stale inventory row that only
    // the self-healing registry sweep would later clean up, violating the "no
    // stale `_sys_worker` row" invariant. ──
    let reg_db_id = 7_004_u64;
    let reg_task_id = 1_200_i64;
    {
        // DROP's reap committed the tombstone for this db_id.
        let mut t = system_store.begin().await.unwrap();
        system_store
            .put_dropped_db_tombstone(&mut t, &keyspace, reg_db_id)
            .await
            .unwrap();
        t.commit().await.unwrap();
    }
    // Pre-state: no registry row exists for the dropped db_id (reap deleted it).
    {
        let mut t = system_store.begin().await.unwrap();
        let pre = system_store
            .get_worker_registry(&mut t, &keyspace, reg_db_id)
            .await
            .unwrap();
        assert!(
            pre.is_none(),
            "precondition: dropped db_id must have no registry row before the late enqueue"
        );
        t.rollback().await.ok();
    }
    // A late SQL cron-enqueue for the dropped db_id: BOTH registry and queue
    // writes must be suppressed by the single tombstone fence.
    {
        let entry = make_entry(reg_db_id, reg_task_id);
        let mut t = system_store.begin().await.unwrap();
        let staged = system_store
            .enqueue_cron_registry_and_task_unless_db_dropped(
                &mut t,
                &entry,
                crate::worker::now_epoch_ms(),
            )
            .await
            .unwrap();
        assert!(
            !staged,
            "the SQL cron-enqueue path must suppress for a tombstoned (dropped) DB"
        );
        t.commit().await.unwrap();
    }
    // Decisive end-state: NEITHER a registry inventory row NOR a next-fire queue
    // row may exist for the dropped db_id.
    {
        let mut t = system_store.begin().await.unwrap();
        let reg = system_store
            .get_worker_registry(&mut t, &keyspace, reg_db_id)
            .await
            .unwrap();
        assert!(
            reg.is_none(),
            "INVARIANT VIOLATED: a stale `_sys_worker` registry row was re-created \
             for a dropped db_id by the late SQL cron-enqueue path"
        );
        let due = system_store
            .scan_due_v2(&mut t, i64::MAX, 100_000)
            .await
            .unwrap();
        assert!(
            !due.iter().any(|(_, d)| d.db_id == reg_db_id),
            "INVARIANT VIOLATED: a next-fire row exists for a tombstoned DB after the \
             SQL cron-enqueue path"
        );
        t.rollback().await.ok();
    }

    // The registry fence must NOT block a live db_id: the SQL cron-enqueue path
    // for an un-tombstoned db_id writes BOTH the registry bit and the queue row.
    {
        let live_reg_db_id = 7_005_u64;
        let live_reg_task_id = 1_300_i64;
        let entry = make_entry(live_reg_db_id, live_reg_task_id);
        let mut t = system_store.begin().await.unwrap();
        let staged = system_store
            .enqueue_cron_registry_and_task_unless_db_dropped(
                &mut t,
                &entry,
                crate::worker::now_epoch_ms(),
            )
            .await
            .unwrap();
        assert!(
            staged,
            "the SQL cron-enqueue path must enqueue for a live (un-tombstoned) db_id"
        );
        t.commit().await.unwrap();

        let mut t = system_store.begin().await.unwrap();
        let reg = system_store
            .get_worker_registry(&mut t, &keyspace, live_reg_db_id)
            .await
            .unwrap();
        assert!(
            reg.is_some_and(|r| r.task_types & TASK_TYPE_CRON != 0),
            "live db_id must get a registry row with the cron bit set"
        );
        let due = system_store
            .scan_due_v2(&mut t, i64::MAX, 100_000)
            .await
            .unwrap();
        assert!(
            due.iter()
                .any(|(_, d)| d.db_id == live_reg_db_id && d.task_id == live_reg_task_id),
            "live db_id must get a next-fire queue row"
        );
        t.rollback().await.ok();
    }
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
        cache_nonce: 0,
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
        cache_nonce: 0,
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
        cache_nonce: 0,
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
        cache_nonce: 0,
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
        cache_nonce: 0,
    };
    // Exactly at boundary: not oversize (uses >).
    let result = super::check_graph_oversize_freeze(super::HNSW_GRAPH_MAX_BYTES, &meta);
    assert!(result.is_none());
}

#[test]
fn hnsw_s3_first_migration_deletes_legacy_tikv_graph_by_previous_version() {
    assert!(
        super::helpers::should_delete_legacy_tikv_graph_after_s3_migration(0),
        "graph_version=0 means the previous graph still lives in TiKV and must be deleted after S3 migration"
    );
    assert!(
        !super::helpers::should_delete_legacy_tikv_graph_after_s3_migration(1),
        "S3 graph versions do not need legacy TiKV graph cleanup"
    );
    assert!(
        !super::helpers::should_delete_legacy_tikv_graph_after_s3_migration(7_401_000_000_000_000),
        "TSO-sized S3 graph versions must not be treated as first-migration sentinels"
    );
}

#[test]
fn hnsw_s3_merge_upload_is_liveness_locked_and_commit_uncertain_retained() {
    let helper_source = include_str!("helpers.rs");
    let merge_fn = helper_source
        .split("pub(super) async fn execute_hnsw_merge(")
        .nth(1)
        .expect("execute_hnsw_merge helper must exist");
    let s3_branch = merge_fn
        .split("if crate::sql::hnsw::s3::hnsw_s3_client().is_some()")
        .nth(1)
        .and_then(|rest| rest.split("} else {").next())
        .expect("execute_hnsw_merge must have an S3 branch");

    assert!(
        s3_branch.contains("put_hnsw_s3_graph_with_intent"),
        "HNSW S3 merge must upload through the external-object intent helper"
    );
    assert!(
        s3_branch.contains("hnsw_s3_graph_version_for_txn")
            && !s3_branch.contains("graph_version + 1"),
        "HNSW S3 merge must allocate collision-free graph versions from the source transaction TSO"
    );
    assert!(
        s3_branch.contains("should_delete_legacy_tikv_graph_after_s3_migration(previous_version)")
            && !s3_branch.contains("new_version == 1"),
        "first TiKV-to-S3 migration cleanup must key off previous graph storage, not the new TSO version"
    );

    let upload_helper = helper_source
        .split("pub(crate) async fn put_hnsw_s3_graph_with_intent(")
        .nth(1)
        .and_then(|rest| {
            rest.split("pub(crate) async fn cleanup_hnsw_s3_graph_upload_after_failed_txn")
                .next()
        })
        .expect("worker S3 upload helper must exist before cleanup helper");
    let intent_pos = upload_helper
        .find("put_hnsw_s3_graph_upload_intent")
        .expect("upload helper must write a durable intent before S3 PUT");
    let fence_pos = upload_helper
        .find("assert_database_alive_for_update(tenant_txn, db_id)")
        .expect("upload helper must lock DB liveness before external upload");
    let put_pos = upload_helper
        .find(".put_graph(")
        .expect("upload helper must upload graph");
    assert!(
        intent_pos < fence_pos && fence_pos < put_pos,
        "HNSW S3 upload helper must write intent, then lock DB liveness, then put_graph"
    );

    // The pre-commit liveness fence (assert_database_alive_for_update) is a
    // DEFINITE non-commit when it errors (DB dropped, or get_for_update failed —
    // commit has not run). It MUST be a SEPARATE fallible step before
    // txn.commit(), NOT fused with the commit in one async block, so its error
    // can be cleaned up rather than retained. A dropped database that short-
    // circuits a fused assert+commit would otherwise reach the retain path and
    // leave a no-meta S3 orphan.
    assert!(
        !merge_fn.contains("let commit_result: Result<()>"),
        "the pre-commit liveness fence must NOT be fused with txn.commit() in one \
         async block: a definite pre-commit abort would wrongly reach the retain path"
    );
    let liveness_fence_block = merge_fn
        .split("if let Err(e) = store")
        .nth(1)
        .and_then(|rest| rest.split("if let Err(e) = txn.commit().await").next())
        .expect("execute_hnsw_merge must run the liveness fence as a separate pre-commit step");
    assert!(
        liveness_fence_block.contains("assert_database_alive_for_update(&mut txn, db_id)"),
        "the separate pre-commit step must be the DB liveness fence"
    );
    assert!(
        liveness_fence_block.contains("cleanup_uploaded_hnsw_s3_graph_after_failed_batch"),
        "a pre-commit liveness-fence error is a DEFINITE non-commit and MUST clean up \
         the speculative S3 upload (cleanup, not retain) to avoid a no-meta S3 orphan \
         on a DROP DATABASE race"
    );
    assert!(
        !liveness_fence_block.contains("retain_uploaded_hnsw_s3_graph_after_uncertain_commit"),
        "retain is only correct for AMBIGUOUS commit errors, never for the definite \
         pre-commit liveness fence"
    );
    assert!(
        helper_source.contains("cleanup_uploaded_hnsw_s3_graph_after_failed_batch")
            && helper_source.contains("cleanup_hnsw_s3_graph_upload_after_failed_txn"),
        "HNSW S3 merge must retain the speculative-upload cleanup helpers"
    );

    // The commit-error branch handles ONLY txn.commit() failures, which are
    // genuinely ambiguous (TiKV may have committed despite a client error).
    let commit_error_branch = merge_fn
        .split("if let Err(e) = txn.commit().await")
        .nth(1)
        .and_then(|rest| rest.split("return Err(e);").next())
        .expect("execute_hnsw_merge must handle commit errors explicitly");
    assert!(
        commit_error_branch.contains("retain_uploaded_hnsw_s3_graph_after_uncertain_commit"),
        "HNSW S3 merge must retain uploaded graph and durable intent after TiKV commit errors"
    );
    assert!(
        !commit_error_branch.contains("cleanup_uploaded_hnsw_s3_graph_after_failed_batch")
            && !commit_error_branch.contains("cleanup_hnsw_s3_graph_upload_after_failed_txn")
            && !commit_error_branch.contains("delete_hnsw_s3_graph_upload_intent"),
        "TiKV commit errors can be ambiguous; the merge commit-error branch must not delete S3 graph objects or upload intents"
    );
}

/// REGRESSION GUARD (P1 commit-adjacency), BEHAVIORAL: the HNSW merge claim-lease
/// fence must be COMMIT-ADJACENT, not merely at loop-top. The normal batch commit
/// is reached only after long work (delta scan, graph build/serialize, TiKV
/// mutations) during which the claim lease can lapse. If the only fence were at
/// loop-top, a second worker could take over the expired claim while the original
/// commits the batch -> duplicate tenant write (the at-most-once hole).
///
/// This drives the REAL `execute_hnsw_merge` against TiKV with a lease-cancel
/// fuse armed to fire on the SECOND `bail_if_cancelled` call: check 1 (loop-top)
/// PASSES, so the merge does all of its work (scans the delta, builds the graph,
/// writes graph+meta into the batch txn); check 2 (the commit-adjacent fence
/// immediately before `txn.commit`) FIRES. We then assert the observable
/// commit-adjacency contract:
///   - the merge returns the canonical claim-cancelled error
///     (`is_claim_cancelled_error` true);
///   - the tenant batch did NOT commit — the delta is still present (not
///     consumed) and the graph blob / meta graph_version are unchanged;
///   - no speculative S3 upload is retained (this is the no-S3 TiKV path, so
///     `uploaded_s3_graph_version` is always None and the bail's cleanup branch
///     is a no-op — there is no no-meta S3 orphan to leave behind).
/// A loop-top-only fence would have ALREADY committed by check 2, consuming the
/// delta and bumping the graph blob — so this fails if the fence regresses to
/// loop-top only.
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn hnsw_merge_commit_adjacent_lease_cancel_aborts_tenant_commit_behaviorally() {
    use crate::sql::hnsw::storage::{
        hnsw_delta_key, hnsw_graph_key, hnsw_meta_key, HnswDelta, HnswLabelMode, HnswMeta,
    };
    use crate::txn::txn_put;

    // This test exercises the TiKV (no-S3) path. If a stray HNSW_S3_BUCKET is set
    // in the environment, the S3 branch would run instead — skip rather than
    // assert against the wrong path.
    if crate::sql::hnsw::s3::hnsw_s3_client().is_some() {
        eprintln!("skipping: HNSW S3 is configured; this test targets the TiKV path");
        return;
    }

    let pd_endpoints = std::env::var("PD_ENDPOINTS")
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let pool = Arc::new(crate::pool::TikvClientPool::new(pd_endpoints.clone()));

    let keyspace = format!(
        "test_merge_fence_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    // The pool's tenant connect (`with_keyspace`) requires the keyspace to exist
    // in PD; pre-create it with the canonical PD-API helper (no-op under TLS).
    crate::worker::ensure_system_keyspace(&pd_endpoints, &keyspace)
        .await
        .expect("pre-create tenant keyspace in PD");
    // Acquiring a tenant handle bootstraps db_id=1 (postgres), so the pre-commit
    // liveness fence in the merge passes — only the lease fence can abort here.
    let store = {
        let handle = pool
            .acquire(Some(keyspace.clone()))
            .await
            .expect("acquire tenant handle");
        handle.store().clone()
    };
    let db_id = 1_u64;
    let table_id = 4242_u64;
    let index_id = 7_u64;

    // Seed a real, mergeable (storage_version=1, not frozen) HNSW index: meta +
    // one delta. The merge will scan the delta, build the graph, write graph+meta
    // into the batch txn, then hit the commit-adjacent fence.
    let meta = HnswMeta {
        count: 0,
        capacity: 0,
        dimensions: 4,
        distance_metric: "l2".to_string(),
        m: 16,
        ef_construction: 200,
        storage_version: 1,
        label_mode: HnswLabelMode::Direct,
        frozen: false,
        graph_version: 0,
        dropped_at: None,
        cache_nonce: 0,
    };
    let delta = HnswDelta {
        label: 1,
        vector: vec![0.1_f32, 0.2, 0.3, 0.4],
    };
    let delta_key = hnsw_delta_key(db_id, table_id, index_id, 1);
    {
        let mut txn = store.begin().await.unwrap();
        txn_put(
            &mut txn,
            hnsw_meta_key(db_id, table_id, index_id),
            serde_json::to_vec(&meta).unwrap(),
        )
        .await
        .unwrap();
        txn_put(
            &mut txn,
            delta_key.clone(),
            bincode::serialize(&delta).unwrap(),
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();
    }

    // Arm the fuse to fire on check 2: loop-top passes, commit-adjacent fires.
    let lease_cancel = crate::worker::LeaseCancel::new_tripping_at_check(2);
    let result = execute_hnsw_merge(&store, db_id, table_id, index_id, &lease_cancel).await;

    // 1. The merge bails with the canonical claim-cancelled error.
    let err = result.expect_err("commit-adjacent lease cancel must abort the merge");
    assert!(
        is_claim_cancelled_error(&err),
        "fence must propagate the canonical claim-cancelled error, got: {err}"
    );

    // 2. The tenant batch did NOT commit: the delta is still present (not
    //    consumed) and the meta graph_version is unchanged.
    {
        let mut txn = store.begin().await.unwrap();
        // The single seeded delta must still be present at its exact key: a
        // commit-adjacent bail must NOT consume it (the batch did not commit).
        assert!(
            txn.get(delta_key.clone()).await.unwrap().is_some(),
            "a commit-adjacent bail must NOT consume the delta (the batch did not commit)"
        );
        let meta_after: HnswMeta = serde_json::from_slice(
            &txn.get(hnsw_meta_key(db_id, table_id, index_id))
                .await
                .unwrap()
                .expect("meta must still exist"),
        )
        .unwrap();
        assert_eq!(
            meta_after.graph_version, 0,
            "graph_version must be unchanged: the batch never committed"
        );
        // No new graph blob was committed on the TiKV path either.
        assert!(
            txn.get(hnsw_graph_key(db_id, table_id, index_id))
                .await
                .unwrap()
                .is_none(),
            "no graph blob may be committed when the commit-adjacent fence aborts"
        );
        txn.rollback().await.ok();
    }

    // Cleanup the seeded keys.
    {
        let mut txn = store.begin().await.unwrap();
        crate::txn::txn_delete(&mut txn, hnsw_meta_key(db_id, table_id, index_id))
            .await
            .ok();
        crate::txn::txn_delete(&mut txn, delta_key).await.ok();
        txn.commit().await.ok();
    }
}

#[test]
fn hnsw_s3_graph_uploads_go_through_external_object_helper() {
    let production_sources: &[(&str, &str)] = &[
        (
            "sql/ddl/create_index.rs",
            include_str!("../../sql/ddl/create_index.rs"),
        ),
        (
            "storage/tikv_store/tables.rs",
            include_str!("../../storage/tikv_store/tables.rs"),
        ),
    ];

    for (path, source) in production_sources {
        let prod_source = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            !prod_source.contains(".put_graph("),
            "{path} must not call S3 put_graph directly; use put_hnsw_s3_graph_with_intent"
        );
        assert!(
            prod_source.contains("put_hnsw_s3_graph_with_intent"),
            "{path} must route HNSW S3 graph uploads through the external-object intent helper"
        );
    }

    let helper_source = include_str!("helpers.rs");
    let merge_fn = helper_source
        .split("pub(super) async fn execute_hnsw_merge(")
        .nth(1)
        .expect("execute_hnsw_merge helper must exist");
    assert!(
        !merge_fn.contains(".put_graph("),
        "execute_hnsw_merge must not call S3 put_graph directly; use put_hnsw_s3_graph_with_intent"
    );
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
                cache_nonce: 0,
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
                cache_nonce: 0,
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

fn skip_ws(source: &str, mut pos: usize) -> usize {
    while pos < source.len() && source.as_bytes()[pos].is_ascii_whitespace() {
        pos += 1;
    }
    pos
}

fn skip_attribute(source: &str, mut pos: usize) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0i32;
    while pos < bytes.len() {
        match bytes[pos] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return skip_ws(source, pos + 1);
                }
            }
            _ => {}
        }
        pos += 1;
    }
    source.len()
}

fn skip_braced_item(source: &str, mut pos: usize) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0i32;
    let mut in_line_comment = false;
    let mut in_string = false;
    let mut in_raw_string = false;
    let mut raw_hashes = 0usize;
    let mut escaped = false;

    while pos < bytes.len() {
        let b = bytes[pos];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            pos += 1;
            continue;
        }
        if in_raw_string {
            if b == b'"' {
                let mut hashes = 0usize;
                while pos + 1 + hashes < bytes.len()
                    && bytes[pos + 1 + hashes] == b'#'
                    && hashes < raw_hashes
                {
                    hashes += 1;
                }
                if hashes == raw_hashes {
                    in_raw_string = false;
                    pos += 1 + hashes;
                    continue;
                }
            }
            pos += 1;
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            pos += 1;
            continue;
        }

        match b {
            b'/' if pos + 1 < bytes.len() && bytes[pos + 1] == b'/' => {
                in_line_comment = true;
                pos += 2;
                continue;
            }
            b'r' if pos + 1 < bytes.len() => {
                let mut hashes = 0usize;
                while pos + 1 + hashes < bytes.len() && bytes[pos + 1 + hashes] == b'#' {
                    hashes += 1;
                }
                if pos + 1 + hashes < bytes.len() && bytes[pos + 1 + hashes] == b'"' {
                    in_raw_string = true;
                    raw_hashes = hashes;
                    pos += 2 + hashes;
                    continue;
                }
            }
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return pos + 1;
                }
            }
            _ => {}
        }
        pos += 1;
    }

    source.len()
}

fn skip_cfg_test_item(source: &str, mut pos: usize) -> usize {
    pos = skip_ws(source, pos);
    while source[pos..].starts_with("#[") {
        pos = skip_attribute(source, pos + 1);
        pos = skip_ws(source, pos);
        if pos >= source.len() {
            return pos;
        }
    }

    let semi = source[pos..].find(';').map(|offset| pos + offset);
    let brace = source[pos..].find('{').map(|offset| pos + offset);
    match (semi, brace) {
        (Some(semi), Some(brace)) if semi < brace => semi + 1,
        (Some(semi), None) => semi + 1,
        (_, Some(brace)) => skip_braced_item(source, brace),
        _ => source.len(),
    }
}

fn strip_cfg_test_items(source: &str) -> String {
    const CFG_TEST: &str = "#[cfg(test)]";
    let mut stripped = String::with_capacity(source.len());
    let mut pos = 0usize;

    while let Some(offset) = source[pos..].find(CFG_TEST) {
        let cfg_pos = pos + offset;
        stripped.push_str(&source[pos..cfg_pos]);
        pos = skip_cfg_test_item(source, cfg_pos + CFG_TEST.len());
    }

    stripped.push_str(&source[pos..]);
    stripped
}

#[test]
fn cfg_test_stripper_keeps_production_items_after_test_items() {
    let source = r#"
#[cfg(test)]
mod tests;

pub async fn production_one(store: &TikvStore) {
    let _txn = store.begin().await?;
}

#[cfg(test)]
#[allow(dead_code)]
fn helper() {
    let s = "{ not a real brace }";
}

pub async fn production_two(store: &TikvStore) {
    let _txn = store.begin().await?;
}
"#;
    let stripped = strip_cfg_test_items(source);
    assert!(!stripped.contains("mod tests"));
    assert!(!stripped.contains("fn helper"));
    assert!(stripped.contains("production_one"));
    assert!(stripped.contains("production_two"));
}

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
            if trimmed.contains(".begin().await") || trimmed.contains(".begin_optimistic().await") {
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
        ("worker/engine.rs", include_str!("../engine.rs")),
        ("worker/engine/helpers.rs", include_str!("./helpers.rs")),
        ("worker/gc.rs", include_str!("../gc.rs")),
        ("worker/gc/hnsw_impl.rs", include_str!("../gc/hnsw_impl.rs")),
        ("cron/worker.rs", include_str!("../../cron/worker.rs")),
        (
            "sql/ddl/create_index.rs",
            include_str!("../../sql/ddl/create_index.rs"),
        ),
        ("sql/ddl/mod.rs", include_str!("../../sql/ddl/mod.rs")),
        (
            "sql/executor/bg_sql.rs",
            include_str!("../../sql/executor/bg_sql.rs"),
        ),
        (
            "sql/executor/core/mod.rs",
            include_str!("../../sql/executor/core/mod.rs"),
        ),
        (
            "sql/executor/core/guc_engine.rs",
            include_str!("../../sql/executor/core/guc_engine.rs"),
        ),
        (
            "sql/executor/cron.rs",
            include_str!("../../sql/executor/cron.rs"),
        ),
        (
            "sql/executor/dml_analyzed/mod.rs",
            include_str!("../../sql/executor/dml_analyzed/mod.rs"),
        ),
        (
            "sql/executor/procedure/materialized_views.rs",
            include_str!("../../sql/executor/procedure/materialized_views.rs"),
        ),
        (
            "sql/executor/table_utils/mod.rs",
            include_str!("../../sql/executor/table_utils/mod.rs"),
        ),
        (
            "session_context.rs",
            include_str!("../../session_context.rs"),
        ),
        ("main.rs", include_str!("../../main.rs")),
        (
            "protocol/handler/dynamic/startup.rs",
            include_str!("../../protocol/handler/dynamic/startup.rs"),
        ),
        (
            "protocol/handler/dynamic/query.rs",
            include_str!("../../protocol/handler/dynamic/query.rs"),
        ),
        (
            "sql/session/transaction.rs",
            include_str!("../../sql/session/transaction.rs"),
        ),
        ("auth/rbac.rs", include_str!("../../auth/rbac.rs")),
        (
            "extensions/fs/ws/auth.rs",
            include_str!("../../extensions/fs/ws/auth.rs"),
        ),
        (
            "storage/tikv_store/mod.rs",
            include_str!("../../storage/tikv_store/mod.rs"),
        ),
        (
            "storage/tikv_store/migrations.rs",
            include_str!("../../storage/tikv_store/migrations.rs"),
        ),
        (
            "storage/tikv_store/sequences.rs",
            include_str!("../../storage/tikv_store/sequences.rs"),
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
        // [reconcile] Reads one raw-cursor registry page + immediate rollback.
        "registry_sweep_tick",
        // [reconcile] Deletes one registry row after liveness check.
        "process_registry_sweep_entry",
        // [lookup] Point read of registry backoff / DISABLED keyspace retention.
        "registry_sweep_should_skip",
        // [reconcile] Reads system queue + tenant cron state; bounded metadata operations.
        "reconcile_cron_for_db",
        // [reconcile] Scans one table-schema page for a single DB; queue-aware repair + commit.
        "reconcile_incomplete_cic_indexes_for_db_safe",
        // [lookup] Point read of persisted storage stats; immediate rollback.
        "storage_scan_due",
        // [lookup] Point read of tenant-local applied marker; immediate rollback.
        "latest_storage_scan_applied_work_id",
        // [claim] StorageScan state claim + bounded capacity-token repair in one
        // system txn; no snapshot is held across PD work.
        "claim_storage_scan_derived_run",
        // [finalize] StorageScan state update + capacity-token release in one
        // system txn after the effect has completed.
        "finish_storage_scan_derived_run",
        // [claim] Pessimistic claim attempt: 1 key check + commit.
        "claim_and_execute_core",
        // [claim] Release a just-won claim: single key delete + immediate commit.
        "release_claim",
        // [claim] Per-renewal txn: get_for_update own claim + put + immediate
        // commit (or rollback). Each renewal is independent and short-lived —
        // it does not hold a snapshot across the task's execution.
        "spawn_claim_lease_renewer",
        // [claim] One lease-renewal attempt extracted from the loop above: begin
        // + get_for_update own claim + put + immediate commit (or rollback).
        // Short-lived, holds no snapshot across the task's execution.
        "renew_lease_once",
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
        // [enqueue] Single put to system store queue + commit.
        "enqueue_storage_scan",
        // [enqueue] Shared storage-scan enqueue helper; writes one singleton
        // queue entry in the system store, then commits or rolls back.
        "enqueue_storage_scan_at",
        // [pd lookup + finalize] PD HTTP stats lookup, then one short tenant
        // stats-key write with immediate commit.
        "execute_storage_size_scan",
        // [pd lookup + finalize] Derived path uses short precheck/finalize tenant
        // txns; the scheduler owner fence txn itself registers with GC.
        "execute_storage_size_scan_derived",
        // [enqueue] Manual/legacy refresh nudge writes one derived state row and
        // registry bit in a single system txn.
        "request_storage_scan_refresh",
        // [reconcile] Scans DDL journal + cleans orphaned data in batches with txn rotation.
        "reconcile_ddl_journal_for_db",
        // ── worker/engine/helpers.rs ──

        // [enqueue] Writes one durable HNSW S3 upload intent in system store, then commits.
        "put_hnsw_s3_graph_with_intent",
        // [finalize] Best-effort delete of one durable HNSW S3 upload intent, then commits.
        "delete_hnsw_s3_graph_upload_intent_best_effort",
        // ── worker/gc.rs ──

        // [lookup] Neutralize GC instance state: single key write + commit.
        "clear_gc_instance_state",
        // [lookup] Publish GC instance state: single key write + commit.
        "publish_gc_instance_state",
        // [lookup] Read all GC instance states (small registry) + rollback.
        "advance_gc_safepoint",
        // [claim] Scans claim batch (bounded by batch_size) + commit/rollback.
        "cleanup_orphan_claims_batch",
        // [lookup] Read GC instance states (small registry) + rollback.
        "reap_stale_gc_instance_states",
        // [lookup] Delete stale GC instance rows (bounded) + commit.
        "reap_stale_gc_instance_states_from_scan",
        // [lookup] Read all HNSW metas for S3 GC sweep; bounded scan + rollback.
        "read_all_hnsw_metas",
        // [lookup] Point-read HNSW metas referenced by one S3 page + rollback.
        "read_hnsw_metas_for_indexes",
        // [reconcile] Bounded system-store intent pages plus tenant point reads/S3 cleanup.
        "cleanup_hnsw_s3_external_object_intents",
        "cleanup_hnsw_s3_graph_upload_intents",
        "cleanup_hnsw_s3_db_prefix_cleanup_intents",
        // [finalize] Single system-store intent delete + commit.
        "delete_hnsw_s3_graph_upload_intent",
        "delete_hnsw_s3_db_prefix_cleanup_intent",
        // [lookup] Scan one DB's S3 orphan prefixes + delete; bounded to current sweep entry.
        "sweep_hnsw_s3_orphans_for_entry",
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
        // [pre-check] gc_database uses two short-lived read-only txns (cron_enabled
        // check + job metadata load) with immediate rollback. The actual batch
        // processing happens in gc_database_batch_inner which has track_worker_txn.
        "gc_database",
        // (gc_database_batch_inner IS long-lived and MUST have track_worker_txn — not in allowlist)

        // ── sql/ddl/create_index.rs ──

        // [enqueue] Write queue entry + registry update for CIC backfill; immediate commit.
        "execute_create_index",
        // [enqueue] Write registry entry for HNSW merge discovery; immediate commit.
        // Called from execute_create_index (session-scoped); system_store.begin() is short-lived.
        "build_hnsw_index",
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

        // [lookup] Virtual table helper; uses caller-owned session txn plus a
        // short-lived system-store stats read for trigger queue stats.
        "get_table_data_filtered",
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
        // [repair] Disabled-worker DROP-intent repair uses bounded page reads and
        // immediate per-intent commits/rollbacks; it never holds a worker snapshot
        // across task execution.
        "repair_dropping_db_intents_once",
        // ── worker/gc.rs ──

        // [lookup] GC registry reader for safepoint computation; bounded scan +
        // immediate rollback, not a long-lived worker transaction.
        "scan_gc_instance_states",
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

        // [facade] Dead-code facade constructor; first production caller must
        // register any long-lived returned transaction at the call site.
        "begin_facade",
        // [facade] Dead-code facade constructor; first production caller must
        // register any long-lived returned transaction at the call site.
        "begin_optimistic_facade",
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
        // Strip only cfg(test) items; production code may appear after test-only
        // imports/modules/helpers near the top of a file.
        let prod_source = strip_cfg_test_items(source);

        let fns = extract_fns_with_begin(&prod_source);

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

// ── is_retryable_region_error tests ────────────────────────────────────────

/// Helper: wrap a tikv_client::Error in anyhow so is_retryable_region_error can inspect it.
fn anyhow_tikv(err: tikv_client::Error) -> anyhow::Error {
    anyhow::Error::new(err)
}

#[test]
fn retryable_region_error_matches_region_not_found() {
    let re = tikv_client::proto::errorpb::Error {
        region_not_found: Some(tikv_client::proto::errorpb::RegionNotFound::default()),
        ..Default::default()
    };
    let err = anyhow_tikv(tikv_client::Error::RegionError(Box::new(re)));
    assert!(super::is_retryable_region_error(&err));
}

#[test]
fn retryable_region_error_matches_epoch_not_match() {
    let re = tikv_client::proto::errorpb::Error {
        epoch_not_match: Some(tikv_client::proto::errorpb::EpochNotMatch::default()),
        ..Default::default()
    };
    let err = anyhow_tikv(tikv_client::Error::RegionError(Box::new(re)));
    assert!(super::is_retryable_region_error(&err));
}

#[test]
fn retryable_region_error_matches_not_leader() {
    let re = tikv_client::proto::errorpb::Error {
        not_leader: Some(tikv_client::proto::errorpb::NotLeader::default()),
        ..Default::default()
    };
    let err = anyhow_tikv(tikv_client::Error::RegionError(Box::new(re)));
    assert!(super::is_retryable_region_error(&err));
}

#[test]
fn retryable_region_error_rejects_server_is_busy() {
    let re = tikv_client::proto::errorpb::Error {
        server_is_busy: Some(tikv_client::proto::errorpb::ServerIsBusy::default()),
        ..Default::default()
    };
    let err = anyhow_tikv(tikv_client::Error::RegionError(Box::new(re)));
    assert!(!super::is_retryable_region_error(&err));
}

#[test]
fn retryable_region_error_rejects_raft_entry_too_large() {
    let re = tikv_client::proto::errorpb::Error {
        raft_entry_too_large: Some(tikv_client::proto::errorpb::RaftEntryTooLarge::default()),
        ..Default::default()
    };
    let err = anyhow_tikv(tikv_client::Error::RegionError(Box::new(re)));
    assert!(!super::is_retryable_region_error(&err));
}

#[test]
fn retryable_region_error_rejects_disk_full() {
    let re = tikv_client::proto::errorpb::Error {
        disk_full: Some(tikv_client::proto::errorpb::DiskFull::default()),
        ..Default::default()
    };
    let err = anyhow_tikv(tikv_client::Error::RegionError(Box::new(re)));
    assert!(!super::is_retryable_region_error(&err));
}

#[test]
fn retryable_region_error_rejects_non_region_error() {
    let err = anyhow_tikv(tikv_client::Error::StringError("not a region error".into()));
    assert!(!super::is_retryable_region_error(&err));
}

#[test]
fn retryable_region_error_unwraps_undetermined() {
    let re = tikv_client::proto::errorpb::Error {
        region_not_found: Some(tikv_client::proto::errorpb::RegionNotFound::default()),
        ..Default::default()
    };
    let inner = tikv_client::Error::RegionError(Box::new(re));
    let err = anyhow_tikv(tikv_client::Error::UndeterminedError(Box::new(inner)));
    assert!(super::is_retryable_region_error(&err));
}

#[test]
fn retryable_region_error_rejects_mixed_batch_with_non_retryable() {
    // A batch containing [RegionNotFound, DiskFull] must NOT be retried —
    // the DiskFull error is non-retryable and won't resolve on retry.
    let retryable = tikv_client::Error::RegionError(Box::new(tikv_client::proto::errorpb::Error {
        region_not_found: Some(tikv_client::proto::errorpb::RegionNotFound::default()),
        ..Default::default()
    }));
    let non_retryable =
        tikv_client::Error::RegionError(Box::new(tikv_client::proto::errorpb::Error {
            disk_full: Some(tikv_client::proto::errorpb::DiskFull::default()),
            ..Default::default()
        }));
    let err = anyhow_tikv(tikv_client::Error::ExtractedErrors(vec![
        retryable,
        non_retryable,
    ]));
    assert!(
        !super::is_retryable_region_error(&err),
        "Mixed batch with DiskFull should NOT be retryable"
    );
}

#[test]
fn retryable_region_error_accepts_batch_all_retryable() {
    let err1 = tikv_client::Error::RegionError(Box::new(tikv_client::proto::errorpb::Error {
        region_not_found: Some(tikv_client::proto::errorpb::RegionNotFound::default()),
        ..Default::default()
    }));
    let err2 = tikv_client::Error::RegionError(Box::new(tikv_client::proto::errorpb::Error {
        not_leader: Some(tikv_client::proto::errorpb::NotLeader::default()),
        ..Default::default()
    }));
    let err = anyhow_tikv(tikv_client::Error::ExtractedErrors(vec![err1, err2]));
    assert!(
        super::is_retryable_region_error(&err),
        "Batch of all-retryable errors should be retryable"
    );
}

// ── Regression test: reproduce #2271 and verify fix ────────────────────────
//
// Simulates the exact production scenario: a TiKV scan returns RegionNotFound
// (region 32451 was split/merged) on the first N attempts, then succeeds on a
// fresh transaction.
//
// Part 1 (old_pattern): no retry → permanent failure after first region error.
// Part 2 (new_pattern): retry with fresh txn → recovers after transient errors.

/// Simulate a TiKV scan that fails with RegionNotFound for the first
/// `failures` calls, then returns Ok on subsequent calls.
struct RegionErrorSimulator {
    failures_remaining: std::sync::atomic::AtomicU32,
}

impl RegionErrorSimulator {
    fn new(failures: u32) -> Self {
        Self {
            failures_remaining: std::sync::atomic::AtomicU32::new(failures),
        }
    }

    /// Simulate a TiKV scan. Each call represents a fresh transaction attempt.
    fn scan(&self) -> anyhow::Result<Vec<u8>> {
        let remaining = self
            .failures_remaining
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |v| if v > 0 { Some(v - 1) } else { Some(0) },
            )
            .unwrap();
        if remaining > 0 {
            // Reproduce the exact error from production logs:
            // "region 32451 is missing"
            let re = tikv_client::proto::errorpb::Error {
                message: "region 32451 is missing".to_string(),
                region_not_found: Some(tikv_client::proto::errorpb::RegionNotFound {
                    region_id: 32451,
                }),
                ..Default::default()
            };
            Err(anyhow::Error::new(tikv_client::Error::RegionError(
                Box::new(re),
            )))
        } else {
            Ok(vec![1, 2, 3]) // simulated delta data
        }
    }
}

/// Reproduce #2271: old code pattern — single attempt, no retry.
/// A single RegionNotFound causes permanent failure.
#[test]
fn regression_2271_old_pattern_fails_permanently() {
    // Simulate: region error on first attempt, would succeed on second.
    let sim = RegionErrorSimulator::new(1);

    // Old code pattern: call scan once, propagate error via `?`.
    let result: anyhow::Result<Vec<u8>> = sim.scan();

    // OLD BEHAVIOR: error propagates, sweep aborts for this tenant.
    // Next sweep (600s later) would hit the same error.
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        super::is_retryable_region_error(&err),
        "Error should be a retryable region error but was: {err}"
    );

    // The scan WOULD succeed now (region cache refreshed), but old code
    // never retries — it already returned Err to the caller.
    assert!(
        sim.scan().is_ok(),
        "Second attempt would succeed, but old code never tries it"
    );
}

/// Verify fix: new code pattern — retry loop with fresh transaction.
/// Transient RegionNotFound errors recover after retry.
#[test]
fn regression_2271_new_pattern_retries_and_recovers() {
    // Simulate: 2 region errors, then success on 3rd attempt.
    let sim = RegionErrorSimulator::new(2);

    // New code pattern: mirrors the retry loop in enqueue_pending_hnsw_merges.
    let mut result: anyhow::Result<Vec<u8>> = Err(anyhow::anyhow!("not started"));
    for attempt in 0..=super::REGION_ERROR_MAX_RETRIES {
        // Each iteration = fresh transaction (fresh region cache).
        result = sim.scan();
        match &result {
            Ok(_) => break,
            Err(e)
                if super::is_retryable_region_error(e)
                    && attempt < super::REGION_ERROR_MAX_RETRIES =>
            {
                // In production: region_error_backoff(attempt).await
                // In test: no sleep needed, just retry.
                continue;
            }
            Err(_) => break, // non-retryable or retries exhausted
        }
    }

    // NEW BEHAVIOR: recovered after 2 failures + 1 success.
    assert!(
        result.is_ok(),
        "Should recover after transient region errors, got: {}",
        result.unwrap_err()
    );
}

/// Verify fix: retries exhausted → still fails (no infinite retry).
#[test]
fn regression_2271_retries_exhausted_still_fails() {
    // Simulate: more failures than retries allowed.
    let sim = RegionErrorSimulator::new(super::REGION_ERROR_MAX_RETRIES + 1);

    let mut result: anyhow::Result<Vec<u8>> = Err(anyhow::anyhow!("not started"));
    for attempt in 0..=super::REGION_ERROR_MAX_RETRIES {
        result = sim.scan();
        match &result {
            Ok(_) => break,
            Err(e)
                if super::is_retryable_region_error(e)
                    && attempt < super::REGION_ERROR_MAX_RETRIES =>
            {
                continue;
            }
            Err(_) => break,
        }
    }

    // After REGION_ERROR_MAX_RETRIES+1 failures, we give up (no infinite loop).
    assert!(result.is_err(), "Should fail when retries exhausted");
}

/// Verify fix: non-retryable errors (server_is_busy) are NOT retried.
#[test]
fn regression_2271_non_retryable_region_error_not_retried() {
    let call_count = std::sync::atomic::AtomicU32::new(0);

    let do_scan = || -> anyhow::Result<Vec<u8>> {
        call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let re = tikv_client::proto::errorpb::Error {
            server_is_busy: Some(tikv_client::proto::errorpb::ServerIsBusy::default()),
            ..Default::default()
        };
        Err(anyhow::Error::new(tikv_client::Error::RegionError(
            Box::new(re),
        )))
    };

    let mut result: anyhow::Result<Vec<u8>> = Err(anyhow::anyhow!("not started"));
    for attempt in 0..=super::REGION_ERROR_MAX_RETRIES {
        result = do_scan();
        match &result {
            Ok(_) => break,
            Err(e)
                if super::is_retryable_region_error(e)
                    && attempt < super::REGION_ERROR_MAX_RETRIES =>
            {
                continue;
            }
            Err(_) => break,
        }
    }

    assert!(result.is_err());
    // server_is_busy is NOT retryable, so only 1 call should have been made.
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "server_is_busy should NOT trigger retry — must fail on first attempt"
    );
}

// ── classify_renew_result: the wiring the original bug got wrong ─────────────
//
// classify_renew_result is the single place that maps a renewal-txn result onto
// the loop's action. These tests drive all THREE arms, including the Err arm
// that re-opens double-execution if it compares against the prospective deadline
// instead of the committed one. They would FAIL if line that builds Errored were
// reverted to pass `new_lease_until_ms` (always in the future).

#[test]
fn classify_renew_ok_true_advances_to_prospective_committed_deadline() {
    // Ok(true) means the renew txn committed extending the lease to the
    // prospective deadline; ONLY this arm advances the loop's committed value.
    let new_lease_until = 50_000;
    let outcome = WorkerEngine::classify_renew_result(
        Ok(true),
        new_lease_until,
        /*now*/ 10_000,
        /*last_committed*/ 12_000,
    );
    assert_eq!(
        outcome,
        LeaseRenewOutcome::Renewed {
            committed_lease_until_ms: new_lease_until
        },
        "Ok(true) must advance to the prospective deadline that just committed"
    );
}

#[test]
fn classify_renew_ok_false_is_claim_lost_regardless_of_deadlines() {
    // A missing/foreign claim cancels no matter how much committed lease remains.
    let outcome = WorkerEngine::classify_renew_result(
        Ok(false),
        /*new_lease_until*/ 99_999,
        /*now*/ 1_000,
        /*last_committed far in the future*/ 1_000_000,
    );
    assert_eq!(
        outcome,
        LeaseRenewOutcome::ClaimLost,
        "Ok(false) (claim gone/foreign) must be ClaimLost"
    );
}

#[test]
fn classify_renew_error_uses_committed_not_prospective_deadline() {
    // THE regression guard. The prospective deadline is far in the future
    // (now + lease); if the Err arm compared against it, it would NEVER cancel.
    // Drive both transitions using the COMMITTED deadline:
    let prospective_far_future = 1_000_000;

    // (i) committed lease still valid (now < committed) → must NOT cancel, even
    //     though the prospective deadline is also in the future.
    let outcome = WorkerEngine::classify_renew_result(
        Err(anyhow!("transient renew error")),
        prospective_far_future,
        /*now*/ 9_000,
        /*last_committed*/ 10_000,
    );
    assert_eq!(
        outcome,
        LeaseRenewOutcome::Errored { cancel: false },
        "error while the COMMITTED lease is still valid must not cancel"
    );

    // (ii) committed lease lapsed (now >= committed) → MUST cancel. If the arm
    //      used `new_lease_until` (prospective_far_future) it would stay false.
    let outcome = WorkerEngine::classify_renew_result(
        Err(anyhow!("transient renew error")),
        prospective_far_future,
        /*now*/ 10_001,
        /*last_committed*/ 10_000,
    );
    assert_eq!(
        outcome,
        LeaseRenewOutcome::Errored { cancel: true },
        "error once the COMMITTED lease has lapsed must cancel \
         (reverting this arm to the prospective deadline would leave it false)"
    );
}

// ── Lease-renewer LOOP-LEVEL behavioral tests (diff-review P1) ──────────────
//
// The pure `classify_renew_result` tests above guard the decision math (the Err
// arm must compare against the COMMITTED, not the prospective, deadline). They
// CANNOT catch a regression in the loop wiring that was the actual bug: seeding
// the committed deadline, advancing
// it ONLY on a committed renewal, passing the COMMITTED (not prospective) value
// into the cancel decision, and cancelling on a lost/stolen claim. The tests
// below drive the REAL renewal step (`renew_lease_once`) and the REAL spawned
// loop (`spawn_claim_lease_renewer`) against TiKV, so they fail if any of that
// wiring is reverted. They are double-execution safety branches, so they are
// worth the TiKV dependency.

/// Stand up a system store and CLAIM one worker task in it. Returns everything
/// the renewer needs plus the lease the claim committed. No tenant/cron state is
/// needed: the lease renewer only touches the system-store claim row.
async fn setup_claimed_lease_fixture(
    tag: &str,
    claim_lease_ms: i64,
) -> (
    Arc<TikvStore>,
    crate::worker::config::WorkerConfig,
    String, // keyspace
    u64,    // db_id
    i64,    // task_id
    i64,    // fire_time_ms
    i64,    // committed lease_until_ms of the claim
) {
    let pd_endpoints = std::env::var("PD_ENDPOINTS")
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let system_keyspace = format!(
        "_sys_lease_{tag}_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let cfg = crate::worker::config::WorkerConfig {
        enabled: true,
        system_keyspace,
        worker_id: format!("lease-worker-{tag}"),
        claim_lease_ms: claim_lease_ms.max(0) as u64,
        ..Default::default()
    };
    let system_store = crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
        .await
        .expect("init system store");

    let keyspace = format!(
        "lease_tenant_{tag}_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let db_id = 9_u64;
    let task_id = 7_i64;
    let fire_time_ms = crate::worker::now_epoch_ms();
    let task_type = TaskType::BgSql;

    let claim = WorkerClaim::with_lease(cfg.worker_id.clone(), task_type, claim_lease_ms);
    let committed_lease_until = claim.lease_until_ms;

    let mut txn = system_store.begin().await.unwrap();
    let claimed = system_store
        .try_claim_worker_task(&mut txn, &keyspace, db_id, task_id, fire_time_ms, &claim, 0)
        .await
        .unwrap();
    assert!(claimed, "fixture must win the initial claim");
    txn.commit().await.unwrap();

    (
        system_store,
        cfg,
        keyspace,
        db_id,
        task_id,
        fire_time_ms,
        committed_lease_until,
    )
}

/// Read back the stored lease for a claim by owner, or None if the claim row is
/// gone. Each test uses a unique system_keyspace + worker_id, so matching on the
/// owner alone is unambiguous within this test's isolated claim set.
async fn read_stored_lease(system_store: &Arc<TikvStore>, worker_id: &str) -> Option<i64> {
    let mut txn = system_store.begin().await.unwrap();
    let claims = system_store.list_worker_claims(&mut txn).await.unwrap();
    txn.rollback().await.ok();
    claims
        .into_iter()
        .find(|(_, c)| c.worker_id == worker_id)
        .map(|(_, c)| c.lease_until_ms)
}

/// Ok(true) ADVANCE wiring: a successful renewal must (a) actually extend the
/// STORED lease and (b) hand back the new committed deadline so the loop can
/// advance `last_committed_lease_until`. Guards the seed→advance path: if the
/// loop advanced on Ok(false)/Err or never advanced at all, the committed
/// deadline would not track the stored lease.
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn renew_lease_once_commits_and_returns_advanced_deadline() {
    let (system_store, cfg, keyspace, db_id, task_id, fire_time_ms, seed_lease) =
        setup_claimed_lease_fixture("advance", 60_000).await;

    let outcome = WorkerEngine::renew_lease_once(
        &system_store,
        &keyspace,
        db_id,
        task_id,
        fire_time_ms,
        &cfg.worker_id,
        TaskType::BgSql,
        60_000,
        seed_lease,
    )
    .await;

    let advanced = match outcome {
        LeaseRenewOutcome::Renewed {
            committed_lease_until_ms,
        } => committed_lease_until_ms,
        other => panic!("expected Renewed, got {other:?}"),
    };
    assert!(
        advanced >= seed_lease,
        "renewal must not move the committed deadline backwards"
    );

    // The STORED lease must equal the value the loop will advance to. If the
    // renewer returned a prospective deadline it never committed, this fails.
    let stored = read_stored_lease(&system_store, &cfg.worker_id)
        .await
        .expect("claim must still exist after a successful renewal");
    assert_eq!(
        stored, advanced,
        "Renewed deadline must equal the committed stored lease"
    );
}

/// Ok(false) CLAIM-LOST wiring: if the claim row is deleted/stolen out from
/// under the renewer, the step must report `ClaimLost` (which the loop turns
/// into a cancel). Guards the steal branch (engine.rs Ok(false) arm), which the
/// pure-function tests do not touch at all.
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn renew_lease_once_reports_claim_lost_when_claim_deleted() {
    let (system_store, cfg, keyspace, db_id, task_id, fire_time_ms, seed_lease) =
        setup_claimed_lease_fixture("stolen", 60_000).await;

    // Steal the claim out from under us (another worker takes over / unschedule).
    let mut txn = system_store.begin().await.unwrap();
    system_store
        .delete_worker_claim(
            &mut txn,
            &keyspace,
            db_id,
            task_id,
            fire_time_ms,
            TaskType::BgSql,
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();

    let outcome = WorkerEngine::renew_lease_once(
        &system_store,
        &keyspace,
        db_id,
        task_id,
        fire_time_ms,
        &cfg.worker_id,
        TaskType::BgSql,
        60_000,
        // Even with a far-future committed deadline, a missing/foreign claim is
        // ClaimLost (not Errored) — the loop cancels regardless of the deadline.
        seed_lease + 1_000_000,
    )
    .await;

    assert_eq!(
        outcome,
        LeaseRenewOutcome::ClaimLost,
        "deleted/stolen claim must yield ClaimLost"
    );
}

/// FULL-LOOP Ok(false) cancel: spawn the real renewer, delete the claim row, and
/// assert the loop cancels `exec_shutdown`. This drives the spawned loop's
/// ClaimLost→cancel wiring end to end (the regression surface the diff-review
/// flagged), not just the predicate.
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn spawned_renewer_cancels_exec_shutdown_when_claim_stolen() {
    // Short lease so the renew interval floor (1s) drives a tick quickly.
    let (system_store, cfg, keyspace, db_id, task_id, fire_time_ms, seed_lease) =
        setup_claimed_lease_fixture("loopsteal", 3_000).await;

    let exec_shutdown = CancellationToken::new();
    let guard = WorkerEngine::spawn_claim_lease_renewer(
        system_store.clone(),
        cfg.clone(),
        keyspace.clone(),
        db_id,
        task_id,
        fire_time_ms,
        TaskType::BgSql,
        seed_lease,
        exec_shutdown.clone(),
    );

    // Steal the claim so the next renewal tick observes a missing claim.
    let mut txn = system_store.begin().await.unwrap();
    system_store
        .delete_worker_claim(
            &mut txn,
            &keyspace,
            db_id,
            task_id,
            fire_time_ms,
            TaskType::BgSql,
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();

    // The renew interval floor is 1s; allow a couple of ticks plus slack.
    let cancelled = tokio::time::timeout(Duration::from_secs(6), exec_shutdown.cancelled())
        .await
        .is_ok();
    drop(guard);
    assert!(
        cancelled,
        "renewer loop must cancel exec_shutdown once the claim is stolen"
    );
}

/// FULL-LOOP foreign-takeover cancel: a second worker wins the expired-lease CAS
/// (the claim row now carries a different worker_id). `renew_worker_claim`
/// returns Ok(false), so the loop must cancel `exec_shutdown` (ClaimLost). This
/// is the concrete double-execution scenario the lease mechanism guards against,
/// driven through the real spawned loop. (The ERROR-branch committed-vs-
/// prospective comparison — which cannot be forced against a live cluster — is
/// guarded purely by `classify_renew_error_uses_committed_not_prospective_deadline`
/// below, which drives the exact `Err` arm of `classify_renew_result`.)
#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn spawned_renewer_cancels_when_claim_taken_over_by_another_worker() {
    let (system_store, cfg, keyspace, db_id, task_id, fire_time_ms, seed_lease) =
        setup_claimed_lease_fixture("foreign", 3_000).await;

    // Overwrite the claim with a FOREIGN owner (simulating an expired-lease CAS
    // takeover by a second worker). renew_worker_claim then returns Ok(false).
    let foreign = WorkerClaim::with_lease("other-worker".to_string(), TaskType::BgSql, 60_000);
    let mut txn = system_store.begin().await.unwrap();
    system_store
        .delete_worker_claim(
            &mut txn,
            &keyspace,
            db_id,
            task_id,
            fire_time_ms,
            TaskType::BgSql,
        )
        .await
        .unwrap();
    system_store
        .try_claim_worker_task(
            &mut txn,
            &keyspace,
            db_id,
            task_id,
            fire_time_ms,
            &foreign,
            0,
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();

    let exec_shutdown = CancellationToken::new();
    let guard = WorkerEngine::spawn_claim_lease_renewer(
        system_store.clone(),
        cfg.clone(),
        keyspace.clone(),
        db_id,
        task_id,
        fire_time_ms,
        TaskType::BgSql,
        seed_lease,
        exec_shutdown.clone(),
    );

    let cancelled = tokio::time::timeout(Duration::from_secs(6), exec_shutdown.cancelled())
        .await
        .is_ok();
    drop(guard);
    assert!(
        cancelled,
        "renewer loop must cancel exec_shutdown when the claim is owned by another worker"
    );
}

// ---- derived StorageSizeScan: behavioral unit tests (pure state machine) ----
//
// These exercise the pure helpers in `engine.rs:189-341` by CALLING them and
// asserting computed values (not by grepping source). They build minimal
// fixtures for `StorageScanBgState`, `StorageScanCapacityToken`, and
// `StorageScanOwnerFence` (the store-dependent fields of the fence are never
// touched — only the pure predicate `storage_scan_owner_state_matches`, which
// reads plain fields, is tested; the store-backed
// `lock_storage_scan_owner_for_commit` is intentionally skipped).
mod storage_scan_derived_behavioral {
    use super::super::*;
    use crate::worker::types::{
        StorageScanBgState, StorageScanBgStateStatus, StorageScanCapacityToken,
    };

    /// Minimal Running `StorageScanBgState` fixture. Callers override individual
    /// fields to construct mismatch cases.
    fn running_state() -> StorageScanBgState {
        StorageScanBgState {
            keyspace: "ks".to_string(),
            db_id: 7,
            tenant_incarnation: 3,
            status: StorageScanBgStateStatus::Running,
            work_id: 300_000,
            run_after_ms: 0,
            lease_until_ms: 1_000,
            attempt: 2,
            last_done_work_id: 290_000,
            capacity_token_id: Some(1),
        }
    }

    // NOTE: helper #7 (`storage_scan_owner_state_matches`) is intentionally NOT
    // unit-tested here. Although the predicate itself is pure, it takes a
    // `&StorageScanOwnerFence`, whose `system_store: Arc<TikvStore>` field cannot
    // be constructed without a live TiKV/PD cluster (every store-backed test in
    // this file is `#[ignore = "requires TiKV / PD cluster"]`). The same
    // identity/lease/incarnation invariants the predicate enforces are covered
    // structurally by `storage_scan_completion_owner_matches` (test #8) and
    // `storage_scan_capacity_token_matches_state` (test #9), which are pure.

    /// `StorageScanCapacityToken` matching a state's owner identity + lease.
    fn capacity_token(state: &StorageScanBgState) -> StorageScanCapacityToken {
        StorageScanCapacityToken {
            keyspace: state.keyspace.clone(),
            db_id: state.db_id,
            tenant_incarnation: state.tenant_incarnation,
            work_id: state.work_id,
            attempt: state.attempt,
            lease_until_ms: state.lease_until_ms,
        }
    }

    // 1. interval = max(1)*1000, min 1
    #[test]
    fn interval_ms_floors_to_one_second() {
        assert_eq!(storage_scan_interval_ms(300), 300_000);
        assert_eq!(storage_scan_interval_ms(0), 1_000);
        assert_eq!(storage_scan_interval_ms(1), 1_000);
    }

    // 2. work_id = floor(now/interval)*interval, monotonic-nondecreasing
    #[test]
    fn work_id_floors_to_window_and_is_monotonic() {
        assert_eq!(storage_scan_work_id(300_000, 300), 300_000);
        assert_eq!(storage_scan_work_id(350_000, 300), 300_000);
        assert_eq!(storage_scan_work_id(299_999, 300), 0);
        assert_eq!(storage_scan_work_id(600_001, 300), 600_000);

        // Monotonic-nondecreasing as now_ms increases across a boundary.
        let mut prev = i64::MIN;
        for now in [
            299_998, 299_999, 300_000, 300_001, 599_999, 600_000, 600_001,
        ] {
            let w = storage_scan_work_id(now, 300);
            assert!(
                w >= prev,
                "work_id must be monotonic-nondecreasing: now={now} -> {w} < prev {prev}"
            );
            prev = w;
        }
    }

    // 3. next_after = work_id + 1
    #[test]
    fn next_work_id_after_increments_by_one() {
        assert_eq!(next_storage_scan_work_id_after(300_000), 300_001);
    }

    // 4. at_least_after = max(base, prev+1) with None -> base
    #[test]
    fn work_id_at_least_after_picks_strictly_greater_of_base_or_prev_plus_one() {
        assert_eq!(storage_scan_work_id_at_least_after(300_000, None), 300_000);
        // base wins (base >= prev+1)
        assert_eq!(
            storage_scan_work_id_at_least_after(300_000, Some(290_000)),
            300_000
        );
        // prev+1 wins (prev == base, so we must advance past it)
        assert_eq!(
            storage_scan_work_id_at_least_after(300_000, Some(300_000)),
            300_001
        );
        // prev far ahead -> prev+1 wins
        assert_eq!(
            storage_scan_work_id_at_least_after(300_000, Some(400_000)),
            400_001
        );
    }

    // 5. retry_after = now + 30_000 * 2^min(attempt,5) (grows then caps; NOT a 6h wait)
    #[test]
    fn retry_after_ms_exponential_then_caps_at_attempt_five() {
        let now = 1_000_000;
        assert_eq!(storage_scan_retry_after_ms(now, 0), 1_030_000);
        assert_eq!(storage_scan_retry_after_ms(now, 1), 1_060_000);
        assert_eq!(storage_scan_retry_after_ms(now, 2), 1_120_000);
        assert_eq!(storage_scan_retry_after_ms(now, 5), 1_960_000);
        assert_eq!(storage_scan_retry_after_ms(now, 6), 1_960_000);
        assert_eq!(storage_scan_retry_after_ms(now, 100), 1_960_000);
    }

    // 6. idle_after_success: reset to Idle, advance last_done monotonically, preserve identity
    #[test]
    fn idle_after_success_resets_and_advances_last_done_monotonically() {
        let state = running_state();
        let next = storage_scan_idle_after_success(state.clone(), 300_000, 1_000_000, 300);

        assert_eq!(next.status, StorageScanBgStateStatus::Idle);
        assert_eq!(next.work_id, 0);
        assert_eq!(next.run_after_ms, 1_300_000); // now + interval(300s)
        assert_eq!(next.lease_until_ms, 0);
        assert_eq!(next.attempt, 0);
        assert_eq!(next.capacity_token_id, None);
        assert_eq!(next.last_done_work_id, 300_000); // max(290_000, 300_000)

        // identity preserved
        assert_eq!(next.keyspace, "ks");
        assert_eq!(next.db_id, 7);
        assert_eq!(next.tenant_incarnation, 3);

        // completed LOWER than existing last_done -> last_done stays (max).
        let lower = storage_scan_idle_after_success(state, 280_000, 1_000_000, 300);
        assert_eq!(lower.last_done_work_id, 290_000);
    }

    // 7. SKIPPED — see the NOTE above `capacity_token`: the predicate is pure but
    //    its `&StorageScanOwnerFence` argument is unconstructable without a store.

    // 8. completion_owner_matches: inc + Running + work_id + attempt
    #[test]
    fn completion_owner_matches_requires_incarnation_status_work_attempt() {
        let state = running_state();
        assert!(storage_scan_completion_owner_matches(
            Some(&state),
            state.tenant_incarnation,
            state.work_id,
            state.attempt,
        ));

        // tenant_incarnation drift inert at completion time too
        assert!(!storage_scan_completion_owner_matches(
            Some(&state),
            state.tenant_incarnation + 1,
            state.work_id,
            state.attempt,
        ));

        // status != Running
        let mut idle = state.clone();
        idle.status = StorageScanBgStateStatus::Idle;
        assert!(!storage_scan_completion_owner_matches(
            Some(&idle),
            state.tenant_incarnation,
            state.work_id,
            state.attempt,
        ));
    }

    // 9. capacity_token_matches_state: liveness/repair predicate
    #[test]
    fn capacity_token_matches_state_checks_liveness_and_identity() {
        let now = 500; // state.lease_until_ms = 1_000 > now
        let state = running_state();
        let token = capacity_token(&state);
        let token_id = 1; // state.capacity_token_id = Some(1)

        assert!(storage_scan_capacity_token_matches_state(
            token_id, &token, &state, now
        ));

        // stale lease -> token reclaimable
        let mut stale = state.clone();
        stale.lease_until_ms = now; // not strictly greater
                                    // token must mirror the state's lease for the equality arm; reuse fresh
                                    // state's token but with a stale-lease state still has lease_until_ms !=
                                    // token.lease_until_ms, so build a matching stale token.
        let stale_token = capacity_token(&stale);
        assert!(!storage_scan_capacity_token_matches_state(
            token_id,
            &stale_token,
            &stale,
            now
        ));

        // token_id mismatch (state.capacity_token_id != Some(token_id))
        assert!(!storage_scan_capacity_token_matches_state(
            2, &token, &state, now
        ));

        // tenant_incarnation mismatch
        let mut wrong_inc = state.clone();
        wrong_inc.tenant_incarnation = state.tenant_incarnation + 1;
        assert!(!storage_scan_capacity_token_matches_state(
            token_id, &token, &wrong_inc, now
        ));

        // work_id mismatch
        let mut wrong_work = state.clone();
        wrong_work.work_id = state.work_id + 1;
        assert!(!storage_scan_capacity_token_matches_state(
            token_id,
            &token,
            &wrong_work,
            now
        ));
    }

    // 10. derived_lease_ms: invariants only (>= claim_lease, >= 1, monotonic in endpoints)
    #[test]
    fn derived_lease_ms_respects_floor_and_endpoint_monotonicity() {
        let config = crate::worker::config::WorkerConfig::default();
        let floor = config.claim_lease_ms as i64;

        let lease_1 = storage_scan_derived_lease_ms(&config, 1);
        assert!(
            lease_1 >= floor,
            "lease {lease_1} must be >= claim_lease {floor}"
        );
        assert!(lease_1 >= 1);

        // 0 endpoints is clamped to 1 internally -> same as 1.
        let lease_0 = storage_scan_derived_lease_ms(&config, 0);
        assert!(lease_0 >= floor && lease_0 >= 1);

        // Non-decreasing as the endpoint count grows (more endpoints => larger or
        // equal PD HTTP budget => larger or equal lease).
        let mut prev = i64::MIN;
        for n in [1usize, 2, 3, 5, 10, 50] {
            let lease = storage_scan_derived_lease_ms(&config, n);
            assert!(
                lease >= prev,
                "lease must be monotonic-nondecreasing in endpoint count: n={n} -> {lease} < prev {prev}"
            );
            assert!(lease >= floor && lease >= 1);
            prev = lease;
        }
    }
}
