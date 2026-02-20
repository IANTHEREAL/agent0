use super::copy::copy_row_column_mismatch_error;
use super::encode::pgtype_to_datatype;
use super::encode::{datatype_to_pgtype, effective_result_format, result_to_response};
use super::errors::{
    ambiguous_column_error_with_position, in_failed_sql_transaction_pgwire_error,
    sqlstate_for_executor_error,
};
use super::params::{count_sql_parameters, decode_parameters};
use super::portal::{
    on_execute_with_tx_status_fix, on_query_with_tx_status_fix, SuspendedPortalState,
};
use super::prepared::{PreparedExec, PreparedStatement};
use super::query_parser::strip_leading_whitespace_and_comments;
use super::tenant::parse_tenant_username;
use super::{
    client_allows_notice, parse_startup_options, resolve_copy_columns,
    rollback_autocommit_or_mark_failed, send_notices_and_get_last_response_with_format,
    CopyContext, PgServerParameterProvider, TipgQueryParser, CONNECTION_ID_COUNTER,
    METADATA_ACTUAL_USER, METADATA_AUTH_IS_SUPERUSER, METADATA_KEYSPACE,
};
use crate::auth::{AuthManager, Privilege};
use crate::config;
use crate::config::SharedServerConfig;
use crate::observability;
use crate::pool::{TenantHandle, TikvClientPool};
use crate::sql::error::SqlError;
use crate::sql::executor::core::prepared_analysis::PreparedAnalysis;
use crate::sql::{ExecuteResult, Executor, Session};
use crate::storage::TikvStore;
use crate::types::{DataType, TableSchema, Value};
use anyhow::Context as _;
use async_trait::async_trait;
use futures::{Sink, SinkExt};
use pgwire::api::auth::StartupHandler;
use pgwire::api::copy::CopyHandler;
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    CopyResponse, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    Response,
};
use pgwire::api::stmt::StoredStatement;
use pgwire::api::store::PortalStore;
use pgwire::api::{
    ClientInfo, ClientPortalStore, NoopErrorHandler, PgWireConnectionState, PgWireServerHandlers,
    Type, METADATA_DATABASE, METADATA_USER,
};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::copy::{CopyData, CopyDone, CopyFail};
use pgwire::messages::response::{CommandComplete, NoticeResponse};
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use sqlparser::ast::{CopySource, CopyTarget, Ident, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, error, info, warn};

/// Returns true for SELECT/INSERT/UPDATE/DELETE — statements that require
/// Analyzer output for correct Describe schema.  Uses parse_sql for
/// precise AST classification (handles SELECT\n, WITH\t, etc.).
pub(super) fn is_data_statement(sql: &str) -> bool {
    match crate::sql::parse_sql(sql) {
        Ok(stmts) if !stmts.is_empty() => matches!(
            &stmts[0],
            Statement::Query(_)
                | Statement::Insert { .. }
                | Statement::Update { .. }
                | Statement::Delete { .. }
        ),
        _ => false, // unparseable → accepted by should_accept_sql_without_sqlparser → utility
    }
}

/// Reject RawSqlUtility that should have been analyzed.
/// Check order: data statement first (XX000 with infra failure reason),
/// then param_count > 0 (42P02 for utility + params).
pub(super) fn reject_unanalyzed_if_needed(
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
        None // genuine utility, no params — safe as RawSqlUtility
    }
}

/// Static Describe schema for known row-producing utility statements.
/// Uses AST classification — no heuristic inference.
pub(super) fn utility_describe_fields(sql: &str) -> Vec<FieldInfo> {
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
            vec![FieldInfo::new(
                name,
                None,
                None,
                Type::TEXT,
                FieldFormat::Text,
            )]
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
        _ => vec![], // DDL/SET/etc — no rows
    }
}

pub struct DynamicPgHandler {
    client_pool: Option<Arc<TikvClientPool>>,
    pd_endpoints: Vec<String>,
    default_keyspace: Option<String>,
    executor: OnceCell<Arc<Executor>>,
    session: Mutex<Option<Session>>,
    connection_guard: OnceCell<observability::ConnectionGuard>,
    tenant_handle: OnceCell<TenantHandle>,
    copy_context: Mutex<Option<CopyContext>>,
    suspended_portals: Mutex<HashMap<String, SuspendedPortalState>>,
    query_parser: Arc<TipgQueryParser>,
    connection_id: i32,
    server_config: SharedServerConfig,
}

impl DynamicPgHandler {
    pub fn new_with_pool(
        client_pool: Arc<TikvClientPool>,
        default_keyspace: Option<String>,
        server_config: SharedServerConfig,
    ) -> Self {
        Self {
            client_pool: Some(client_pool),
            pd_endpoints: Vec::new(),
            default_keyspace,
            executor: OnceCell::new(),
            session: Mutex::new(None),
            connection_guard: OnceCell::new(),
            tenant_handle: OnceCell::new(),
            copy_context: Mutex::new(None),
            suspended_portals: Mutex::new(HashMap::new()),
            query_parser: Arc::new(TipgQueryParser::new()),
            connection_id: CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            server_config,
        }
    }

