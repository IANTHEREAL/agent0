//! Simple-query dispatch: `execute` entry point, statement parsing,
//! per-statement observability recording, and the `execute_single` loop.

mod guc;
mod prepared;
mod roles;
mod transaction;
mod utils;

use super::*;
use guc::{build_show_all_result, execute_set_variable};
use transaction::check_observability_statement_permission;
use utils::{validate_transaction_modes, wrap_with_runtime_context, RuntimeSettings};

/// Dispatch a raw-SQL command: time it, record observability, handle txn failure.
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

        let mut results = Vec::new();
        for statement in statements {
            let ExecuteResults(mut statement_results) =
                self.execute_single(session, statement).await?;
            results.append(&mut statement_results);
        }
        Ok(ExecuteResults(results))
    }

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
                let sql_stripped = strip_leading_sql_comments(sql);
                let sql_trimmed = sql_stripped.trim_start();
                let is_observability_user =
                    session.current_user() == Some(OBSERVABILITY_USER) && !session.is_superuser();
                let starts_with = |prefix: &str| starts_with_ignore_ascii_case(sql_trimmed, prefix);
                let sql_upper = sql_trimmed.trim().to_ascii_uppercase();
                let sql_for_observability = sql_trimmed.to_string();
                let raw_kind = crate::sql::raw_sql::classify(&sql_upper);

                if session.is_transaction_failed()
                    && !sql_trimmed.trim().is_empty()
                    && !starts_with("ROLLBACK")
                    && !starts_with("COMMIT")
                    && !starts_with("END")
                {
                    if !is_observability_user {
                        self.observability.record_statement(
                            Duration::from_millis(0),
                            false,
                            || sql_trimmed.to_string(),
                        );
                    }
                    return Err(SqlError::InFailedTransaction.into());
                }

                if !is_observability_user {
                    if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::CreateDatabase)) {
                        dispatch_raw!(multi: self, session, sql_trimmed,
                            self.execute_create_database_cmd(session, sql).await);
                    }
                    if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::DropDatabase)) {
                        dispatch_raw!(multi: self, session, sql_trimmed,
                            self.execute_drop_database_cmd(session, sql).await);
                    }
                    if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::AlterDatabase)) {
                        dispatch_raw!(multi: self, session, sql_trimmed,
                            self.execute_alter_database_cmd(session, sql).await);
                    }
                    if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::CreateExtension)) {
                        dispatch_raw!(self, session, sql_trimmed,
                            self.execute_create_extension_cmd(session, sql).await);
                    }
                    if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::DropExtension)) {
                        dispatch_raw!(self, session, sql_trimmed,
                            self.execute_drop_extension_cmd(session, sql).await);
                    }
                    if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::CommentOn)) {
                        dispatch_raw!(self, session, sql_trimmed,
                            self.execute_comment_on_cmd(session, sql).await);
                    }
                    if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::CreateFunction)) {
                        dispatch_raw!(self, session, sql_trimmed,
                            self.execute_create_function_cmd(session, sql).await);
                    }
                    if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::DropFunction)) {
                        dispatch_raw!(self, session, sql_trimmed,
                            self.execute_drop_function_cmd(session, sql).await);
                    }
                    if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::CreateTrigger)) {
                        dispatch_raw!(self, session, sql_trimmed,
                            self.execute_create_trigger_cmd(session, sql).await);
                    }
                    if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::DropTrigger)) {
                        dispatch_raw!(self, session, sql_trimmed,
                            self.execute_drop_trigger_cmd(session, sql).await);
                    }
                }

            if !is_observability_user {
                if let Some(reason) = get_skip_reason(&sql_upper) {
                    return Err(SqlError::Unsupported(reason).into());
                }
            }

            if !is_observability_user {
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::AlterOwnerTo)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_alter_owner_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::AlterDefaultPrivileges)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_alter_default_privileges_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::AlterSequenceOwnedBy)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_alter_sequence_owned_by_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::RefreshMaterializedView)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_refresh_materialized_view_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::DropMaterializedView)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_drop_materialized_view_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::Call)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_call_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::DropProcedure)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_drop_procedure_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::CreateProcedure)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_create_procedure_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::CreateTypeEnum)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_create_type_enum_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::AlterType)) {
                    dispatch_raw!(multi: self, session, sql_trimmed,
                        self.execute_alter_type_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::DropType)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_drop_type_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::CreateCollation)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_create_collation_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::DropCollation)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_drop_collation_cmd(session, sql).await);
                }
                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::Analyze)) {
                    dispatch_raw!(self, session, sql_trimmed,
                        self.execute_analyze_cmd(session, sql_trimmed).await);
                }
            }

                if sql_upper.starts_with("ALTER SYSTEM SET ") {
                    if !session.is_superuser() {
                        return Err(SqlError::PermissionDenied {
                            object_type: "system".to_string(),
                            object_name: "ALTER SYSTEM SET".to_string(),
                        }
                        .into());
                    }

                    let rest = sql_trimmed.get(17..).unwrap_or("").trim();
                    let rest_clean = rest.trim_end_matches(';').trim();
                    let (name, raw_value) = if let Some(pos) = rest_clean.find('=') {
                        (&rest_clean[..pos], &rest_clean[pos + 1..])
                    } else {
                        let rest_upper = rest_clean.to_ascii_uppercase();
                        if let Some(pos) = rest_upper.find(" TO ") {
                            (&rest_clean[..pos], &rest_clean[pos + 4..])
                        } else {
                            return Err(anyhow!("syntax error in ALTER SYSTEM SET").into());
                        }
                    };

                    let name_lower = name.trim().to_lowercase();
                    let value_clean = raw_value
                        .trim()
                        .trim_matches('\'')
                        .trim_matches('"')
                        .trim();

                    match name_lower.as_str() {
                        "statement_timeout" | "idle_in_transaction_session_timeout" => {}
                        _ => {
                            return Err(anyhow!(
                                "ALTER SYSTEM SET is only supported for statement_timeout and idle_in_transaction_session_timeout"
                            )
                            .into());
                        }
                    }

                    let ms = crate::sql::session::SessionSettings::parse_timeout_value(value_clean)?;

                    let server_config = session
                        .server_config()
                        .ok_or_else(|| anyhow!("server configuration not available"))?;

                    {
                        let mut cfg = server_config.write().unwrap();
                        match name_lower.as_str() {
                            "statement_timeout" => cfg.statement_timeout_ms = ms,
                            "idle_in_transaction_session_timeout" => {
                                cfg.idle_in_transaction_session_timeout_ms = ms
                            }
                            _ => unreachable!(),
                        }
                    }

                    return Ok(ExecuteResults::single(ExecuteResult::CommandComplete {
                        tag: "ALTER SYSTEM",
                    }));
                }

                // RESET <guc> / RESET ALL — handled directly from raw SQL, bypassing
                // sqlparser entirely. This avoids sentinel-value collisions that arise
                // from rewriting RESET to SET.
                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::Reset)
                ) {
                    let after_kw = sql_trimmed.get(5..).unwrap_or("");
                    let name = crate::sql::raw_sql::extract_reset_name(after_kw)
                        .ok_or_else(|| anyhow!("syntax error at or near \"RESET\""))?;
                    if name.eq_ignore_ascii_case("ALL") {
                        session.reset_all_settings();
                    } else {
                        session.reset_setting(&name.to_lowercase());
                    }
                    return Ok(ExecuteResults::single(ExecuteResult::CommandComplete {
                        tag: "RESET",
                    }));
                }

            let statements = match parse_sql(sql) {
                Ok(stmts) => stmts,
                Err(e) => {
                    if !is_observability_user {
                        if let Some(reason) = get_unsupported_reason(&sql_upper) {
                            return Err(SqlError::Unsupported(reason).into());
                        }
                        // Parse error counts as a statement attempt (for error rate / p99, etc).
                        self.observability.record_statement(
                            Duration::from_millis(0),
                            false,
                            || sql_trimmed.to_string(),
                        );
                    }
                    if session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    return Err(e);
                }
            };

            if statements.is_empty() {
                return Ok(ExecuteResults::single(ExecuteResult::Empty));
            }

            let mut results: Vec<ExecuteResult> = Vec::with_capacity(statements.len());

            for stmt in &statements {
                debug!("Executing statement: {:?}", stmt);
                let is_observability_query = is_observability_user
                    && (is_observability_system_query(stmt) || is_observability_tableless_query(stmt));
                if is_observability_user {
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
                                let value = set_variable_value_to_string(std::slice::from_ref(
                                    value,
                                ))?;
                                if *local && !session.is_in_transaction() {
                                    crate::sql::session::SessionSettings::validate_and_normalize_value(
                                        "timezone",
                                        &value,
                                    )?;
                                    return Ok(vec![
                                        ExecuteResult::Notice {
                                            message:
                                                "SET LOCAL can only be used in transaction blocks"
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
                            Statement::SetNames { charset_name, collation_name } => {
                                if collation_name.is_some() {
                                    return Err(SqlError::Unsupported(
                                        "SET NAMES with COLLATE is not supported".into()
                                    ).into());
                                }
                                session.set_known_setting("client_encoding", charset_name.clone())?;
                                Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
                            }
                            Statement::SetTransaction { modes, snapshot, session: _ } => {
                                if snapshot.is_some() {
                                    return Err(SqlError::Unsupported(
                                        "SET TRANSACTION SNAPSHOT is not supported".into()
                                    ).into());
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
                    })
                .await;

                if stmt_exec.is_err() && session.is_in_transaction() {
                    session.mark_transaction_failed();
                }

                if !is_observability_query {
                    self.observability
                        .record_statement(start.elapsed(), stmt_exec.is_ok(), || {
                            sql_for_observability.clone()
                        });
                }

                results.extend(stmt_exec?);
            }

            // Parser ASTs for large statements (e.g. deep OR chains) can be
            // deeply recursive; drop them on a grown stack to avoid worker
            // stack overflow after successful execution.
            crate::sql::stack_safety::drop_on_grown_stack(statements);

            Ok(ExecuteResults(results))
                }),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
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
}
