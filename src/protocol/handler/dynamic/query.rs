//! Simple-query and extended-query protocol handling for [`DynamicPgHandler`].
//!
//! Contains `is_data_statement`, `reject_unanalyzed_if_needed`,
//! `utility_describe_fields`, `merge_parameter_types`, and the
//! [`SimpleQueryHandler`] / [`ExtendedQueryHandler`] trait implementations.

use super::DynamicPgHandler;
use crate::auth::Privilege;
use crate::model::DataType;
use crate::pool::ConcurrencyGuard;
use crate::sql::error::SqlError;
use crate::sql::executor::core::prepared_analysis::PreparedAnalysis;
use crate::sql::ExecuteResult;
use async_trait::async_trait;
use futures::{Sink, SinkExt};
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo, Response,
};
use pgwire::api::stmt::StoredStatement;
use pgwire::api::store::PortalStore;
use pgwire::api::{ClientInfo, ClientPortalStore, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::response::NoticeResponse;
use pgwire::messages::PgWireBackendMessage;
use pgwire::tokio::CancellationToken;
use sqlparser::ast::Statement;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, error, warn};

use super::super::encode::pgtype_to_datatype;
use super::super::encode::{datatype_to_pgtype, effective_result_format, result_to_response};
use super::super::errors::{
    ambiguous_column_error_with_position, in_failed_sql_transaction_pgwire_error, pg_error_message,
    sqlstate_for_executor_error,
};
use super::super::params::{count_sql_parameters, decode_parameters};
use super::super::portal::{
    on_execute_with_tx_status_fix_with_guards, on_query_with_tx_status_fix,
};
use super::super::prepared::{PreparedExec, PreparedStatement};
use super::super::resolve_copy_columns;
use super::super::{
    client_allows_message, rollback_autocommit_or_mark_failed,
    send_notices_and_get_last_response_with_format, CopyContext,
};

/// Read the `bytea_output` session setting and return the corresponding enum.
fn parse_bytea_output_from_session(
    session: &crate::sql::Session,
) -> crate::sql::bytea::ByteaOutput {
    match session.show_setting_value("bytea_output").as_deref() {
        Some("escape") => crate::sql::bytea::ByteaOutput::Escape,
        _ => crate::sql::bytea::ByteaOutput::Hex,
    }
}

/// Returns true for SELECT/INSERT/UPDATE/DELETE -- statements that require
/// Analyzer output for correct Describe schema.  Uses parse_sql for
/// precise AST classification (handles SELECT\n, WITH\t, etc.).
///
/// Used by tests; the `on_parse` hot path uses [`is_data_statement_stmts`]
/// with the cached AST instead.
#[allow(dead_code)]
pub(in crate::protocol::handler) fn is_data_statement(sql: &str) -> bool {
    match crate::sql::parse_sql(sql) {
        Ok(stmts) if !stmts.is_empty() => is_data_statement_stmts(&stmts),
        _ => false, // unparseable -> accepted by should_accept_sql_without_sqlparser -> utility
    }
}

/// Classify pre-parsed statements as data statements.
/// Returns true for SELECT/INSERT/UPDATE/DELETE.
pub(in crate::protocol::handler) fn is_data_statement_stmts(stmts: &[Statement]) -> bool {
    !stmts.is_empty()
        && matches!(
            &stmts[0],
            Statement::Query(_)
                | Statement::Insert { .. }
                | Statement::Update { .. }
                | Statement::Delete { .. }
        )
}

/// Returns true for transaction-control statements that must never be
/// rate-limited: BEGIN, COMMIT, END, ROLLBACK, ROLLBACK TO SAVEPOINT,
/// SAVEPOINT, RELEASE SAVEPOINT, SET TRANSACTION.  Blocking these would
/// prevent transaction cleanup and violate PostgreSQL recovery semantics.
pub(in crate::protocol::handler) fn is_transaction_control(sql: &str) -> bool {
    match crate::sql::parse_sql(sql) {
        Ok(stmts) if !stmts.is_empty() => is_transaction_control_stmts(&stmts),
        _ => false,
    }
}

/// Classify pre-parsed statements as transaction-control.
/// Returns true when all statements are BEGIN/COMMIT/ROLLBACK/SAVEPOINT/etc.
pub(in crate::protocol::handler) fn is_transaction_control_stmts(stmts: &[Statement]) -> bool {
    !stmts.is_empty()
        && stmts.iter().all(|s| {
            matches!(
                s,
                Statement::StartTransaction { .. }
                    | Statement::Commit { .. }
                    | Statement::Rollback { .. }
                    | Statement::Savepoint { .. }
                    | Statement::ReleaseSavepoint { .. }
                    | Statement::SetTransaction { .. }
            )
        })
}

/// Reject RawSqlUtility that should have been analyzed.
/// Check order: data statement first (XX000 with infra failure reason),
/// then param_count > 0 (42P02 for utility + params).
///
/// Used by tests; the `on_parse` hot path uses [`is_data_statement_stmts`]
/// with the cached AST instead.
#[allow(dead_code)]
pub(in crate::protocol::handler) fn reject_unanalyzed_if_needed(
    sql: &str,
    param_count: usize,
    reason: &str,
) -> Option<PgWireError> {
    if is_data_statement(sql) {
        Some(PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".into(),
            "XX000".to_string(),
            format!("cannot describe data statement: {}", reason),
        ))))
    } else if param_count > 0 {
        let err: anyhow::Error = SqlError::InvalidParameterUsage {
            index: 1,
            context: "utility statements do not support parameters".into(),
        }
        .into();
        let sqlstate = sqlstate_for_executor_error(&err);
        Some(PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".into(),
            sqlstate.to_string(),
            pg_error_message(&err, sqlstate),
        ))))
    } else {
        None // genuine utility, no params -- safe as RawSqlUtility
    }
}

/// Static Describe schema for known row-producing utility statements.
/// Uses AST classification -- no heuristic inference.
pub(in crate::protocol::handler) fn utility_describe_fields(sql: &str) -> Vec<FieldInfo> {
    let stmts = match crate::sql::parse_sql(sql) {
        Ok(s) if !s.is_empty() => s,
        _ => return vec![],
    };
    match &stmts[0] {
        Statement::ShowVariable { variable } => {
            // Mirror execution: dispatch.rs normalizes idents (quoted preserved,
            // unquoted lowercased), joins with ".", then lowercases the result.
            let name = variable
                .iter()
                .map(|i| {
                    if i.quote_style.is_some() {
                        i.value.clone()
                    } else {
                        i.value.to_lowercase()
                    }
                })
                .collect::<Vec<_>>()
                .join(".")
                .to_lowercase();

            if name == "all" {
                vec![
                    FieldInfo::new(
                        "name".to_string(),
                        None,
                        None,
                        Type::TEXT,
                        FieldFormat::Text,
                    ),
                    FieldInfo::new(
                        "setting".to_string(),
                        None,
                        None,
                        Type::TEXT,
                        FieldFormat::Text,
                    ),
                    FieldInfo::new(
                        "description".to_string(),
                        None,
                        None,
                        Type::TEXT,
                        FieldFormat::Text,
                    ),
                ]
            } else {
                vec![FieldInfo::new(
                    name,
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Text,
                )]
            }
        }
        Statement::ShowTables { .. } => {
            vec![FieldInfo::new(
                "table_name".to_string(),
                None,
                None,
                Type::TEXT,
                FieldFormat::Text,
            )]
        }
        Statement::Explain { .. } => {
            vec![FieldInfo::new(
                "QUERY PLAN".to_string(),
                None,
                None,
                Type::TEXT,
                FieldFormat::Text,
            )]
        }
        _ => vec![], // DDL/SET/etc -- no rows
    }
}

/// Merge finalized analyzer types into wire types.
/// Client-specified non-UNKNOWN OIDs win (preserves INT2/FLOAT4 fidelity).
/// UNKNOWN slots filled from inferred DataType -> Type.
pub(in crate::protocol::handler) fn merge_parameter_types(
    client_types: &[Type],
    finalized: &[DataType],
) -> Vec<Type> {
    let mut result = Vec::with_capacity(finalized.len());
    for (i, fin_type) in finalized.iter().enumerate() {
        let client = client_types.get(i).cloned().unwrap_or(Type::UNKNOWN);
        if client != Type::UNKNOWN {
            result.push(client); // preserve client INT2/FLOAT4
        } else {
            result.push(datatype_to_pgtype(Some(fin_type)));
        }
    }
    result
}