    async fn init_executor(
        &self,
        keyspace: Option<String>,
        username: Option<String>,
        is_superuser: bool,
        database: String,
    ) -> PgWireResult<()> {
        let fatal_internal = |message: String| -> PgWireError {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_owned(),
                "XX000".to_owned(),
                message,
            )))
        };

        let effective_keyspace = keyspace
            .or_else(|| self.default_keyspace.clone())
            .unwrap_or_else(|| "default".to_string());

        let tenant_obs = observability::registry().tenant(&effective_keyspace);
        if self.connection_guard.get().is_none() {
            let _ = self.connection_guard.set(tenant_obs.connection_open());
        }

        let (store, trigger_cache, stats_cache) = if let Some(pool) = &self.client_pool {
            let handle = pool
                .acquire(Some(effective_keyspace.clone()))
                .await
                .map_err(|e| fatal_internal(format!("Failed to get client from pool: {}", e)))?;
            let s = handle.store().clone();
            let tc = handle.trigger_cache().clone();
            let sc = handle.stats_cache().clone();
            let _ = self.tenant_handle.set(handle);
            (s, tc, sc)
        } else {
            use crate::sql::stats::TableStatsCache;
            use crate::sql::triggers::TriggerBodyCache;
            let s = TikvStore::new_with_keyspace(
                self.pd_endpoints.clone(),
                Some(effective_keyspace.clone()),
            )
            .await
            .map_err(|e| fatal_internal(format!("Failed to connect to TiKV: {}", e)))?;
            (
                Arc::new(s),
                Arc::new(TriggerBodyCache::new()),
                Arc::new(TableStatsCache::new()),
            )
        };

        let executor = Arc::new(Executor::new(
            store.clone(),
            effective_keyspace.clone(),
            tenant_obs.clone(),
            trigger_cache,
            stats_cache,
        ));

        let database_name = database.trim();
        let database_name = if database_name.is_empty() {
            "postgres"
        } else {
            database_name
        };
        let database_name = database_name.to_ascii_lowercase();
        let (default_stmt_timeout, default_idle_txn_timeout) = {
            let cfg = self.server_config.read().unwrap();
            (
                cfg.statement_timeout_ms,
                cfg.idle_in_transaction_session_timeout_ms,
            )
        };

        let mut db_txn = store
            .begin()
            .await
            .map_err(|e| fatal_internal(e.to_string()))?;
        let database_id = match store
            .get_database_id(&mut db_txn, &database_name)
            .await
            .map_err(|e| fatal_internal(e.to_string()))?
        {
            Some(id) => id,
            None => {
                db_txn.rollback().await.ok();
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "FATAL".to_owned(),
                    "3D000".to_owned(),
                    format!("database \"{}\" does not exist", database_name),
                ))));
            }
        };
        db_txn.rollback().await.ok();

        let mut session = match username {
            Some(user) => Session::new_with_user_and_database(
                store,
                tenant_obs,
                user,
                is_superuser,
                self.connection_id,
                database_id,
                database_name,
                default_stmt_timeout,
                default_idle_txn_timeout,
            ),
            None => Session::new_with_database(
                store,
                tenant_obs,
                self.connection_id,
                database_id,
                database_name,
                default_stmt_timeout,
                default_idle_txn_timeout,
            ),
        };
        session.set_server_config(self.server_config.clone());

        let _ = self.executor.set(executor);

        let mut session_guard = self.session.lock().await;
        *session_guard = Some(session);

        debug!(
            "Initialized executor with keyspace: {:?}",
            effective_keyspace
        );
        Ok(())
    }

    fn get_executor(&self) -> Result<&Arc<Executor>, PgWireError> {
        self.executor.get().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "XX000".to_string(),
                "Executor not initialized - authentication required".to_string(),
            )))
        })
    }

    pub(super) fn parse_copy_command(query: &str) -> Option<(String, Vec<String>)> {
        let query = strip_leading_whitespace_and_comments(query)?;
        let query_upper = query.to_uppercase();
        // COPY FROM STDIN must start at statement start (after leading whitespace/comments).
        if !query_upper.starts_with("COPY")
            || !query_upper.contains("FROM")
            || !query_upper.contains("STDIN")
        {
            return None;
        }

        // Regex: COPY [schema.]table_name (col1, col2, ...) FROM stdin
        let re = regex::Regex::new(r"(?i)^COPY\s+(?:(\w+)\.)?(\w+)\s*\(([^)]+)\)\s+FROM\s+stdin")
            .ok()?;
        if let Some(caps) = re.captures(query) {
            let schema = caps.get(1).map(|m| m.as_str().to_string());
            let table = caps.get(2)?.as_str().to_string();
            let table_name = match schema {
                Some(s) => format!("{}.{}", s, table),
                None => table,
            };
            let columns_str = caps.get(3)?.as_str();
            let columns: Vec<String> = columns_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            return Some((table_name, columns));
        }

        // Regex: COPY [schema.]table_name FROM stdin (no column list)
        let re2 = regex::Regex::new(r"(?i)^COPY\s+(?:(\w+)\.)?(\w+)\s+FROM\s+stdin").ok()?;
        if let Some(caps) = re2.captures(query) {
            let schema = caps.get(1).map(|m| m.as_str().to_string());
            let table = caps.get(2)?.as_str().to_string();
            let table_name = match schema {
                Some(s) => format!("{}.{}", s, table),
                None => table,
            };
            return Some((table_name, vec![]));
        }

        None
    }

    pub(super) fn parse_copy_to_command(
        query: &str,
    ) -> Result<
        Option<(
            String,
            Vec<String>,
            crate::protocol::copy_format::CopyOptions,
        )>,
        ErrorInfo,
    > {
        fn unsupported_copy_to_stdout_syntax() -> ErrorInfo {
            ErrorInfo::new(
                "ERROR".to_string(),
                "0A000".to_string(),
                "Unsupported COPY TO STDOUT syntax. Supported: COPY [schema.]table [(col1, col2, ...)] TO STDOUT [WITH (options)]".to_string(),
            )
        }

        fn is_valid_unquoted_ident(ident: &str) -> bool {
            let mut chars = ident.chars();
            let Some(first) = chars.next() else {
                return false;
            };
            if first != '_' && !first.is_ascii_alphabetic() {
                return false;
            }
            chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        }

        let Some(query_trimmed) = strip_leading_whitespace_and_comments(query) else {
            return Ok(None);
        };

        match query_trimmed.get(..4) {
            Some(prefix) if prefix.eq_ignore_ascii_case("COPY") => {}
            _ => return Ok(None),
        }

        let dialect = PostgreSqlDialect {};
        let Ok(stmts) = Parser::parse_sql(&dialect, query_trimmed) else {
            return Ok(None);
        };
        let Some(stmt) = stmts.first() else {
            return Ok(None);
        };

        let Statement::Copy {
            source,
            to,
            target,
            options,
            legacy_options,
            values,
        } = stmt
        else {
            return Ok(None);
        };

        if !*to || !matches!(target, CopyTarget::Stdout) {
            return Ok(None);
        }

        if stmts.len() != 1 || !legacy_options.is_empty() || !values.is_empty() {
            return Err(unsupported_copy_to_stdout_syntax());
        }

        let copy_opts = crate::protocol::copy_format::CopyOptions::from_copy_options(options)
            .map_err(|e| ErrorInfo::new("ERROR".to_string(), "0A000".to_string(), e))?;

        let CopySource::Table {
            table_name,
            columns,
        } = source
        else {
            return Err(unsupported_copy_to_stdout_syntax());
        };

        let (schema_ident, table_ident) = match table_name.0.as_slice() {
            [table] => (None, table),
            [schema, table] => (Some(schema), table),
            _ => return Err(unsupported_copy_to_stdout_syntax()),
        };

        let validate_ident = |ident: &Ident| -> Result<(), ErrorInfo> {
            if ident.quote_style.is_some() {
                return Err(unsupported_copy_to_stdout_syntax());
            }
            if !is_valid_unquoted_ident(&ident.value) {
                return Err(ErrorInfo::new(
                    "ERROR".to_string(),
                    "42602".to_string(),
                    format!("Invalid identifier in COPY TO STDOUT: \"{}\"", ident.value),
                ));
            }
            Ok(())
        };

        if let Some(schema) = schema_ident {
            validate_ident(schema)?;
        }
        validate_ident(table_ident)?;
        for col in columns.iter() {
            validate_ident(col)?;
        }

        let table_name = match schema_ident {
            Some(schema) => format!("{}.{}", schema.value, table_ident.value),
            None => table_ident.value.clone(),
        };
        let columns = columns.iter().map(|c| c.value.clone()).collect();

        Ok(Some((table_name, columns, copy_opts)))
    }

    fn copy_out_response_from_select_result(
        result: ExecuteResult,
    ) -> Result<(CopyResponse, Vec<String>, Vec<crate::types::Row>), ErrorInfo> {
        match result {
            ExecuteResult::Select { columns, rows, .. } => {
                let col_count = columns.len();
                let column_formats: Vec<i16> = vec![0; col_count];
                Ok((
                    CopyResponse::new(0, col_count, column_formats),
                    columns,
                    rows,
                ))
            }
            _ => Err(ErrorInfo::new(
                "ERROR".to_string(),
                "0A000".to_string(),
                "COPY TO STDOUT is only supported for tables".to_string(),
            )),
        }
    }

    async fn handle_copy_to_stdout<'a, C>(
        &self,
        client: &mut C,
        table_name: &str,
        columns: &[String],
        copy_opts: &crate::protocol::copy_format::CopyOptions,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let executor = self.get_executor()?;

        let select_sql = if columns.is_empty() {
            format!("SELECT * FROM {}", table_name)
        } else {
            format!("SELECT {} FROM {}", columns.join(", "), table_name)
        };

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "XX000".to_string(),
                "Session not initialized".to_string(),
            )))
        })?;

        let result = executor
            .execute(session, &select_sql)
            .await
            .map(|r| r.last())
            .map_err(|e| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    e.to_string(),
                )))
            })?;

        let (copy_resp, col_names, rows) = Self::copy_out_response_from_select_result(result)
            .map_err(|e| PgWireError::UserError(Box::new(e)))?;

        drop(session_guard);

        pgwire::api::copy::send_copy_out_response(client, copy_resp).await?;

        let mut buf = Vec::with_capacity(4096);

        // Emit HEADER row if requested, encoding through format-specific logic
        // so column names containing delimiter/quote/newline are handled correctly.
        if copy_opts.header {
            let header_values: Vec<crate::types::Value> = col_names
                .iter()
                .map(|name| crate::types::Value::Text(name.clone()))
                .collect();
            crate::protocol::copy_format::encode_row_with_options(
                &header_values,
                &mut buf,
                copy_opts,
            );
            let data = pgwire::messages::copy::CopyData::new(bytes::Bytes::copy_from_slice(&buf));
            client.send(PgWireBackendMessage::CopyData(data)).await?;
        }

        for row in &rows {
            buf.clear();
            crate::protocol::copy_format::encode_row_with_options(&row.values, &mut buf, copy_opts);
            let data = pgwire::messages::copy::CopyData::new(bytes::Bytes::copy_from_slice(&buf));
            client.send(PgWireBackendMessage::CopyData(data)).await?;
        }

        let done = pgwire::messages::copy::CopyDone::new();
        client.send(PgWireBackendMessage::CopyDone(done)).await?;

        let complete =
            pgwire::messages::response::CommandComplete::new(format!("COPY {}", rows.len()));
        client
            .send(PgWireBackendMessage::CommandComplete(complete))
            .await?;

        Ok(vec![])
    }

    async fn authenticate_user(
        &self,
        keyspace: &Option<String>,
        username: &str,
        password: &str,
    ) -> Result<(bool, bool), anyhow::Error> {
        let effective_keyspace = keyspace
            .clone()
            .or_else(|| self.default_keyspace.clone())
            .or_else(|| Some("default".to_string()));

        let ks_name = effective_keyspace
            .clone()
            .unwrap_or_else(|| "default".to_string());

        let store = if let Some(pool) = &self.client_pool {
            match pool.get_client(effective_keyspace).await {
                Ok(s) => s,
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("does not exist") {
                        error!("Tenant '{}' does not exist (user: {})", ks_name, username);
                    } else {
                        error!("Failed to connect to TiKV for tenant '{}': {}", ks_name, e);
                    }
                    return Ok((false, false));
                }
            }
        } else {
            match TikvStore::new_with_keyspace(self.pd_endpoints.clone(), effective_keyspace).await
            {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    error!("Failed to connect to TiKV: {}", e);
                    return Ok((false, false));
                }
            }
        };

        let auth_manager = AuthManager::new();

        // Try bootstrap. Any failure must deny authentication.
        {
            let mut txn = store.begin().await.context("Failed to bootstrap auth")?;
            auth_manager
                .bootstrap(&mut txn)
                .await
                .context("Failed to bootstrap auth")?;
            txn.commit().await.context("Failed to bootstrap auth")?;
        }

        let mut txn = store.begin().await.context("Failed to begin transaction")?;

        match auth_manager
            .authenticate(&mut txn, username, password)
            .await
        {
            Ok(Some(user)) => {
                txn.commit().await.context("Failed to commit")?;
                Ok((true, user.is_superuser))
            }
            Ok(None) => {
                txn.rollback().await.ok();
                Ok((false, false))
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e.context("Authentication error"))
            }
        }
    }
}

