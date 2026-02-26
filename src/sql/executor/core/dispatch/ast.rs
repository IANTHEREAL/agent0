//! Parsed-SQL (AST) dispatch: parse the SQL string, then execute each
//! statement in the resulting AST list.
//!
//! **Invariant I3** (parse-error remap ordering):
//! On `parse_sql` error for non-observability users:
//! 1. Check `get_unsupported_reason` first — if hit, return `Unsupported`
//!    immediately (skip parse-error record, skip mark-failed).
//! 2. Otherwise record zero-duration parse failure, then mark transaction
//!    failed if in a transaction, then return the parse error.
//!
//! Observability users: no parse-error record, but in-transaction mark-failed
//! still applies.

use super::super::*;
use super::guc::{build_show_all_result, execute_set_variable};
use super::scaffold::DispatchContext;
use super::transaction::check_observability_statement_permission;
use super::utils::{validate_transaction_modes, wrap_with_runtime_context, RuntimeSettings};

impl Executor {
    /// Parse SQL and dispatch each AST statement.
    ///
    /// This is the final dispatch phase after raw-SQL routing has been
    /// exhausted. Returns the combined results of all parsed statements.
    pub(super) async fn dispatch_parsed_statements(
        &self,
        session: &mut Session,
        sql: &str,
        ctx: &DispatchContext,
    ) -> Result<ExecuteResults> {
        let statements = match parse_sql(sql) {
            Ok(stmts) => stmts,
            Err(e) => {
                return self.handle_parse_error(session, ctx, e);
            }
        };

        if statements.is_empty() {
            return Ok(ExecuteResults::single(ExecuteResult::Empty));
        }

        let mut results: Vec<ExecuteResult> = Vec::with_capacity(statements.len());

        for stmt in &statements {
            debug!("Executing statement: {:?}", stmt);
            let is_observability_query = ctx.is_observability_user
                && (is_observability_system_query(stmt) || is_observability_tableless_query(stmt));
            if ctx.is_observability_user {
                if let Some(handled) = check_observability_statement_permission(
                    self,
                    session,
                    stmt,
                    is_observability_query,
                )
                .await?
                {
                    results.extend(handled);
                    continue;
                }
            }
            let start = Instant::now();
            let rt_settings = RuntimeSettings::from_session(session);
            let stmt_exec: Result<Vec<ExecuteResult>> = wrap_with_runtime_context(
                &rt_settings,
                self.tenant_keyspace(),
                self.store.transaction_client(),
                async {
                    match stmt {
                        // Transaction Control
                        Statement::StartTransaction { modes, .. } => {
                            validate_transaction_modes(session, modes)?;
                            session.begin().await?;
                            Ok(vec![ExecuteResult::TransactionStart { tag: "BEGIN" }])
                        }
                        Statement::Commit { .. } => {
                            let tag = if session.is_transaction_failed() {
                                "ROLLBACK"
                            } else {
                                "COMMIT"
                            };
                            session.commit().await?;
                            if tag == "COMMIT" {
                                self.flush_trigger_activations();
                            } else {
                                self.clear_trigger_activations();
                            }
                            Ok(vec![ExecuteResult::TransactionEnd { tag }])
                        }
                        Statement::Savepoint { name } => {
                            session.create_savepoint(normalize_ident(name)).await?;
                            Ok(vec![ExecuteResult::CommandComplete { tag: "SAVEPOINT" }])
                        }
                        Statement::ReleaseSavepoint { name } => {
                            let sp = normalize_ident(name);
                            session.release_savepoint(&sp).await?;
                            Ok(vec![ExecuteResult::CommandComplete { tag: "RELEASE" }])
                        }
                        Statement::Rollback {
                            savepoint: Some(name),
                            ..
                        } => {
                            let sp = normalize_ident(name);
                            session.rollback_to_savepoint(&sp).await?;
                            Ok(vec![ExecuteResult::CommandComplete { tag: "ROLLBACK" }])
                        }
                        Statement::Rollback {
                            savepoint: None, ..
                        } => {
                            session.rollback().await?;
                            self.clear_trigger_activations();
                            Ok(vec![ExecuteResult::TransactionEnd { tag: "ROLLBACK" }])
                        }
                        Statement::SetRole { role_name, .. } => {
                            self.execute_set_role(session, role_name).await
                        }
                        Statement::SetVariable {
                            local,
                            variable,
                            value,
                            ..
                        } => execute_set_variable(session, *local, variable, value),
                        Statement::SetTimeZone { local, value, .. } => {
                            let value = set_variable_value_to_string(std::slice::from_ref(value))?;
                            if *local && !session.is_in_transaction() {
                                crate::sql::session::SessionSettings::validate_and_normalize_value(
                                    "timezone", &value,
                                )?;
                                return Ok(vec![
                                    ExecuteResult::Notice {
                                        message: "SET LOCAL can only be used in transaction blocks"
                                            .to_string(),
                                        severity: "WARNING".to_string(),
                                    },
                                    ExecuteResult::CommandComplete { tag: "SET" },
                                ]);
                            }

                            if *local {
                                session.set_local_setting("timezone", value)?;
                            } else {
                                session.set_known_setting("timezone", value)?;
                            }
                            Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
                        }
                        Statement::SetNames {
                            charset_name,
                            collation_name,
                        } => {
                            if collation_name.is_some() {
                                return Err(SqlError::Unsupported(
                                    "SET NAMES with COLLATE is not supported".into(),
                                )
                                .into());
                            }
                            session.set_known_setting("client_encoding", charset_name.clone())?;
                            Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
                        }
                        Statement::SetTransaction {
                            modes,
                            snapshot,
                            session: _,
                        } => {
                            if snapshot.is_some() {
                                return Err(SqlError::Unsupported(
                                    "SET TRANSACTION SNAPSHOT is not supported".into(),
                                )
                                .into());
                            }
                            validate_transaction_modes(session, modes)?;
                            Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
                        }
                        Statement::ShowVariable { variable } => {
                            let var_name = variable
                                .iter()
                                .map(normalize_ident)
                                .collect::<Vec<_>>()
                                .join(".")
                                .to_lowercase();

                            if var_name == "all" {
                                return Ok(vec![build_show_all_result(
                                    session,
                                    session_context::current_timezone(),
                                )]);
                            }

                            let value = session.show_setting_value(&var_name).ok_or_else(|| {
                                anyhow!("unrecognized configuration parameter \"{}\"", var_name)
                            })?;

                            Ok(vec![ExecuteResult::Select {
                                columns: vec![var_name],
                                column_types: Some(vec![DataType::Text]),
                                rows: vec![Row::new(vec![Value::Text(value)])],
                                timezone: session_context::current_timezone(),
                            }])
                        }
                        Statement::Prepare {
                            name,
                            data_types,
                            statement,
                        } => {
                            self.execute_sql_prepare_statement(
                                session,
                                name,
                                data_types,
                                statement.as_ref(),
                            )
                            .await
                        }
                        Statement::Execute { name, parameters } => {
                            self.execute_sql_execute_statement(session, name, parameters)
                                .await
                        }
                        Statement::Deallocate { name, .. } => {
                            self.execute_sql_deallocate_statement(session, name)
                        }
                        // DDL/DML - delegated to session transaction management
                        stmt => {
                            self.execute_ddl_dml_with_autocommit(
                                session,
                                stmt,
                                is_observability_query,
                            )
                            .await
                        }
                    }
                },
            )
            .await;

            if stmt_exec.is_err() && session.is_in_transaction() {
                session.mark_transaction_failed();
            }

            if !is_observability_query {
                self.observability
                    .record_statement(start.elapsed(), stmt_exec.is_ok(), || {
                        ctx.sql_for_observability.clone()
                    });
            }

            results.extend(stmt_exec?);
        }

        // Parser ASTs for large statements (e.g. deep OR chains) can be
        // deeply recursive; drop them on a grown stack to avoid worker
        // stack overflow after successful execution.
        crate::sql::stack_safety::drop_on_grown_stack(statements);

        Ok(ExecuteResults(results))
    }

    /// Handle a parse error preserving I3 ordering.
    ///
    /// For non-observability users:
    /// 1. `get_unsupported_reason` check — if hit, return `Unsupported`
    ///    immediately (skips parse-error record AND mark-failed).
    /// 2. Record zero-duration parse failure.
    /// 3. Mark transaction failed if in a transaction.
    /// 4. Return the original parse error.
    ///
    /// For observability users:
    /// - No parse-error record, but in-transaction mark-failed still applies.
    fn handle_parse_error(
        &self,
        session: &mut Session,
        ctx: &DispatchContext,
        parse_error: anyhow::Error,
    ) -> Result<ExecuteResults> {
        if !ctx.is_observability_user {
            if let Some(reason) = get_unsupported_reason(&ctx.sql_upper) {
                return Err(SqlError::Unsupported(reason).into());
            }
            // Parse error counts as a statement attempt (for error rate / p99, etc).
            self.observability
                .record_statement(Duration::from_millis(0), false, || ctx.sql_trimmed.clone());
        }
        if session.is_in_transaction() {
            session.mark_transaction_failed();
        }
        Err(parse_error)
    }
}
