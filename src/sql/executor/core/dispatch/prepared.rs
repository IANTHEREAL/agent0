//! Prepared statement execution: `execute_prepared*` family, autocommit/retry
//! framework, observability policy enforcement, and schema drift fallback.

use super::super::prepared_analysis::PreparedAnalysis;
use super::super::prepared_stmt::PreparedExec;
use super::super::prepared_stmt::PreparedStatement;
use super::super::*;
use super::utils::{
    apply_statement_timeout, autocommit_backoff, wrap_with_runtime_context, RuntimeSettings,
};
use crate::sql::expr::bridge::eval_const_ast_expr;
use crate::sql::types::sql_datatype_to_internal_strict;
use std::future::Future;
use std::pin::Pin;
use tracing::warn;

#[derive(Debug)]
pub(in crate::sql::executor::core) enum PreparedTxnResult {
    Executed(ExecuteResult),
    SchemaDrift {
        table_name: String,
        expected: u64,
        current: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::sql::executor::core) enum PreparedObservabilityMode {
    Normal,
    Observability,
}

impl Executor {
    /// Execute a prepared analyzed statement without re-parsing SQL text.
    ///
    /// For schema drift (table version mismatch), this falls back to the
    /// text-based execution path (`execute`) so Parse+Analyze run again.
    pub async fn execute_prepared(
        &self,
        session: &mut Session,
        sql: &str,
        exec: &PreparedExec,
        params: Vec<Option<Value>>,
        param_data_types: &[DataType],
        table_versions: &[(String, u64)],
    ) -> Result<ExecuteResults> {
        if matches!(exec, PreparedExec::RawSqlUtility) {
            unreachable!("RawSqlUtility should not be routed to execute_prepared")
        }

        // Recursive CTE execution over analyzed prepared IR is not implemented yet.
        // Preserve correctness by falling back to the normal text execution path.
        if matches!(
            exec,
            PreparedExec::AnalyzedQuery {
                has_recursive_cte: true,
                ..
            }
        ) {
            warn!("prepared recursive CTE detected; falling back to SQL parse/analyze");
            if !params.is_empty() {
                session.set_pending_params(params);
            }
            if !param_data_types.is_empty() {
                session
                    .set_pending_param_types(param_data_types.iter().cloned().map(Some).collect());
            }
            return self.execute(session, sql).await;
        }

        let qctx = self.build_prepared_query_context(session, params, param_data_types);
        self.execute_prepared_with_framework(session, sql, exec, table_versions, &qctx)
            .await
    }

    fn build_prepared_query_context(
        &self,
        session: &mut Session,
        params: Vec<Option<Value>>,
        param_data_types: &[DataType],
    ) -> Arc<crate::sql::query_context::QueryContext> {
        let statement_ts = statement_time::now_timestamp_millis();
        let transaction_ts = session.transaction_timestamp_ms().unwrap_or(statement_ts);
        let mut qctx = session.query_context_for_statement(statement_ts, transaction_ts);
        qctx.params = params;
        if !param_data_types.is_empty() {
            qctx.param_types = param_data_types.iter().cloned().map(Some).collect();
        }
        Arc::new(qctx)
    }

