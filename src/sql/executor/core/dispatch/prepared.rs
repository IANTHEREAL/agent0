//! Prepared statement execution: `execute_prepared*` family, autocommit/retry
//! framework, observability policy enforcement, and schema drift fallback.

use super::super::prepared_analysis::PreparedAnalysis;
use super::super::prepared_stmt::PreparedExec;
use super::super::prepared_stmt::PreparedStatement;
use super::super::*;
use super::utils::{apply_pending_set_config_mutations, apply_statement_timeout};
use crate::sql::expr::bridge::eval_const_ast_expr;
use crate::sql::runtime_context::{wrap_with_statement_runtime_context, StatementRuntimeContext};
use crate::sql::sequences::SequenceSession;
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

fn prepared_text_fallback_reason(exec: &PreparedExec, rls_sensitive: bool) -> Option<&'static str> {
    if rls_sensitive {
        Some("RLS-sensitive prepared statement")
    } else if matches!(
        exec,
        PreparedExec::AnalyzedQuery {
            has_recursive_cte: true,
            ..
        }
    ) {
        Some("prepared recursive CTE")
    } else {
        None
    }
}

fn is_plan_cache_eligible(exec: &PreparedExec, rls_sensitive: bool) -> bool {
    if rls_sensitive {
        return false;
    }

    match exec {
        PreparedExec::AnalyzedQuery {
            analyzed,
            has_recursive_cte: false,
            ..
        } => !crate::sql::executor::select::analyzed::query_needs_pre_materialization(analyzed),
        _ => false,
    }
}
/// Decide prepared plan cache action and mutate cache state for this attempt.
///
/// This helper is intentionally mutating (entry LRU touches + miss-path counter
/// updates). It always returns owned cached entry data for async-safe use.
fn decide_and_update_prepared_cache_action(
    plan_cache: &mut crate::sql::executor::core::plan_cache::PreparedPlanCache,
    cache_key: &crate::sql::executor::core::plan_cache::PlanCacheKey,
    cache_eligible: bool,
) -> (
    Option<crate::sql::executor::core::plan_cache::PlanCacheEntry>,
    bool,
) {
    use crate::sql::executor::core::plan_cache::PromotionCounterOutcome;

    if !cache_eligible {
        return (None, false);
    }

    let cached = plan_cache.get(cache_key).cloned();
    if cached.is_some() {
        return (cached, false);
    }

    let min_exec = plan_cache.min_exec();
    // Promotion must be miss-only. NotTracked must never drive promotion, even
    // when min_exec = 0.
    let should_promote = matches!(
        plan_cache.record_execution(cache_key),
        PromotionCounterOutcome::MissCount(exec_count) if exec_count >= min_exec
    );
    (None, should_promote)
}

