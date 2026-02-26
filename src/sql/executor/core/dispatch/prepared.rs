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

fn is_plan_cache_eligible(exec: &PreparedExec) -> bool {
    match exec {
        PreparedExec::AnalyzedQuery {
            analyzed,
            has_recursive_cte: false,
            ..
        } => !crate::sql::executor::select::analyzed::query_needs_pre_materialization(analyzed),
        _ => false,
    }
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
        table_versions: &[(String, u64, u64)],
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
        self.execute_prepared_with_framework(
            session,
            sql,
            exec,
            param_data_types,
            table_versions,
            &qctx,
        )
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
        param_data_types: &[DataType],
        table_versions: &[(String, u64, u64)],
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
                                param_data_types,
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
        param_data_types: &'a [DataType],
        table_versions: &'a [(String, u64, u64)],
        qctx: &'a crate::sql::query_context::QueryContext,
        is_observability_query: bool,
    ) -> Pin<Box<dyn Future<Output = Result<ExecuteResults>> + Send + 'a>> {
        let rt_settings = RuntimeSettings::from_session(session);

        let execute_future: Pin<Box<dyn Future<Output = Result<ExecuteResults>> + Send + 'a>> =
            Box::pin(self.execute_prepared_autocommit(
                session,
                sql,
                exec,
                param_data_types,
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
        param_data_types: &[DataType],
        table_versions: &[(String, u64, u64)],
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
                    sql,
                    exec,
                    param_data_types,
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
        sql: &str,
        exec: &PreparedExec,
        param_data_types: &[DataType],
        table_versions: &[(String, u64, u64)],
        current_role: Option<&str>,
    ) -> Result<PreparedTxnResult> {
        use crate::sql::executor::core::plan_cache::{
            PlanCacheEntry, PlanCacheKey, PlanDependency,
        };

        // ── Plan cache: build key + lookup ──
        let resolved_table_ids: Vec<u64> = table_versions
            .iter()
            .map(|(_, table_id, _)| *table_id)
            .collect();
        let cache_key = PlanCacheKey::new(
            sql.to_string(),
            param_data_types,
            db_id,
            session.search_path(),
            &resolved_table_ids,
        );

        // Check eligibility: analyzed SELECT only, and never cache plans that
        // require pre-materialization (subquery/async constants).
        let cache_eligible = is_plan_cache_eligible(exec);

        // Record execution and check cache.
        let (cached_entry, should_promote) = if cache_eligible {
            let exec_count = session.plan_cache().record_execution(&cache_key);
            let min_exec = session.plan_cache().min_exec();
            let cached = session.plan_cache().get(&cache_key).cloned();
            let should_promote = cached.is_none() && exec_count >= min_exec;
            (cached, should_promote)
        } else {
            (None, false)
        };

        // Extract cached plan/dependency metadata for hit-time drift validation.
        let cached_physical = cached_entry.as_ref().map(|e| e.physical_plan.clone());
        let cached_dependency_versions: Option<Vec<(String, u64, u64)>> =
            cached_entry.as_ref().map(|entry| {
                entry
                    .dependencies
                    .iter()
                    .map(|dep| (dep.table_name.clone(), dep.table_id, dep.schema_version))
                    .collect()
            });

        let timeout = session.statement_timeout();
        let table_versions_for_cache: Vec<(String, u64, u64)> = table_versions.to_vec();
        let fut = async {
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let mut invalidate_cached_hit = false;
            let mut cached_physical_for_exec = cached_physical.as_ref();

            if let Some(cache_deps) = cached_dependency_versions.as_ref() {
                if self
                    .first_schema_drift_on_txn(txn, db_id, cache_deps)
                    .await?
                    .is_some()
                {
                    // Cache hit exists but entry dependencies are stale.
                    invalidate_cached_hit = true;
                    cached_physical_for_exec = None;
                }
            }

            if let Some((table_name, expected, current)) = self
                .first_schema_drift_on_txn(txn, db_id, table_versions)
                .await?
            {
                return Ok::<(PreparedTxnResult, _, bool), anyhow::Error>((
                    PreparedTxnResult::SchemaDrift {
                        table_name,
                        expected,
                        current,
                    },
                    None,
                    invalidate_cached_hit,
                ));
            }

            let capture_plan = should_promote || invalidate_cached_hit;
            let (result, new_plan) = self
                .execute_prepared_on_txn_with_plan(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    exec,
                    current_role,
                    cached_physical_for_exec,
                    capture_plan,
                )
                .await?;
            Ok::<(PreparedTxnResult, _, bool), anyhow::Error>((
                PreparedTxnResult::Executed(result),
                new_plan,
                invalidate_cached_hit,
            ))
        };

        let result = apply_statement_timeout(timeout, fut).await;

        // ── Plan cache: invalidate stale hit entries before potential reinsert ──
        if let Ok((_, _, invalidate_cached_hit)) = &result {
            if *invalidate_cached_hit {
                session.plan_cache().invalidate(&cache_key);
            }
        }

        // ── Plan cache: promote new plan into cache ──
        if let Ok((PreparedTxnResult::Executed(_), Some(new_physical), invalidate_cached_hit)) =
            &result
        {
            if should_promote || *invalidate_cached_hit {
                let deps: Vec<PlanDependency> = table_versions_for_cache
                    .iter()
                    .map(|(name, table_id, version)| PlanDependency {
                        table_name: name.clone(),
                        table_id: *table_id,
                        schema_version: *version,
                    })
                    .collect();
                session.plan_cache().insert(
                    cache_key,
                    PlanCacheEntry {
                        physical_plan: new_physical.clone(),
                        dependencies: deps,
                    },
                );
            }
        }

        result.map(|(txn_result, _, _)| txn_result)
    }

    /// Execute a prepared statement on a transaction, optionally using a
    /// pre-computed physical plan from the cache.
    ///
    /// When `capture_plan` is true and no cached plan was used, returns the
    /// newly optimized `PhysicalPlan` for the caller to promote into the cache.
    pub(in crate::sql::executor::core) async fn execute_prepared_on_txn_with_plan(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        exec: &PreparedExec,
        current_role: Option<&str>,
        cached_plan: Option<&crate::sql::optimizer::physical_plan::PhysicalPlan>,
        capture_plan: bool,
    ) -> Result<(
        ExecuteResult,
        Option<crate::sql::optimizer::physical_plan::PhysicalPlan>,
    )> {
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

                let (result, new_plan) = self
                    .execute_via_optimizer(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        std::borrow::Cow::Borrowed(analyzed),
                        &prepared_ctes,
                        locks,
                        cached_plan,
                        capture_plan,
                    )
                    .await?;

                if let Some(target_name) = select_into.as_ref().map(|into| into.name.clone()) {
                    let result = self
                        .create_table_from_result(txn, db_id, search_path, &target_name, result)
                        .await?;
                    return Ok((result, new_plan));
                }

                Ok((result, new_plan))
            }
            _ => {
                // Non-query variants don't use plan cache.
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
                Ok((result, None))
            }
        }
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

                let (result, _) = self
                    .execute_via_optimizer(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        std::borrow::Cow::Borrowed(analyzed),
                        &prepared_ctes,
                        locks,
                        None,
                        false,
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
                    crate::sql::analyzer::types::AnalyzedStatement::Insert(ref ins) => {
                        self.execute_analyzed_insert(txn, db_id, sequence_values, search_path, ins)
                            .await
                    }
                    crate::sql::analyzer::types::AnalyzedStatement::Update(ref upd) => {
                        self.execute_analyzed_update(txn, db_id, sequence_values, search_path, upd)
                            .await
                    }
                    crate::sql::analyzer::types::AnalyzedStatement::Delete(ref del) => {
                        self.execute_analyzed_delete(txn, db_id, sequence_values, search_path, del)
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
    ///
    /// Unified through `execute_prepared_with_framework` so that SQL EXECUTE
    /// benefits from schema drift detection, plan cache, and observability —
    /// the same pipeline as pgwire Extended Query Execute.
    ///
    /// Returns `Pin<Box<dyn Future + Send>>` (instead of `async fn`) to break
    /// the recursive type created by `execute → execute_single →
    /// execute_sql_execute_statement → self.execute()` for the recursive CTE
    /// fallback path.
    pub(super) fn execute_sql_execute_statement<'a>(
        &'a self,
        session: &'a mut Session,
        name: &'a sqlparser::ast::Ident,
        parameters: &'a [Expr],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ExecuteResult>>> + Send + 'a>> {
        Box::pin(async move {
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

            // Recursive CTE fallback: re-parse through the text path.
            if matches!(
                prepared.exec,
                PreparedExec::AnalyzedQuery {
                    has_recursive_cte: true,
                    ..
                }
            ) {
                warn!("SQL EXECUTE: recursive CTE detected; falling back to SQL parse/analyze");
                if !param_values.is_empty() {
                    session.set_pending_params(param_values);
                }
                if !prepared.param_data_types.is_empty() {
                    session.set_pending_param_types(
                        prepared
                            .param_data_types
                            .iter()
                            .cloned()
                            .map(Some)
                            .collect(),
                    );
                }
                return self
                    .execute(session, &prepared.sql)
                    .await
                    .map(|r| r.into_vec());
            }

            let qctx = self.build_prepared_query_context(
                session,
                param_values,
                &prepared.param_data_types,
            );
            self.execute_prepared_with_framework(
                session,
                &prepared.sql,
                &prepared.exec,
                &prepared.param_data_types,
                &prepared.table_versions,
                &qctx,
            )
            .await
            .map(|r| r.into_vec())
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use crate::sql::analyzer::types::{
        AnalyzedDistinct, AnalyzedProjection, AnalyzedQueryBody, AnalyzedSelect, TypedExpr,
        TypedExprKind,
    };
    use crate::sql::analyzer::AnalyzedQuery;

    fn const_int(v: i32) -> TypedExpr {
        TypedExpr::new(TypedExprKind::Constant(Value::Int32(v)), DataType::Int32)
    }

    fn values_query() -> AnalyzedQuery {
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Values(vec![vec![const_int(1)]]),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("v".to_string(), DataType::Int32, None)],
        }
    }

    fn query_with_scalar_subquery_projection() -> AnalyzedQuery {
        let subquery = values_query();
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ScalarSubquery(Box::new(subquery)),
                        DataType::Int32,
                    ),
                    output_name: "x".to_string(),
                }],
                from: vec![],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("x".to_string(), DataType::Int32, None)],
        }
    }

    fn analyzed_query_exec(analyzed: AnalyzedQuery, has_recursive_cte: bool) -> PreparedExec {
        PreparedExec::AnalyzedQuery {
            analyzed,
            locks: vec![],
            select_into: None,
            required_privileges: vec![],
            has_recursive_cte,
        }
    }

    #[test]
    fn plan_cache_eligible_for_simple_analyzed_query() {
        let exec = analyzed_query_exec(values_query(), false);
        assert!(is_plan_cache_eligible(&exec));
    }

    #[test]
    fn plan_cache_ineligible_for_recursive_cte_query() {
        let exec = analyzed_query_exec(values_query(), true);
        assert!(!is_plan_cache_eligible(&exec));
    }

    #[test]
    fn plan_cache_ineligible_when_query_needs_pre_materialization() {
        let exec = analyzed_query_exec(query_with_scalar_subquery_projection(), false);
        assert!(!is_plan_cache_eligible(&exec));
    }
}