/// Merge finalized analyzer types into wire types.
/// Client-specified non-UNKNOWN OIDs win (preserves INT2/FLOAT4 fidelity).
/// UNKNOWN slots filled from inferred DataType → Type.
fn merge_parameter_types(client_types: &[Type], finalized: &[DataType]) -> Vec<Type> {
    let mut result = Vec::with_capacity(finalized.len());
    for i in 0..finalized.len() {
        let client = client_types.get(i).cloned().unwrap_or(Type::UNKNOWN);
        if client != Type::UNKNOWN {
            result.push(client); // preserve client INT2/FLOAT4
        } else {
            result.push(datatype_to_pgtype(Some(&finalized[i])));
        }
    }
    result
}

fn copy_display_table_name(resolved_table: &str) -> &str {
    resolved_table.rsplit('.').next().unwrap_or(resolved_table)
}

fn copy_missing_data_error(
    resolved_table: &str,
    column_name: &str,
    line_no: usize,
    raw_line: &str,
) -> PgWireError {
    let table = copy_display_table_name(resolved_table);
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        "22P04".to_string(),
        format!(
            "missing data for column \"{}\"\nCONTEXT:  COPY {}, line {}: \"{}\"",
            column_name, table, line_no, raw_line
        ),
    )))
}

fn copy_value_parse_error(
    resolved_table: &str,
    line_no: usize,
    column_name: &str,
    raw_value: &str,
    err: &anyhow::Error,
) -> PgWireError {
    let table = copy_display_table_name(resolved_table);
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        sqlstate_for_executor_error(err).to_string(),
        format!(
            "{}\nCONTEXT:  COPY {}, line {}, column {}: \"{}\"",
            err, table, line_no, column_name, raw_value
        ),
    )))
}