/// Apply post-attempt cache invalidation and promotion updates.
fn update_plan_cache_after_attempt(
    plan_cache: &mut crate::sql::executor::core::plan_cache::PreparedPlanCache,
    cache_key: &crate::sql::executor::core::plan_cache::PlanCacheKey,
    table_versions_for_cache: &[(String, u64, u64)],
    should_promote: bool,
    txn_result: &PreparedTxnResult,
    new_physical: Option<&crate::sql::optimizer::physical_plan::PhysicalPlan>,
    invalidate_cached_hit: bool,
) {
    use crate::sql::executor::core::plan_cache::{PlanCacheEntry, PlanDependency};

    if invalidate_cached_hit {
        plan_cache.invalidate(cache_key);
    }

    if matches!(txn_result, PreparedTxnResult::Executed(_))
        && (should_promote || invalidate_cached_hit)
    {
        if let Some(new_physical) = new_physical {
            let deps: Vec<PlanDependency> = table_versions_for_cache
                .iter()
                .map(|(name, table_id, version)| PlanDependency {
                    table_name: name.clone(),
                    table_id: *table_id,
                    schema_version: *version,
                })
                .collect();
            plan_cache.insert(
                cache_key.clone(),
                PlanCacheEntry {
                    physical_plan: new_physical.clone(),
                    dependencies: deps,
                },
            );
        }
    }
}
impl Executor {
    /// Execute a prepared analyzed statement without re-parsing SQL text.
    ///
    /// For schema drift (table version mismatch), this falls back to the
    /// text-based execution path (`execute`) so Parse+Analyze run again.
    ///
    /// Phase-1 RLS also routes `rls_sensitive` statements through the same
    /// text path on every Execute so role-dependent policy injection always
    /// runs under the current principal.
    pub async fn execute_prepared(
        &self,
        session: &mut Session,
        sql: &str,
        exec: &PreparedExec,
        params: Vec<Option<Value>>,
        param_data_types: &[DataType],
        table_versions: &[(String, u64, u64)],
        rls_sensitive: bool,
    ) -> Result<ExecuteResults> {
        if matches!(exec, PreparedExec::RawSqlUtility) {
            unreachable!("RawSqlUtility should not be routed to execute_prepared")
        }

        let qctx = self.build_prepared_query_context(session, params, param_data_types);
        if let Some(reason) = prepared_text_fallback_reason(exec, rls_sensitive) {
            warn!(
                reason,
                "prepared statement requires execute-time SQL parse/analyze fallback"
            );
            return self
                .execute_prepared_text_fallback(session, sql, qctx.as_ref())
                .await;
        }

        self.execute_prepared_with_framework(
            session,
            sql,
            exec,
            param_data_types,
            table_versions,
            rls_sensitive,
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
        rls_sensitive: bool,
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
                                rls_sensitive,
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
                if exec_result.is_ok() {
                    apply_pending_set_config_mutations(session)?;
                }

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
        rls_sensitive: bool,
        qctx: &'a crate::sql::query_context::QueryContext,
        is_observability_query: bool,
    ) -> Pin<Box<dyn Future<Output = Result<ExecuteResults>> + Send + 'a>> {
        let runtime = StatementRuntimeContext::from_session(
            session,
            self.tenant_keyspace(),
            self.store.transaction_client(),
        );
        let execute_future: Pin<Box<dyn Future<Output = Result<ExecuteResults>> + Send + 'a>> =
            Box::pin(self.execute_prepared_autocommit(
                session,
                sql,
                exec,
                param_data_types,
                table_versions,
                rls_sensitive,
                qctx,
                is_observability_query,
            ));
        wrap_with_statement_runtime_context(&runtime, execute_future)
    }