impl DynamicPgHandler {
    /// Check per-tenant QPS rate limit. Returns an error if the limit is exceeded.
    fn check_rate_limit(&self) -> PgWireResult<()> {
        if let Some(handle) = self.tenant_handle.get() {
            if let Some(limiter) = handle.rate_limiter() {
                if !limiter.try_acquire() {
                    let keyspace = handle.keyspace();
                    crate::observability::registry()
                        .tenant(keyspace)
                        .record_rate_limited();
                    warn!(
                        keyspace = keyspace,
                        limit = limiter.rate(),
                        "Query rate-limited for tenant"
                    );
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "53300".to_string(),
                        format!(
                            "too many queries for tenant (rate limit: {} QPS)",
                            limiter.rate()
                        ),
                    ))));
                }
            }
        }
        Ok(())
    }

    /// Check TiKV adaptive backpressure. Returns a guard that tracks in-flight
    /// concurrency (SQLSTATE 53300 on rejection). Caller must hold the guard for
    /// the duration of the query.
    pub(super) fn check_backpressure(
        &self,
    ) -> PgWireResult<Option<crate::storage::backpressure::BackpressureGuard>> {
        if let Some(ctrl) = crate::storage::backpressure::controller() {
            match ctrl.try_acquire() {
                Ok(guard) => return Ok(Some(guard)),
                Err(rejection) => {
                    if let Some(handle) = self.tenant_handle.get() {
                        tracing::warn!(
                            keyspace = handle.keyspace(),
                            "Query rejected by TiKV backpressure"
                        );
                    }
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "53300".to_string(),
                        rejection.to_string(),
                    ))));
                }
            }
        }
        Ok(None)
    }

    /// Check per-principal concurrent query limit.
    ///
    /// Returns a [`ConcurrencyGuard`] that decrements the in-flight counter on
    /// drop. The caller must hold the guard for the duration of the query.
    fn check_principal_concurrency(&self) -> PgWireResult<Option<ConcurrencyGuard>> {
        if let Some(handle) = self.tenant_handle.get() {
            let tracker = handle.concurrency_tracker();
            // Effective limit = min(server default, JWT budget_max_concurrent claim).
            // Claims can only tighten, never relax.
            let effective_limit = match self.budget_max_concurrent.get() {
                Some(&claim_limit) if claim_limit > 0 => {
                    if tracker.limit() == 0 {
                        claim_limit
                    } else {
                        tracker.limit().min(claim_limit)
                    }
                }
                _ => tracker.limit(),
            };
            if effective_limit == 0 {
                return Ok(None); // disabled
            }
            let principal = self.principal_identity();
            let result = if effective_limit == tracker.limit() {
                tracker.try_acquire(principal)
            } else {
                tracker.try_acquire_with_limit(principal, effective_limit)
            };
            match result {
                Ok(guard) => Ok(Some(guard)),
                Err(rejection) => {
                    warn!(
                        keyspace = handle.keyspace(),
                        principal = %principal,
                        limit = rejection.limit,
                        "Query rejected: too many concurrent queries for principal"
                    );
                    Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "53300".to_string(),
                        format!(
                            "too many concurrent queries for principal (limit: {})",
                            rejection.limit
                        ),
                    ))))
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Notify the session registry that a query is starting.
    /// Stores the query-level child CancellationToken so the execution path
    /// can check it for admin-initiated query cancellation (SQLSTATE 57014).
    pub(in crate::protocol::handler) fn begin_query_tracking(&self, query: &str) {
        if let Some(info) = self.session_info.get() {
            let child = info.begin_query(query);
            *self.active_query_cancel.lock() = Some(child);
        }
    }

    /// Query the session's transaction state for post-query tracking.
    async fn query_transaction_state(&self) -> (bool, bool) {
        if let Some(auth) = self.auth_state.get() {
            let session = auth.session.lock().await;
            (session.is_in_transaction(), session.is_transaction_failed())
        } else {
            (false, false)
        }
    }

    /// Notify the session registry that a query has ended and clear the
    /// query-level cancellation token.
    pub(in crate::protocol::handler) fn end_query_tracking(
        &self,
        in_transaction: bool,
        in_failed_transaction: bool,
    ) {
        *self.active_query_cancel.lock() = None;
        if let Some(info) = self.session_info.get() {
            let state = if in_failed_transaction {
                crate::admin::SessionState::IdleInFailedTransaction
            } else if in_transaction {
                crate::admin::SessionState::IdleInTransaction
            } else {
                crate::admin::SessionState::Idle
            };
            info.end_query(state);
        }
    }

    /// Return a clone of the active query-level cancellation token, if any.
    pub(in crate::protocol::handler) fn query_cancel_token(&self) -> Option<CancellationToken> {
        self.active_query_cancel.lock().clone()
    }

    /// Return the principal identity string used as the concurrency bucket key.
    ///
    /// Uses the cached identity set during authentication — no session lock needed.
    /// Note: concurrency is keyed by principal, while admission budget is keyed
    /// by `budget_owner_id` (tenant). These are intentionally different scopes.
    fn principal_identity(&self) -> &str {
        self.principal_identity
            .get()
            .map(|s| s.as_str())
            .unwrap_or("unknown")
    }

    /// Check per-budget-owner admission rate limit (connect-token sessions only).
    ///
    /// Returns an error if the token bucket for this budget owner is exhausted.
    /// Direct pgwire sessions (no JWT budget claims) bypass this check.
    fn check_admission_budget(&self) -> PgWireResult<()> {
        if let Some(bucket) = self.admission_budget.get() {
            if !bucket.try_acquire() {
                if let Some(handle) = self.tenant_handle.get() {
                    crate::observability::registry()
                        .tenant(handle.keyspace())
                        .record_rate_limited();
                }
                warn!(
                    principal = %self.principal_identity(),
                    rate = bucket.rate(),
                    "Query rejected: admission budget exhausted"
                );
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "53300".to_string(),
                    format!(
                        "too many queries: admission budget exhausted (limit: {} QPS)",
                        bucket.rate()
                    ),
                ))));
            }
        }
        Ok(())
    }

    /// Normalize a single SQL identifier token from the COPY tokenizer.
    ///
    /// - Quoted (`"Foo""Bar"`) → strip outer quotes, unescape `""` → `Foo"Bar` (case preserved).
    /// - Unquoted (`foo`) → fold to lowercase (PostgreSQL convention).
    fn normalize_ident(ident: &str) -> String {
        let ident = ident.trim();
        if ident.starts_with('"') && ident.ends_with('"') && ident.len() >= 2 {
            ident[1..ident.len() - 1].replace("\"\"", "\"")
        } else {
            ident.to_lowercase()
        }
    }

    /// Split a potentially schema-qualified table name into `(Option<schema>, table)`,
    /// respecting quoted identifiers. Each part is normalized via [`Self::normalize_ident`].
    ///
    /// Input comes from the COPY tokenizer and may be:
    /// `"Schema"."Table"`, `schema.table`, `"Table"`, or `table`.
    fn split_schema_table(name: &str) -> (Option<String>, String) {
        let bytes = name.as_bytes();
        let mut i = 0;
        let mut in_quotes = false;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    if in_quotes {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                            i += 2; // escaped quote ""
                            continue;
                        }
                        in_quotes = false;
                    } else {
                        in_quotes = true;
                    }
                }
                b'.' if !in_quotes => {
                    let schema = Self::normalize_ident(&name[..i]);
                    let table = Self::normalize_ident(&name[i + 1..]);
                    return (Some(schema), table);
                }
                _ => {}
            }
            i += 1;
        }
        (None, Self::normalize_ident(name))
    }

    async fn handle_copy_from_simple_query<'a>(
        &self,
        query: &'a str,
    ) -> PgWireResult<Option<Vec<Response<'a>>>> {
        let Some((table_name, columns, copy_options)) =
            DynamicPgHandler::parse_copy_command_with_options(query)
                .map_err(|e| PgWireError::UserError(Box::new(e)))?
        else {
            return Ok(None);
        };

        debug!(
            "COPY FROM STDIN: table={}, columns={:?}, format={:?}",
            table_name, columns, copy_options.format
        );

        let state = self.auth();
        let executor = &state.executor;
        let (
            resolved_table,
            resolved_columns,
            column_types,
            col_count,
            started_txn,
            qctx,
            runtime_context,
        ) = {
            let mut session = state.session.lock().await;

            // Recheck after acquiring lock — watchdog may have fired in the gap.
            if self.cancel_token.is_cancelled() {
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "FATAL".to_string(),
                    "25P03".to_string(),
                    "terminating connection due to idle-in-transaction timeout".to_string(),
                ))));
            }

            if let Err(e) = session.check_idle_in_transaction_timeout() {
                let _ = session.rollback().await;
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "FATAL".to_string(),
                    e.sqlstate().to_string(),
                    e.to_string(),
                ))));
            }

            if session.is_transaction_failed() {
                return Err(in_failed_sql_transaction_pgwire_error());
            }

            let started_txn = !session.is_in_transaction();
            if started_txn {
                session.begin().await.map_err(|e| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        e.to_string(),
                    )))
                })?;
            }

            let statement_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let transaction_ts = session.transaction_timestamp_ms().unwrap_or(statement_ts);
            let qctx = session.query_context_for_statement(statement_ts, transaction_ts);

            let db_id = session.current_database_id();
            let search_path: Vec<String> = session.search_path().to_vec();
            let (resolved_table, schema, table_ident) = {
                let txn = session.get_mut_txn().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        "No transaction".to_string(),
                    )))
                })?;

                let (schema_opt, table_ident) = Self::split_schema_table(&table_name);

                if let Some(schema_ident) = schema_opt {
                    // Check schema existence first — PG returns 3F000 for
                    // missing schema before checking table existence.
                    match executor
                        .store()
                        .schema_exists(txn, db_id, &schema_ident)
                        .await
                    {
                        Ok(false) => {
                            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "3F000".to_string(),
                                format!("schema \"{}\" does not exist", schema_ident),
                            ))));
                        }
                        Err(e) => {
                            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "XX000".to_string(),
                                e.to_string(),
                            ))));
                        }
                        Ok(true) => {}
                    }

                    let resolved_table = format!("{}.{}", schema_ident, table_ident);
                    match executor
                        .store()
                        .get_schema(txn, db_id, &resolved_table)
                        .await
                    {
                        Ok(Some(schema)) => (resolved_table, schema, table_ident),
                        Ok(None) => {
                            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "42P01".to_string(),
                                format!("relation \"{}\" does not exist", resolved_table),
                            ))));
                        }
                        Err(e) => {
                            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "XX000".to_string(),
                                e.to_string(),
                            ))));
                        }
                    }
                } else {
                    let schema_entries: Vec<&str> = if search_path.is_empty() {
                        vec!["public"]
                    } else {
                        search_path.iter().map(|s| s.as_str()).collect()
                    };

                    // Build ordered candidates, deduplicating to avoid redundant
                    // TiKV key reads while preserving search_path priority.
                    let mut seen = std::collections::HashSet::new();
                    let mut candidates: Vec<String> = Vec::with_capacity(schema_entries.len());
                    let mut ordered: Vec<String> = Vec::with_capacity(schema_entries.len());
                    for schema_ident in &schema_entries {
                        let full = format!("{}.{}", schema_ident, table_ident);
                        ordered.push(full.clone());
                        if seen.insert(full.clone()) {
                            candidates.push(full);
                        }
                    }

                    // Single batch fetch for all candidate names.
                    let batch_schemas = match executor
                        .store()
                        .list_table_schemas(txn, db_id, &candidates)
                        .await
                    {
                        Ok(schemas) => schemas,
                        Err(e) => {
                            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "XX000".to_string(),
                                e.to_string(),
                            ))));
                        }
                    };

                    // Build map keyed by full qualified name.
                    let schema_map: std::collections::HashMap<String, crate::model::TableSchema> =
                        batch_schemas
                            .into_iter()
                            .map(|s| (s.name.clone(), s))
                            .collect();

                    // Return first hit in original search_path order.
                    let found = ordered
                        .into_iter()
                        .find_map(|name| schema_map.get(&name).map(|s| (name, s.clone())));

                    match found {
                        Some((name, schema)) => (name, schema, table_ident),
                        None => {
                            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "42P01".to_string(),
                                format!("relation \"{}\" does not exist", table_ident),
                            ))));
                        }
                    }
                }
            };

            // Authorization: COPY FROM STDIN bypasses statement execution.
            // Require INSERT privilege before entering COPY mode.
            let current_role = session.current_user().map(|s| s.to_string());
            let privilege_result = {
                let txn = session.get_mut_txn().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        "No transaction".to_string(),
                    )))
                })?;
                executor
                    .require_table_privilege(
                        txn,
                        current_role.as_deref(),
                        Privilege::Insert,
                        &resolved_table,
                    )
                    .await
            };

            if let Err(e) = privilege_result {
                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                let sqlstate = sqlstate_for_executor_error(&e);
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    sqlstate.to_string(),
                    pg_error_message(&e, sqlstate),
                ))));
            }

            let (resolved_columns, column_types) =
                match resolve_copy_columns(&schema, &columns, &table_ident) {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                        return Err(e);
                    }
                };

            let runtime_context =
                crate::sql::runtime_context::StatementRuntimeContext::from_session_with_store(
                    &session,
                    executor.tenant_keyspace(),
                    &executor.store(),
                );

            let col_count = resolved_columns.len();
            (
                resolved_table,
                resolved_columns,
                column_types,
                col_count,
                started_txn,
                qctx,
                runtime_context,
            )
        };

        let mut ctx = self.copy_context.lock().await;
        *ctx = Some(CopyContext {
            table_name: resolved_table,
            columns: resolved_columns,
            column_types,
            query_context: qctx,
            runtime_context,
            backpressure_guard: None,
            line_buffer: Vec::new(),
            row_count: 0,
            started_txn,
            reached_end_marker: false,
            copy_options,
            header_skipped: false,
            batch_rows_since_commit: 0,
            pending_self_fk_keys: std::collections::HashMap::new(),
            deferred_self_fk_checks: Vec::new(),
            dirty_table_ids: std::collections::HashSet::new(),
        });

        let column_formats: Vec<i16> = vec![0; col_count];
        Ok(Some(vec![Response::CopyIn(
            pgwire::api::results::CopyResponse::new(0, col_count, column_formats),
        )]))
    }
}