    async fn execute_prepared_with_framework(
        &self,
        session: &mut Session,
        sql: &str,
        exec: &PreparedExec,
        table_versions: &[(String, u64)],
        qctx: &Arc<crate::sql::query_context::QueryContext>,
    ) -> Result<ExecuteResults> {
        let savepoints = session.savepoints();
        crate::sql::query_context::with_scoped_query_context(
            qctx.as_ref(),
            crate::txn::with_savepoints(savepoints, async {
                let sql_stripped = strip_leading_sql_comments(sql);
                let sql_trimmed = sql_stripped.trim_start();
                let sql_for_observability = sql_trimmed.to_string();
                let is_observability_user =
                    session.current_user() == Some(OBSERVABILITY_USER) && !session.is_superuser();

                if session.is_transaction_failed() && !sql_trimmed.trim().is_empty() {
                    if !is_observability_user {
                        self.observability.record_statement(
                            Duration::from_millis(0),
                            false,
                            || sql_trimmed.to_string(),
                        );
                    }
                    return Err(SqlError::InFailedTransaction.into());
                }

                let observability_policy =
                    self.enforce_observability_prepared_policy(session, sql_trimmed, exec);

                let start = Instant::now();
                let (exec_result, should_record_statement) = match observability_policy {
                    Ok(mode) => {
                        let is_observability_query =
                            matches!(mode, PreparedObservabilityMode::Observability);
                        (
                            self.execute_prepared_with_runtime_context(
                                session,
                                sql,
                                exec,
                                table_versions,
                                qctx.as_ref(),
                                is_observability_query,
                            )
                            .await,
                            matches!(mode, PreparedObservabilityMode::Normal),
                        )
                    }
                    Err(err) => {
                        // Keep parity with execute_single: denied observability
                        // statements are rejected before per-statement recording.
                        (Err(err), false)
                    }
                };

                if exec_result.is_err() && session.is_in_transaction() {
                    session.mark_transaction_failed();
                }

                if should_record_statement {
                    self.observability.record_statement(
                        start.elapsed(),
                        exec_result.is_ok(),
                        || sql_for_observability.clone(),
                    );
                }

                exec_result
            }),
        )
        .await
    }

    fn execute_prepared_with_runtime_context<'a>(
        &'a self,
        session: &'a mut Session,
        sql: &'a str,
        exec: &'a PreparedExec,
        table_versions: &'a [(String, u64)],
        qctx: &'a crate::sql::query_context::QueryContext,
        is_observability_query: bool,
    ) -> Pin<Box<dyn Future<Output = Result<ExecuteResults>> + Send + 'a>> {
        let rt_settings = RuntimeSettings::from_session(session);

        let execute_future: Pin<Box<dyn Future<Output = Result<ExecuteResults>> + Send + 'a>> =
            Box::pin(self.execute_prepared_autocommit(
                session,
                sql,
                exec,
                table_versions,
                qctx,
                is_observability_query,
            ));

