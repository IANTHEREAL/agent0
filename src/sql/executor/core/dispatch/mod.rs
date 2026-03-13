//! Simple-query dispatch: `execute` entry point, the `execute_single` state
//! machine, and the `dispatch_raw!` macro.
//!
//! `execute_single` orchestrates three phases:
//! 1. **Scaffold** ([`scaffold::DispatchContext`]) — pre-compute trimmed SQL,
//!    classification, and observability flags.
//! 2. **Failed-txn precheck** (I1) — reject disallowed statements early.
//! 3. **Raw dispatch** ([`raw`]) — handle statements that bypass `sqlparser`.
//! 4. **AST dispatch** ([`ast`]) — parse SQL and dispatch each AST statement.

mod ast;
mod guc;
mod prepared;
mod raw;
mod roles;
mod scaffold;
mod transaction;
mod utils;

use super::*;
use scaffold::DispatchContext;

/// Dispatch a raw-SQL command: time it, record observability, handle txn failure.
///
/// Retained for unit tests that verify the instrumented-dispatch contract
/// (mark-failed, record-statement, return shape) in isolation. Production
/// code uses the equivalent helpers in `raw.rs` (`finish_raw_single` /
/// `finish_raw_multi`).
#[cfg(test)]
macro_rules! dispatch_raw {
    ($self:expr, $session:expr, $sql_obs:expr, $cmd:expr) => {{
        let start = Instant::now();
        let res = $cmd;
        if res.is_err() && $session.is_in_transaction() {
            $session.mark_transaction_failed();
        }
        $self
            .observability
            .record_statement(start.elapsed(), res.is_ok(), || $sql_obs.to_string());
        return res.map(ExecuteResults::single);
    }};
    // Variant for commands returning ExecuteResults directly (database ops).
    (multi: $self:expr, $session:expr, $sql_obs:expr, $cmd:expr) => {{
        let start = Instant::now();
        let res = $cmd;
        if res.is_err() && $session.is_in_transaction() {
            $session.mark_transaction_failed();
        }
        $self
            .observability
            .record_statement(start.elapsed(), res.is_ok(), || $sql_obs.to_string());
        return res;
    }};
}