#[async_trait]
impl SimpleQueryHandler for DynamicPgHandler {
    async fn on_query<C>(
        &self,
        client: &mut C,
        query: pgwire::messages::simplequery::Query,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        self.begin_query_tracking(&query.query);
        let memory_accountant = Some(self.auth().executor.tenant_memory_accountant().clone());
        let result =
            on_query_with_tx_status_fix(self, memory_accountant, self.connection_id, client, query)
                .await;
        let (in_txn, in_failed) = self.query_transaction_state().await;
        self.end_query_tracking(in_txn, in_failed);
        result
    }

    async fn do_query<'a, C>(
        &self,
        client: &mut C,
        query: &'a str,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        debug!("Received query: {}", query);

        // Transaction-control statements (BEGIN, COMMIT, ROLLBACK, SAVEPOINT, …)
        // are exempt from rate limiting -- blocking them would prevent transaction
        // cleanup and violate PostgreSQL recovery semantics.
        let (_bp_guard, _concurrency_guard) = if !is_transaction_control(query) {
            self.check_rate_limit()?;
            self.check_admission_budget()?;
            let bp = Some(self.check_backpressure()?);
            let cg = self.check_principal_concurrency()?;
            (bp, cg)
        } else {
            (None, None)
        };

        let state = self.auth();
        let executor = &state.executor;

