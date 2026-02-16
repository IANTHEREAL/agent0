//! Simple-query dispatch

use super::*;

/// Validate and apply transaction modes (isolation level, access mode) from
/// `BEGIN ISOLATION LEVEL ...` or `START TRANSACTION ...` statements.
///
/// Rejects SERIALIZABLE (TiKV cannot provide true serializable guarantees)
/// and stores accepted modes in the session for `SHOW` readback.
fn validate_transaction_modes(session: &mut Session, modes: &[TransactionMode]) -> Result<()> {
    for mode in modes {
        match mode {
            TransactionMode::IsolationLevel(level) => {
                let level_str = match level {
                    TransactionIsolationLevel::ReadUncommitted
                    | TransactionIsolationLevel::ReadCommitted => "read committed",
                    TransactionIsolationLevel::RepeatableRead => "repeatable read",
                    TransactionIsolationLevel::Serializable => {
                        return Err(SqlError::Unsupported(
                            "SERIALIZABLE isolation level is not supported".into(),
                        )
                        .into());
                    }
                };
                session.set_known_setting("transaction_isolation", level_str.to_string())?;
            }
            TransactionMode::AccessMode(access_mode) => {
                let mode_str = match access_mode {
                    TransactionAccessMode::ReadOnly => "on",
                    TransactionAccessMode::ReadWrite => "off",
                };
                session.set_known_setting("default_transaction_read_only", mode_str.to_string())?;
            }
        }
    }
    Ok(())
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
        let savepoints = session.savepoints();
        let connection_id = session.connection_id();
        let database_name = session.current_database_name_arc();
        let use_optimizer = session.use_optimizer();
        crate::sql::query_context::with_query_context(
            connection_id,
            database_name,
            use_optimizer,
            statement_time::with_timestamps(
                statement_ts,
                transaction_ts,
                crate::txn::with_savepoints(savepoints, async {
                let sql_stripped = strip_leading_sql_comments(sql);
                let sql_trimmed = sql_stripped.trim_start();
                let is_observability_user =
                    session.current_user() == Some(OBSERVABILITY_USER) && !session.is_superuser();
                let starts_with = |prefix: &str| starts_with_ignore_ascii_case(sql_trimmed, prefix);
                let sql_upper = sql_trimmed.trim().to_ascii_uppercase();
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
                    return Err(anyhow::Error::new(InFailedSqlTransaction));
                }

                if !is_observability_user {
                    if matches!(
                        raw_kind,
                        Some(crate::sql::raw_sql::RawSqlKind::CreateDatabase)
                    ) {
                        let start = Instant::now();
                        let res = self.execute_create_database_cmd(session, sql).await;
                        if res.is_err() && session.is_in_transaction() {
                            session.mark_transaction_failed();
                        }
                        self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                            sql_trimmed.to_string()
                        });
                        return res;
                    }
                    if matches!(
                        raw_kind,
                        Some(crate::sql::raw_sql::RawSqlKind::DropDatabase)
                    ) {
                        let start = Instant::now();
                        let res = self.execute_drop_database_cmd(session, sql).await;
                        if res.is_err() && session.is_in_transaction() {
                            session.mark_transaction_failed();
                        }
                        self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                            sql_trimmed.to_string()
                        });
                        return res;
                    }
                    if matches!(
                        raw_kind,
                        Some(crate::sql::raw_sql::RawSqlKind::AlterDatabase)
                    ) {
                        let start = Instant::now();
                        let res = self.execute_alter_database_cmd(session, sql).await;
                        if res.is_err() && session.is_in_transaction() {
                            session.mark_transaction_failed();
                        }
                        self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                            sql_trimmed.to_string()
                        });
                        return res;
                    }
                    if matches!(
                        raw_kind,
                        Some(crate::sql::raw_sql::RawSqlKind::CreateExtension)
                    ) {
                        let start = Instant::now();
                        let res = self.execute_create_extension_cmd(session, sql).await;
                        if res.is_err() && session.is_in_transaction() {
                            session.mark_transaction_failed();
                        }
                        self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                            sql_trimmed.to_string()
                        });
                        return res.map(ExecuteResults::single);
                    }
                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::DropExtension)
                ) {
                    let start = Instant::now();
                    let res = self.execute_drop_extension_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::CommentOn)
                ) {
                    let start = Instant::now();
                    let res = self.execute_comment_on_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::CreateFunction)
                ) {
                    let start = Instant::now();
                    let res = self.execute_create_function_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::DropFunction)
                ) {
                    let start = Instant::now();
                    let res = self.execute_drop_function_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::CreateTrigger)
                ) {
                    let start = Instant::now();
                    let res = self.execute_create_trigger_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::DropTrigger)
                ) {
                    let start = Instant::now();
                    let res = self.execute_drop_trigger_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
            }

            if !is_observability_user {
                if let Some(reason) = get_skip_reason(&sql_upper) {
                    return Err(SqlError::Unsupported(reason).into());
                }
            }

            if !is_observability_user {
                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::AlterOwnerTo)
                ) {
                    let start = Instant::now();
                    let res = self.execute_alter_owner_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::AlterDefaultPrivileges)
                ) {
                    let start = Instant::now();
                    let res = self
                        .execute_alter_default_privileges_cmd(session, sql)
                        .await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::AlterSequenceOwnedBy)
                ) {
                    let start = Instant::now();
                    let res = self.execute_alter_sequence_owned_by_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::RefreshMaterializedView)
                ) {
                    let start = Instant::now();
                    let res = self.execute_refresh_materialized_view_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::DropMaterializedView)
                ) {
                    let start = Instant::now();
                    let res = self.execute_drop_materialized_view_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if matches!(raw_kind, Some(crate::sql::raw_sql::RawSqlKind::Call)) {
                    let start = Instant::now();
                    let res = self.execute_call_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::DropProcedure)
                ) {
                    let start = Instant::now();
                    let res = self.execute_drop_procedure_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::CreateProcedure)
                ) {
                    let start = Instant::now();
                    let res = self.execute_create_procedure_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::CreateTypeEnum)
                ) {
                    let start = Instant::now();
                    let res = self.execute_create_type_enum_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::DropType)
                ) {
                    let start = Instant::now();
                    let res = self.execute_drop_type_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if matches!(
                    raw_kind,
                    Some(crate::sql::raw_sql::RawSqlKind::Analyze)
                ) {
                    let start = Instant::now();
                    let res = self.execute_analyze_cmd(session, sql_trimmed).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
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
                    match stmt {
                        // For observability user, allow common utility statements and return
                        // semantically correct protocol responses (never `EmptyQueryResponse` for
                        // non-empty SQL).
                        Statement::StartTransaction { modes, .. } => {
                            validate_transaction_modes(session, modes)?;
                            session.begin().await?;
                            results.push(ExecuteResult::TransactionStart { tag: "BEGIN" });
                            continue;
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
                            results.push(ExecuteResult::TransactionEnd { tag });
                            continue;
                        }
                        Statement::Savepoint { name } => {
                            session.create_savepoint(normalize_ident(name)).await?;
                            results.push(ExecuteResult::CommandComplete { tag: "SAVEPOINT" });
                            continue;
                        }
                        Statement::ReleaseSavepoint { name } => {
                            let sp = normalize_ident(name);
                            session.release_savepoint(&sp).await?;
                            results.push(ExecuteResult::CommandComplete { tag: "RELEASE" });
                            continue;
                        }
                        Statement::Rollback {
                            savepoint: Some(name),
                            ..
                        } => {
                            let sp = normalize_ident(name);
                            session.rollback_to_savepoint(&sp).await?;
                            results.push(ExecuteResult::CommandComplete { tag: "ROLLBACK" });
                            continue;
                        }
                        Statement::Rollback {
                            savepoint: None, ..
                        } => {
                            session.rollback().await?;
                            self.clear_trigger_activations();
                            results.push(ExecuteResult::TransactionEnd { tag: "ROLLBACK" });
                            continue;
                        }
                        // SET variants: fall through to the real-user SET handling
                        // below. Session settings are per-connection and safe for
                        // observability users.
                        Statement::SetVariable { .. }
                        | Statement::SetTimeZone { .. }
                        | Statement::SetNames { .. }
                        | Statement::SetTransaction { .. } => {}
                        Statement::ShowVariable { variable } => {
                            let var_name = variable
                                .iter()
                                .map(normalize_ident)
                                .collect::<Vec<_>>()
                                .join(".")
                                .to_lowercase();
                            let value = match session.show_setting_value(&var_name) {
                                Some(value) => value,
                                None => {
                                    let err = anyhow!(
                                        "unrecognized configuration parameter \"{}\"",
                                        var_name
                                    );
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

                            results.push(ExecuteResult::Select {
                                columns: vec![var_name],
                                column_types: Some(vec![DataType::Text]),
                                rows: vec![Row::new(vec![Value::Text(value)])],
                                timezone,
                            });
                            continue;
                        }
                        Statement::Query(_) => {
                            if !is_observability_query {
                                if session.is_in_transaction() {
                                    session.mark_transaction_failed();
                                }
                                return Err(SqlError::PermissionDenied {
                                    object_type: "role".into(),
                                    object_name: OBSERVABILITY_USER.to_string(),
                                }.into());
                            }
                        }
                        _ => {
                            if session.is_in_transaction() {
                                session.mark_transaction_failed();
                            }
                            return Err(SqlError::PermissionDenied {
                                object_type: "role".into(),
                                object_name: OBSERVABILITY_USER.to_string(),
                            }.into());
                        }
                    }
                }
                let start = Instant::now();
                let is_superuser = session.is_superuser();
                let timezone = Arc::from(
                    session
                        .show_setting_value("timezone")
                        .unwrap_or_else(|| "UTC".to_string()),
                );
                let max_sort_bytes = session.max_sort_bytes();
                let stmt_exec: Result<Vec<ExecuteResult>> = session_context::with_timezone(
                    timezone,
                    session_context::with_max_sort_bytes(
                        max_sort_bytes,
                        crate::extensions::context::with_context(is_superuser, async {
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
	                                if let Some(role_ident) = role_name.as_ref() {
	                                    if role_ident.quote_style.is_none()
	                                        && role_ident.value.eq_ignore_ascii_case("default")
	                                    {
	                                        session.reset_role();
	                                    } else {
	                                        let role_name = role_ident.value.clone();
	                                    let session_user = session
	                                        .session_user()
	                                        .map(|u| u.to_string())
	                                        .ok_or_else(|| anyhow!("Missing session user"))?;

	                                    let is_autocommit = !session.is_in_transaction();
	                                    if is_autocommit {
	                                        session.begin().await?;
	                                    }

	                                    let result = async {
	                                        let (txn, _sequence_values, _search_path) = session
	                                            .get_mut_txn_sequence_values_and_search_path()
	                                            .expect("Transaction must be active");

	                                        let user = self.auth_manager.get_user(txn, &role_name).await?;
	                                        let role = self.auth_manager.get_role(txn, &role_name).await?;
	                                        let is_superuser = user
	                                            .as_ref()
	                                            .map(|u| u.is_superuser)
	                                            .or_else(|| role.as_ref().map(|r| r.is_superuser))
	                                            .ok_or_else(|| anyhow!("role \"{}\" does not exist", role_name))?;

	                                        let session_user_def =
	                                            self.auth_manager.get_user(txn, &session_user).await?;
	                                        let can_set_role = match session_user_def.as_ref() {
	                                            Some(user) if user.is_superuser => true,
	                                            Some(user) => {
	                                                role_name == session_user
	                                                    || user.roles.contains(&role_name)
	                                            }
	                                            None => role_name == session_user,
	                                        };
	                                        if !can_set_role {
	                                            return Err(SqlError::PermissionDenied {
	                                                object_type: "role".into(),
	                                                object_name: role_name.clone(),
	                                            }
	                                            .into());
	                                        }
	                                        Ok::<bool, anyhow::Error>(is_superuser)
	                                    }
	                                    .await;

                                    if is_autocommit {
                                        if result.is_ok() {
                                            session.commit().await?;
                                            self.flush_trigger_activations();
                                        } else {
                                            session.rollback().await?;
                                            self.clear_trigger_activations();
                                        }
                                    }

	                                    let is_superuser = result?;
	                                    session.set_current_role(role_name, is_superuser);
	                                    }
	                                } else {
	                                    // `SET ROLE NONE` (and our `RESET ROLE` rewrite) resets to the
	                                    // session user.
	                                    session.reset_role();
	                                }
	                                Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
	                            }
                            Statement::SetVariable {
                                variable, value, ..
                            } => {
                                let var_name = variable
                                    .0
                                    .iter()
                                    .map(normalize_ident)
                                    .collect::<Vec<_>>()
                                    .join(".")
                                    .to_lowercase();

                                if var_name == "search_path" {
                                    let mut new_search_path = Vec::new();
                                    for expr in value {
                                        match expr {
                                            Expr::Identifier(ident) => {
                                                new_search_path.push(normalize_ident(ident));
                                            }
                                            Expr::CompoundIdentifier(idents) if idents.len() == 1 => {
                                                new_search_path.push(normalize_ident(&idents[0]));
                                            }
                                            Expr::Value(sqlparser::ast::Value::SingleQuotedString(
                                                s,
                                            )) => {
                                                new_search_path.extend(parse_search_path_guc_value(s));
                                            }
                                            _ => {
                                                return Err(anyhow!(
                                                    "Unsupported search_path value: {}",
                                                    expr
                                                ));
                                            }
                                        }
                                    }

                                    new_search_path.retain(|s| !s.is_empty() && s != "$user");
                                    if new_search_path.len() == 1
                                        && new_search_path[0] == "default"
                                    {
                                        new_search_path = vec!["public".to_string()];
                                    }
                                    for schema in &new_search_path {
                                        if schema.contains('.') {
                                            return Err(anyhow!(
                                                "schema name '{}' must not contain '.'",
                                                schema
                                            ));
                                        }
                                    }
                                    if new_search_path.is_empty() {
                                        new_search_path.push("public".to_string());
                                    }
                                    session.set_search_path(new_search_path);
                                } else {
                                    let value = set_variable_value_to_string(value)?;
                                    session.set_known_setting(&var_name, value)?;
                                }
                                Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
                            }
                            Statement::SetTimeZone { value, .. } => {
                                let value = set_variable_value_to_string(std::slice::from_ref(
                                    value,
                                ))?;
                                session.set_known_setting("timezone", value)?;
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
                            // DDL/DML - delegated to session transaction management
                            _ => {
                                if let Statement::Query(query) = stmt {
                                    if let Some(result) =
                                        try_execute_set_config_select(session, query.as_ref())?
                                    {
                                        return Ok(vec![result]);
                                    }
                                    if let Some(result) =
                                        try_execute_current_setting_select(session, query.as_ref())?
                                    {
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
                                            .collect_notices_before_statement(
                                                txn,
                                                db_id,
                                                search_path,
                                                stmt,
                                            )
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
                                        Ok::<(Vec<ExecuteResult>, ExecuteResult), anyhow::Error>((
                                            notices, result,
                                        ))
                                    };

                                    let res = match timeout {
                                        Some(timeout) => match tokio::time::timeout(timeout, fut).await {
                                            Ok(res) => res,
                                            Err(_) => Err(anyhow::Error::new(StatementTimeoutError)),
                                        },
                                        None => fut.await,
                                    };

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
                                                let should_retry = attempt + 1 < max_attempts
                                                    && is_retryable_tikv_error(&err);
                                                if should_retry {
                                                    // Exponential backoff with jitter to reduce contention
                                                    let base_ms = 5u64.saturating_mul(1u64 << attempt.min(6));
                                                    let jitter_ms = rand::random::<u64>() % (base_ms + 1);
                                                    let backoff_ms = base_ms + jitter_ms;
                                                    tokio::time::sleep(Duration::from_millis(backoff_ms))
                                                        .await;
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
                    }),
                    ),
                )
                .await;

                if stmt_exec.is_err() && session.is_in_transaction() {
                    session.mark_transaction_failed();
                }

                if !is_observability_query {
                    self.observability
                        .record_statement(start.elapsed(), stmt_exec.is_ok(), || stmt.to_string());
                }

                results.extend(stmt_exec?);
            }

            Ok(ExecuteResults(results))
                }),
            ),
        )
        .await
    }

    async fn collect_notices_before_statement(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        stmt: &Statement,
    ) -> Result<Vec<ExecuteResult>> {
        use sqlparser::ast::ObjectType;

        match stmt {
            Statement::Drop {
                object_type: ObjectType::Table,
                names: drop_names,
                if_exists: true,
                ..
            } => {
                let mut notices = Vec::new();
                for name in drop_names {
                    let exists = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        name,
                        search_path,
                    )
                    .await?
                    .is_some();

                    if exists {
                        continue;
                    }

                    let base = name
                        .0
                        .last()
                        .map(|ident| ident.value.as_str())
                        .unwrap_or("?");
                    notices.push(ExecuteResult::Notice {
                        message: format!("table \"{}\" does not exist, skipping", base),
                    });
                }
                Ok(notices)
            }
            _ => Ok(Vec::new()),
        }
    }
}