fn parse_copy_text_line(
    executor: &Executor,
    resolved_table: &str,
    columns: &[String],
    column_types: &[Option<DataType>],
    line_no: usize,
    line_bytes: &[u8],
) -> PgWireResult<Vec<(String, Value)>> {
    let line = String::from_utf8_lossy(line_bytes);
    let values: Vec<&str> = line.split('\t').collect();

    if values.len() > columns.len() {
        return Err(copy_row_column_mismatch_error(values.len(), columns.len()));
    }

    let mut col_values: Vec<(String, Value)> = Vec::with_capacity(columns.len());
    for (idx, (col_name, col_type)) in columns.iter().zip(column_types.iter()).enumerate() {
        let Some(val) = values.get(idx).copied() else {
            return Err(copy_missing_data_error(
                resolved_table,
                col_name,
                line_no,
                line.as_ref(),
            ));
        };

        let value = if val == "\\N" {
            Value::Null
        } else if let Some(dt) = col_type.as_ref() {
            executor
                .parse_value_for_copy(val, dt)
                .map_err(|e| copy_value_parse_error(resolved_table, line_no, col_name, val, &e))?
        } else {
            Value::Text(val.to_string())
        };
        col_values.push((col_name.clone(), value));
    }

    Ok(col_values)
}

fn should_add_copy_insert_context(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<SqlError>(),
        Some(SqlError::UniqueViolation { .. })
    )
}

