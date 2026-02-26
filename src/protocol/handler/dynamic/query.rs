//! Simple-query and extended-query protocol handling for [`DynamicPgHandler`].
//!
//! Contains `is_data_statement`, `reject_unanalyzed_if_needed`,
//! `utility_describe_fields`, `merge_parameter_types`, and the
//! [`SimpleQueryHandler`] / [`ExtendedQueryHandler`] trait implementations.

use super::DynamicPgHandler;
use crate::auth::Privilege;
use crate::model::DataType;
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
use sqlparser::ast::Statement;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, error, warn};

use super::super::encode::pgtype_to_datatype;
use super::super::encode::{datatype_to_pgtype, effective_result_format, result_to_response};
use super::super::errors::{
    ambiguous_column_error_with_position, in_failed_sql_transaction_pgwire_error,
    sqlstate_for_executor_error,
};
use super::super::params::{count_sql_parameters, decode_parameters};
use super::super::portal::{on_execute_with_tx_status_fix, on_query_with_tx_status_fix};
use super::super::prepared::{PreparedExec, PreparedStatement};
use super::super::resolve_copy_columns;
use super::super::{
    client_allows_message, rollback_autocommit_or_mark_failed,
    send_notices_and_get_last_response_with_format, CopyContext,
};

/// Returns true for SELECT/INSERT/UPDATE/DELETE -- statements that require
/// Analyzer output for correct Describe schema.  Uses parse_sql for
/// precise AST classification (handles SELECT\n, WITH\t, etc.).
pub(in crate::protocol::handler) fn is_data_statement(sql: &str) -> bool {
    match crate::sql::parse_sql(sql) {
        Ok(stmts) if !stmts.is_empty() => matches!(
            &stmts[0],
            Statement::Query(_)
                | Statement::Insert { .. }
                | Statement::Update { .. }
                | Statement::Delete { .. }
        ),
        _ => false, // unparseable -> accepted by should_accept_sql_without_sqlparser -> utility
    }
}

/// Returns true for transaction-control statements that must never be
/// rate-limited: BEGIN, COMMIT, END, ROLLBACK, ROLLBACK TO SAVEPOINT,
/// SAVEPOINT, RELEASE SAVEPOINT, SET TRANSACTION.  Blocking these would
/// prevent transaction cleanup and violate PostgreSQL recovery semantics.
pub(in crate::protocol::handler) fn is_transaction_control(sql: &str) -> bool {
    match crate::sql::parse_sql(sql) {
        Ok(stmts) if !stmts.is_empty() => stmts.iter().all(|s| {
            matches!(
                s,
                Statement::StartTransaction { .. }
                    | Statement::Commit { .. }
                    | Statement::Rollback { .. }
                    | Statement::Savepoint { .. }
                    | Statement::ReleaseSavepoint { .. }
                    | Statement::SetTransaction { .. }
            )
        }),
        _ => false,
    }
}

