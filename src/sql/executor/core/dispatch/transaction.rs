//! Transaction/autocommit framework and observability permission checks.

use super::super::*;
use super::guc::build_show_all_result;
use super::utils::{
    apply_statement_timeout, effective_retry_timeout, remaining_statement_timeout,
    validate_begin_transaction_modes,
};

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
            validate_begin_transaction_modes(session, modes)?;
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

/// Returns `true` when the statement is `ALTER ROLE ... WITH PASSWORD ...`,
/// i.e. the exact path that should consume `db9.password_grace_seconds`.
fn is_alter_role_with_password(stmt: &Statement) -> bool {
    if let Statement::AlterRole {
        operation: sqlparser::ast::AlterRoleOperation::WithOptions { options },
        ..
    } = stmt
    {
        options
            .iter()
            .any(|opt| matches!(opt, sqlparser::ast::RoleOption::Password(_)))
    } else {
        false
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

pub(super) fn execute_result_modifies_database(result: &ExecuteResult) -> bool {
    result.modifies_database()
}

fn query_has_select_into(query: &sqlparser::ast::Query) -> bool {
    matches!(&*query.body, sqlparser::ast::SetExpr::Select(select) if select.into.is_some())
}

fn read_only_forbidden_statement_tag(stmt: &Statement) -> Option<&'static str> {
    match stmt {
        Statement::Insert { .. } => Some("INSERT"),
        Statement::Delete { .. } => Some("DELETE"),
        Statement::Update { .. } => Some("UPDATE"),
        Statement::Query(query) if query_has_select_into(query) => Some("SELECT INTO"),
        Statement::Query(query) if !query.locks.is_empty() => Some("SELECT FOR UPDATE/SHARE"),
        Statement::Copy { to: false, .. } => Some("COPY FROM"),
        Statement::CreateTable { .. }
        | Statement::CreateIndex { .. }
        | Statement::Drop { .. }
        | Statement::Truncate { .. }
        | Statement::AlterTable { .. }
        | Statement::CreateType { .. }
        | Statement::CreateSchema { .. }
        | Statement::CreateFunction { .. }
        | Statement::CreateProcedure { .. }
        | Statement::CreateSequence { .. }
        | Statement::CreateView { .. }
        | Statement::AlterIndex { .. }
        | Statement::DropFunction { .. } => Some("DDL"),
        Statement::CreateRole { .. }
        | Statement::AlterRole { .. }
        | Statement::Grant { .. }
        | Statement::Revoke { .. } => Some("RBAC"),
        _ => None,
    }
}

fn database_write_fence_statement_tag(stmt: &Statement) -> Option<&'static str> {
    match stmt {
        Statement::Query(query) if query_has_select_into(query) => Some("SELECT INTO"),
        Statement::Query(_) => None,
        _ => read_only_forbidden_statement_tag(stmt),
    }
}

pub(super) fn statement_requires_database_write_fence(stmt: &Statement) -> bool {
    database_write_fence_statement_tag(stmt).is_some()
}

const CIC_WAIT_POLL_MS: u64 = 10;