        // Defense-in-depth: block COPY statements after idle-in-transaction timeout.
        // Checked before ALL COPY branches including parquet/fs9 paths.
        if self.cancel_token.is_cancelled() {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "25P03".to_string(),
                "terminating connection due to idle-in-transaction timeout".to_string(),
            ))));
        }

        #[cfg(feature = "parquet")]
        if let Some(result) = self.try_handle_copy_from_fs9(client, query).await? {
            return Ok(result);
        }

        #[cfg(feature = "parquet")]
        if let Some(result) = self.try_handle_copy_from_parquet(client, query).await? {
            return Ok(result);
        }

        // Export snapshot COPY: streaming table export at pinned timestamp.
        if let Some(result) = self.try_handle_export_snapshot_copy(client, query).await? {
            return Ok(result);
        }

        match DynamicPgHandler::parse_copy_to_command(query) {
            Ok(Some((table_name, columns, copy_opts))) => {
                debug!(
                    "COPY TO STDOUT: table={}, columns={:?}, format={:?}",
                    table_name, columns, copy_opts.format
                );
                return self
                    .handle_copy_to_stdout(client, &table_name, &columns, &copy_opts)
                    .await;
            }
            Ok(None) => {}
            Err(e) => return Err(PgWireError::UserError(Box::new(e))),
        }

        if let Some(result) = self.handle_copy_from_simple_query(query).await? {
            return Ok(result);
        }

        // Defense-in-depth: check if the cancel token was fired before acquiring
        // the session lock (Timeline A gap in #1124).
        if self.cancel_token.is_cancelled() {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "25P03".to_string(),
                "terminating connection due to idle-in-transaction timeout".to_string(),
            ))));
        }

        let mut session = state.session.lock().await;

        // Recheck after acquiring lock — watchdog may have fired in the gap.
        if self.cancel_token.is_cancelled() {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "25P03".to_string(),
                "terminating connection due to idle-in-transaction timeout".to_string(),
            ))));
        }

        if let Err(e) = session.check_idle_in_transaction_timeout() {
            let _ = session.rollback().await;
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                e.sqlstate().to_string(),
                e.to_string(),
            ))));
        }

        // Check query-level cancel token before entering the executor.
        // This handles the case where cancel_query() fired between
        // begin_query_tracking and here — catches it without relying on
        // the executor yielding at an async point.
        let query_cancel = self.query_cancel_token();
        if let Some(ref qc) = query_cancel {
            if qc.is_cancelled() {
                rollback_autocommit_or_mark_failed(&mut session, false).await;
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "57014".to_string(),
                    "canceling statement due to user request".to_string(),
                ))));
            }
        }
        // Race the executor against the query-level cancel token so that
        // admin cancel_query() can interrupt a running statement (SQLSTATE 57014).
        let exec_result = if let Some(ref qc) = query_cancel {
            tokio::select! {
                biased;
                result = executor.execute(&mut session, query) => result,
                _ = qc.cancelled() => {
                    rollback_autocommit_or_mark_failed(&mut session, false).await;
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "57014".to_string(),
                        "canceling statement due to user request".to_string(),
                    ))));
                }
            }
        } else {
            executor.execute(&mut session, query).await
        };
        match exec_result {
            Ok(results) => {
                session.record_command_complete();
                let mut responses: Vec<Response<'a>> = Vec::new();
                for result in results.into_vec() {
                    if let ExecuteResult::Notice {
                        message,
                        severity,
                        sqlstate,
                    } = result
                    {
                        if client_allows_message(
                            session.show_setting_value("client_min_messages").as_deref(),
                            &severity,
                        ) {
                            let notice =
                                NoticeResponse::from(ErrorInfo::new(severity, sqlstate, message));
                            client
                                .send(PgWireBackendMessage::NoticeResponse(notice))
                                .await?;
                        }
                        continue;
                    }
                    let bytea_output = parse_bytea_output_from_session(&session);
                    responses.push(result_to_response(result, bytea_output)?);
                }
                Ok(responses)
            }
            Err(e) => {
                // Drain pending notices (e.g., SET LOCAL warning before reserved-GUC error)
                for (severity, sqlstate, message) in session.drain_pending_notices() {
                    if client_allows_message(
                        session.show_setting_value("client_min_messages").as_deref(),
                        &severity,
                    ) {
                        let notice =
                            NoticeResponse::from(ErrorInfo::new(severity, sqlstate, message));
                        client
                            .send(PgWireBackendMessage::NoticeResponse(notice))
                            .await?;
                    }
                }
                let sqlstate = sqlstate_for_executor_error(&e);
                let pg_msg = pg_error_message(&e, sqlstate);
                error!(sqlstate, "Query execution error: {}", pg_msg);
                debug!("Query execution error detail: {}", e);
                let mut error_info =
                    ErrorInfo::new("ERROR".to_string(), sqlstate.to_string(), pg_msg);
                if let Some((_col, pos)) =
                    ambiguous_column_error_with_position(query, &error_info.message)
                {
                    error_info.code = "42702".to_string();
                    error_info.position = Some(pos.to_string());
                }
                Err(PgWireError::UserError(Box::new(error_info)))
            }
        }
    }
}

