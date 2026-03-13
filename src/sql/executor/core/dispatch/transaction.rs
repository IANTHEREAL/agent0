//! Transaction/autocommit framework and observability permission checks.

use super::super::*;
use super::guc::build_show_all_result;
use super::utils::{apply_statement_timeout, validate_transaction_modes};

/// Check whether an observability user is permitted to execute the given
/// statement.
///
/// Returns:
/// - `Ok(Some(results))` if the statement was fully handled (caller should
///   extend its results and `continue` the loop).
/// - `Ok(None)` if the statement should fall through to normal handling
///   (SET/SHOW variants).
/// - `Err(...)` if the statement is denied.
pub(super) async fn check_observability_statement_permission(
    executor: &Executor,
    session: &mut Session,
    stmt: &Statement,
    is_observability_query: bool,
) -> Result<Option<Vec<ExecuteResult>>> {
    match stmt {
        // Transaction control: handle directly and return results.
        Statement::StartTransaction { modes, .. } => {
            validate_transaction_modes(session, modes)?;
            session.begin().await?;
            Ok(Some(vec![ExecuteResult::TransactionStart { tag: "BEGIN" }]))
        }
        Statement::Commit { .. } => {
            let tag = if session.is_transaction_failed() {
                "ROLLBACK"
            } else {
                "COMMIT"
            };
            session.commit().await?;
            if tag == "COMMIT" {
                executor.flush_trigger_activations();
                executor.flush_pending_hnsw_merges();
                executor.flush_pending_init_cache_invalidation();
            } else {
                executor.clear_trigger_activations();
            }
            Ok(Some(vec![ExecuteResult::TransactionEnd { tag }]))
        }
        Statement::Savepoint { name } => {
            session.create_savepoint(normalize_ident(name)).await?;
            Ok(Some(vec![ExecuteResult::CommandComplete {
                tag: "SAVEPOINT",
            }]))
        }
        Statement::ReleaseSavepoint { name } => {
            let sp = normalize_ident(name);
            session.release_savepoint(&sp).await?;
            Ok(Some(vec![ExecuteResult::CommandComplete {
                tag: "RELEASE",
            }]))
        }
        Statement::Rollback {
            savepoint: Some(name),
            ..
        } => {
            let sp = normalize_ident(name);
            session.rollback_to_savepoint(&sp).await?;
            Ok(Some(vec![ExecuteResult::CommandComplete {
                tag: "ROLLBACK",
            }]))
        }
        Statement::Rollback {
            savepoint: None, ..
        } => {
            session.rollback().await?;
            executor.clear_trigger_activations();
            Ok(Some(vec![ExecuteResult::TransactionEnd {
                tag: "ROLLBACK",
            }]))
        }
        // SET variants: fall through to the real-user SET handling below.
        // Session settings are per-connection and safe for observability users.
        Statement::SetVariable { .. }
        | Statement::SetTimeZone { .. }
        | Statement::SetNames { .. }
        | Statement::SetTransaction { .. } => Ok(None),
        Statement::ShowVariable { variable } => {
            let var_name = variable
                .iter()
                .map(normalize_ident)
                .collect::<Vec<_>>()
                .join(".")
                .to_lowercase();

            if var_name == "all" {
                let tz = Arc::from(
                    session
                        .show_setting_value("timezone")
                        .unwrap_or_else(|| "UTC".into()),
                );
                return Ok(Some(vec![build_show_all_result(session, tz)]));
            }

            let value = match session.show_setting_value(&var_name) {
                Some(value) => value,
                None => {
                    let err = anyhow!("unrecognized configuration parameter \"{}\"", var_name);
                    if session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    return Err(err);
                }
            };
            let timezone = Arc::from(
                session
                    .show_setting_value("timezone")
                    .unwrap_or_else(|| "UTC".to_string()),
            );

            Ok(Some(vec![ExecuteResult::Select {
                columns: vec![var_name],
                column_types: Some(vec![DataType::Text]),
                rows: vec![Row::new(vec![Value::Text(value)])],
                timezone,
            }]))
        }
        Statement::Query(_) => {
            if !is_observability_query {
                if session.is_in_transaction() {
                    session.mark_transaction_failed();
                }
                return Err(SqlError::PermissionDenied {
                    object_type: "role".into(),
                    object_name: OBSERVABILITY_USER.to_string(),
                }
                .into());
            }
            Ok(None)
        }
        _ => {
            if session.is_in_transaction() {
                session.mark_transaction_failed();
            }
            Err(SqlError::PermissionDenied {
                object_type: "role".into(),
                object_name: OBSERVABILITY_USER.to_string(),
            }
            .into())
        }
    }
}