impl Executor {
    /// Execute a SQL statement string using the provided session.
    ///
    /// Supports multiple statements separated by semicolons (e.g., "BEGIN; UPDATE...; COMMIT;")
    /// and returns all results for proper PostgreSQL Simple Query Protocol compliance.
    pub async fn execute(&self, session: &mut Session, sql: &str) -> Result<ExecuteResults> {
        let statements = split_sql_statements(sql)
            .into_iter()
            .filter(|stmt| !strip_leading_sql_comments(stmt).trim().is_empty())
            .collect::<Vec<_>>();

        if statements.is_empty() {
            return Ok(ExecuteResults::single(ExecuteResult::Empty));
        }

        if statements.len() == 1 {
            let statement = statements[0];
            return self.execute_single(session, statement).await;
        }

        // Multi-statement batch: PostgreSQL wraps these in an implicit
        // transaction, so SET LOCAL effects persist across statements.
        // Set the flag so apply_pending_set_config_mutations applies LOCAL
        // mutations to session local_overrides instead of dropping them.
        session.set_in_implicit_batch(true);
        let mut results = Vec::new();
        let batch_result = async {
            for statement in statements {
                let ExecuteResults(mut statement_results) =
                    self.execute_single(session, statement).await?;
                results.append(&mut statement_results);
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        // Clean up: revert LOCAL overrides accumulated during the implicit
        // batch (mirrors COMMIT clearing local_overrides in explicit txns).
        // Only clear if we didn't enter an explicit transaction during the
        // batch (e.g. a bare BEGIN inside the batch).
        if !session.is_in_transaction() {
            session.clear_local_overrides();
        }
        session.set_in_implicit_batch(false);
        batch_result?;
        Ok(ExecuteResults(results))
    }

    /// Dispatch a single SQL statement through the phased state machine.
    ///
    /// Phases (in order):
    /// 1. Scaffold — build [`DispatchContext`] (timestamps, query context, savepoints).
    /// 2. Failed-txn precheck (I1) — reject if transaction is failed and statement
    ///    is not ROLLBACK/COMMIT/END.
    /// 3. Raw instrumented dispatch — non-observability raw-SQL handlers with
    ///    `dispatch_raw!` side effects.
    /// 4. Raw passthrough dispatch (I2) — ALTER SYSTEM SET / RESET without
    ///    `dispatch_raw!` side effects.
    /// 5. AST dispatch (I3) — parse SQL and execute each statement.
    async fn execute_single(&self, session: &mut Session, sql: &str) -> Result<ExecuteResults> {
        let statement_ts = statement_time::now_timestamp_millis();
        // For explicit transactions, use the stored transaction start time;
        // for implicit (autocommit), the transaction timestamp equals the statement timestamp.
        let transaction_ts = session.transaction_timestamp_ms().unwrap_or(statement_ts);
        let qctx = session.query_context_for_statement(statement_ts, transaction_ts);
        let savepoints = session.savepoints();
        crate::sql::query_context::with_scoped_query_context(
            &qctx,
            crate::txn::with_savepoints(savepoints, async {
                let ctx = DispatchContext::new(sql, session);

                // ── Phase 1: Failed-txn precheck (I1) ──────────────────
                if session.is_transaction_failed()
                    && !ctx.sql_trimmed.trim().is_empty()
                    && !ctx.starts_with("ROLLBACK")
                    && !ctx.starts_with("COMMIT")
                    && !ctx.starts_with("END")
                {
                    if !ctx.is_observability_user {
                        self.observability.record_statement(
                            Duration::from_millis(0),
                            false,
                            || ctx.sql_trimmed.clone(),
                        );
                    }
                    return Err(SqlError::InFailedTransaction.into());
                }

                // ── Phase 2: Raw instrumented dispatch ─────────────────
                if !ctx.is_observability_user {
                    if let Some(result) =
                        self.try_dispatch_raw_instrumented(session, sql, &ctx).await
                    {
                        return result;
                    }
                }

                // ── Phase 3: Raw passthrough dispatch (I2) ─────────────
                if let Some(result) = self.try_dispatch_raw_passthrough(session, &ctx) {
                    return result;
                }

                // ── Phase 4: Parse + AST dispatch (I3) ─────────────────
                self.dispatch_parsed_statements(session, sql, &ctx).await
            }),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::future::Future;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeSession {
        in_transaction: bool,
        marked_failed: bool,
    }

    impl FakeSession {
        fn is_in_transaction(&self) -> bool {
            self.in_transaction
        }

        fn mark_transaction_failed(&mut self) {
            self.marked_failed = true;
        }
    }

    #[derive(Default)]
    struct FakeObservability {
        records: RefCell<Vec<(bool, String)>>,
    }

    impl FakeObservability {
        fn record_statement<F>(&self, _elapsed: Duration, ok: bool, sql: F)
        where
            F: FnOnce() -> String,
        {
            self.records.borrow_mut().push((ok, sql()));
        }
    }

    #[derive(Default)]
    struct FakeExecutor {
        observability: FakeObservability,
    }

    fn run_dispatch_raw_single(
        exec: &FakeExecutor,
        session: &mut FakeSession,
        sql_obs: &str,
        cmd: Result<ExecuteResult>,
    ) -> Result<ExecuteResults> {
        dispatch_raw!(exec, session, sql_obs, cmd);
    }

    fn run_dispatch_raw_multi(
        exec: &FakeExecutor,
        session: &mut FakeSession,
        sql_obs: &str,
        cmd: Result<ExecuteResults>,
    ) -> Result<ExecuteResults> {
        dispatch_raw!(multi: exec, session, sql_obs, cmd);
    }

    #[test]
    fn dispatch_raw_single_wraps_execute_result_and_records_success() {
        let exec = FakeExecutor::default();
        let mut session = FakeSession {
            in_transaction: true,
            marked_failed: false,
        };

        let out = run_dispatch_raw_single(
            &exec,
            &mut session,
            "CREATE EXTENSION foo",
            Ok(ExecuteResult::CommandComplete {
                tag: "CREATE EXTENSION",
            }),
        )
        .expect("dispatch should succeed");

        assert_eq!(out.0.len(), 1);
        assert!(matches!(
            out.0.as_slice(),
            [ExecuteResult::CommandComplete {
                tag: "CREATE EXTENSION"
            }]
        ));
        assert!(!session.marked_failed);

        let records = exec.observability.records.borrow();
        assert_eq!(records.as_slice(), &[(true, "CREATE EXTENSION foo".into())]);
    }

    #[test]
    fn dispatch_raw_single_marks_txn_failed_and_records_error_in_transaction() {
        let exec = FakeExecutor::default();
        let mut session = FakeSession {
            in_transaction: true,
            marked_failed: false,
        };

        let err = run_dispatch_raw_single(
            &exec,
            &mut session,
            "DROP EXTENSION foo",
            Err(anyhow!("boom")),
        )
        .expect_err("dispatch should fail");

        assert_eq!(err.to_string(), "boom");
        assert!(session.marked_failed);

        let records = exec.observability.records.borrow();
        assert_eq!(records.as_slice(), &[(false, "DROP EXTENSION foo".into())]);
    }

    #[test]
    fn dispatch_raw_single_does_not_mark_txn_failed_outside_transaction() {
        let exec = FakeExecutor::default();
        let mut session = FakeSession {
            in_transaction: false,
            marked_failed: false,
        };

        run_dispatch_raw_single(
            &exec,
            &mut session,
            "COMMENT ON TABLE t IS 'x'",
            Err(anyhow!("boom")),
        )
        .expect_err("dispatch should fail");

        assert!(!session.marked_failed);
    }

    #[test]
    fn dispatch_raw_multi_preserves_execute_results_shape() {
        let exec = FakeExecutor::default();
        let mut session = FakeSession {
            in_transaction: true,
            marked_failed: false,
        };
        let multi = ExecuteResults(vec![
            ExecuteResult::CommandComplete {
                tag: "CREATE DATABASE",
            },
            ExecuteResult::Notice {
                message: "note".to_string(),
                severity: "NOTICE".to_string(),
                sqlstate: "00000".to_string(),
            },
        ]);

        let out = run_dispatch_raw_multi(&exec, &mut session, "CREATE DATABASE x", Ok(multi))
            .expect("dispatch should succeed");

        assert_eq!(out.0.len(), 2);
        assert!(matches!(
            out.0.as_slice(),
            [
                ExecuteResult::CommandComplete {
                    tag: "CREATE DATABASE"
                },
                ExecuteResult::Notice { .. }
            ]
        ));
        assert!(!session.marked_failed);

        let records = exec.observability.records.borrow();
        assert_eq!(records.as_slice(), &[(true, "CREATE DATABASE x".into())]);
    }

    #[derive(Clone, Copy)]
    struct ObsCounts {
        statements: u64,
        errors: u64,
    }

    fn obs_counts(obs: &Arc<crate::observability::TenantObservability>) -> ObsCounts {
        let summary = obs.snapshot_summary();
        ObsCounts {
            statements: summary.statement_count,
            errors: summary.error_count,
        }
    }

    fn assert_obs_delta(
        obs: &Arc<crate::observability::TenantObservability>,
        before: ObsCounts,
        statements_delta: u64,
        errors_delta: u64,
    ) {
        let after = obs_counts(obs);
        assert_eq!(after.statements, before.statements + statements_delta);
        assert_eq!(after.errors, before.errors + errors_delta);
    }

    static NEXT_TEST_FIXTURE_ID: AtomicU64 = AtomicU64::new(1);

    fn run_async_on_large_stack<F>(future: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        std::thread::Builder::new()
            .name("dispatch-test-runtime".to_string())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build test runtime");
                runtime.block_on(future)
            })
            .expect("spawn large-stack test thread")
            .join()
            .expect("join large-stack test thread")
    }

    fn make_executor_and_session(
        observability_user: bool,
    ) -> (
        Executor,
        Session,
        Arc<crate::observability::TenantObservability>,
    ) {
        let fixture_id = NEXT_TEST_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let keyspace = format!("dispatch_execute_single_fixture_{fixture_id}");
        let store = crate::storage::TikvStore::new_stub();
        let observability = crate::observability::registry().tenant(&keyspace);
        let trigger_cache = Arc::new(crate::sql::triggers::TriggerBodyCache::new());
        let rls_policy_cache = Arc::new(crate::sql::rls::cache::RlsPolicyCache::new());
        let stats_cache = Arc::new(crate::sql::stats::TableStatsCache::new());
        let executor = Executor::new(
            store.clone(),
            keyspace,
            observability.clone(),
            crate::pool::TenantMemoryAccountant::unlimited("dispatch_test".to_string()),
            trigger_cache,
            rls_policy_cache,
            stats_cache,
        );
        let user = if observability_user {
            OBSERVABILITY_USER.to_string()
        } else {
            format!("app_user_{fixture_id}")
        };
        let connection_id = i64::try_from(fixture_id).unwrap_or(i64::MAX);
        let session = Session::new_with_user_and_database(
            store,
            observability.clone(),
            user,
            false,
            connection_id,
            1,
            "postgres".to_string(),
            0,
            0,
        );
        (executor, session, observability)
    }

    // ── Characterization tests: invariant I1 ────────────────────────
    // Failed-transaction precheck: only ROLLBACK/COMMIT/END pass gate.

    async fn assert_failed_txn_gate_allows(sql: &str) {
        let (exec, mut session, _) = make_executor_and_session(false);
        session.force_test_transaction_state(true, true);
        let out = exec.execute_single(&mut session, sql).await;
        assert!(out.is_ok(), "failed-txn gate should allow {sql}");
    }

    /// T1: failed-txn precheck allows ROLLBACK.
    #[test]
    fn t1_failed_txn_precheck_gate_allows_rollback() {
        run_async_on_large_stack(async {
            assert_failed_txn_gate_allows("ROLLBACK").await;
        });
    }

    /// T1: failed-txn precheck allows COMMIT.
    #[test]
    fn t1_failed_txn_precheck_gate_allows_commit() {
        run_async_on_large_stack(async {
            assert_failed_txn_gate_allows("COMMIT").await;
        });
    }

    /// T1: failed-txn precheck allows END.
    #[test]
    fn t1_failed_txn_precheck_gate_allows_end() {
        run_async_on_large_stack(async {
            assert_failed_txn_gate_allows("END TRANSACTION").await;
        });
    }

    /// T1 (continued): other statements are blocked by the precheck.
    #[test]
    fn t1_failed_txn_precheck_gate_blocks_other_statements() {
        run_async_on_large_stack(async {
            for sql in ["SELECT 1", "INSERT INTO t VALUES (1)", "BEGIN", "SET x = 1"] {
                let (exec, mut session, _) = make_executor_and_session(false);
                session.force_test_transaction_state(true, true);
                let err = exec
                    .execute_single(&mut session, sql)
                    .await
                    .expect_err("failed-txn gate should reject non-ROLLBACK/COMMIT/END");
                assert!(
                    err.to_string().contains("current transaction is aborted"),
                    "unexpected error for {sql}: {err}"
                );
            }
        });
    }

    /// T2: failed-txn precheck records a failure for non-observability user.
    #[test]
    fn t2_failed_txn_precheck_records_failure_for_non_observability_user() {
        run_async_on_large_stack(async {
            let (exec, mut session, obs) = make_executor_and_session(false);
            session.force_test_transaction_state(true, true);
            let before = obs_counts(&obs);

            let err = exec
                .execute_single(&mut session, "SELECT blocked")
                .await
                .expect_err("failed-txn gate should reject");
            assert!(err.to_string().contains("current transaction is aborted"));
            assert_obs_delta(&obs, before, 1, 1);
        });
    }

    /// T2 (continued): observability user does not get precheck observability record.
    #[test]
    fn t2_failed_txn_precheck_no_record_for_observability_user() {
        run_async_on_large_stack(async {
            let (exec, mut session, obs) = make_executor_and_session(true);
            session.force_test_transaction_state(true, true);
            let before = obs_counts(&obs);

            let err = exec
                .execute_single(&mut session, "SELECT blocked")
                .await
                .expect_err("failed-txn gate should reject");
            assert!(err.to_string().contains("current transaction is aborted"));
            assert_obs_delta(&obs, before, 0, 0);
        });
    }

    // ── Characterization tests: invariant I2 ────────────────────────
    // ALTER SYSTEM SET / RESET bypass dispatch_raw! instrumentation.

    /// T3: ALTER SYSTEM SET is classified as passthrough (not instrumented).
    #[test]
    fn t3_alter_system_set_classified_as_passthrough() {
        let kind = crate::sql::raw_sql::classify("ALTER SYSTEM SET STATEMENT_TIMEOUT = '5S'");
        assert_eq!(kind, Some(crate::sql::raw_sql::RawSqlKind::AlterSystemSet));
        // Verify it does NOT match any first-block or second-block kind.
        // The passthrough handler catches AlterSystemSet, not the instrumented path.
    }

    /// T4: RESET success goes through passthrough dispatch (no raw instrumentation).
    #[test]
    fn t4_reset_success_passthrough_without_raw_instrumentation() {
        run_async_on_large_stack(async {
            let (exec, mut session, obs) = make_executor_and_session(false);
            session.force_test_transaction_state(true, false);
            let before = obs_counts(&obs);

            let out = exec
                .execute_single(&mut session, "RESET TIMEZONE")
                .await
                .expect("RESET should succeed");
            assert!(matches!(
                out.0.as_slice(),
                [ExecuteResult::CommandComplete { tag: "RESET" }]
            ));
            assert!(!session.is_transaction_failed());
            assert_obs_delta(&obs, before, 0, 0);
        });
    }

    /// T5: ALTER SYSTEM SET / RESET errors do NOT use instrumented dispatch.
    /// Verify that finish_raw_single (the instrumented path) records
    /// observability, confirming the passthrough path differs.
    #[test]
    fn t5_instrumented_path_records_but_passthrough_does_not() {
        let exec = FakeExecutor::default();
        let mut session = FakeSession {
            in_transaction: true,
            marked_failed: false,
        };
        // Instrumented path: records observability and marks failed.
        run_dispatch_raw_single(&exec, &mut session, "DROP TYPE t", Err(anyhow!("boom")))
            .expect_err("should fail");
        assert!(session.marked_failed, "instrumented path marks txn failed");
        let records = exec.observability.records.borrow();
        assert_eq!(records.len(), 1, "instrumented path records observability");
        drop(records);

        // By contrast, passthrough (ALTER SYSTEM SET / RESET) would NOT
        // call record_statement or mark_transaction_failed through the
        // raw helper. This is the structural guarantee of I2.
    }

    // ── Characterization tests: invariant I3 ────────────────────────
    // Parse-error remap ordering.

    /// T6: unsupported remap short-circuits parse-error record and mark-failed.
    /// SQL matching `get_unsupported_reason` returns Unsupported immediately.
    #[test]
    fn t6_unsupported_remap_short_circuits() {
        run_async_on_large_stack(async {
            let (exec, mut session, obs) = make_executor_and_session(false);
            session.force_test_transaction_state(true, false);
            let before = obs_counts(&obs);

            let err = exec
                .execute_single(&mut session, "CREATE DOMAIN foo")
                .await
                .expect_err("CREATE DOMAIN should be remapped to unsupported");
            assert_eq!(err.to_string(), "CREATE DOMAIN not supported");
            assert!(!session.is_transaction_failed());
            assert_obs_delta(&obs, before, 0, 0);
        });
    }

    /// T7: ordinary parse error retains record-then-mark ordering.
    /// Non-observability user: record zero-duration failure, then mark failed.
    #[test]
    fn t7_ordinary_parse_error_records_then_marks() {
        run_async_on_large_stack(async {
            let (exec, mut session, obs) = make_executor_and_session(false);
            session.force_test_transaction_state(true, false);
            let before = obs_counts(&obs);

            let err = exec
                .execute_single(&mut session, "SELCT typo")
                .await
                .expect_err("ordinary parse error should fail");
            assert!(
                !err.to_string().contains("not supported"),
                "ordinary parse error should not be remapped to unsupported"
            );
            assert!(session.is_transaction_failed());
            assert_obs_delta(&obs, before, 1, 1);
        });
    }

    /// T7 (continued): observability user parse error — no record, but
    /// mark-failed still applies when in a transaction.
    #[test]
    fn t7_observability_user_parse_error_no_record_but_marks_failed() {
        run_async_on_large_stack(async {
            let (exec, mut session, obs) = make_executor_and_session(true);
            session.force_test_transaction_state(true, false);
            let before = obs_counts(&obs);

            exec.execute_single(&mut session, "SELCT typo")
                .await
                .expect_err("observability user parse error should fail");
            assert!(session.is_transaction_failed());
            assert_obs_delta(&obs, before, 0, 0);
        });
    }
}