#[async_trait]
impl ExtendedQueryHandler for DynamicPgHandler {
    type Statement = PreparedStatement;
    type QueryParser = super::super::Db9QueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn on_parse<C>(
        &self,
        client: &mut C,
        message: pgwire::messages::extendedquery::Parse,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        // 1. Parse via QueryParser — single authoritative parse for the
        //    entire extended Parse message.  The parser caches the AST so we
        //    can reuse it for classification and analysis below.
        let parser = self.query_parser();
        let mut stored = StoredStatement::parse(&message, parser).await?;

        // Retrieve the cached AST produced by parse_sql.
        // `None` means the SQL was empty or accepted only via the
        // should_accept_sql_without_sqlparser fallback (unparseable).
        let parsed_stmts = self.query_parser.take_parsed_statements();

        // 2. Count $N placeholders in the SQL
        let param_count = count_sql_parameters(&stored.statement.sql);

        // 3. Map client OIDs to Option<DataType> for Analyzer
        let client_oids: Vec<Option<DataType>> = stored
            .parameter_types
            .iter()
            .map(|t| {
                if *t == Type::UNKNOWN {
                    None
                } else {
                    pgtype_to_datatype(t)
                }
            })
            .collect();

        // Keep transaction-control statements exempt from backpressure for
        // cleanup/recovery paths.  Uses cached AST — no re-parse.
        let is_txn_control = parsed_stmts
            .as_ref()
            .is_some_and(|stmts| is_transaction_control_stmts(stmts));
        let _bp_guard = if !is_txn_control {
            Some(self.check_backpressure()?)
        } else {
            None
        };

        // 4. Reject fallback-accepted SQL (unparseable) that carries $N params.
        //    Fallback SQL is always utility-class; params are never valid.
        if parsed_stmts.is_none() && param_count > 0 {
            let err: anyhow::Error = SqlError::InvalidParameterUsage {
                index: 1,
                context: "utility statements do not support parameters".into(),
            }
            .into();
            let sqlstate = sqlstate_for_executor_error(&err);
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".into(),
                sqlstate.to_string(),
                pg_error_message(&err, sqlstate),
            ))));
        }

        // 5. Analyze for frozen execution IR.
        //    Only attempt analysis when the SQL was successfully parsed
        //    (parsed_stmts is Some).  Fallback-accepted SQL (None) stays as
        //    RawSqlUtility — analysis would fail at the internal parse anyway.
        if let Some(ref stmts) = parsed_stmts {
            let state = self.auth();
            let executor = &state.executor;
            let store = executor.store();

            // Brief session lock to read db_id + search_path + is_superuser + bypass_rls
            //
            // session_user() (not current_user()) is the login role and matches
            // what StatementRuntimeContext::from_session uses on the execution
            // path. SET ROLE must not influence fs-plane scope.
            let (db_id, search_path, is_superuser, bypass_rls, authenticated_role) = {
                let session = state.session.lock().await;
                (
                    session.current_database_id(),
                    session.search_path().to_vec(),
                    session.is_superuser(),
                    session.bypass_rls(),
                    session.session_user().map(str::to_string),
                )
            };

            // Extension context for fs9 schema inference during catalog prefetch.
            // Without this, fs9 backend acquisition fails because the statement
            // scope has neither a cached backend nor a TiKV client. JuiceFS
            // tenants additionally need `authenticated_role` to derive scp at
            // mint time — without it `init_juicefs_backend` rejects the call.
            let tenant_keyspace = executor.tenant_keyspace().to_string();
            let tikv_client = store.transaction_client();
            let ext_opts = crate::extensions::context::ExtensionContextOpts::statement(
                is_superuser,
                bypass_rls,
                &tenant_keyspace,
            )
            .with_tikv_client(tikv_client)
            .with_authenticated_role(authenticated_role);

            // Temporary read-only transaction for catalog access
            match store.begin().await {
                Ok(mut txn) => {
                    match crate::extensions::context::with_context_opts(
                        ext_opts,
                        executor.analyze_for_prepared_with_statements(
                            &mut txn,
                            db_id,
                            &search_path,
                            stmts,
                            param_count,
                            &client_oids,
                        ),
                    )
                    .await
                    {
                        Ok(analysis) => {
                            match analysis {
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
                                        .map(|t| (t, Privilege::Select))
                                        .collect();
                                    stored.parameter_types = merge_parameter_types(
                                        &stored.parameter_types,
                                        &param_types,
                                    );
                                    stored.statement = PreparedStatement {
                                        sql: stored.statement.sql,
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
                                    };
                                }
                                PreparedAnalysis::Dml {
                                    analyzed,
                                    output_schema,
                                    param_types,
                                    table_versions,
                                    rls_sensitive,
                                    has_with_cte,
                                } => {
                                    let required_privileges =
                                        PreparedStatement::compute_privileges(&analyzed, &[]);
                                    stored.parameter_types = merge_parameter_types(
                                        &stored.parameter_types,
                                        &param_types,
                                    );
                                    stored.statement = PreparedStatement {
                                        sql: stored.statement.sql,
                                        exec: PreparedExec::AnalyzedDml {
                                            analyzed,
                                            required_privileges,
                                            has_with_cte,
                                        },
                                        output_schema,
                                        param_data_types: param_types,
                                        table_versions,
                                        rls_sensitive,
                                    };
                                }
                                PreparedAnalysis::Utility => {
                                    // Keep RawSqlUtility
                                }
                            }
                        }
                        Err(e) if param_count > 0 || is_data_statement_stmts(stmts) => {
                            // Data statements and parameterized statements must be analyzed.
                            let _ = txn.rollback().await;
                            let sqlstate = sqlstate_for_executor_error(&e);
                            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                sqlstate.to_string(),
                                pg_error_message(&e, sqlstate),
                            ))));
                        }
                        Err(_) => {
                            // Utility, no params -- keep RawSqlUtility
                        }
                    }
                    let _ = txn.rollback().await; // read-only, discard
                }
                Err(e) => {
                    // Can't begin txn -- reject data/parameterized SQL.
                    // Uses cached AST for classification (no re-parse).
                    if is_data_statement_stmts(stmts) {
                        return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".into(),
                            "XX000".to_string(),
                            format!(
                                "cannot describe data statement: failed to begin catalog transaction: {}",
                                e
                            ),
                        ))));
                    } else if param_count > 0 {
                        let err: anyhow::Error = SqlError::InvalidParameterUsage {
                            index: 1,
                            context: "utility statements do not support parameters".into(),
                        }
                        .into();
                        let sqlstate = sqlstate_for_executor_error(&err);
                        return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".into(),
                            sqlstate.to_string(),
                            pg_error_message(&err, sqlstate),
                        ))));
                    }
                    // Utility, no params -- keep RawSqlUtility
                }
            }
        }

        // 6. Store immutable
        client.portal_store().put_statement(Arc::new(stored))?;
        client
            .send(PgWireBackendMessage::ParseComplete(
                pgwire::messages::extendedquery::ParseComplete::new(),
            ))
            .await?;
        Ok(())
    }

    async fn on_execute<C>(
        &self,
        client: &mut C,
        message: pgwire::messages::extendedquery::Execute,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        // Query tracking (begin_query) happens inside do_query() where the
        // SQL text is available from the portal's prepared statement.
        let state = self.auth();
        let memory_accountant = Some(state.executor.tenant_memory_accountant().clone());
        let result = on_execute_with_tx_status_fix_with_guards(
            self,
            &self.suspended_portals,
            memory_accountant,
            self.connection_id,
            Some(&self.cancel_token),
            Some(state.session.as_ref()),
            client,
            message,
        )
        .await;
        let (in_txn, in_failed) = self.query_transaction_state().await;
        self.end_query_tracking(in_txn, in_failed);
        result
    }

    async fn on_bind<C>(
        &self,
        client: &mut C,
        message: pgwire::messages::extendedquery::Bind,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let portal_name = message
            .portal_name
            .as_deref()
            .unwrap_or(pgwire::api::DEFAULT_NAME);
        {
            // Rebind removal point: dropping old suspended state releases any
            // portal-owned reservation that was handed off at suspend time.
            let mut guard = self.suspended_portals.lock().await;
            guard.remove(portal_name);
        }

        let statement_name = message
            .statement_name
            .as_deref()
            .unwrap_or(pgwire::api::DEFAULT_NAME);

        if let Some(statement) = client.portal_store().get_statement(statement_name) {
            // Validate parameter count: on_parse already finalized types,
            // so Bind just checks that the client supplies the right number.
            let expected = statement.parameter_types.len();
            let provided = message.parameters.len();
            if provided != expected {
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "08P01".to_string(),
                    format!(
                        "bind message supplies {} parameters, but prepared statement requires {}",
                        provided, expected
                    ),
                ))));
            }

            let portal = Portal::try_new(&message, statement)?;
            client.portal_store().put_portal(Arc::new(portal))?;
            client
                .send(PgWireBackendMessage::BindComplete(
                    pgwire::messages::extendedquery::BindComplete::new(),
                ))
                .await?;
            Ok(())
        } else {
            Err(PgWireError::StatementNotFound(statement_name.to_owned()))
        }
    }

    async fn on_close<C>(
        &self,
        client: &mut C,
        message: pgwire::messages::extendedquery::Close,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let name = message.name.as_deref().unwrap_or(pgwire::api::DEFAULT_NAME);
        match message.target_type {
            pgwire::messages::extendedquery::TARGET_TYPE_BYTE_STATEMENT => {
                client.portal_store().rm_statement(name);
            }
            pgwire::messages::extendedquery::TARGET_TYPE_BYTE_PORTAL => {
                client.portal_store().rm_portal(name);
                // Explicit close-removal release point for portal-owned reservations.
                let mut guard = self.suspended_portals.lock().await;
                guard.remove(name);
            }
            _ => {}
        }

        client
            .send(PgWireBackendMessage::CloseComplete(
                pgwire::messages::extendedquery::CloseComplete::new(),
            ))
            .await?;
        Ok(())
    }

    /// Override default `on_describe` to prevent the pgwire default from writing
    /// inferred parameter types back into the stored statement. The statement is
    /// immutable after Parse -- Describe must be read-only.
    async fn on_describe<C>(
        &self,
        client: &mut C,
        message: pgwire::messages::extendedquery::Describe,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let name = message.name.as_deref().unwrap_or(pgwire::api::DEFAULT_NAME);
        match message.target_type {
            pgwire::messages::extendedquery::TARGET_TYPE_BYTE_STATEMENT => {
                if let Some(stmt) = client.portal_store().get_statement(name) {
                    let resp = self.do_describe_statement(client, &stmt).await?;
                    // NO write-back -- statement is immutable after Parse
                    pgwire::api::query::send_describe_response(client, &resp).await?;
                } else {
                    return Err(PgWireError::StatementNotFound(name.to_owned()));
                }
            }
            pgwire::messages::extendedquery::TARGET_TYPE_BYTE_PORTAL => {
                if let Some(portal) = client.portal_store().get_portal(name) {
                    let resp = self.do_describe_portal(client, &portal).await?;
                    pgwire::api::query::send_describe_response(client, &resp).await?;
                } else {
                    return Err(PgWireError::PortalNotFound(name.to_owned()));
                }
            }
            _ => return Err(PgWireError::InvalidTargetType(message.target_type)),
        }
        Ok(())
    }

    async fn do_query<'a, 'b: 'a, C>(
        &'b self,
        client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let prepared = &portal.statement.statement;

        // Track query metadata + create query-level cancel token (same as simple query).
        self.begin_query_tracking(&prepared.sql);

        // Transaction-control statements bypass rate limiting (see simple-query path).
        let (_bp_guard, _concurrency_guard) = if !is_transaction_control(&prepared.sql) {
            self.check_rate_limit()?;
            self.check_admission_budget()?;
            let bp = Some(self.check_backpressure()?);
            let cg = self.check_principal_concurrency()?;
            (bp, cg)
        } else {
            (None, None)
        };

        let state = self.auth();
        let executor = &state.executor;

        debug!("Extended query: {}", prepared.sql);

        // Defense-in-depth: check if the cancel token was fired before acquiring
        // the session lock (Timeline A gap in #1124).
        if self.cancel_token.is_cancelled() {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "25P03".to_string(),
                "terminating connection due to idle-in-transaction timeout".to_string(),
            ))));
        }

        let mut session = state.session.lock().await;

        // Recheck after acquiring lock — watchdog may have fired in the gap.
        if self.cancel_token.is_cancelled() {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "25P03".to_string(),
                "terminating connection due to idle-in-transaction timeout".to_string(),
            ))));
        }

        if let Err(e) = session.check_idle_in_transaction_timeout() {
            let _ = session.rollback().await;
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                e.sqlstate().to_string(),
                e.to_string(),
            ))));
        }

        // Check query-level cancel token before entering the executor.
        // Must be before exec_future creation since that borrows session.
        let query_cancel = self.query_cancel_token();
        if let Some(ref qc) = query_cancel {
            if qc.is_cancelled() {
                rollback_autocommit_or_mark_failed(&mut session, false).await;
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "57014".to_string(),
                    "canceling statement due to user request".to_string(),
                ))));
            }
        }

        let exec_future: Pin<
            Box<dyn Future<Output = Result<crate::sql::ExecuteResults, anyhow::Error>> + Send + '_>,
        > = match &prepared.exec {
            PreparedExec::RawSqlUtility => {
                debug_assert!(
                    portal.statement.parameter_types.is_empty(),
                    "RawSqlUtility should never have parameters after Parse"
                );
                Box::pin(executor.execute(&mut session, &prepared.sql))
            }
            PreparedExec::AnalyzedQuery { .. } | PreparedExec::AnalyzedDml { .. } => {
                let params = decode_parameters(portal)?;
                Box::pin(executor.execute_prepared(
                    &mut session,
                    &prepared.sql,
                    &prepared.exec,
                    params,
                    &prepared.param_data_types,
                    &prepared.table_versions,
                    prepared.rls_sensitive,
                ))
            }
        };
        // Race the executor against the query-level cancel token so that
        // admin cancel_query() can interrupt a running statement (SQLSTATE 57014).
        let exec_results = if let Some(ref qc) = query_cancel {
            tokio::select! {
                biased;
                result = exec_future => result,
                _ = qc.cancelled() => {
                    rollback_autocommit_or_mark_failed(&mut session, false).await;
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "57014".to_string(),
                        "canceling statement due to user request".to_string(),
                    ))));
                }
            }
        } else {
            exec_future.await
        };

        match exec_results {
            Ok(results) => {
                session.record_command_complete();
                let resp = send_notices_and_get_last_response_with_format(
                    client,
                    session.show_setting_value("client_min_messages"),
                    results,
                    &portal.result_column_format,
                    parse_bytea_output_from_session(&session),
                )
                .await;
                Ok(resp?)
            }
            Err(e) => {
                // Drain pending notices (e.g., SET LOCAL warning before reserved-GUC error)
                for (severity, sqlstate, message) in session.drain_pending_notices() {
                    if client_allows_message(
                        session.show_setting_value("client_min_messages").as_deref(),
                        &severity,
                    ) {
                        let notice =
                            NoticeResponse::from(ErrorInfo::new(severity, sqlstate, message));
                        client
                            .send(PgWireBackendMessage::NoticeResponse(notice))
                            .await?;
                    }
                }
                let sqlstate = sqlstate_for_executor_error(&e);
                let pg_msg = pg_error_message(&e, sqlstate);
                error!(sqlstate, "Extended query execution error: {}", pg_msg);
                debug!("Extended query execution error detail: {}", e);
                Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    sqlstate.to_string(),
                    pg_msg,
                ))))
            }
        }
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        stmt: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let prepared = &stmt.statement;

        // If on_parse produced analyzed IR, use the stored schema directly.
        if !matches!(prepared.exec, PreparedExec::RawSqlUtility) {
            let param_types = stmt.parameter_types.clone();
            let fields: Vec<FieldInfo> = prepared
                .output_schema
                .iter()
                .map(|(name, dt)| {
                    FieldInfo::new(
                        name.to_string(),
                        None,
                        None,
                        datatype_to_pgtype(Some(dt)),
                        FieldFormat::Text,
                    )
                })
                .collect();
            return Ok(DescribeStatementResponse::new(param_types, fields));
        }

        // RawSqlUtility: use static utility schema (no heuristic inference).
        let param_types = stmt.parameter_types.clone();
        let fields = utility_describe_fields(&prepared.sql);
        Ok(DescribeStatementResponse::new(param_types, fields))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let prepared = &portal.statement.statement;

        // If on_parse produced analyzed IR, use the stored schema directly.
        if !matches!(prepared.exec, PreparedExec::RawSqlUtility) {
            let fields: Vec<FieldInfo> = prepared
                .output_schema
                .iter()
                .enumerate()
                .map(|(i, (name, dt))| {
                    let pg_type = datatype_to_pgtype(Some(dt));
                    let requested = portal.result_column_format.format_for(i);
                    let format = effective_result_format(&pg_type, requested);
                    FieldInfo::new(name.to_string(), None, None, pg_type, format)
                })
                .collect();
            return Ok(DescribePortalResponse::new(fields));
        }

        // RawSqlUtility: use static utility schema
        let fields = utility_describe_fields(&prepared.sql);
        let fields: Vec<FieldInfo> = fields
            .into_iter()
            .enumerate()
            .map(|(i, f)| {
                let requested = portal.result_column_format.format_for(i);
                let format = effective_result_format(f.datatype(), requested);
                FieldInfo::new(
                    f.name().to_string(),
                    f.table_id(),
                    f.column_id(),
                    f.datatype().clone(),
                    format,
                )
            })
            .collect();

        Ok(DescribePortalResponse::new(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_control_detected() {
        // All transaction-control variants must be recognized.
        assert!(is_transaction_control("BEGIN"));
        assert!(is_transaction_control("begin"));
        assert!(is_transaction_control("START TRANSACTION"));
        assert!(is_transaction_control("COMMIT"));
        assert!(is_transaction_control("END"));
        assert!(is_transaction_control("ROLLBACK"));
        assert!(is_transaction_control("SAVEPOINT sp1"));
        assert!(is_transaction_control("ROLLBACK TO SAVEPOINT sp1"));
        assert!(is_transaction_control("RELEASE SAVEPOINT sp1"));
        assert!(is_transaction_control(
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"
        ));
    }

    #[test]
    fn data_statements_not_transaction_control() {
        assert!(!is_transaction_control("SELECT 1"));
        assert!(!is_transaction_control("INSERT INTO t VALUES (1)"));
        assert!(!is_transaction_control("UPDATE t SET x = 1"));
        assert!(!is_transaction_control("DELETE FROM t"));
    }

    #[test]
    fn utility_statements_not_transaction_control() {
        assert!(!is_transaction_control("CREATE TABLE t (id int)"));
        assert!(!is_transaction_control("SET search_path TO public"));
        assert!(!is_transaction_control("SHOW server_version"));
    }

    /// Regression: multi-statement batches that mix transaction-control with
    /// non-transaction statements must NOT be exempt from rate limiting.
    #[test]
    fn mixed_batch_not_exempt() {
        // Transaction-control + data statement → rate-limited
        assert!(!is_transaction_control("BEGIN; SELECT 1"));
        assert!(!is_transaction_control("COMMIT; INSERT INTO t VALUES (1)"));
        assert!(!is_transaction_control("ROLLBACK; UPDATE t SET x = 1"));
        // Data statement + transaction-control → rate-limited
        assert!(!is_transaction_control("SELECT 1; COMMIT"));
        assert!(!is_transaction_control("INSERT INTO t VALUES (1); BEGIN"));
    }

    // ─── normalize_ident tests ───────────────────────────────────────

    #[test]
    fn normalize_ident_unquoted_lowercases() {
        assert_eq!(DynamicPgHandler::normalize_ident("Foo"), "foo");
        assert_eq!(DynamicPgHandler::normalize_ident("BAR"), "bar");
        assert_eq!(DynamicPgHandler::normalize_ident("baz"), "baz");
    }

    #[test]
    fn normalize_ident_quoted_preserves_case() {
        assert_eq!(DynamicPgHandler::normalize_ident("\"Foo\""), "Foo");
        assert_eq!(DynamicPgHandler::normalize_ident("\"BAR\""), "BAR");
    }

    #[test]
    fn normalize_ident_quoted_unescapes_double_quotes() {
        assert_eq!(DynamicPgHandler::normalize_ident("\"a\"\"b\""), "a\"b");
    }

    #[test]
    fn normalize_ident_trims_whitespace() {
        assert_eq!(DynamicPgHandler::normalize_ident("  foo  "), "foo");
        assert_eq!(DynamicPgHandler::normalize_ident(" \"Foo\" "), "Foo");
    }

    // ─── split_schema_table tests ─────────────────────────────────────

    #[test]
    fn split_schema_table_simple_table() {
        let (schema, table) = DynamicPgHandler::split_schema_table("users");
        assert_eq!(schema, None);
        assert_eq!(table, "users");
    }

    #[test]
    fn split_schema_table_simple_qualified() {
        let (schema, table) = DynamicPgHandler::split_schema_table("public.users");
        assert_eq!(schema, Some("public".to_string()));
        assert_eq!(table, "users");
    }

    #[test]
    fn split_schema_table_quoted_table() {
        let (schema, table) = DynamicPgHandler::split_schema_table("\"MyTable\"");
        assert_eq!(schema, None);
        assert_eq!(table, "MyTable");
    }

    #[test]
    fn split_schema_table_quoted_schema_and_table() {
        let (schema, table) = DynamicPgHandler::split_schema_table("\"MySchema\".\"MyTable\"");
        assert_eq!(schema, Some("MySchema".to_string()));
        assert_eq!(table, "MyTable");
    }

    #[test]
    fn split_schema_table_dot_inside_quoted_ident() {
        // The dot inside "my.schema" must NOT be treated as a separator.
        let (schema, table) = DynamicPgHandler::split_schema_table("\"my.schema\".\"t\"");
        assert_eq!(schema, Some("my.schema".to_string()));
        assert_eq!(table, "t");
    }

    #[test]
    fn split_schema_table_unquoted_uppercased_folds() {
        let (schema, table) = DynamicPgHandler::split_schema_table("PUBLIC.USERS");
        assert_eq!(schema, Some("public".to_string()));
        assert_eq!(table, "users");
    }

    #[test]
    fn split_schema_table_mixed_quoting() {
        let (schema, table) = DynamicPgHandler::split_schema_table("public.\"MyTable\"");
        assert_eq!(schema, Some("public".to_string()));
        assert_eq!(table, "MyTable");
    }

    #[test]
    fn split_schema_table_escaped_quotes_in_ident() {
        let (schema, table) = DynamicPgHandler::split_schema_table("\"a\"\"b\".\"c\"");
        assert_eq!(schema, Some("a\"b".to_string()));
        assert_eq!(table, "c");
    }

    /// Pure transaction-control batches remain exempt.
    #[test]
    fn pure_transaction_control_batch_exempt() {
        assert!(is_transaction_control("BEGIN; SAVEPOINT sp1"));
        assert!(is_transaction_control(
            "ROLLBACK TO SAVEPOINT sp1; ROLLBACK"
        ));
        assert!(is_transaction_control("COMMIT; BEGIN"));
    }

    /// Verify that transaction-control statements bypass rate limiting even
    /// when the token bucket is completely exhausted.
    #[test]
    fn transaction_control_bypasses_exhausted_rate_limiter() {
        use crate::pool::{TenantHandle, TikvClientPool};
        use std::sync::Arc;

        // Build a handler with a rate limiter that has 1 QPS capacity.
        let pool = Arc::new(TikvClientPool::new(vec![]));
        let server_config = crate::config::ServerConfig::default().shared();
        let handler = DynamicPgHandler::new_with_pool(
            pool,
            None,
            server_config,
            pgwire::tokio::CancellationToken::new(),
        );

        // Install a tenant handle whose rate limiter is immediately drained.
        let tenant = TenantHandle::new_with_rate_limit(1);
        tenant.rate_limiter().unwrap().drain();
        assert!(
            handler.tenant_handle.set(tenant.clone()).is_ok(),
            "tenant_handle already set"
        );

        // check_rate_limit should eventually reject once bucket is exhausted.
        // Some limiter implementations can permit one in-flight token after drain.
        let mut last_err = None;
        for _ in 0..8 {
            match handler.check_rate_limit() {
                Ok(()) => continue,
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
        let err = last_err.expect("expected rate limiter to reject after exhaustion");
        let msg = err.to_string();
        assert!(
            msg.contains("rate limit"),
            "expected rate-limit error: {msg}"
        );

        // Transaction-control SQL passes through the exemption guard:
        // `if !is_transaction_control(query) { self.check_rate_limit()?; }`
        // Since is_transaction_control returns true, check_rate_limit is never called.
        for sql in &[
            "BEGIN",
            "COMMIT",
            "END",
            "ROLLBACK",
            "ROLLBACK TO SAVEPOINT sp1",
            "SAVEPOINT sp1",
            "RELEASE SAVEPOINT sp1",
        ] {
            assert!(
                is_transaction_control(sql),
                "{sql} must be recognized as transaction control"
            );
        }

        // Non-transaction SQL is still blocked.
        assert!(!is_transaction_control("SELECT 1"));
        tenant.rate_limiter().unwrap().drain();
        let mut blocked = false;
        for _ in 0..8 {
            if let Err(err) = handler.check_rate_limit() {
                assert!(
                    err.to_string().contains("rate limit"),
                    "SELECT should still be rate-limited"
                );
                blocked = true;
                break;
            }
        }
        assert!(blocked, "expected non-transaction SQL to be rate-limited");
    }

    /// Verify that check_principal_concurrency rejects when the limit is reached.
    #[test]
    fn concurrency_guard_rejects_at_limit() {
        use crate::pool::TenantHandle;

        let pool = Arc::new(crate::pool::TikvClientPool::new(vec![]));
        let server_config = crate::config::ServerConfig::default().shared();
        let handler = DynamicPgHandler::new_with_pool(
            pool,
            None,
            server_config,
            pgwire::tokio::CancellationToken::new(),
        );

        // Set up tenant with concurrency limit of 1.
        let tenant = TenantHandle::new_with_all_limits(0, 0, 1);
        assert!(handler.tenant_handle.set(tenant).is_ok());

        // Cache a principal identity (simulating post-authentication state).
        let _ = handler.principal_identity.set("test_user".to_string());

        // First acquire should succeed.
        let guard1 = handler.check_principal_concurrency().unwrap();
        assert!(guard1.is_some(), "first query should acquire a slot");

        // Second acquire should be rejected (limit=1).
        let err = handler.check_principal_concurrency().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("too many concurrent queries"),
            "expected concurrency rejection, got: {msg}"
        );
        assert!(msg.contains("53300"), "expected SQLSTATE 53300, got: {msg}");

        // After dropping the guard, next acquire should succeed.
        drop(guard1);
        let guard2 = handler.check_principal_concurrency().unwrap();
        assert!(
            guard2.is_some(),
            "should acquire slot after guard is dropped"
        );
    }

    /// Verify that transaction-control statements bypass concurrency limits.
    #[test]
    fn transaction_control_bypasses_concurrency_limit() {
        // Transaction-control exemption works at the do_query level via
        // is_transaction_control(). When it returns true, check_principal_concurrency
        // is never called. This test verifies the structural property.
        for sql in &[
            "BEGIN",
            "COMMIT",
            "END",
            "ROLLBACK",
            "ROLLBACK TO SAVEPOINT sp1",
            "SAVEPOINT sp1",
            "RELEASE SAVEPOINT sp1",
        ] {
            assert!(
                is_transaction_control(sql),
                "{sql} must be recognized as transaction control (exempt from concurrency limit)"
            );
        }

        // Non-transaction SQL is NOT exempt.
        assert!(!is_transaction_control("SELECT 1"));
        assert!(!is_transaction_control("INSERT INTO t VALUES (1)"));
        assert!(!is_transaction_control("SET statement_timeout = 1000"));
    }

    /// Verify that concurrency check returns None (disabled) when limit is 0.
    #[test]
    fn concurrency_guard_disabled_when_limit_zero() {
        use crate::pool::TenantHandle;

        let pool = Arc::new(crate::pool::TikvClientPool::new(vec![]));
        let server_config = crate::config::ServerConfig::default().shared();
        let handler = DynamicPgHandler::new_with_pool(
            pool,
            None,
            server_config,
            pgwire::tokio::CancellationToken::new(),
        );

        // Concurrency limit = 0 means disabled.
        let tenant = TenantHandle::new_with_all_limits(0, 0, 0);
        assert!(handler.tenant_handle.set(tenant).is_ok());
        let _ = handler.principal_identity.set("test_user".to_string());

        // Should return None (no guard needed), not an error.
        let result = handler.check_principal_concurrency().unwrap();
        assert!(result.is_none(), "limit=0 should disable concurrency check");
    }

    /// Verify that check_admission_budget rejects when the token bucket is exhausted.
    #[test]
    fn admission_budget_rejects_when_exhausted() {
        use crate::pool::TokenBucket;

        let pool = Arc::new(crate::pool::TikvClientPool::new(vec![]));
        let server_config = crate::config::ServerConfig::default().shared();
        let handler = DynamicPgHandler::new_with_pool(
            pool,
            None,
            server_config,
            pgwire::tokio::CancellationToken::new(),
        );
        let _ = handler.principal_identity.set("test_user".to_string());

        // Set up a budget bucket with burst=2 (only 2 queries allowed).
        let bucket = Arc::new(TokenBucket::new_with_burst(1, 2));
        let _ = handler.admission_budget.set(bucket);

        // First two should succeed.
        assert!(handler.check_admission_budget().is_ok());
        assert!(handler.check_admission_budget().is_ok());

        // Third should be rejected.
        let err = handler.check_admission_budget().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("admission budget exhausted"),
            "expected budget rejection, got: {msg}"
        );
        assert!(msg.contains("53300"), "expected SQLSTATE 53300, got: {msg}");
    }

    /// Verify that connections without admission budget (direct pgwire) are not checked.
    #[test]
    fn admission_budget_skipped_when_not_set() {
        let pool = Arc::new(crate::pool::TikvClientPool::new(vec![]));
        let server_config = crate::config::ServerConfig::default().shared();
        let handler = DynamicPgHandler::new_with_pool(
            pool,
            None,
            server_config,
            pgwire::tokio::CancellationToken::new(),
        );

        // No admission_budget set — should always succeed.
        assert!(handler.check_admission_budget().is_ok());
        assert!(handler.check_admission_budget().is_ok());
    }

    /// Verify that multiple sessions sharing the same budget owner share one bucket.
    #[test]
    fn admission_budget_shared_across_sessions() {
        use crate::pool::{admission_budget_registry, AdmissionBudgetParams};

        let params = AdmissionBudgetParams {
            owner_id: format!("test_shared_{}", std::process::id()),
            rps: 3,
            burst: 3,
        };

        let pool = Arc::new(crate::pool::TikvClientPool::new(vec![]));
        let server_config = crate::config::ServerConfig::default().shared();

        // Simulate two connections for the same budget owner.
        let handler1 = DynamicPgHandler::new_with_pool(
            pool.clone(),
            None,
            server_config.clone(),
            pgwire::tokio::CancellationToken::new(),
        );
        let handler2 = DynamicPgHandler::new_with_pool(
            pool,
            None,
            server_config,
            pgwire::tokio::CancellationToken::new(),
        );

        let bucket = admission_budget_registry().get_or_create(&params);
        let _ = handler1.admission_budget.set(bucket.clone());
        let _ = handler2
            .admission_budget
            .set(admission_budget_registry().get_or_create(&params));

        // Consume 2 tokens via handler1.
        assert!(handler1.check_admission_budget().is_ok());
        assert!(handler1.check_admission_budget().is_ok());

        // Handler2 should only have 1 token left (shared bucket).
        assert!(handler2.check_admission_budget().is_ok());
        assert!(
            handler2.check_admission_budget().is_err(),
            "shared bucket should be exhausted"
        );
    }

    /// Verify that budget_max_concurrent tightens the server's concurrency limit.
    #[test]
    fn budget_max_concurrent_tightens_concurrency() {
        use crate::pool::TenantHandle;

        let pool = Arc::new(crate::pool::TikvClientPool::new(vec![]));
        let server_config = crate::config::ServerConfig::default().shared();
        let handler = DynamicPgHandler::new_with_pool(
            pool,
            None,
            server_config,
            pgwire::tokio::CancellationToken::new(),
        );

        // Server limit = 5, but budget claim tightens to 2.
        let tenant = TenantHandle::new_with_all_limits(0, 0, 5);
        assert!(handler.tenant_handle.set(tenant).is_ok());
        let _ = handler.principal_identity.set("test_user".to_string());
        let _ = handler.budget_max_concurrent.set(2);

        // First two should succeed (budget claim limit = 2).
        let guard1 = handler.check_principal_concurrency().unwrap();
        assert!(guard1.is_some());
        let guard2 = handler.check_principal_concurrency().unwrap();
        assert!(guard2.is_some());

        // Third should be rejected at the tighter claim limit.
        let err = handler.check_principal_concurrency().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("too many concurrent queries"),
            "expected rejection at tightened limit, got: {msg}"
        );

        // After dropping a guard, next should succeed.
        drop(guard1);
        let guard3 = handler.check_principal_concurrency().unwrap();
        assert!(guard3.is_some());
    }

    /// Verify that begin_query_tracking stores the child cancel token and
    /// cancel_query via registry makes it visible to query_cancel_token().
    #[test]
    fn cancel_query_token_plumbing_simple_path() {
        let pool = Arc::new(crate::pool::TikvClientPool::new(vec![]));
        let server_config = crate::config::ServerConfig::default().shared();
        let cancel_token = CancellationToken::new();
        let handler =
            DynamicPgHandler::new_with_pool(pool, None, server_config, cancel_token.clone());

        // Register a session in the global registry.
        let registry = crate::admin::global_session_registry();
        let info = crate::admin::session_registry::SessionInfo::new(
            handler.connection_id,
            "test-tenant".to_owned(),
            "test-user".to_owned(),
            "testdb".to_owned(),
            "127.0.0.1:5432".to_owned(),
            cancel_token.clone(),
        );
        registry.register(info);
        if let Some(session_info) = registry.get_session(handler.connection_id) {
            let _ = handler.session_info.set(session_info);
        }

        // Before begin_query_tracking: no active cancel token.
        assert!(
            handler.query_cancel_token().is_none(),
            "no cancel token before query starts"
        );

        // Begin query tracking — stores child token.
        handler.begin_query_tracking("SELECT pg_sleep(60)");
        let token = handler
            .query_cancel_token()
            .expect("cancel token must exist after begin_query_tracking");
        assert!(
            !token.is_cancelled(),
            "token should not be cancelled initially"
        );

        // Admin cancel_query via registry cancels the child token.
        registry
            .cancel_query(handler.connection_id)
            .expect("cancel_query should succeed");
        assert!(
            token.is_cancelled(),
            "query cancel token must be cancelled after registry cancel_query"
        );

        // Connection-level token is NOT cancelled (query cancel, not terminate).
        assert!(
            !cancel_token.is_cancelled(),
            "connection token must NOT be cancelled by cancel_query"
        );

        // end_query_tracking clears the token.
        handler.end_query_tracking(false, false);
        assert!(
            handler.query_cancel_token().is_none(),
            "cancel token must be cleared after end_query_tracking"
        );

        // Cleanup.
        registry.unregister(handler.connection_id);
    }

    /// Verify that a pre-cancelled query token triggers SQLSTATE 57014 in the
    /// tokio::select! pattern used by both simple and extended query paths.
    #[tokio::test]
    async fn cancelled_token_produces_57014() {
        let token = CancellationToken::new();
        token.cancel(); // pre-cancel

        // Simulate the tokio::select! pattern from do_query:
        let result: Result<&str, pgwire::error::PgWireError> = tokio::select! {
            biased;
            _ = std::future::pending::<()>() => {
                // This simulates executor.execute() — never completes.
                Ok("should not reach")
            }
            _ = token.cancelled() => {
                Err(pgwire::error::PgWireError::UserError(Box::new(
                    pgwire::error::ErrorInfo::new(
                        "ERROR".to_string(),
                        "57014".to_string(),
                        "canceling statement due to user request".to_string(),
                    ),
                )))
            }
        };

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("57014"), "expected SQLSTATE 57014, got: {msg}");
        assert!(
            msg.contains("canceling statement due to user request"),
            "expected cancel message, got: {msg}"
        );
    }

    /// Extended query path: begin_query_tracking is called inside do_query,
    /// producing the same cancel contract as simple queries.
    #[test]
    fn cancel_query_token_plumbing_extended_path() {
        let pool = Arc::new(crate::pool::TikvClientPool::new(vec![]));
        let server_config = crate::config::ServerConfig::default().shared();
        let cancel_token = CancellationToken::new();
        let handler =
            DynamicPgHandler::new_with_pool(pool, None, server_config, cancel_token.clone());

        // Register session.
        let registry = crate::admin::global_session_registry();
        let info = crate::admin::session_registry::SessionInfo::new(
            handler.connection_id,
            "test-tenant".to_owned(),
            "test-user".to_owned(),
            "testdb".to_owned(),
            "127.0.0.1:5432".to_owned(),
            cancel_token.clone(),
        );
        registry.register(info);
        if let Some(session_info) = registry.get_session(handler.connection_id) {
            let _ = handler.session_info.set(session_info);
        }

        // Simulate extended query do_query calling begin_query_tracking.
        handler.begin_query_tracking("SELECT * FROM users WHERE id = $1");
        let token = handler
            .query_cancel_token()
            .expect("cancel token must exist after begin_query_tracking");
        assert!(!token.is_cancelled());

        // Cancel via registry.
        registry
            .cancel_query(handler.connection_id)
            .expect("cancel_query should succeed");
        assert!(
            token.is_cancelled(),
            "extended query cancel token must be cancelled"
        );
        assert!(
            !cancel_token.is_cancelled(),
            "connection must stay alive after query cancel"
        );

        // After end_query_tracking, a new query starts fresh.
        handler.end_query_tracking(false, false);
        handler.begin_query_tracking("SELECT 1");
        let new_token = handler.query_cancel_token().unwrap();
        assert!(
            !new_token.is_cancelled(),
            "new query should have a fresh cancel token"
        );
        handler.end_query_tracking(false, false);

        // Cleanup.
        registry.unregister(handler.connection_id);
    }
}