#[async_trait]
impl StartupHandler for DynamicPgHandler {
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        match message {
            PgWireFrontendMessage::Startup(ref startup) => {
                pgwire::api::auth::save_startup_parameters_to_metadata(client, startup);

                let raw_user = client
                    .metadata()
                    .get(METADATA_USER)
                    .cloned()
                    .filter(|u| !u.trim().is_empty())
                    .ok_or(PgWireError::UserNameRequired)?;

                let (keyspace, actual_user) = parse_tenant_username(&raw_user);

                if let Some(ks) = &keyspace {
                    client
                        .metadata_mut()
                        .insert(METADATA_KEYSPACE.to_string(), ks.clone());
                    debug!("Extracted keyspace '{}' from username '{}'", ks, raw_user);
                }
                client
                    .metadata_mut()
                    .insert(METADATA_ACTUAL_USER.to_string(), actual_user.clone());
                debug!("Actual user: {}", actual_user);

                client.set_state(PgWireConnectionState::AuthenticationInProgress);

                let require_tls = config::env_bool("PG_REQUIRE_TLS");
                let dev_mode = config::env_bool("PGTIKV_DEV");
                let insecure_mode = config::env_bool("PGTIKV_INSECURE");

                if require_tls && !client.is_secure() {
                    let error_info = ErrorInfo::new(
                        "FATAL".to_owned(),
                        "28000".to_owned(),
                        "TLS is required (PG_REQUIRE_TLS=1). Reconnect with sslmode=require and ensure server TLS is configured (PG_TLS_CERT/PG_TLS_KEY).".to_string(),
                    );
                    return Err(PgWireError::UserError(Box::new(error_info)));
                }

                if !client.is_secure() {
                    let peer_ip = client.socket_addr().ip();
                    let allow_cleartext = peer_ip.is_loopback() || dev_mode || insecure_mode;
                    if !allow_cleartext {
                        let error_info = ErrorInfo::new(
                            "FATAL".to_owned(),
                            "28000".to_owned(),
                            "Cleartext password authentication without TLS is disabled by default for non-loopback clients. Enable TLS (PG_TLS_CERT/PG_TLS_KEY) or explicitly opt into insecure mode (PGTIKV_DEV=1 or PGTIKV_INSECURE=1).".to_string(),
                        );
                        return Err(PgWireError::UserError(Box::new(error_info)));
                    }

                    if !peer_ip.is_loopback() && (dev_mode || insecure_mode) {
                        warn!(
                            "Allowing non-TLS cleartext auth for non-loopback connection from {} (PGTIKV_DEV={}, PGTIKV_INSECURE={})",
                            peer_ip, dev_mode, insecure_mode
                        );
                    }
                }
                client
                    .send(PgWireBackendMessage::Authentication(
                        Authentication::CleartextPassword,
                    ))
                    .await?;
            }
            PgWireFrontendMessage::PasswordMessageFamily(pwd) => {
                let pwd = pwd.into_password()?;
                let provided_password = pwd.password.clone();
                let keyspace = client.metadata().get(METADATA_KEYSPACE).cloned();
                let actual_user = client
                    .metadata()
                    .get(METADATA_ACTUAL_USER)
                    .cloned()
                    .ok_or(PgWireError::UserNameRequired)?;
                let database = client
                    .metadata()
                    .get(METADATA_DATABASE)
                    .cloned()
                    .unwrap_or_else(|| "postgres".to_string());

                let auth_result = self
                    .authenticate_user(&keyspace, &actual_user, &provided_password)
                    .await;

                match auth_result {
                    Ok((is_authenticated, is_superuser)) => {
                        if is_authenticated {
                            self.init_executor(
                                keyspace.clone(),
                                Some(actual_user.clone()),
                                is_superuser,
                                database,
                            )
                            .await?;

                            let mut session_guard = self.session.lock().await;
                            if let Some(session) = session_guard.as_mut() {
                                if let Some(options) = client.metadata().get("options") {
                                    for (key, value) in parse_startup_options(options) {
                                        if let Err(e) = session
                                            .set_known_setting(&key.to_ascii_lowercase(), value)
                                        {
                                            warn!("Failed to apply startup option {}: {}", key, e);
                                        }
                                    }
                                }

                                if let Some(app_name) = client.metadata().get("application_name") {
                                    if let Err(e) = session
                                        .set_known_setting("application_name", app_name.clone())
                                    {
                                        warn!(
                                            "Failed to apply application_name from startup: {}",
                                            e
                                        );
                                    }
                                }
                            }

                            client.metadata_mut().insert(
                                METADATA_AUTH_IS_SUPERUSER.to_string(),
                                if is_superuser { "on" } else { "off" }.to_string(),
                            );

                            pgwire::api::auth::finish_authentication(
                                client,
                                &PgServerParameterProvider,
                            )
                            .await?;
                            let peer = client.socket_addr();
                            info!(
                                "New connection from {}:{} user='{}' keyspace='{}'",
                                peer.ip(),
                                peer.port(),
                                actual_user,
                                keyspace.as_deref().unwrap_or("default"),
                            );
                        } else {
                            let error_info = ErrorInfo::new(
                                "FATAL".to_owned(),
                                "28P01".to_owned(),
                                format!(
                                    "Password authentication failed for user \"{}\"",
                                    actual_user
                                ),
                            );
                            return Err(PgWireError::UserError(Box::new(error_info)));
                        }
                    }
                    Err(e) => {
                        let sqlstate = sqlstate_for_executor_error(&e);
                        let error_info =
                            ErrorInfo::new("FATAL".to_owned(), sqlstate.to_owned(), e.to_string());
                        return Err(PgWireError::UserError(Box::new(error_info)));
                    }
                }
            }
            _ => {}
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

        let executor = self.get_executor()?;

        match Self::parse_copy_to_command(query) {
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

        if let Some((table_name, columns)) = Self::parse_copy_command(query) {
            debug!(
                "COPY FROM STDIN: table={}, columns={:?}",
                table_name, columns
            );

            let (resolved_table, resolved_columns, column_types, col_count, started_txn, qctx) = {
                let mut session_guard = self.session.lock().await;
                let session = session_guard.as_mut().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        "Session not initialized".to_string(),
                    )))
                })?;

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
                                rollback_autocommit_or_mark_failed(session, started_txn).await;
                                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "42P01".to_string(),
                                    format!("relation \"{}\" does not exist", table_name),
                                ))));
                            }
                            Err(e) => {
                                rollback_autocommit_or_mark_failed(session, started_txn).await;
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

                        let mut found: Option<(String, TableSchema)> = None;
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
                                    rollback_autocommit_or_mark_failed(session, started_txn).await;
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
                                rollback_autocommit_or_mark_failed(session, started_txn).await;
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
                    rollback_autocommit_or_mark_failed(session, started_txn).await;
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
                            rollback_autocommit_or_mark_failed(session, started_txn).await;
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
            return Ok(vec![Response::CopyIn(CopyResponse::new(
                0,
                col_count,
                column_formats,
            ))]);
        }

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "XX000".to_string(),
                "Session not initialized".to_string(),
            )))
        })?;

        if let Err(e) = session.check_idle_in_transaction_timeout() {
            let _ = session.rollback().await;
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                e.sqlstate().to_string(),
                e.to_string(),
            ))));
        }

        match executor.execute(session, query).await {
            Ok(results) => {
                session.record_command_complete();
                let mut responses: Vec<Response<'a>> = Vec::new();
                for result in results.into_vec() {
                    if let ExecuteResult::Notice { message } = result {
                        if client_allows_notice(session.show_setting_value("client_min_messages")) {
                            let notice = NoticeResponse::from(ErrorInfo::new(
                                "NOTICE".to_string(),
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
impl CopyHandler for DynamicPgHandler {
    async fn on_copy_data<C>(&self, _client: &mut C, copy_data: CopyData) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let executor = self.get_executor()?;

        let (parse_res, table_name, started_txn, qctx) = {
            let mut ctx_guard = self.copy_context.lock().await;
            let Some(ctx) = ctx_guard.as_mut() else {
                return Ok(());
            };

            let table_name = ctx.table_name.clone();
            let started_txn = ctx.started_txn;
            let qctx = ctx.query_context.clone();

            let parse_res = (|| -> PgWireResult<Vec<(usize, Vec<(String, Value)>)>> {
                if ctx.reached_end_marker {
                    return Ok(Vec::new());
                }

                let lines = ctx.push_copy_data(copy_data.data.as_ref())?;
                if lines.is_empty() {
                    return Ok(Vec::new());
                }

                let mut rows_to_insert: Vec<(usize, Vec<(String, Value)>)> =
                    Vec::with_capacity(lines.len());
                for line_bytes in lines {
                    if ctx.reached_end_marker {
                        break;
                    }
                    if line_bytes.as_slice() == b"\\." {
                        ctx.reached_end_marker = true;
                        ctx.line_buffer.clear();
                        break;
                    }

                    let line_no = ctx
                        .row_count
                        .saturating_add(rows_to_insert.len())
                        .saturating_add(1);
                    let col_values = parse_copy_text_line(
                        executor,
                        &ctx.table_name,
                        &ctx.columns,
                        &ctx.column_types,
                        line_no,
                        &line_bytes,
                    )?;
                    rows_to_insert.push((line_no, col_values));
                }

                Ok(rows_to_insert)
            })();

            (parse_res, table_name, started_txn, qctx)
        };

        let rows_to_insert = match parse_res {
            Ok(rows) => rows,
            Err(e) => {
                let mut ctx_guard = self.copy_context.lock().await;
                *ctx_guard = None;
                drop(ctx_guard);

                let mut session_guard = self.session.lock().await;
                if let Some(session) = session_guard.as_mut() {
                    rollback_autocommit_or_mark_failed(session, started_txn).await;
                }
                return Err(e);
            }
        };

        let inserted_count = rows_to_insert.len();
        if inserted_count == 0 {
            return Ok(());
        }

        let insert_res: PgWireResult<()> =
            crate::sql::query_context::with_scoped_query_context(&qctx, async {
                let mut session_guard = self.session.lock().await;
                let session = session_guard.as_mut().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        "Session not initialized".to_string(),
                    )))
                })?;

                if session.is_transaction_failed() {
                    return Err(in_failed_sql_transaction_pgwire_error());
                }

                let savepoints = session.savepoints();
                crate::txn::with_savepoints(savepoints, async {
                    for (line_no, col_values) in rows_to_insert {
                        executor
                            .execute_copy_insert(session, &table_name, col_values)
                            .await
                            .map_err(|e| {
                                error!("COPY insert error: {}", e);
                                let table = copy_display_table_name(&table_name);
                                let message = if should_add_copy_insert_context(&e) {
                                    format!("{}\nCONTEXT:  COPY {}, line {}", e, table, line_no)
                                } else {
                                    e.to_string()
                                };
                                PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    sqlstate_for_executor_error(&e).to_string(),
                                    message,
                                )))
                            })?;
                    }

                    Ok::<(), PgWireError>(())
                })
                .await?;

                Ok(())
            })
            .await;

        if let Err(e) = insert_res {
            let mut session_guard = self.session.lock().await;
            if let Some(session) = session_guard.as_mut() {
                rollback_autocommit_or_mark_failed(session, started_txn).await;
            }
            drop(session_guard);

            let mut ctx_guard = self.copy_context.lock().await;
            *ctx_guard = None;
            return Err(e);
        }

        let mut ctx_guard = self.copy_context.lock().await;
        if let Some(ctx) = ctx_guard.as_mut() {
            ctx.row_count = ctx.row_count.saturating_add(inserted_count);
        }

        Ok(())
    }

    async fn on_copy_done<C>(&self, client: &mut C, _done: CopyDone) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let ctx_opt = {
            let mut ctx_guard = self.copy_context.lock().await;
            ctx_guard.take()
        };

        let row_count = if let Some(mut ctx) = ctx_opt {
            let executor = self.get_executor()?;
            let qctx = ctx.query_context.clone();

            crate::sql::query_context::with_scoped_query_context(&qctx, async {
                let mut session_guard = self.session.lock().await;
                let session = session_guard.as_mut().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        "Session not initialized".to_string(),
                    )))
                })?;

                if session.is_transaction_failed() {
                    return Err(in_failed_sql_transaction_pgwire_error());
                }

                if let Some(final_line_bytes) = ctx.drain_final_line() {
                    if final_line_bytes.as_slice() == b"\\." {
                        ctx.reached_end_marker = true;
                    } else if !ctx.reached_end_marker {
                        let line_no = ctx.row_count.saturating_add(1);
                        let col_values = match parse_copy_text_line(
                            executor,
                            &ctx.table_name,
                            &ctx.columns,
                            &ctx.column_types,
                            line_no,
                            &final_line_bytes,
                        ) {
                            Ok(values) => values,
                            Err(e) => {
                                rollback_autocommit_or_mark_failed(session, ctx.started_txn).await;
                                return Err(e);
                            }
                        };

                        let savepoints = session.savepoints();
                        let insert_res = crate::txn::with_savepoints(savepoints, async {
                            executor
                                .execute_copy_insert(session, &ctx.table_name, col_values)
                                .await
                                .map_err(|e| {
                                    error!("COPY insert error: {}", e);
                                    let table = copy_display_table_name(&ctx.table_name);
                                    let message = if should_add_copy_insert_context(&e) {
                                        format!("{}\nCONTEXT:  COPY {}, line {}", e, table, line_no)
                                    } else {
                                        e.to_string()
                                    };
                                    PgWireError::UserError(Box::new(ErrorInfo::new(
                                        "ERROR".to_string(),
                                        sqlstate_for_executor_error(&e).to_string(),
                                        message,
                                    )))
                                })
                        })
                        .await;

                        if let Err(e) = insert_res {
                            rollback_autocommit_or_mark_failed(session, ctx.started_txn).await;
                            return Err(e);
                        }

                        ctx.row_count = ctx.row_count.saturating_add(1);
                    }
                }

                if ctx.started_txn {
                    session.commit().await.map_err(|e| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "XX000".to_string(),
                            e.to_string(),
                        )))
                    })?;
                }

                Ok::<usize, PgWireError>(ctx.row_count)
            })
            .await?
        } else {
            0
        };

        client
            .send(PgWireBackendMessage::CommandComplete(CommandComplete::new(
                format!("COPY {}", row_count),
            )))
            .await?;

        Ok(())
    }

    async fn on_copy_fail<C>(&self, _client: &mut C, fail: CopyFail) -> PgWireError
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let ctx_opt = {
            let mut ctx_guard = self.copy_context.lock().await;
            ctx_guard.take()
        };

        if let Some(ctx) = ctx_opt {
            let mut session_guard = self.session.lock().await;
            if let Some(session) = session_guard.as_mut() {
                rollback_autocommit_or_mark_failed(session, ctx.started_txn).await;
            }
        }

        warn!("COPY failed: {}", fail.message);

        PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_owned(),
            "XX000".to_owned(),
            format!("COPY IN mode terminated: {}", fail.message),
        )))
    }
}