        wrap_with_runtime_context(
            &rt_settings,
            self.tenant_keyspace(),
            self.store.transaction_client(),
            execute_future,
        )
    }

    async fn execute_prepared_autocommit(
        &self,
        session: &mut Session,
        sql: &str,
        exec: &PreparedExec,
        table_versions: &[(String, u64)],
        qctx: &crate::sql::query_context::QueryContext,
        is_observability_query: bool,
    ) -> Result<ExecuteResults> {
        let is_autocommit = !session.is_in_transaction();
        let db_id = session.current_database_id();
        let max_attempts = if is_autocommit { 10usize } else { 1usize };

        for attempt in 0..max_attempts {
            if is_autocommit {
                session.begin().await?;
            }

            let current_role = session.current_user().map(|u| u.to_string());
            let res = self
                .execute_prepared_attempt(
                    session,
                    db_id,
                    exec,
                    table_versions,
                    current_role.as_deref(),
                )
                .await;

            if res
                .as_ref()
                .err()
                .is_some_and(|e| e.is::<StatementTimeoutError>())
                && !is_autocommit
            {
                // Keep non-autocommit timeout behavior aligned with execute_single:
                // abort explicit transaction to avoid unknown partial state.
                session.rollback().await?;
                self.clear_trigger_activations();
            }

            if is_autocommit {
                match res {
                    Ok(PreparedTxnResult::Executed(result)) => {
                        if is_observability_query {
                            session.rollback().await?;
                            self.clear_trigger_activations();
                        } else {
                            session.commit().await?;
                            self.flush_trigger_activations();
                        }
                        return Ok(ExecuteResults::single(result));
                    }
                    Ok(PreparedTxnResult::SchemaDrift {
                        table_name,
                        expected,
                        current,
                    }) => {
                        session.rollback().await?;
                        self.clear_trigger_activations();
                        warn!(
                            table = %table_name,
                            expected_version = expected,
                            current_version = ?current,
                            "prepared schema drift detected; falling back to SQL parse/analyze"
                        );
                        return self
                            .execute_prepared_text_fallback(session, sql, qctx)
                            .await;
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
                return match res? {
                    PreparedTxnResult::Executed(result) => Ok(ExecuteResults::single(result)),
                    PreparedTxnResult::SchemaDrift {
                        table_name,
                        expected,
                        current,
                    } => {
                        warn!(
                            table = %table_name,
                            expected_version = expected,
                            current_version = ?current,
                            "prepared schema drift detected; falling back to SQL parse/analyze"
                        );
                        self.execute_prepared_text_fallback(session, sql, qctx)
                            .await
                    }
                };
            }
        }

        unreachable!("retry loop must return")
    }

    async fn execute_prepared_attempt(
        &self,
        session: &mut Session,
        db_id: u64,
        exec: &PreparedExec,
        table_versions: &[(String, u64)],
        current_role: Option<&str>,
    ) -> Result<PreparedTxnResult> {
        let timeout = session.statement_timeout();
        let fut = async {
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            if let Some((table_name, expected, current)) = self
                .first_schema_drift_on_txn(txn, db_id, table_versions)
                .await?
            {
                return Ok::<PreparedTxnResult, anyhow::Error>(PreparedTxnResult::SchemaDrift {
                    table_name,
                    expected,
                    current,
                });
            }
            let result = self
                .execute_prepared_on_txn(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    exec,
                    current_role,
                )
                .await?;
            Ok::<PreparedTxnResult, anyhow::Error>(PreparedTxnResult::Executed(result))
        };

        apply_statement_timeout(timeout, fut).await
    }

    fn enforce_observability_prepared_policy(
        &self,
        session: &mut Session,
        sql_trimmed: &str,
        exec: &PreparedExec,
    ) -> Result<PreparedObservabilityMode> {
        let is_observability_user =
            session.current_user() == Some(OBSERVABILITY_USER) && !session.is_superuser();
        if !is_observability_user {
            return Ok(PreparedObservabilityMode::Normal);
        }

        // Prepared analyzed path only handles SELECT/DML. For observability users,
        // keep the same policy as execute_single: only allow system/tableless SELECT.
        if !matches!(exec, PreparedExec::AnalyzedQuery { .. }) {
            if session.is_in_transaction() {
                session.mark_transaction_failed();
            }
            return Err(SqlError::PermissionDenied {
                object_type: "role".into(),
                object_name: OBSERVABILITY_USER.to_string(),
            }
            .into());
        }

        let statements = parse_sql(sql_trimmed)?;
        let Some(stmt) = statements.first() else {
            if session.is_in_transaction() {
                session.mark_transaction_failed();
            }
            return Err(SqlError::PermissionDenied {
                object_type: "role".into(),
                object_name: OBSERVABILITY_USER.to_string(),
            }
            .into());
        };

        let is_observability_query =
            is_observability_system_query(stmt) || is_observability_tableless_query(stmt);
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

        Ok(PreparedObservabilityMode::Observability)
    }

    async fn execute_prepared_text_fallback(
        &self,
        session: &mut Session,
        sql: &str,
        qctx: &crate::sql::query_context::QueryContext,
    ) -> Result<ExecuteResults> {
        if !qctx.params.is_empty() {
            session.set_pending_params(qctx.params.clone());
        }
        if !qctx.param_types.is_empty() {
            session.set_pending_param_types(qctx.param_types.clone());
        }
        self.execute(session, sql).await
    }

    pub(in crate::sql::executor::core) async fn execute_prepared_on_txn(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        exec: &PreparedExec,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        match exec {
            PreparedExec::AnalyzedQuery {
                analyzed,
                locks,
                select_into,
                required_privileges,
                ..
            } => {
                for (table_name, privilege) in required_privileges {
                    self.require_table_privilege(
                        txn,
                        current_role,
                        (*privilege).clone(),
                        table_name,
                    )
                    .await?;
                }

                let prepared_ctes = self
                    .build_cte_context_from_analyzed_with_base(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        analyzed,
                        &HashMap::new(),
                    )
                    .await?;

                let result = self
                    .execute_via_optimizer(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        std::borrow::Cow::Borrowed(analyzed),
                        &prepared_ctes,
                        locks,
                    )
                    .await?;

                if let Some(target_name) = select_into.as_ref().map(|into| into.name.clone()) {
                    return self
                        .create_table_from_result(txn, db_id, search_path, &target_name, result)
                        .await;
                }

                Ok(result)
            }
            PreparedExec::AnalyzedDml {
                analyzed,
                required_privileges,
            } => {
                for (table_name, privilege) in required_privileges {
                    self.require_table_privilege(
                        txn,
                        current_role,
                        (*privilege).clone(),
                        table_name,
                    )
                    .await?;
                }
                match analyzed {
                    crate::sql::analyzer::types::AnalyzedStatement::Insert(ins) => {
                        self.execute_analyzed_insert(txn, db_id, sequence_values, search_path, &ins)
                            .await
                    }
                    crate::sql::analyzer::types::AnalyzedStatement::Update(upd) => {
                        self.execute_analyzed_update(txn, db_id, sequence_values, search_path, &upd)
                            .await
                    }
                    crate::sql::analyzer::types::AnalyzedStatement::Delete(del) => {
                        self.execute_analyzed_delete(txn, db_id, sequence_values, search_path, &del)
                            .await
                    }
                    crate::sql::analyzer::types::AnalyzedStatement::Query(_) => {
                        Err(anyhow!("prepared DML execution received query variant"))
                    }
                }
            }
            PreparedExec::RawSqlUtility => {
                unreachable!("RawSqlUtility should not be routed to execute_prepared_on_txn")
            }
        }
    }

    /// Execute a SQL-level `PREPARE name (types) AS statement`.
    pub(super) async fn execute_sql_prepare_statement(
        &self,
        session: &mut Session,
        name: &sqlparser::ast::Ident,
        data_types: &[sqlparser::ast::DataType],
        statement: &Statement,
    ) -> Result<Vec<ExecuteResult>> {
        let prepared_name = normalize_ident(name);
        let prepared_sql = statement.to_string();

        let mut client_oids: Vec<Option<DataType>> = Vec::with_capacity(data_types.len());
        for sql_type in data_types {
            client_oids.push(Some(sql_datatype_to_internal_strict(sql_type)?));
        }

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let db_id = session.current_database_id();
        let analysis_result = {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            self.analyze_for_prepared(
                txn,
                db_id,
                search_path,
                &prepared_sql,
                client_oids.len(),
                &client_oids,
            )
            .await
        };

        let analysis = match analysis_result {
            Ok(analysis) => {
                if is_autocommit {
                    session.commit().await?;
                    self.flush_trigger_activations();
                }
                analysis
            }
            Err(err) => {
                if is_autocommit {
                    session.rollback().await?;
                    self.clear_trigger_activations();
                }
                return Err(err);
            }
        };

        let prepared_stmt = match analysis {
            PreparedAnalysis::Query {
                analyzed,
                locks,
                select_into,
                output_schema,
                param_types,
                base_table_names,
                table_versions,
                has_recursive_cte,
            } => {
                let required_privileges = base_table_names
                    .into_iter()
                    .map(|t| (t, crate::auth::Privilege::Select))
                    .collect();
                PreparedStatement {
                    sql: prepared_sql,
                    exec: PreparedExec::AnalyzedQuery {
                        analyzed,
                        locks,
                        select_into,
                        required_privileges,
                        has_recursive_cte,
                    },
                    output_schema,
                    param_data_types: param_types,
                    table_versions,
                }
            }
            PreparedAnalysis::Dml {
                analyzed,
                output_schema,
                param_types,
                table_versions,
            } => {
                let required_privileges = PreparedStatement::compute_privileges(&analyzed, &[]);
                PreparedStatement {
                    sql: prepared_sql,
                    exec: PreparedExec::AnalyzedDml {
                        analyzed,
                        required_privileges,
                    },
                    output_schema,
                    param_data_types: param_types,
                    table_versions,
                }
            }
            PreparedAnalysis::Utility => {
                return Err(SqlError::Unsupported(
                    "PREPARE only supports SELECT/INSERT/UPDATE/DELETE statements".to_string(),
                )
                .into());
            }
        };

        session.put_sql_prepared_statement(prepared_name, prepared_stmt);
        Ok(vec![ExecuteResult::CommandComplete { tag: "PREPARE" }])
    }

    /// Execute a SQL-level `EXECUTE name (params)`.
    pub(super) async fn execute_sql_execute_statement(
        &self,
        session: &mut Session,
        name: &sqlparser::ast::Ident,
        parameters: &[Expr],
    ) -> Result<Vec<ExecuteResult>> {
        let prepared_name = normalize_ident(name);
        let prepared = session
            .get_sql_prepared_statement_cloned(&prepared_name)
            .ok_or_else(|| {
                SqlError::UndefinedObject(format!(
                    "prepared statement \"{}\" does not exist",
                    prepared_name
                ))
            })?;

        if parameters.len() != prepared.param_data_types.len() {
            return Err(SqlError::InvalidParameterUsage {
                index: prepared.param_data_types.len().max(1),
                context: format!(
                    "prepared statement \"{}\" expects {} parameters, but {} were given",
                    prepared_name,
                    prepared.param_data_types.len(),
                    parameters.len()
                ),
            }
            .into());
        }

        let mut param_values: Vec<Option<Value>> = Vec::with_capacity(parameters.len());
        for expr in parameters {
            param_values.push(Some(eval_const_ast_expr(expr)?));
        }

        let statement_ts = statement_time::now_timestamp_millis();
        let transaction_ts = session.transaction_timestamp_ms().unwrap_or(statement_ts);
        let mut qctx = session.query_context_for_statement(statement_ts, transaction_ts);
        qctx.params = param_values;
        if !prepared.param_data_types.is_empty() {
            qctx.param_types = prepared
                .param_data_types
                .iter()
                .cloned()
                .map(Some)
                .collect();
        }
        let qctx = Arc::new(qctx);
        let savepoints = session.savepoints();

        crate::sql::query_context::with_scoped_query_context(
            qctx.as_ref(),
            crate::txn::with_savepoints(savepoints, async {
                let is_autocommit = !session.is_in_transaction();
                let db_id = session.current_database_id();
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
                        self.execute_prepared_on_txn(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &prepared.exec,
                            current_role.as_deref(),
                        )
                        .await
                    };

                    let res = apply_statement_timeout(timeout, fut).await;

                    if res
                        .as_ref()
                        .err()
                        .is_some_and(|e| e.is::<StatementTimeoutError>())
                        && !is_autocommit
                    {
                        session.rollback().await?;
                        self.clear_trigger_activations();
                    }

                    if is_autocommit {
                        match res {
                            Ok(result) => {
                                session.commit().await?;
                                self.flush_trigger_activations();
                                return Ok(vec![result]);
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
                        return Ok(vec![res?]);
                    }
                }

                unreachable!("retry loop must return")
            }),
        )
        .await
    }

    /// Execute a SQL-level `DEALLOCATE name` or `DEALLOCATE ALL`.
    pub(super) fn execute_sql_deallocate_statement(
        &self,
        session: &mut Session,
        name: &sqlparser::ast::Ident,
    ) -> Result<Vec<ExecuteResult>> {
        let prepared_name = normalize_ident(name);
        if prepared_name.eq_ignore_ascii_case("all") {
            session.clear_sql_prepared_statements();
            return Ok(vec![ExecuteResult::CommandComplete { tag: "DEALLOCATE" }]);
        }

        if !session.remove_sql_prepared_statement(&prepared_name) {
            return Err(SqlError::UndefinedObject(format!(
                "prepared statement \"{}\" does not exist",
                prepared_name
            ))
            .into());
        }

        Ok(vec![ExecuteResult::CommandComplete { tag: "DEALLOCATE" }])
    }
}