    async fn execute_prepared_autocommit(
        &self,
        session: &mut Session,
        sql: &str,
        exec: &PreparedExec,
        param_data_types: &[DataType],
        table_versions: &[(String, u64, u64)],
        rls_sensitive: bool,
        qctx: &crate::sql::query_context::QueryContext,
        is_observability_query: bool,
    ) -> Result<ExecuteResults> {
        let is_autocommit = !session.is_in_transaction();
        let db_id = session.current_database_id();
        let max_attempts = if is_autocommit {
            (session.settings().retry_max_attempts as usize).max(1)
        } else {
            1usize
        };

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
            if attempt > 0 {
                if let Some(timeout) = retry_timeout {
                    if retry_start.elapsed() >= timeout {
                        tracing::warn!(
                            attempt,
                            max_attempts,
                            elapsed_ms = retry_start.elapsed().as_millis() as u64,
                            timeout_ms = timeout.as_millis() as u64,
                            "prepared: retry timeout exceeded after backoff, aborting"
                        );
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

            let current_role = session.current_user().map(|u| u.to_string());
            let txn_snapshot_ts_version = session.active_txn_start_ts_version();
            let extension_txn_delta = session.extension_delta_snapshot();
            let res = crate::session_context::with_txn_snapshot_ts_version(
                txn_snapshot_ts_version,
                crate::session_context::with_extension_txn_delta(
                    extension_txn_delta,
                    self.execute_prepared_attempt(
                        session,
                        db_id,
                        sql,
                        exec,
                        param_data_types,
                        table_versions,
                        rls_sensitive,
                        current_role.as_deref(),
                    ),
                ),
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
                            if matches!(result, ExecuteResult::AlterRole | ExecuteResult::DropRole)
                            {
                                self.mark_init_cache_invalidation_pending();
                            }
                            session.commit().await?;
                            self.flush_trigger_activations();
                            self.flush_pending_hnsw_merges();
                            self.flush_pending_init_cache_invalidation();
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
                            if let Some(timeout) = retry_timeout {
                                if retry_start.elapsed() >= timeout {
                                    tracing::warn!(
                                        attempt = attempt + 1,
                                        max_attempts,
                                        elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                        timeout_ms = timeout.as_millis() as u64,
                                        "prepared: retry timeout exceeded, aborting retries"
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
                                "prepared: write conflict, retrying statement"
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
                                "prepared: write conflict retry budget exhausted"
                            );
                            self.observability.record_retry_budget_exhausted();
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
        rls_sensitive: bool,
        current_role: Option<&str>,
    ) -> Result<PreparedTxnResult> {
        use crate::sql::executor::core::plan_cache::PlanCacheKey;

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
            current_role,
        );

        // Check eligibility: analyzed SELECT only, and never cache plans that
        // require pre-materialization (subquery/async constants).
        let cache_eligible = is_plan_cache_eligible(exec, rls_sensitive);

        // Plan cache decision contract:
        // - lookup cached entry first
        // - track miss-path execution counters only on miss
        // - drive promotion only from miss counts
        let (cached_entry, should_promote) = decide_and_update_prepared_cache_action(
            session.plan_cache(),
            &cache_key,
            cache_eligible,
        );

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

        if let Ok((txn_result, new_physical, invalidate_cached_hit)) = &result {
            update_plan_cache_after_attempt(
                session.plan_cache(),
                &cache_key,
                &table_versions_for_cache,
                should_promote,
                txn_result,
                new_physical.as_ref(),
                *invalidate_cached_hit,
            );
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
        sequence_values: &mut SequenceSession,
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
        sequence_values: &mut SequenceSession,
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
                        // RLS: intentionally None — when rls_sensitive, text fallback
                        // re-enters normal DML path where RLS is enforced.
                        self.execute_analyzed_insert(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ins,
                            None,
                        )
                        .await
                    }
                    crate::sql::analyzer::types::AnalyzedStatement::Update(ref upd) => {
                        // RLS: intentionally None — when rls_sensitive, text fallback
                        // re-enters normal DML path where RLS is enforced.
                        self.execute_analyzed_update(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            upd,
                            None,
                        )
                        .await
                    }
                    crate::sql::analyzer::types::AnalyzedStatement::Delete(ref del) => {
                        // RLS: intentionally None — when rls_sensitive, text fallback
                        // re-enters normal DML path where RLS is enforced.
                        self.execute_analyzed_delete(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            del,
                            None,
                        )
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
                    self.flush_pending_hnsw_merges();
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
                rls_sensitive,
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
                    rls_sensitive,
                }
            }
            PreparedAnalysis::Dml {
                analyzed,
                output_schema,
                param_types,
                table_versions,
                rls_sensitive,
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
                    rls_sensitive,
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
    /// Unified through `execute_prepared` so that SQL EXECUTE
    /// benefits from schema drift detection, plan cache, and observability —
    /// the same pipeline as pgwire Extended Query Execute.
    ///
    /// Returns `Pin<Box<dyn Future + Send>>` (instead of `async fn`) to break
    /// the recursive type created by `execute → execute_single →
    /// execute_sql_execute_statement → execute_prepared →
    /// execute_prepared_text_fallback → self.execute()`.
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

            self.execute_prepared(
                session,
                &prepared.sql,
                &prepared.exec,
                param_values,
                &prepared.param_data_types,
                &prepared.table_versions,
                prepared.rls_sensitive,
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
        assert!(is_plan_cache_eligible(&exec, false));
    }

    #[test]
    fn plan_cache_ineligible_for_recursive_cte_query() {
        let exec = analyzed_query_exec(values_query(), true);
        assert!(!is_plan_cache_eligible(&exec, false));
    }

    #[test]
    fn plan_cache_ineligible_for_rls_sensitive_query() {
        let exec = analyzed_query_exec(values_query(), false);
        assert!(!is_plan_cache_eligible(&exec, true));
    }

    #[test]
    fn plan_cache_ineligible_when_query_needs_pre_materialization() {
        let exec = analyzed_query_exec(query_with_scalar_subquery_projection(), false);
        assert!(!is_plan_cache_eligible(&exec, false));
    }

    #[test]
    fn plan_cache_ineligible_for_non_query_variants() {
        let dml = PreparedExec::AnalyzedDml {
            analyzed: crate::sql::analyzer::types::AnalyzedStatement::Query(values_query()),
            required_privileges: vec![],
        };
        assert!(!is_plan_cache_eligible(&dml, false));
        assert!(!is_plan_cache_eligible(&PreparedExec::RawSqlUtility, false));
    }
}
#[cfg(test)]
mod plan_cache_flow_tests {
    use super::*;
    use crate::sql::executor::core::plan_cache::{
        PlanCacheEntry, PlanCacheKey, PlanDependency, PreparedPlanCache,
    };
    use crate::sql::optimizer::logical_plan::PlanSchema;
    use crate::sql::optimizer::physical_plan::{PhysicalCost, PhysicalNode, PhysicalPlan};

    fn make_key(sql: &str, resolved_table_ids: &[u64]) -> PlanCacheKey {
        PlanCacheKey::new(
            sql.to_string(),
            &[],
            1,
            &["public".to_string()],
            resolved_table_ids,
            None,
        )
    }

    fn make_physical_plan() -> PhysicalPlan {
        PhysicalPlan {
            node: PhysicalNode::Empty,
            schema: PlanSchema::empty(),
            cost: PhysicalCost::default(),
        }
    }

    fn make_entry(deps: Vec<PlanDependency>) -> PlanCacheEntry {
        PlanCacheEntry {
            physical_plan: make_physical_plan(),
            dependencies: deps,
        }
    }

    #[test]
    fn decide_cache_action_ineligible_skips_tracking() {
        let mut cache = PreparedPlanCache::new(8, 2);
        let key = make_key("SELECT 1", &[]);
        let (cached, should_promote) =
            decide_and_update_prepared_cache_action(&mut cache, &key, false);
        assert!(cached.is_none());
        assert!(!should_promote);
        assert_eq!(cache.counter_len(), 0);
    }

    #[test]
    fn decide_cache_action_first_miss_promotes_when_min_exec_zero() {
        let mut cache = PreparedPlanCache::new(8, 0);
        let key = make_key("SELECT 1", &[]);

        let (cached, should_promote) =
            decide_and_update_prepared_cache_action(&mut cache, &key, true);

        assert!(cached.is_none());
        assert!(should_promote);
        assert_eq!(cache.counter_count_for(&key), Some(1));
    }

    #[test]
    fn decide_cache_action_not_tracked_never_promotes_even_with_min_exec_zero() {
        let mut cache = PreparedPlanCache::new(0, 0);
        let key = make_key("SELECT 1", &[]);

        let (cached, should_promote) =
            decide_and_update_prepared_cache_action(&mut cache, &key, true);

        assert!(cached.is_none());
        assert!(!should_promote);
        assert_eq!(cache.counter_len(), 0);
    }

    #[test]
    fn decide_cache_action_cached_hit_returns_owned_entry_without_counter_churn() {
        let mut cache = PreparedPlanCache::new(8, 3);
        let key = make_key("SELECT 1", &[10]);
        cache.insert(
            key.clone(),
            make_entry(vec![PlanDependency {
                table_name: "public.t".to_string(),
                table_id: 10,
                schema_version: 1,
            }]),
        );

        let (cached, should_promote) =
            decide_and_update_prepared_cache_action(&mut cache, &key, true);

        assert!(cached.is_some());
        assert!(!should_promote);
        assert_eq!(cache.counter_count_for(&key), None);
        assert_eq!(cache.counter_len(), 0);
    }

    #[test]
    fn post_attempt_stale_hit_invalidates_and_reinserts_without_counter_churn() {
        let mut cache = PreparedPlanCache::new(8, 2);
        let key = make_key("SELECT * FROM t WHERE id = $1", &[42]);
        cache.insert(
            key.clone(),
            make_entry(vec![PlanDependency {
                table_name: "public.t".to_string(),
                table_id: 42,
                schema_version: 1,
            }]),
        );
        assert_eq!(cache.counter_count_for(&key), None);

        let table_versions = vec![("public.t".to_string(), 42, 2)];
        let new_plan = make_physical_plan();
        update_plan_cache_after_attempt(
            &mut cache,
            &key,
            &table_versions,
            false,
            &PreparedTxnResult::Executed(ExecuteResult::Empty),
            Some(&new_plan),
            true,
        );

        let deps = cache
            .get(&key)
            .expect("entry should be reinserted after stale hit")
            .dependencies
            .clone();
        assert_eq!(
            deps.as_slice(),
            &[PlanDependency {
                table_name: "public.t".to_string(),
                table_id: 42,
                schema_version: 2,
            }]
        );
        assert_eq!(cache.counter_count_for(&key), None);
        assert_eq!(cache.counter_len(), 0);
    }
}
#[cfg(test)]
mod prepared_policy_tests {
    use super::*;
    use crate::sql::analyzer::types::AnalyzedQueryBody;
    use crate::sql::analyzer::AnalyzedQuery;
    use std::sync::Arc;

    fn test_executor() -> Executor {
        Executor::new(
            crate::storage::TikvStore::new_stub(),
            "tenant_prepared_ut".to_string(),
            crate::observability::registry().tenant("tenant_prepared_ut"),
            crate::pool::TenantMemoryAccountant::unlimited("tenant_prepared_ut".to_string()),
            Arc::new(crate::sql::triggers::TriggerBodyCache::new()),
            Arc::new(crate::sql::stats::TableStatsCache::new()),
        )
    }

    fn test_session(username: &str, is_superuser: bool) -> Session {
        Session::new_with_user_and_database(
            crate::storage::TikvStore::new_stub(),
            crate::observability::registry().tenant("tenant_prepared_ut"),
            username.to_string(),
            is_superuser,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        )
    }

    fn analyzed_query_exec() -> PreparedExec {
        PreparedExec::AnalyzedQuery {
            analyzed: AnalyzedQuery {
                ctes: vec![],
                body: AnalyzedQueryBody::Values(vec![vec![]]),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![],
            },
            locks: vec![],
            select_into: None,
            required_privileges: vec![],
            has_recursive_cte: false,
        }
    }

    #[test]
    fn build_prepared_query_context_carries_params_and_types() {
        let exec = test_executor();
        let mut session = test_session("tester", true);
        let qctx = exec.build_prepared_query_context(
            &mut session,
            vec![Some(Value::Int32(7)), None],
            &[DataType::Int32, DataType::Text],
        );
        assert_eq!(qctx.params.len(), 2);
        assert_eq!(qctx.param_types.len(), 2);
        assert_eq!(qctx.params[0], Some(Value::Int32(7)));
        assert_eq!(qctx.param_types[0], Some(DataType::Int32));
    }

    #[test]
    fn observability_policy_normal_user_is_always_normal_mode() {
        let exec = test_executor();
        let mut session = test_session("tester", false);
        let mode = exec
            .enforce_observability_prepared_policy(
                &mut session,
                "SELECT 1",
                &PreparedExec::RawSqlUtility,
            )
            .unwrap();
        assert_eq!(mode, PreparedObservabilityMode::Normal);
    }

    #[test]
    fn observability_user_but_superuser_is_treated_as_normal_mode() {
        let exec = test_executor();
        let mut session = test_session(OBSERVABILITY_USER, true);
        let mode = exec
            .enforce_observability_prepared_policy(
                &mut session,
                "SELECT * FROM any_table",
                &PreparedExec::RawSqlUtility,
            )
            .unwrap();
        assert_eq!(mode, PreparedObservabilityMode::Normal);
    }

    #[test]
    fn observability_policy_rejects_non_analyzed_exec() {
        let exec = test_executor();
        let mut session = test_session(OBSERVABILITY_USER, false);
        let err = exec
            .enforce_observability_prepared_policy(
                &mut session,
                "SELECT 1",
                &PreparedExec::RawSqlUtility,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"));
    }

    #[test]
    fn observability_policy_allows_tableless_select_for_analyzed_exec() {
        let exec = test_executor();
        let mut session = test_session(OBSERVABILITY_USER, false);
        let mode = exec
            .enforce_observability_prepared_policy(&mut session, "SELECT 1", &analyzed_query_exec())
            .unwrap();
        assert_eq!(mode, PreparedObservabilityMode::Observability);
    }

    #[test]
    fn observability_policy_rejects_non_observability_select() {
        let exec = test_executor();
        let mut session = test_session(OBSERVABILITY_USER, false);
        let err = exec
            .enforce_observability_prepared_policy(
                &mut session,
                "SELECT * FROM public.t",
                &analyzed_query_exec(),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"));
    }

    #[tokio::test]
    async fn execute_prepared_recursive_cte_falls_back_to_text_path() {
        let exec = test_executor();
        let mut session = test_session("tester", true);
        let PreparedExec::AnalyzedQuery {
            analyzed,
            locks,
            select_into,
            required_privileges,
            ..
        } = analyzed_query_exec()
        else {
            unreachable!()
        };
        let prepared = PreparedExec::AnalyzedQuery {
            analyzed,
            locks,
            select_into,
            required_privileges,
            has_recursive_cte: true,
        };

        let out = exec
            .execute_prepared(
                &mut session,
                "",
                &prepared,
                vec![Some(Value::Int32(11))],
                &[DataType::Int32],
                &[],
                false,
            )
            .await
            .unwrap()
            .into_vec();
        assert!(matches!(out.as_slice(), [ExecuteResult::Empty]));

        // Empty SQL returns early, so pending params/types are still queued.
        let qctx = session.query_context_for_statement(1, 1);
        assert_eq!(qctx.params, vec![Some(Value::Int32(11))]);
        assert_eq!(qctx.param_types, vec![Some(DataType::Int32)]);
    }

    #[tokio::test]
    async fn execute_prepared_rls_sensitive_falls_back_to_text_path() {
        let exec = test_executor();
        let mut session = test_session("tester", true);

        let out = exec
            .execute_prepared(
                &mut session,
                "",
                &analyzed_query_exec(),
                vec![Some(Value::Int32(22))],
                &[DataType::Int32],
                &[],
                true,
            )
            .await
            .unwrap()
            .into_vec();
        assert!(matches!(out.as_slice(), [ExecuteResult::Empty]));

        let qctx = session.query_context_for_statement(1, 1);
        assert_eq!(qctx.params, vec![Some(Value::Int32(22))]);
        assert_eq!(qctx.param_types, vec![Some(DataType::Int32)]);
    }

    #[tokio::test]
    async fn execute_prepared_text_fallback_sets_pending_bind_values() {
        let exec = test_executor();
        let mut session = test_session("tester", true);
        let qctx = exec.build_prepared_query_context(
            &mut session,
            vec![Some(Value::Text("x".to_string()))],
            &[DataType::Text],
        );

        let out = exec
            .execute_prepared_text_fallback(&mut session, "", qctx.as_ref())
            .await
            .unwrap()
            .into_vec();
        assert!(matches!(out.as_slice(), [ExecuteResult::Empty]));

        let drained = session.query_context_for_statement(2, 2);
        assert_eq!(drained.params, vec![Some(Value::Text("x".to_string()))]);
        assert_eq!(drained.param_types, vec![Some(DataType::Text)]);
    }

    #[test]
    fn observability_policy_returns_parse_error_for_invalid_sql() {
        let exec = test_executor();
        let mut session = test_session(OBSERVABILITY_USER, false);
        let err = exec
            .enforce_observability_prepared_policy(&mut session, "SELECT (", &analyzed_query_exec())
            .unwrap_err()
            .to_string();
        assert!(!err.is_empty());
    }

    #[test]
    fn observability_policy_marks_failed_for_rejected_exec_in_transaction() {
        let exec = test_executor();
        let mut session = test_session(OBSERVABILITY_USER, false);
        session.force_test_transaction_state(true, false);
        let _ = exec
            .enforce_observability_prepared_policy(
                &mut session,
                "SELECT 1",
                &PreparedExec::RawSqlUtility,
            )
            .unwrap_err();
        assert!(session.is_transaction_failed());
    }

    #[test]
    fn sql_deallocate_removes_named_statement_and_supports_all() {
        let exec = test_executor();
        let mut session = test_session("tester", true);

        let stmt = PreparedStatement {
            sql: "SELECT 1".to_string(),
            exec: PreparedExec::RawSqlUtility,
            output_schema: vec![],
            param_data_types: vec![],
            table_versions: vec![],
            rls_sensitive: false,
        };
        session.put_sql_prepared_statement("p1".to_string(), stmt.clone());
        session.put_sql_prepared_statement("p2".to_string(), stmt);

        let out = exec
            .execute_sql_deallocate_statement(&mut session, &sqlparser::ast::Ident::new("p1"))
            .unwrap();
        assert!(matches!(
            out.as_slice(),
            [ExecuteResult::CommandComplete { tag: "DEALLOCATE" }]
        ));
        assert!(session.get_sql_prepared_statement_cloned("p1").is_none());
        assert!(session.get_sql_prepared_statement_cloned("p2").is_some());

        let out_all = exec
            .execute_sql_deallocate_statement(&mut session, &sqlparser::ast::Ident::new("ALL"))
            .unwrap();
        assert!(matches!(
            out_all.as_slice(),
            [ExecuteResult::CommandComplete { tag: "DEALLOCATE" }]
        ));
        assert!(session.get_sql_prepared_statement_cloned("p2").is_none());
    }

    #[test]
    fn sql_deallocate_unknown_name_errors() {
        let exec = test_executor();
        let mut session = test_session("tester", true);
        let err = exec
            .execute_sql_deallocate_statement(&mut session, &sqlparser::ast::Ident::new("missing"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn sql_deallocate_all_is_case_insensitive() {
        let exec = test_executor();
        let mut session = test_session("tester", true);
        session.put_sql_prepared_statement(
            "p".to_string(),
            PreparedStatement {
                sql: "SELECT 1".to_string(),
                exec: PreparedExec::RawSqlUtility,
                output_schema: vec![],
                param_data_types: vec![],
                table_versions: vec![],
                rls_sensitive: false,
            },
        );
        let out = exec
            .execute_sql_deallocate_statement(&mut session, &sqlparser::ast::Ident::new("aLl"))
            .unwrap();
        assert!(matches!(
            out.as_slice(),
            [ExecuteResult::CommandComplete { tag: "DEALLOCATE" }]
        ));
        assert!(session.get_sql_prepared_statement_cloned("p").is_none());
    }

    #[tokio::test]
    async fn sql_execute_validates_prepared_exists_and_parameter_count() {
        let exec = test_executor();
        let mut session = test_session("tester", true);

        let missing_err = exec
            .execute_sql_execute_statement(
                &mut session,
                &sqlparser::ast::Ident::new("missing"),
                &[],
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(missing_err.contains("does not exist"));

        let stmt = PreparedStatement {
            sql: "SELECT 1".to_string(),
            exec: analyzed_query_exec(),
            output_schema: vec![],
            param_data_types: vec![DataType::Int32],
            table_versions: vec![],
            rls_sensitive: false,
        };
        session.put_sql_prepared_statement("p1".to_string(), stmt);

        let err = exec
            .execute_sql_execute_statement(&mut session, &sqlparser::ast::Ident::new("p1"), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.to_lowercase().contains("prepared") || err.to_lowercase().contains("parameter")
        );
    }

    #[tokio::test]
    async fn sql_execute_requires_constant_parameter_expressions() {
        let exec = test_executor();
        let mut session = test_session("tester", true);
        let stmt = PreparedStatement {
            sql: "SELECT 1".to_string(),
            exec: analyzed_query_exec(),
            output_schema: vec![],
            param_data_types: vec![DataType::Int32],
            table_versions: vec![],
            rls_sensitive: false,
        };
        session.put_sql_prepared_statement("p1".to_string(), stmt);

        let err = exec
            .execute_sql_execute_statement(
                &mut session,
                &sqlparser::ast::Ident::new("p1"),
                &[Expr::Identifier(sqlparser::ast::Ident::new("x"))],
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.is_empty());
    }

    #[tokio::test]
    async fn sql_execute_recursive_cte_fallback_sets_pending_bindings() {
        let exec = test_executor();
        let mut session = test_session("tester", true);
        let PreparedExec::AnalyzedQuery {
            analyzed,
            locks,
            select_into,
            required_privileges,
            ..
        } = analyzed_query_exec()
        else {
            unreachable!()
        };

        let stmt = PreparedStatement {
            sql: "".to_string(),
            exec: PreparedExec::AnalyzedQuery {
                analyzed,
                locks,
                select_into,
                required_privileges,
                has_recursive_cte: true,
            },
            output_schema: vec![],
            param_data_types: vec![DataType::Int32],
            table_versions: vec![],
            rls_sensitive: false,
        };
        session.put_sql_prepared_statement("p1".to_string(), stmt);

        let out = exec
            .execute_sql_execute_statement(
                &mut session,
                &sqlparser::ast::Ident::new("p1"),
                &[Expr::Value(sqlparser::ast::Value::Number(
                    "7".to_string(),
                    false,
                ))],
            )
            .await
            .unwrap();
        assert!(matches!(out.as_slice(), [ExecuteResult::Empty]));

        let qctx = session.query_context_for_statement(1, 1);
        assert_eq!(qctx.params, vec![Some(Value::Int32(7))]);
        assert_eq!(qctx.param_types, vec![Some(DataType::Int32)]);
    }

    #[tokio::test]
    async fn sql_execute_rls_sensitive_fallback_sets_pending_bindings() {
        let exec = test_executor();
        let mut session = test_session("tester", true);
        let stmt = PreparedStatement {
            sql: "".to_string(),
            exec: analyzed_query_exec(),
            output_schema: vec![],
            param_data_types: vec![DataType::Int32],
            table_versions: vec![],
            rls_sensitive: true,
        };
        session.put_sql_prepared_statement("p1".to_string(), stmt);

        let out = exec
            .execute_sql_execute_statement(
                &mut session,
                &sqlparser::ast::Ident::new("p1"),
                &[Expr::Value(sqlparser::ast::Value::Number(
                    "9".to_string(),
                    false,
                ))],
            )
            .await
            .unwrap();
        assert!(matches!(out.as_slice(), [ExecuteResult::Empty]));

        let qctx = session.query_context_for_statement(1, 1);
        assert_eq!(qctx.params, vec![Some(Value::Int32(9))]);
        assert_eq!(qctx.param_types, vec![Some(DataType::Int32)]);
    }

    #[test]
    fn observability_policy_empty_sql_is_denied_and_marks_failed_in_txn() {
        let exec = test_executor();
        let mut session = test_session(OBSERVABILITY_USER, false);
        session.force_test_transaction_state(true, false);
        let _ = exec
            .enforce_observability_prepared_policy(&mut session, " ; ", &analyzed_query_exec())
            .unwrap_err();
        assert!(session.is_transaction_failed());
    }
}