#[async_trait]
impl ExtendedQueryHandler for DynamicPgHandler {
    type Statement = PreparedStatement;
    type QueryParser = TipgQueryParser;

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
        if let Some(executor) = self.executor.get() {
            let store = executor.store();

            // Brief session lock to read db_id + search_path
            let (db_id, search_path) = {
                let session_guard = self.session.lock().await;
                match session_guard.as_ref() {
                    Some(session) => (
                        session.current_database_id(),
                        session.search_path().to_vec(),
                    ),
                    None => {
                        // No session yet — reject data/parameterized SQL
                        if let Some(err) = reject_unanalyzed_if_needed(
                            &stored.statement.sql,
                            param_count,
                            "session not available",
                        ) {
                            return Err(err);
                        }
                        client.portal_store().put_statement(Arc::new(stored))?;
                        client
                            .send(PgWireBackendMessage::ParseComplete(
                                pgwire::messages::extendedquery::ParseComplete::new(),
                            ))
                            .await?;
                        return Ok(());
                    }
                }
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
                                    stored.parameter_types = merge_parameter_types(
                                        &stored.parameter_types,
                                        &param_types,
                                    );
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
                            // Utility, no params — keep RawSqlUtility
                        }
                    }
                    let _ = txn.rollback().await; // read-only, discard
                }
                Err(e) => {
                    // Can't begin txn — reject data/parameterized SQL
                    if let Some(err) = reject_unanalyzed_if_needed(
                        &stored.statement.sql,
                        param_count,
                        &format!("failed to begin catalog transaction: {}", e),
                    ) {
                        return Err(err);
                    }
                    // Utility, no params — keep RawSqlUtility
                }
            }
        }

        // Belt-and-suspenders: catch any future code path that produces
        // unanalyzed data SQL or parameterized utility SQL.
        if matches!(stored.statement.exec, PreparedExec::RawSqlUtility) {
            if let Some(err) = reject_unanalyzed_if_needed(
                &stored.statement.sql,
                param_count,
                "analysis was not performed",
            ) {
                return Err(err);
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
    /// immutable after Parse — Describe must be read-only.
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
                    // NO write-back — statement is immutable after Parse
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
        let executor = self.get_executor()?;
        let prepared = &portal.statement.statement;

        debug!("Extended query: {}", prepared.sql);

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "XX000".to_string(),
                "Session not initialized".to_string(),
            )))
        })?;

        if let Err(e) = session.check_idle_in_transaction_timeout() {
            let _ = session.rollback().await;
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                e.sqlstate().to_string(),
                e.to_string(),
            ))));
        }

        let exec_results = match &prepared.exec {
            PreparedExec::RawSqlUtility => {
                debug_assert!(
                    portal.statement.parameter_types.is_empty(),
                    "RawSqlUtility should never have parameters after Parse"
                );
                executor.execute(session, &prepared.sql).await
            }
            PreparedExec::AnalyzedQuery { .. } | PreparedExec::AnalyzedDml { .. } => {
                let params = decode_parameters(portal)?;
                executor
                    .execute_prepared(
                        session,
                        &prepared.sql,
                        &prepared.exec,
                        params,
                        &prepared.param_data_types,
                        &prepared.table_versions,
                    )
                    .await
            }
        };

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

