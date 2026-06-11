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
    assert_eq!(
        super::keep_queue_entry_for_claim_status(CronRunClaimStatus::Claimed),
        None
    );
    assert_eq!(
        super::keep_queue_entry_for_claim_status(CronRunClaimStatus::AlreadyClaimedForMinute),
        Some(false),
        "same-minute duplicate due rows must be deleted, not retried forever"
    );
    assert_eq!(
        super::keep_queue_entry_for_claim_status(CronRunClaimStatus::BlockedByRunningGuard),
        Some(true),
        "running-guard blocks should keep the due row so it can retry after the active run"
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
        .find(".try_claim_cron_run(")
        .expect("cron claim path must claim the scheduled minute");

    assert!(
        job_lookup < claim && stale_check < claim,
        "stale legacy cron entries must be rejected before AlreadyClaimedForMinute can keep them"
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
            is_dropped: false,
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
        execute_task_source.contains("let _ = fut.await?;"),
        "individual statements must execute without per-statement timeout wrapping"
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
    assert!(
        prod_source.contains("Some(shutdown_signal.clone())")
            && prod_source.contains("Some(shutdown_signal),"),
        "worker task execution must propagate shutdown cancellation to running tasks"
    );
}

#[test]
fn cron_startup_reconciliation_does_not_scan_legacy_worker_queue() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let reconcile_fn = prod_source
        .split("async fn reconcile_cron_for_db")
        .nth(1)
        .and_then(|rest| {
            rest.split("async fn reconcile_incomplete_cic_indexes")
                .next()
        })
        .expect("reconcile_cron_for_db must exist before CIC reconciliation");

    assert!(
        reconcile_fn.contains(".index_rows_for_db_type("),
        "cron startup reconciliation must use the bounded V2 identity index"
    );
    assert!(
        !reconcile_fn.contains(".legacy_entries_for_db_type("),
        "cron startup reconciliation must not scan legacy _worker_queue_"
    );
    assert!(
        !reconcile_fn.contains(".delete_worker_queue_entry("),
        "cron startup reconciliation must not delete legacy entries by scanning _worker_queue_"
    );
}

#[test]
fn storage_size_scan_tracks_its_long_lived_read_transaction() {
    let source = include_str!("../engine.rs");
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
fn registry_batch_from_cursor_wraps_and_clamps() {
    let entries = (0..5)
        .map(|i| TaskRegistryEntry::new(format!("tenant_{i}"), i))
        .collect::<Vec<_>>();

    let batch = registry_batch_from_cursor(&entries, 3, 4);
    let keys = batch
        .iter()
        .map(|entry| entry.keyspace.as_str())
        .collect::<Vec<_>>();
    assert_eq!(keys, vec!["tenant_3", "tenant_4", "tenant_0", "tenant_1"]);

    let oversized = registry_batch_from_cursor(&entries, 0, 99);
    assert_eq!(oversized.len(), entries.len());

    let empty: Vec<TaskRegistryEntry> = Vec::new();
    assert!(registry_batch_from_cursor(&empty, 0, 1).is_empty());
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
        startup_source.contains("self.reconcile_ddl_journal().await"),
        "startup must keep DDL journal crash recovery"
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
fn ddl_journal_reconciliation_filters_registry_bit_before_tenant_acquire() {
    let source = include_str!("../engine.rs");
    let prod_source = source
        .split("#[cfg(test)]")
        .next()
        .expect("engine.rs must contain #[cfg(test)]");
    let reconcile_fn = prod_source
        .split("async fn reconcile_ddl_journal(&self)")
        .nth(1)
        .and_then(|rest| rest.split("async fn reconcile_ddl_journal_for_db").next())
        .expect("reconcile_ddl_journal must exist before per-db helper");

    assert!(
        reconcile_fn.contains("entry.has_ddl_journal()"),
        "DDL journal startup recovery must only acquire tenants with TASK_TYPE_DDL_JOURNAL set"
    );
}

#[test]
fn hnsw_startup_sweep_tracks_its_read_transaction() {
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
    let (queue_key, descriptor) = {
        let mut txn = system_store.begin().await.unwrap();
        system_store
            .put_task_v2(&mut txn, &entry, fire_time_ms)
            .await
            .unwrap();
        txn.commit().await.unwrap();

        // Read back the V2 due descriptor + key
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

    // Call the REAL claim_and_execute code path with injected finalize failure
    let result = WorkerEngine::claim_and_execute_core(
        &system_store,
        &pool,
        &cfg,
        &metrics,
        queue_key,
        DueItem::V2(descriptor),
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

    // Next cron fire MUST be requeued (new V2 descriptor with future fire time)
    let queue = system_store
        .scan_due_v2(&mut txn, i64::MAX, 1000)
        .await
        .unwrap();
    assert!(
        queue.iter().any(|(_, d)| d.task_id == task_id
            && d.keyspace == keyspace
            && d.task_type == TaskType::Cron),
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
        // [claim] Release a just-won claim: single key delete + immediate commit.
        "release_claim",
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
        // [reconcile] Reads registry list (small metadata) + immediate commit,
        // then processes a bounded tenant batch.
        "reconcile_storage_scans",
        // [enqueue] Single put to system store queue + commit.
        "enqueue_storage_scan",
        // [finalize] Single stats key write after scan completes; immediate commit.
        // (The long-lived scan txn in execute_storage_size_scan IS tracked; this is
        // just the final persist_txn that writes the result.)
        "execute_storage_size_scan",
        // [reconcile] Reads DDL journal entries (small metadata) + immediate commit.
        "reconcile_ddl_journal",
        // [reconcile] Scans DDL journal + cleans orphaned data in batches with txn rotation.
        "reconcile_ddl_journal_for_db",
        // ── worker/gc.rs ──

        // [lookup] Neutralize GC instance state: single key write + commit.
        "clear_gc_instance_state",
        // [lookup] Publish GC instance state: single key write + commit.
        "publish_gc_instance_state",
        // [lookup] Read all GC instance states (small registry) + rollback.
        "advance_gc_safepoint",
        // [reconcile] Read registry list + immediate commit, then process a bounded tenant batch.
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