/// Reject RawSqlUtility that should have been analyzed.
/// Check order: data statement first (XX000 with infra failure reason),
/// then param_count > 0 (42P02 for utility + params).
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
        Some(PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".into(),
            sqlstate_for_executor_error(&err).to_string(),
            err.to_string(),
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
        on_query_with_tx_status_fix(self, client, query).await
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
        if !is_transaction_control(query) {
            self.check_rate_limit()?;
        }

        let state = self.auth();
        let executor = &state.executor;

        #[cfg(feature = "parquet")]
        if let Some(result) = self.try_handle_copy_from_fs9(client, query).await? {
            return Ok(result);
        }

        #[cfg(feature = "parquet")]
        if let Some(result) = self.try_handle_copy_from_parquet(client, query).await? {
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

        if let Some((table_name, columns)) = DynamicPgHandler::parse_copy_command(query) {
            debug!(
                "COPY FROM STDIN: table={}, columns={:?}",
                table_name, columns
            );

            let (resolved_table, resolved_columns, column_types, col_count, started_txn, qctx) = {
                let mut session = state.session.lock().await;

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
                let (resolved_table, schema) = {
                    let txn = session.get_mut_txn().ok_or_else(|| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "XX000".to_string(),
                            "No transaction".to_string(),
                        )))
                    })?;

                    let strip_quotes = |s: &str| -> String { s.trim_matches('"').to_string() };
                    let (schema_opt, table_ident) = if table_name.contains('.') {
                        let parts: Vec<&str> = table_name.splitn(2, '.').collect();
                        if parts.len() == 2 {
                            (
                                Some(strip_quotes(parts[0]).to_lowercase()),
                                strip_quotes(parts[1]).to_lowercase(),
                            )
                        } else {
                            (None, strip_quotes(&table_name).to_lowercase())
                        }
                    } else {
                        (None, strip_quotes(&table_name).to_lowercase())
                    };

                    if let Some(schema_ident) = schema_opt {
                        let resolved_table = format!("{}.{}", schema_ident, table_ident);
                        match executor
                            .store()
                            .get_schema(txn, db_id, &resolved_table)
                            .await
                        {
                            Ok(Some(schema)) => (resolved_table, schema),
                            Ok(None) => {
                                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "42P01".to_string(),
                                    format!("relation \"{}\" does not exist", table_name),
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
                        let schemas: Vec<&str> = if search_path.is_empty() {
                            vec!["public"]
                        } else {
                            search_path.iter().map(|s| s.as_str()).collect()
                        };

                        let mut found: Option<(String, crate::model::TableSchema)> = None;
                        for schema_ident in schemas {
                            let resolved_table = format!("{}.{}", schema_ident, table_ident);
                            match executor
                                .store()
                                .get_schema(txn, db_id, &resolved_table)
                                .await
                            {
                                Ok(Some(schema)) => {
                                    found = Some((resolved_table, schema));
                                    break;
                                }
                                Ok(None) => continue,
                                Err(e) => {
                                    rollback_autocommit_or_mark_failed(&mut session, started_txn)
                                        .await;
                                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                        "ERROR".to_string(),
                                        "XX000".to_string(),
                                        e.to_string(),
                                    ))));
                                }
                            }
                        }

                        match found {
                            Some(found) => found,
                            None => {
                                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "42P01".to_string(),
                                    format!("relation \"{}\" does not exist", table_name),
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
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        sqlstate_for_executor_error(&e).to_string(),
                        e.to_string(),
                    ))));
                }

                let (resolved_columns, column_types) =
                    match resolve_copy_columns(&schema, &columns, &table_name) {
                        Ok(resolved) => resolved,
                        Err(e) => {
                            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                            return Err(e);
                        }
                    };

                let col_count = resolved_columns.len();
                (
                    resolved_table,
                    resolved_columns,
                    column_types,
                    col_count,
                    started_txn,
                    qctx,
                )
            };

            let mut ctx = self.copy_context.lock().await;
            *ctx = Some(CopyContext {
                table_name: resolved_table,
                columns: resolved_columns,
                column_types,
                query_context: qctx,
                line_buffer: Vec::new(),
                row_count: 0,
                started_txn,
                reached_end_marker: false,
            });

            let column_formats: Vec<i16> = vec![0; col_count];
            return Ok(vec![Response::CopyIn(
                pgwire::api::results::CopyResponse::new(0, col_count, column_formats),
            )]);
        }

        let mut session = state.session.lock().await;

        if let Err(e) = session.check_idle_in_transaction_timeout() {
            let _ = session.rollback().await;
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                e.sqlstate().to_string(),
                e.to_string(),
            ))));
        }

        match executor.execute(&mut session, query).await {
            Ok(results) => {
                session.record_command_complete();
                let mut responses: Vec<Response<'a>> = Vec::new();
                for result in results.into_vec() {
                    if let ExecuteResult::Notice { message, severity } = result {
                        if client_allows_message(
                            session.show_setting_value("client_min_messages").as_deref(),
                            &severity,
                        ) {
                            let notice = NoticeResponse::from(ErrorInfo::new(
                                severity,
                                "00000".to_string(),
                                message,
                            ));
                            client
                                .send(PgWireBackendMessage::NoticeResponse(notice))
                                .await?;
                        }
                        continue;
                    }
                    responses.push(result_to_response(result)?);
                }
                Ok(responses)
            }
            Err(e) => {
                error!("Query execution error: {}", e);
                let mut error_info = ErrorInfo::new(
                    "ERROR".to_string(),
                    sqlstate_for_executor_error(&e).to_string(),
                    e.to_string(),
                );
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
        // 1. Parse via QueryParser (preserves multi-statement rejection guard)
        let parser = self.query_parser();
        let mut stored = StoredStatement::parse(&message, parser).await?;

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

        // 4. Analyze for frozen execution IR
        let state = self.auth();
        let executor = &state.executor;
        let store = executor.store();

        // Brief session lock to read db_id + search_path
        let (db_id, search_path) = {
            let session = state.session.lock().await;
            (
                session.current_database_id(),
                session.search_path().to_vec(),
            )
        };

        // Temporary read-only transaction for catalog access
        match store.begin().await {
            Ok(mut txn) => {
                match executor
                    .analyze_for_prepared(
                        &mut txn,
                        db_id,
                        &search_path,
                        &stored.statement.sql,
                        param_count,
                        &client_oids,
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
                            } => {
                                let required_privileges = base_table_names
                                    .into_iter()
                                    .map(|t| (t, Privilege::Select))
                                    .collect();
                                stored.parameter_types =
                                    merge_parameter_types(&stored.parameter_types, &param_types);
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
                                };
                            }
                            PreparedAnalysis::Dml {
                                analyzed,
                                output_schema,
                                param_types,
                                table_versions,
                            } => {
                                let required_privileges =
                                    PreparedStatement::compute_privileges(&analyzed, &[]);
                                stored.parameter_types =
                                    merge_parameter_types(&stored.parameter_types, &param_types);
                                stored.statement = PreparedStatement {
                                    sql: stored.statement.sql,
                                    exec: PreparedExec::AnalyzedDml {
                                        analyzed,
                                        required_privileges,
                                    },
                                    output_schema,
                                    param_data_types: param_types,
                                    table_versions,
                                };
                            }
                            PreparedAnalysis::Utility => {
                                // Keep RawSqlUtility
                            }
                        }
                    }
                    Err(e) if param_count > 0 || is_data_statement(&stored.statement.sql) => {
                        // Data statements and parameterized statements must be analyzed.
                        let _ = txn.rollback().await;
                        return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            sqlstate_for_executor_error(&e).to_string(),
                            e.to_string(),
                        ))));
                    }
                    Err(_) => {
                        // Utility, no params -- keep RawSqlUtility
                    }
                }
                let _ = txn.rollback().await; // read-only, discard
            }
            Err(e) => {
                // Can't begin txn -- reject data/parameterized SQL
                if let Some(err) = reject_unanalyzed_if_needed(
                    &stored.statement.sql,
                    param_count,
                    &format!("failed to begin catalog transaction: {}", e),
                ) {
                    return Err(err);
                }
                // Utility, no params -- keep RawSqlUtility
            }
        }

        // 5. Store immutable
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
        on_execute_with_tx_status_fix(self, &self.suspended_portals, client, message).await
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

        // Transaction-control statements bypass rate limiting (see simple-query path).
        if !is_transaction_control(&prepared.sql) {
            self.check_rate_limit()?;
        }

        let state = self.auth();
        let executor = &state.executor;

        debug!("Extended query: {}", prepared.sql);

        let mut session = state.session.lock().await;

        if let Err(e) = session.check_idle_in_transaction_timeout() {
            let _ = session.rollback().await;
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                e.sqlstate().to_string(),
                e.to_string(),
            ))));
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
                ))
            }
        };
        let exec_results = exec_future.await;

        match exec_results {
            Ok(results) => {
                session.record_command_complete();
                let resp = send_notices_and_get_last_response_with_format(
                    client,
                    session.show_setting_value("client_min_messages"),
                    results,
                    &portal.result_column_format,
                )
                .await;
                Ok(resp?)
            }
            Err(e) => {
                error!("Extended query execution error: {}", e);
                Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    sqlstate_for_executor_error(&e).to_string(),
                    e.to_string(),
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
            handler.tenant_handle.set(tenant).is_ok(),
            "tenant_handle already set"
        );

        // check_rate_limit must reject when the bucket is empty.
        let err = handler.check_rate_limit().unwrap_err();
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
        let err = handler.check_rate_limit().unwrap_err();
        assert!(
            err.to_string().contains("rate limit"),
            "SELECT should still be rate-limited"
        );
    }
}