/// Returns `true` for DDL statements that can alter table schemas and
/// therefore invalidate cached prepared plans.
fn is_plan_cache_invalidating_ddl(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::CreateTable { .. }
            | Statement::CreateIndex { .. }
            | Statement::Drop { .. }
            | Statement::AlterTable { .. }
            | Statement::AlterIndex { .. }
            | Statement::Truncate { .. }
    )
}

// DDL can contend with background schema writers (e.g. CIC backfill phases).
// Default retry budget; configurable via `db9.retry_max_attempts` GUC.
#[cfg(test)]
const DDL_DML_MAX_RETRY_ATTEMPTS: usize = 64;

fn ddl_dml_retry_max_attempts(
    is_autocommit: bool,
    explicit_first_stmt_retry_eligible: bool,
    session_max: usize,
) -> usize {
    if is_autocommit || explicit_first_stmt_retry_eligible {
        session_max.max(1) // defensive clamp (validation rejects 0, but belt-and-suspenders)
    } else {
        1 // execute once, no retry
    }
}

impl Executor {
    /// Execute a DDL/DML statement with autocommit retry logic.
    ///
    /// This is the catch-all arm for statements not handled by dedicated
    /// arms (transaction control, SET, SHOW, PREPARE/EXECUTE/DEALLOCATE).
    pub(super) async fn execute_ddl_dml_with_autocommit(
        &self,
        session: &mut Session,
        stmt: &Statement,
        is_observability_query: bool,
        create_index_with_params: Option<&str>,
    ) -> Result<Vec<ExecuteResult>> {
        if let Statement::Query(query) = stmt {
            if let Some(result) = try_execute_set_config_select(session, query.as_ref()).await? {
                return Ok(vec![result]);
            }
            if let Some(result) = try_execute_current_setting_select(session, query.as_ref())? {
                return Ok(vec![result]);
            }
        }

        let is_autocommit = !session.is_in_transaction();
        let explicit_first_stmt_retry_eligible =
            !is_autocommit && !session.has_executed_statement_in_transaction();
        let db_id = session.current_database_id();

        // Retry up to N times for autocommit to handle concurrent conflicts
        // and for the first statement in an explicit transaction (safe to restart
        // because no prior statement has completed in that transaction yet).
        let max_attempts = ddl_dml_retry_max_attempts(
            is_autocommit,
            explicit_first_stmt_retry_eligible,
            session.settings().retry_max_attempts as usize,
        );

        let retry_start = std::time::Instant::now();
        let retry_timeout = {
            let ms = session.settings().retry_timeout_ms;
            if ms > 0 {
                Some(std::time::Duration::from_millis(ms))
            } else {
                None
            }
        };

        for attempt in 0..max_attempts {
            // On retry iterations (after backoff), re-check wall-time ceiling.
            // This catches the case where backoff sleep pushed us past the deadline.
            if attempt > 0 {
                if let Some(timeout) = retry_timeout {
                    if retry_start.elapsed() >= timeout {
                        tracing::warn!(
                            attempt,
                            max_attempts,
                            elapsed_ms = retry_start.elapsed().as_millis() as u64,
                            timeout_ms = timeout.as_millis() as u64,
                            "retry timeout exceeded after backoff, aborting"
                        );
                        if !is_autocommit {
                            session.rollback().await?;
                            self.clear_trigger_activations();
                        }
                        self.observability.record_retry_timeout_abort();
                        return Err(SqlError::RetryTimeout {
                            elapsed_ms: retry_start.elapsed().as_millis() as u64,
                            limit_ms: timeout.as_millis() as u64,
                        }
                        .into());
                    }
                }
            }
            if is_autocommit {
                session.begin().await?;
            }

            let timeout = session.statement_timeout();
            let current_role = session.current_user().map(|u| u.to_string());
            let session_user = session.session_user().map(|u| u.to_string());
            let txn_snapshot_ts_version = session.active_txn_start_ts_version();
            let extension_txn_delta = session.extension_delta_snapshot();
            let fut = async {
                let (txn, sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");
                let notices = self
                    .collect_notices_before_statement(txn, db_id, search_path, stmt)
                    .await?;
                let result = self
                    .execute_statement_on_txn_with_create_index_with_params(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        stmt,
                        create_index_with_params,
                        current_role.as_deref(),
                        session_user.as_deref(),
                    )
                    .await?;
                Ok::<(Vec<ExecuteResult>, ExecuteResult), anyhow::Error>((notices, result))
            };

            let res = crate::session_context::with_txn_snapshot_ts_version(
                txn_snapshot_ts_version,
                crate::session_context::with_extension_txn_delta(extension_txn_delta, async {
                    apply_statement_timeout(timeout, fut).await
                }),
            )
            .await;

            // Invalidate plan cache after schema-altering DDL succeeds.
            // Applies to both autocommit and explicit transactions: within an
            // explicit txn, cached plans optimized before the DDL would use a
            // stale schema; clearing eagerly prevents that.
            if res.is_ok() && is_plan_cache_invalidating_ddl(stmt) {
                session.clear_plan_cache();
            }

            if res
                .as_ref()
                .err()
                .is_some_and(|e| e.is::<StatementTimeoutError>())
                && !is_autocommit
            {
                // db9-server does not currently implement PostgreSQL's "failed
                // transaction" state. To avoid leaving an open transaction in
                // an unknown partial state, abort it on statement timeout.
                session.rollback().await?;
                self.clear_trigger_activations();
            }

            if is_autocommit {
                match res {
                    Ok((notices, result)) => {
                        if is_observability_query {
                            session.rollback().await?;
                            self.clear_trigger_activations();
                        } else {
                            if matches!(result, ExecuteResult::AlterRole | ExecuteResult::DropRole)
                            {
                                self.mark_init_cache_invalidation_pending();
                            }
                            session.commit().await?;
                            self.flush_trigger_activations();
                            self.flush_pending_hnsw_merges();
                            self.flush_pending_init_cache_invalidation();
                        }
                        let mut stmt_results = notices;
                        stmt_results.push(result);
                        return Ok(stmt_results);
                    }
                    Err(err) => {
                        session.rollback().await?;
                        self.clear_trigger_activations();
                        let should_retry =
                            attempt + 1 < max_attempts && is_retryable_tikv_error(&err);
                        if should_retry {
                            // Check wall-time ceiling BEFORE sleeping
                            if let Some(timeout) = retry_timeout {
                                if retry_start.elapsed() >= timeout {
                                    tracing::warn!(
                                        attempt = attempt + 1,
                                        max_attempts,
                                        elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                        timeout_ms = timeout.as_millis() as u64,
                                        "retry timeout exceeded, aborting retries"
                                    );
                                    self.observability.record_retry_timeout_abort();
                                    return Err(SqlError::RetryTimeout {
                                        elapsed_ms: retry_start.elapsed().as_millis() as u64,
                                        limit_ms: timeout.as_millis() as u64,
                                    }
                                    .into());
                                }
                            }
                            tracing::info!(
                                attempt = attempt + 1,
                                max_attempts,
                                elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                "write conflict, retrying statement"
                            );
                            self.observability
                                .record_retry_attempt(extract_write_conflict_reason(&err));
                            autocommit_backoff(attempt).await;
                            continue;
                        }
                        if is_retryable_tikv_error(&err) {
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                "write conflict retry budget exhausted"
                            );
                            self.observability.record_retry_budget_exhausted();
                        }
                        return Err(err);
                    }
                }
            } else {
                match res {
                    Ok((notices, result)) => {
                        session.note_statement_success_in_transaction();
                        if matches!(result, ExecuteResult::AlterRole | ExecuteResult::DropRole) {
                            self.mark_init_cache_invalidation_pending();
                        }
                        let mut stmt_results = notices;
                        stmt_results.push(result);
                        return Ok(stmt_results);
                    }
                    Err(err) => {
                        let should_retry = explicit_first_stmt_retry_eligible
                            && attempt + 1 < max_attempts
                            && is_retryable_tikv_error(&err);
                        if should_retry {
                            // Check wall-time ceiling BEFORE sleeping
                            if let Some(timeout) = retry_timeout {
                                if retry_start.elapsed() >= timeout {
                                    tracing::warn!(
                                        attempt = attempt + 1,
                                        max_attempts,
                                        elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                        timeout_ms = timeout.as_millis() as u64,
                                        "retry timeout exceeded, aborting retries"
                                    );
                                    session.rollback().await?;
                                    self.clear_trigger_activations();
                                    self.observability.record_retry_timeout_abort();
                                    return Err(SqlError::RetryTimeout {
                                        elapsed_ms: retry_start.elapsed().as_millis() as u64,
                                        limit_ms: timeout.as_millis() as u64,
                                    }
                                    .into());
                                }
                            }
                            tracing::info!(
                                attempt = attempt + 1,
                                max_attempts,
                                elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                "write conflict, retrying statement"
                            );
                            self.observability
                                .record_retry_attempt(extract_write_conflict_reason(&err));
                            session.rollback().await?;
                            self.clear_trigger_activations();
                            session.begin().await?;
                            autocommit_backoff(attempt).await;
                            continue;
                        }
                        if is_retryable_tikv_error(&err) {
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                "write conflict retry budget exhausted"
                            );
                            self.observability.record_retry_budget_exhausted();
                        }
                        return Err(err);
                    }
                }
            }
        }