fn is_create_index_concurrently(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::CreateIndex {
            concurrently: true,
            ..
        }
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
    async fn wait_for_create_index_concurrently(
        &self,
        session: &Session,
        stmt: &Statement,
    ) -> Result<()> {
        let Statement::CreateIndex {
            name, table_name, ..
        } = stmt
        else {
            return Ok(());
        };
        let target_index_name = match name {
            Some(name) => Some(names::split_object_name(name)?.1),
            None => None,
        };
        let db_id = session.current_database_id();
        let search_path = session.search_path().to_vec();

        loop {
            let mut txn = self.store.begin().await?;
            let resolved = names::resolve_existing_table_name(
                self.store.as_ref(),
                &mut txn,
                db_id,
                table_name,
                &search_path,
            )
            .await?;
            let schema = if let Some(resolved) = resolved {
                self.store
                    .get_schema(&mut txn, db_id, &resolved.full)
                    .await?
            } else {
                None
            };
            txn.rollback().await.ok();

            let Some(schema) = schema else {
                return Err(anyhow!("Table '{}' does not exist", table_name));
            };

            let mut saw_in_progress = false;
            for index in &schema.indexes {
                if target_index_name
                    .as_ref()
                    .is_some_and(|target| index.name != *target)
                {
                    continue;
                }
                match index.state {
                    crate::worker::types::IndexState::Ready => {
                        if target_index_name.is_some() {
                            return Ok(());
                        }
                    }
                    crate::worker::types::IndexState::Invalid => {
                        return Err(anyhow!(
                            "CREATE INDEX CONCURRENTLY failed for index '{}'",
                            index.name
                        ));
                    }
                    crate::worker::types::IndexState::Building
                    | crate::worker::types::IndexState::WriteOnly => {
                        saw_in_progress = true;
                    }
                }
            }

            if !saw_in_progress {
                return Ok(());
            }

            tokio::time::sleep(std::time::Duration::from_millis(CIC_WAIT_POLL_MS)).await;
        }
    }

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

        if session.transaction_read_only() {
            if let Some(statement) = read_only_forbidden_statement_tag(stmt) {
                return Err(SqlError::ReadOnlySqlTransaction {
                    statement: statement.to_string(),
                }
                .into());
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
        let statement_timeout = session.statement_timeout();
        let configured_retry_timeout = {
            let ms = session.settings().retry_timeout_ms;
            if ms > 0 {
                Some(std::time::Duration::from_millis(ms))
            } else {
                None
            }
        };
        let retry_timeout = effective_retry_timeout(configured_retry_timeout, statement_timeout);

        // Snapshot grace seconds ONCE before the retry loop so that
        // retryable TiKV conflicts do not consume the grace window.
        // Consumed only after final success or non-retryable failure.
        let password_grace_seconds = session.settings().password_grace_seconds();
        let consume_grace = password_grace_seconds > 0 && is_alter_role_with_password(stmt);

        // Use a macro to consume the grace-period GUC exactly once on any
        // non-retry exit from the loop (success or non-retryable failure).
        macro_rules! consume_grace_if_needed {
            ($session:expr) => {
                if consume_grace {
                    $session.take_password_grace_seconds();
                }
            };
        }

        for attempt in 0..max_attempts {
            // This loop re-runs the operator tree on each retry within one
            // statement memory scope, so per-attempt peak/component tracking must
            // restart each attempt (otherwise expensive_query's component_grow_top
            // inflates by the retry count). No-op when no scope is active.
            crate::pool::reset_statement_memory_attempt();
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
                        consume_grace_if_needed!(session);
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
            if statement_requires_database_write_fence(stmt) {
                if let Err(err) = apply_statement_timeout(
                    remaining_statement_timeout(retry_start, statement_timeout),
                    session.ensure_current_database_write_fence(),
                )
                .await
                {
                    let retryable = is_retryable_tikv_error(&err);
                    if is_autocommit {
                        session
                            .rollback_for_retry_or_abandon("ddl_dml_lifecycle_fence_error")
                            .await;
                        self.clear_trigger_activations();
                    }

                    let should_retry = attempt + 1 < max_attempts
                        && retryable
                        && (is_autocommit || explicit_first_stmt_retry_eligible);
                    if should_retry {
                        if let Some(timeout) = retry_timeout {
                            if retry_start.elapsed() >= timeout {
                                tracing::warn!(
                                    attempt = attempt + 1,
                                    max_attempts,
                                    elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                    timeout_ms = timeout.as_millis() as u64,
                                    "retry timeout exceeded, aborting lifecycle fence retries"
                                );
                                if !is_autocommit {
                                    session
                                        .rollback_for_retry_or_abandon(
                                            "explicit_first_statement_lifecycle_fence_retry_timeout",
                                        )
                                        .await;
                                    self.clear_trigger_activations();
                                }
                                self.observability.record_retry_timeout_abort();
                                consume_grace_if_needed!(session);
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
                            "write conflict or deadlock while acquiring lifecycle fence, retrying statement"
                        );
                        self.observability
                            .record_retry_attempt(extract_write_conflict_reason(&err));
                        if !is_autocommit {
                            session
                                .rollback_for_retry_or_abandon(
                                    "explicit_first_statement_lifecycle_fence_retry",
                                )
                                .await;
                            self.clear_trigger_activations();
                            session.begin().await?;
                        }
                        autocommit_backoff(attempt).await;
                        continue;
                    }
                    if retryable {
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_attempts,
                            elapsed_ms = retry_start.elapsed().as_millis() as u64,
                            "write conflict or deadlock lifecycle fence retry budget exhausted"
                        );
                        self.observability.record_retry_budget_exhausted();
                    }
                    if err.is::<StatementTimeoutError>() && !is_autocommit {
                        session.rollback().await?;
                        self.clear_trigger_activations();
                    }
                    consume_grace_if_needed!(session);
                    return Err(err);
                }
            }

            let timeout = remaining_statement_timeout(retry_start, statement_timeout);
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
                        password_grace_seconds,
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

            let statement_dirty_tables =
                crate::session_context::current_statement_dirty_table_ids();

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
                        let statement_modified = !statement_dirty_tables.is_empty()
                            || execute_result_modifies_database(&result);
                        session.note_transaction_dirty_tables(statement_dirty_tables);
                        if is_observability_query {
                            session.rollback().await?;
                            self.clear_trigger_activations();
                        } else {
                            if matches!(result, ExecuteResult::AlterRole | ExecuteResult::DropRole)
                            {
                                self.mark_init_cache_invalidation_pending();
                            }
                            if let Err(err) = session.commit().await {
                                let retryable = is_retryable_tikv_error(&err);
                                session
                                    .rollback_for_retry_or_abandon("ddl_dml_commit_error")
                                    .await;
                                self.clear_trigger_activations();
                                let should_retry = attempt + 1 < max_attempts && retryable;
                                if should_retry {
                                    if let Some(timeout) = retry_timeout {
                                        if retry_start.elapsed() >= timeout {
                                            tracing::warn!(
                                                attempt = attempt + 1,
                                                max_attempts,
                                                elapsed_ms =
                                                    retry_start.elapsed().as_millis() as u64,
                                                timeout_ms = timeout.as_millis() as u64,
                                                "retry timeout exceeded, aborting commit retries"
                                            );
                                            self.observability.record_retry_timeout_abort();
                                            consume_grace_if_needed!(session);
                                            return Err(SqlError::RetryTimeout {
                                                elapsed_ms: retry_start.elapsed().as_millis()
                                                    as u64,
                                                limit_ms: timeout.as_millis() as u64,
                                            }
                                            .into());
                                        }
                                    }
                                    tracing::info!(
                                        attempt = attempt + 1,
                                        max_attempts,
                                        elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                        "write conflict or deadlock at commit, retrying statement"
                                    );
                                    self.observability
                                        .record_retry_attempt(extract_write_conflict_reason(&err));
                                    autocommit_backoff(attempt).await;
                                    continue;
                                }
                                if retryable {
                                    tracing::warn!(
                                        attempt = attempt + 1,
                                        max_attempts,
                                        elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                        "write conflict or deadlock commit retry budget exhausted"
                                    );
                                    self.observability.record_retry_budget_exhausted();
                                }
                                consume_grace_if_needed!(session);
                                return Err(err);
                            }
                            self.flush_trigger_activations();
                            self.flush_pending_hnsw_merges();
                            self.flush_pending_init_cache_invalidation();
                            self.record_sql_modified_after_success(session, statement_modified);
                            if is_create_index_concurrently(stmt) {
                                apply_statement_timeout(
                                    remaining_statement_timeout(retry_start, retry_timeout),
                                    self.wait_for_create_index_concurrently(session, stmt),
                                )
                                .await?;
                            }
                        }
                        let mut stmt_results = notices;
                        stmt_results.push(result);
                        consume_grace_if_needed!(session);
                        return Ok(stmt_results);
                    }
                    Err(err) => {
                        let retryable = is_retryable_tikv_error(&err);
                        session
                            .rollback_for_retry_or_abandon("ddl_dml_statement_error")
                            .await;
                        self.clear_trigger_activations();
                        let should_retry = attempt + 1 < max_attempts && retryable;
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
                                    consume_grace_if_needed!(session);
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
                                "write conflict or deadlock, retrying statement"
                            );
                            self.observability
                                .record_retry_attempt(extract_write_conflict_reason(&err));
                            autocommit_backoff(attempt).await;
                            continue;
                        }
                        if retryable {
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                elapsed_ms = retry_start.elapsed().as_millis() as u64,
                                "write conflict or deadlock retry budget exhausted"
                            );
                            self.observability.record_retry_budget_exhausted();
                        }
                        consume_grace_if_needed!(session);
                        return Err(err);
                    }
                }
            } else {
                match res {
                    Ok((notices, result)) => {
                        let statement_modified = !statement_dirty_tables.is_empty()
                            || execute_result_modifies_database(&result);
                        session.note_transaction_dirty_tables(statement_dirty_tables);
                        if statement_modified {
                            session.note_transaction_activity_modified();
                        }
                        session.note_statement_success_in_transaction();
                        if matches!(result, ExecuteResult::AlterRole | ExecuteResult::DropRole) {
                            self.mark_init_cache_invalidation_pending();
                        }
                        let mut stmt_results = notices;
                        stmt_results.push(result);
                        consume_grace_if_needed!(session);
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
                                    session
                                        .rollback_for_retry_or_abandon(
                                            "explicit_first_statement_retry_timeout",
                                        )
                                        .await;
                                    self.clear_trigger_activations();
                                    self.observability.record_retry_timeout_abort();
                                    consume_grace_if_needed!(session);
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
                                "write conflict or deadlock, retrying statement"
                            );
                            self.observability
                                .record_retry_attempt(extract_write_conflict_reason(&err));
                            session
                                .rollback_for_retry_or_abandon("explicit_first_statement_retry")
                                .await;
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
                                "write conflict or deadlock retry budget exhausted"
                            );
                            self.observability.record_retry_budget_exhausted();
                        }
                        consume_grace_if_needed!(session);
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
        is_plan_cache_invalidating_ddl, read_only_forbidden_statement_tag,
        statement_requires_database_write_fence, DDL_DML_MAX_RETRY_ATTEMPTS,
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
        let rls_policy_cache = std::sync::Arc::new(crate::sql::rls::cache::RlsPolicyCache::new());
        let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
        let executor = Executor::new(
            store.clone(),
            keyspace,
            observability.clone(),
            crate::pool::TenantMemoryAccountant::unlimited(
                "dispatch_transaction_tests".to_string(),
            ),
            trigger_cache,
            rls_policy_cache,
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
        )
        .unwrap();
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
    fn database_write_statement_classifier_matches_lifecycle_fence_contract() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET id = id + 1",
            "DELETE FROM t",
            "SELECT 1 INTO lifecycle_fence_target",
            "COPY t (id) FROM STDIN;",
            "CREATE TABLE t(id INT)",
            "DROP TABLE t",
            "TRUNCATE TABLE t",
        ] {
            assert!(
                statement_requires_database_write_fence(&parse_stmt(sql)),
                "{sql} must acquire the database lifecycle write fence"
            );
        }

        for sql in [
            "SELECT 1",
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t FOR UPDATE SKIP LOCKED",
            "SELECT * FROM t FOR SHARE NOWAIT",
            "SHOW statement_timeout",
            "COPY t TO STDOUT;",
        ] {
            assert!(
                !statement_requires_database_write_fence(&parse_stmt(sql)),
                "{sql} must not acquire the database lifecycle write fence"
            );
        }
    }

    #[test]
    fn read_only_forbidden_classifier_includes_locking_reads_without_lifecycle_fence() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET id = id + 1",
            "DELETE FROM t",
            "SELECT 1 INTO read_only_forbidden_target",
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t FOR SHARE NOWAIT",
            "COPY t (id) FROM STDIN;",
            "CREATE TABLE t(id INT)",
        ] {
            assert!(
                read_only_forbidden_statement_tag(&parse_stmt(sql)).is_some(),
                "{sql} must be forbidden in a read-only transaction"
            );
        }

        for sql in ["SELECT 1", "SHOW statement_timeout", "COPY t TO STDOUT;"] {
            assert!(
                read_only_forbidden_statement_tag(&parse_stmt(sql)).is_none(),
                "{sql} must be allowed by the read-only classifier"
            );
        }
    }

    #[test]
    fn lifecycle_write_fence_is_taken_before_statement_execution() {
        let source = include_str!("transaction.rs");
        let prod_source = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("transaction.rs must contain the test module");
        let execute_fn = prod_source
            .split("pub(super) async fn execute_ddl_dml_with_autocommit(")
            .nth(1)
            .expect("execute_ddl_dml_with_autocommit must exist");

        let begin_pos = execute_fn
            .find("session.begin().await?")
            .expect("write execution must begin a transaction");
        let fence_pos = execute_fn
            .find("session.ensure_current_database_write_fence()")
            .expect("write execution must take the database lifecycle fence");
        let fence_timeout_pos = execute_fn[..fence_pos]
            .rfind("apply_statement_timeout(")
            .expect("lifecycle fence acquisition must be statement-timeout bounded");
        let fence_remaining_pos = fence_timeout_pos
            + execute_fn[fence_timeout_pos..fence_pos]
                .find("remaining_statement_timeout(retry_start, statement_timeout)")
                .expect("lifecycle fence must use the remaining statement timeout");
        let fence_retry_pos = execute_fn
            .find("ddl_dml_lifecycle_fence_error")
            .expect("lifecycle fence conflicts must participate in statement retry cleanup");
        let exec_pos = execute_fn
            .find("execute_statement_on_txn_with_create_index_with_params")
            .expect("write execution must run through the statement executor");
        let attempt_timeout_pos = fence_retry_pos
            + execute_fn[fence_retry_pos..exec_pos]
                .find("remaining_statement_timeout(retry_start, statement_timeout)")
                .expect("statement execution attempts must use the remaining statement timeout");

        assert!(
            begin_pos < fence_timeout_pos
                && fence_timeout_pos < fence_pos
                && fence_remaining_pos < fence_pos
                && fence_pos < fence_retry_pos
                && attempt_timeout_pos < exec_pos
                && fence_retry_pos < exec_pos,
            "the lifecycle row lock is the commit permit; it must be acquired before user-row locks or mutations"
        );
    }

    #[test]
    fn observability_start_transaction_uses_nested_begin_safe_mode_helper() {
        let source = include_str!("transaction.rs");
        let prod_source = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("transaction.rs must contain the test module");
        let start_branch = prod_source
            .split("Statement::StartTransaction { modes, .. } => {")
            .nth(1)
            .expect("observability dispatch must handle START TRANSACTION");
        let branch_prefix = start_branch
            .split("Ok(Some(vec![ExecuteResult::TransactionStart { tag: \"BEGIN\" }]))")
            .next()
            .expect("START TRANSACTION branch must return BEGIN");
        let validate_pos = branch_prefix
            .find("validate_begin_transaction_modes(session, modes)?")
            .expect("START TRANSACTION branch must use nested-BEGIN-safe mode validation");
        let begin_pos = branch_prefix
            .find("session.begin().await?")
            .expect("START TRANSACTION branch must call session.begin");

        assert!(
            validate_pos < begin_pos,
            "observability START TRANSACTION must not rewrite active transaction modes before the nested BEGIN no-op"
        );
        assert!(
            !branch_prefix.contains("validate_transaction_modes(session, modes, false)"),
            "observability START TRANSACTION must not directly mutate transaction modes inside an active transaction"
        );
    }

    #[test]
    fn create_index_concurrently_wait_is_statement_timeout_bounded() {
        let source = include_str!("transaction.rs");
        let prod_source = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("transaction.rs must contain the test module");
        let execute_fn = prod_source
            .split("pub(super) async fn execute_ddl_dml_with_autocommit(")
            .nth(1)
            .expect("execute_ddl_dml_with_autocommit must exist");
        let wait_pos = execute_fn
            .find("self.wait_for_create_index_concurrently(session, stmt)")
            .expect("CIC autocommit path must wait for background index completion");
        let wait_wrapper = execute_fn[..wait_pos]
            .rfind("apply_statement_timeout(")
            .expect("CIC wait must be wrapped in statement timeout");
        let remaining_pos = execute_fn[wait_wrapper..wait_pos]
            .find("remaining_statement_timeout(retry_start, retry_timeout)")
            .expect("CIC wait must use the remaining effective statement/retry timeout");

        assert!(
            wait_wrapper < wait_pos && remaining_pos > 0,
            "CREATE INDEX CONCURRENTLY wait must not run unbounded after commit"
        );
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

    #[test]
    fn retry_paths_classify_original_error_before_rollback_cleanup() {
        let source = include_str!("transaction.rs");
        let prod_source = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("transaction.rs must contain the test module");
        let execute_fn = prod_source
            .split("pub(super) async fn execute_ddl_dml_with_autocommit(")
            .nth(1)
            .expect("execute_ddl_dml_with_autocommit must exist");

        for context in [
            "ddl_dml_commit_error",
            "ddl_dml_statement_error",
            "explicit_first_statement_retry_timeout",
            "explicit_first_statement_retry",
        ] {
            let quoted_context = format!("\"{context}\"");
            let context_pos = execute_fn
                .find(&quoted_context)
                .unwrap_or_else(|| panic!("{context} cleanup must be present"));
            if context == "ddl_dml_commit_error" || context == "ddl_dml_statement_error" {
                let prefix = &execute_fn[..context_pos];
                let retryable_pos = prefix
                    .rfind("let retryable = is_retryable_tikv_error(&err);")
                    .unwrap_or_else(|| panic!("{context} must classify the original error"));
                assert!(
                    retryable_pos < context_pos,
                    "{context} must classify retryability before rollback cleanup can fail"
                );
            }
        }

        assert!(
            execute_fn.contains("rollback_for_retry_or_abandon"),
            "retry cleanup must abandon a failed rollback instead of leaking 25P02"
        );
    }

    #[test]
    fn create_index_concurrently_waits_for_worker_after_autocommit_commit() {
        let source = include_str!("transaction.rs");
        let prod_source = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("transaction.rs must contain the test module");
        let execute_fn = prod_source
            .split("pub(super) async fn execute_ddl_dml_with_autocommit(")
            .nth(1)
            .expect("execute_ddl_dml_with_autocommit must exist");
        let commit_pos = execute_fn
            .find("session.commit().await")
            .expect("autocommit commit must exist");
        let wait_pos = execute_fn
            .find("wait_for_create_index_concurrently")
            .expect("CREATE INDEX CONCURRENTLY must wait for BgDdl completion");
        let return_pos = execute_fn
            .find("return Ok(stmt_results)")
            .expect("autocommit success return must exist");

        assert!(
            commit_pos < wait_pos && wait_pos < return_pos,
            "CIC wait must run after the enqueue transaction commits and before the SQL statement returns"
        );
    }
}