pub struct DynamicHandlerFactory {
    handler: Arc<DynamicPgHandler>,
}

impl DynamicHandlerFactory {
    pub fn new_with_pool(
        client_pool: Arc<TikvClientPool>,
        default_keyspace: Option<String>,
        server_config: SharedServerConfig,
    ) -> Self {
        Self {
            handler: Arc::new(DynamicPgHandler::new_with_pool(
                client_pool,
                default_keyspace,
                server_config,
            )),
        }
    }
}

impl PgWireServerHandlers for DynamicHandlerFactory {
    type StartupHandler = DynamicPgHandler;
    type SimpleQueryHandler = DynamicPgHandler;
    type ExtendedQueryHandler = DynamicPgHandler;
    type CopyHandler = DynamicPgHandler;
    type ErrorHandler = NoopErrorHandler;

    fn simple_query_handler(&self) -> Arc<Self::SimpleQueryHandler> {
        self.handler.clone()
    }

    fn extended_query_handler(&self) -> Arc<Self::ExtendedQueryHandler> {
        self.handler.clone()
    }

    fn startup_handler(&self) -> Arc<Self::StartupHandler> {
        self.handler.clone()
    }

    fn copy_handler(&self) -> Arc<Self::CopyHandler> {
        self.handler.clone()
    }

    fn error_handler(&self) -> Arc<Self::ErrorHandler> {
        Arc::new(NoopErrorHandler)
    }
}
