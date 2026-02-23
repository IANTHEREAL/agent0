//! Transaction/autocommit framework and observability permission checks.

use super::super::*;
use super::guc::build_show_all_result;
use super::utils::{apply_statement_timeout, autocommit_backoff, validate_transaction_modes};

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
    ) -> Result<Vec<ExecuteResult>> {
        if let Statement::Query(query) = stmt {
            if let Some(result) = try_execute_set_config_select(session, query.as_ref())? {
                return Ok(vec![result]);
            }
            if let Some(result) = try_execute_current_setting_select(session, query.as_ref())? {
                return Ok(vec![result]);
            }
        }

        let is_autocommit = !session.is_in_transaction();
        let db_id = session.current_database_id();

        // Retry up to 10 times for autocommit to handle concurrent conflicts
        let max_attempts = if is_autocommit { 10usize } else { 1usize };

        for attempt in 0..max_attempts {
            if is_autocommit {
                session.begin().await?;
            }

            let timeout = session.statement_timeout();
            let current_role = session.current_user().map(|u| u.to_string());
            let fut = async {
                let (txn, sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");
                let notices = self
                    .collect_notices_before_statement(txn, db_id, search_path, stmt)
                    .await?;
                let result = self
                    .execute_statement_on_txn(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        stmt,
                        current_role.as_deref(),
                    )
                    .await?;
                Ok::<(Vec<ExecuteResult>, ExecuteResult), anyhow::Error>((notices, result))
            };

            let res = apply_statement_timeout(timeout, fut).await;

            if res
                .as_ref()
                .err()
                .is_some_and(|e| e.is::<StatementTimeoutError>())
                && !is_autocommit
            {
                // pg-tikv does not currently implement PostgreSQL's "failed
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
                            session.commit().await?;
                            self.flush_trigger_activations();
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
                            autocommit_backoff(attempt).await;
                            continue;
                        }
                        return Err(err);
                    }
                }
            } else {
                let (notices, result) = res?;
                let mut stmt_results = notices;
                stmt_results.push(result);
                return Ok(stmt_results);
            }
        }

        unreachable!("retry loop must return")
    }
}