        unreachable!("retry loop must return")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        check_observability_statement_permission, ddl_dml_retry_max_attempts,
        is_plan_cache_invalidating_ddl, DDL_DML_MAX_RETRY_ATTEMPTS,
    };
    use crate::sql::parse_sql;
    use crate::sql::{ExecuteResult, Executor, Session};
    use sqlparser::ast::Statement;

    fn parse_stmt(sql: &str) -> Statement {
        let mut stmts = parse_sql(sql).expect("parse");
        stmts.remove(0)
    }

    fn make_executor_and_session() -> (Executor, Session) {
        let store = crate::storage::TikvStore::new_stub();
        let keyspace = "dispatch_transaction_tests".to_string();
        let observability = crate::observability::registry().tenant(&keyspace);
        let trigger_cache = std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new());
        let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
        let executor = Executor::new(
            store.clone(),
            keyspace,
            observability.clone(),
            crate::pool::TenantMemoryAccountant::unlimited(
                "dispatch_transaction_tests".to_string(),
            ),
            trigger_cache,
            stats_cache,
        );
        let session = Session::new_with_user_and_database(
            store,
            observability,
            "observer".to_string(),
            false,
            false,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        );
        (executor, session)
    }

    #[tokio::test]
    async fn observability_permission_allows_set_to_fall_through() {
        let (executor, mut session) = make_executor_and_session();
        let stmt = parse_stmt("SET statement_timeout = 1000");
        let out =
            check_observability_statement_permission(&executor, &mut session, &stmt, false).await;
        assert!(out.unwrap().is_none());
    }

    #[tokio::test]
    async fn observability_permission_show_all_returns_select() {
        let (executor, mut session) = make_executor_and_session();
        let stmt = parse_stmt("SHOW ALL");
        let out =
            check_observability_statement_permission(&executor, &mut session, &stmt, false).await;
        let results = out.unwrap().expect("show all handled");
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], ExecuteResult::Select { .. }));
    }

    #[tokio::test]
    async fn observability_permission_show_known_variable_returns_select() {
        let (executor, mut session) = make_executor_and_session();
        session
            .set_known_setting("statement_timeout", "1200".to_string())
            .unwrap();
        let stmt = parse_stmt("SHOW statement_timeout");
        let out =
            check_observability_statement_permission(&executor, &mut session, &stmt, false).await;
        let results = out.unwrap().expect("show handled");
        assert_eq!(results.len(), 1);
        match &results[0] {
            ExecuteResult::Select { rows, .. } => {
                assert_eq!(rows.len(), 1);
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn observability_permission_denies_non_observability_query() {
        let (executor, mut session) = make_executor_and_session();
        session.force_test_transaction_state(true, false);
        let stmt = parse_stmt("SELECT 1");
        let err = check_observability_statement_permission(&executor, &mut session, &stmt, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"));
        assert!(session.is_transaction_failed());
    }

    #[tokio::test]
    async fn observability_permission_show_unknown_errors_and_marks_failed() {
        let (executor, mut session) = make_executor_and_session();
        session.force_test_transaction_state(true, false);
        let stmt = parse_stmt("SHOW totally_unknown_setting");
        let err = check_observability_statement_permission(&executor, &mut session, &stmt, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unrecognized configuration parameter"));
        assert!(session.is_transaction_failed());
    }

    #[tokio::test]
    async fn observability_permission_allows_observability_query_flag() {
        let (executor, mut session) = make_executor_and_session();
        let stmt = parse_stmt("SELECT 1");
        let out =
            check_observability_statement_permission(&executor, &mut session, &stmt, true).await;
        assert!(out.unwrap().is_none());
    }

    #[tokio::test]
    async fn observability_permission_denies_unsupported_statement() {
        let (executor, mut session) = make_executor_and_session();
        session.force_test_transaction_state(true, false);
        let stmt = parse_stmt("CREATE TABLE t(id INT)");
        let err = check_observability_statement_permission(&executor, &mut session, &stmt, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"));
        assert!(session.is_transaction_failed());
    }

    #[test]
    fn plan_cache_invalidating_ddl_classifier_matches_contract() {
        assert!(is_plan_cache_invalidating_ddl(&parse_stmt(
            "CREATE TABLE t(id INT)"
        )));
        assert!(is_plan_cache_invalidating_ddl(&parse_stmt(
            "CREATE INDEX i ON t(id)"
        )));
        assert!(is_plan_cache_invalidating_ddl(&parse_stmt(
            "ALTER TABLE t ADD COLUMN c INT"
        )));
        assert!(is_plan_cache_invalidating_ddl(&parse_stmt(
            "TRUNCATE TABLE t"
        )));
        assert!(is_plan_cache_invalidating_ddl(&parse_stmt("DROP TABLE t")));
        assert!(!is_plan_cache_invalidating_ddl(&parse_stmt("SELECT 1")));
    }

    #[test]
    fn ddl_dml_retry_budget_uses_full_budget_for_autocommit() {
        assert_eq!(
            ddl_dml_retry_max_attempts(true, false, DDL_DML_MAX_RETRY_ATTEMPTS),
            DDL_DML_MAX_RETRY_ATTEMPTS
        );
    }

    #[test]
    fn ddl_dml_retry_budget_uses_full_budget_for_first_explicit_statement() {
        assert_eq!(
            ddl_dml_retry_max_attempts(false, true, DDL_DML_MAX_RETRY_ATTEMPTS),
            DDL_DML_MAX_RETRY_ATTEMPTS
        );
    }

    #[test]
    fn ddl_dml_retry_budget_disables_retries_after_first_explicit_statement() {
        assert_eq!(
            ddl_dml_retry_max_attempts(false, false, DDL_DML_MAX_RETRY_ATTEMPTS),
            1
        );
    }

    #[test]
    fn ddl_dml_retry_budget_respects_session_override() {
        assert_eq!(ddl_dml_retry_max_attempts(true, false, 5), 5);
        assert_eq!(ddl_dml_retry_max_attempts(false, true, 10), 10);
        // session_max=0 is clamped to 1 (defensive)
        assert_eq!(ddl_dml_retry_max_attempts(true, false, 0), 1);
    }
}
