use crate::auth::AuthManager;
use crate::observability;
use crate::pool::TikvClientPool;
use crate::sql::expr::set_connection_id;
use crate::sql::types::{TypeContext, TypeInferrer};
use crate::sql::{ExecuteResult, Executor, InFailedSqlTransaction, Session};
use crate::storage::TikvStore;
use crate::types::{ColumnDef, DataType, TableSchema, Value};
use async_trait::async_trait;
use futures::{stream, Sink, SinkExt, StreamExt};
use pgwire::api::auth::{ServerParameterProvider, StartupHandler};
use pgwire::api::copy::CopyHandler;
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    CopyResponse, DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat,
    FieldInfo, QueryResponse, Response, Tag,
};
use pgwire::api::stmt::StoredStatement;
use pgwire::api::store::PortalStore;
use pgwire::api::{
    ClientInfo, ClientPortalStore, NoopErrorHandler, PgWireConnectionState, PgWireServerHandlers,
    Type, METADATA_DATABASE, METADATA_USER,
};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::copy::{CopyData, CopyDone, CopyFail};
use pgwire::messages::data::DataRow;
use pgwire::messages::response::{
    CommandComplete, EmptyQueryResponse, ErrorResponse, NoticeResponse, TransactionStatus,
};
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, Ident, ObjectName, Query, Select, SelectItem, SetExpr,
    Statement, TableFactor, TableWithJoins, Values,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::future::Future;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use tikv_client::Transaction;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, error, info, warn};

/// Custom metadata key for storing the extracted keyspace
const METADATA_KEYSPACE: &str = "keyspace";
/// Custom metadata key for storing the actual username (after parsing tenant.user)
const METADATA_ACTUAL_USER: &str = "actual_user";

/// Global atomic counter for generating unique connection IDs
static CONNECTION_ID_COUNTER: AtomicI32 = AtomicI32::new(1);

tokio::task_local! {
    static VIEW_INFERENCE_STACK: RefCell<Vec<String>>;
}

const MAX_VIEW_INFERENCE_DEPTH: usize = 64;

async fn with_view_inference_stack<T>(future: impl Future<Output = T>) -> T {
    if VIEW_INFERENCE_STACK.try_with(|_| ()).is_ok() {
        future.await
    } else {
        VIEW_INFERENCE_STACK
            .scope(RefCell::new(Vec::new()), future)
            .await
    }
}

struct ViewInferenceGuard {
    view_name: String,
}

impl ViewInferenceGuard {
    fn push(view_name: String) -> Option<Self> {
        VIEW_INFERENCE_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.contains(&view_name) || stack.len() >= MAX_VIEW_INFERENCE_DEPTH {
                return None;
            }
            stack.push(view_name.clone());
            Some(Self { view_name })
        })
    }
}

impl Drop for ViewInferenceGuard {
    fn drop(&mut self) {
        let view_name = &self.view_name;
        let _ = VIEW_INFERENCE_STACK.try_with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.last().map(|s| s == view_name).unwrap_or(false) {
                stack.pop();
            } else if let Some(pos) = stack.iter().rposition(|s| s == view_name) {
                stack.remove(pos);
            }
        });
    }
}

fn sqlstate_for_executor_error(err: &anyhow::Error) -> &'static str {
    if err.is::<InFailedSqlTransaction>() {
        "25P02"
    } else {
        "XX000"
    }
}

fn find_unqualified_identifier_position(query: &str, ident: &str) -> Option<usize> {
    if ident.is_empty() {
        return None;
    }
    let query_lower = query.to_ascii_lowercase();
    let ident_lower = ident.to_ascii_lowercase();
    let haystack = query_lower.as_bytes();
    let needle = ident_lower.as_bytes();

    if needle.len() > haystack.len() {
        return None;
    }

    for i in 0..=haystack.len().saturating_sub(needle.len()) {
        if &haystack[i..i + needle.len()] != needle {
            continue;
        }

        let prev = i.checked_sub(1).map(|idx| haystack[idx]);
        if prev.is_some_and(|b| b == b'.' || is_ident_char(b)) {
            continue;
        }

        let next = haystack.get(i + needle.len()).copied();
        if next.is_some_and(is_ident_char) {
            continue;
        }

        return Some(i + 1);
    }

    None
}

fn ambiguous_column_error_with_position(query: &str, message: &str) -> Option<(String, usize)> {
    let trimmed = message.trim();
    let prefix = "column reference \"";
    let suffix = "\" is ambiguous";
    let col_name = trimmed.strip_prefix(prefix)?.strip_suffix(suffix)?;
    let pos = find_unqualified_identifier_position(query, col_name)?;
    Some((col_name.to_string(), pos))
}

fn in_failed_sql_transaction_pgwire_error() -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        "25P02".to_string(),
        InFailedSqlTransaction.to_string(),
    )))
}

fn syntax_error_pgwire_error(message: String) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_owned(),
        "42601".to_owned(),
        message,
    )))
}

fn is_refresh_materialized_view_sql(sql_upper: &str) -> bool {
    let mut words = sql_upper.split_whitespace();
    matches!(
        (words.next(), words.next(), words.next()),
        (Some("REFRESH"), Some("MATERIALIZED"), Some("VIEW"))
    )
}

fn is_drop_materialized_view_sql(sql_upper: &str) -> bool {
    let mut words = sql_upper.split_whitespace();
    matches!(
        (words.next(), words.next(), words.next()),
        (Some("DROP"), Some("MATERIALIZED"), Some("VIEW"))
    )
}

fn is_create_type_as_enum_sql(sql_upper: &str) -> bool {
    if !sql_upper.starts_with("CREATE TYPE") {
        return false;
    }
    let mut prev = "";
    for token in sql_upper.split_whitespace() {
        if prev == "AS" && token.starts_with("ENUM") {
            return true;
        }
        prev = token;
    }
    false
}

fn is_unsupported_sql_that_executor_skips(sql_upper: &str) -> bool {
    if sql_upper.starts_with("CREATE DOMAIN") {
        return true;
    }
    if sql_upper.starts_with("CREATE AGGREGATE") {
        return true;
    }
    if sql_upper.starts_with("ALTER TYPE") {
        return true;
    }
    if sql_upper.starts_with("ALTER DOMAIN") {
        return true;
    }
    if sql_upper.starts_with("ALTER AGGREGATE") {
        return true;
    }
    if sql_upper.starts_with("ALTER FUNCTION") {
        return !sql_upper.contains(" OWNER TO ");
    }
    if sql_upper.starts_with("ALTER SEQUENCE") {
        return !sql_upper.contains(" OWNER TO ") && !sql_upper.contains(" OWNED BY ");
    }
    false
}

fn should_accept_sql_without_sqlparser(sql_upper: &str) -> bool {
    // Keep consistent with `get_skip_reason` and `get_unsupported_reason` behavior in the executor:
    // allow these statements to proceed (they'll be handled or skipped later) instead of failing
    // Parse for Extended Query.
    if sql_upper.starts_with('\\') {
        return true;
    }
    if sql_upper.starts_with("COPY ") || sql_upper.contains(" FROM STDIN") {
        return true;
    }

    if sql_upper.starts_with("CREATE DATABASE")
        || sql_upper.starts_with("DROP DATABASE")
        || sql_upper.starts_with("ALTER DATABASE")
        || sql_upper.starts_with("CREATE EXTENSION")
        || sql_upper.starts_with("DROP EXTENSION")
        || sql_upper.starts_with("COMMENT ON")
        || sql_upper.starts_with("CREATE OR REPLACE FUNCTION")
        || sql_upper.starts_with("CREATE FUNCTION")
        || sql_upper.starts_with("DROP FUNCTION")
        || sql_upper.starts_with("CREATE CONSTRAINT TRIGGER")
        || sql_upper.starts_with("CREATE TRIGGER")
        || sql_upper.starts_with("DROP TRIGGER")
        || ((sql_upper.starts_with("ALTER TABLE")
            || sql_upper.starts_with("ALTER SEQUENCE")
            || sql_upper.starts_with("ALTER FUNCTION"))
            && sql_upper.contains(" OWNER TO "))
        || (sql_upper.starts_with("ALTER SEQUENCE") && sql_upper.contains("OWNED"))
        || is_refresh_materialized_view_sql(sql_upper)
        || is_drop_materialized_view_sql(sql_upper)
        || sql_upper.starts_with("CALL ")
        || sql_upper.starts_with("DROP PROCEDURE")
        || sql_upper.starts_with("CREATE PROCEDURE")
        || sql_upper.starts_with("CREATE OR REPLACE PROCEDURE")
        || is_create_type_as_enum_sql(sql_upper)
        || sql_upper.starts_with("DROP TYPE")
        || is_unsupported_sql_that_executor_skips(sql_upper)
    {
        return true;
    }

    false
}

#[derive(Debug, Default)]
pub struct TipgQueryParser;

impl TipgQueryParser {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl pgwire::api::stmt::QueryParser for TipgQueryParser {
    type Statement = String;

    async fn parse_sql(&self, sql: &str, _types: &[Type]) -> PgWireResult<Self::Statement> {
        // Match libpq behavior for empty queries (handled later by executor/protocol).
        if sql.trim().is_empty() {
            return Ok(sql.to_owned());
        }

        let parse_err = match crate::sql::parse_sql(sql) {
            Ok(_) => return Ok(sql.to_owned()),
            Err(e) => e,
        };

        let Some(sql_no_comments) = strip_leading_whitespace_and_comments(sql) else {
            return Err(syntax_error_pgwire_error(parse_err.to_string()));
        };

        let sql_upper = sql_no_comments.trim_start().to_ascii_uppercase();
        if should_accept_sql_without_sqlparser(&sql_upper) {
            return Ok(sql.to_owned());
        }

        Err(syntax_error_pgwire_error(parse_err.to_string()))
    }
}

async fn rollback_autocommit_or_mark_failed(session: &mut Session, started_txn: bool) {
    if started_txn {
        let _ = session.rollback().await;
    } else {
        session.mark_transaction_failed();
    }
}

fn copy_from_stdin_line_too_long_error() -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        "54000".to_string(),
        format!(
            "COPY FROM STDIN row exceeded max size ({} bytes)",
            MAX_COPY_FROM_STDIN_LINE_BYTES
        ),
    )))
}

fn copy_row_column_mismatch_error(actual: usize, expected: usize) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        "22P04".to_string(),
        format!(
            "COPY row has {} columns but {} columns expected",
            actual, expected
        ),
    )))
}

fn is_empty_simple_query(query: &str) -> bool {
    let trimmed = query.trim();
    trimmed.is_empty() || trimmed == ";"
}

fn update_tx_status_after_execution(status: TransactionStatus, tag: &Tag) -> TransactionStatus {
    if *tag == Tag::new("ROLLBACK") {
        // `ROLLBACK TO SAVEPOINT` clears the failed-transaction state without ending the
        // transaction block, so ReadyForQuery must move from `E` -> `T`.
        TransactionStatus::Transaction
    } else {
        status
    }
}

async fn on_query_with_tx_status_fix<H, C>(
    handler: &H,
    client: &mut C,
    query: pgwire::messages::simplequery::Query,
) -> PgWireResult<()>
where
    H: SimpleQueryHandler,
    C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
    C::Error: Debug,
    PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
{
    if !matches!(client.state(), PgWireConnectionState::ReadyForQuery) {
        return Err(PgWireError::NotReadyForQuery);
    }

    let mut transaction_status = client.transaction_status();
    client.set_state(PgWireConnectionState::QueryInProgress);
    let query_string = query.query;

    if is_empty_simple_query(&query_string) {
        client
            .feed(PgWireBackendMessage::EmptyQueryResponse(
                EmptyQueryResponse::new(),
            ))
            .await?;
    } else {
        let resp = <H as SimpleQueryHandler>::do_query(handler, client, &query_string).await?;
        for r in resp {
            match r {
                Response::EmptyQuery => {
                    client
                        .feed(PgWireBackendMessage::EmptyQueryResponse(
                            EmptyQueryResponse::new(),
                        ))
                        .await?;
                }
                Response::Query(results) => {
                    pgwire::api::query::send_query_response(client, results, true).await?;
                }
                Response::Execution(tag) => {
                    transaction_status = update_tx_status_after_execution(transaction_status, &tag);
                    pgwire::api::query::send_execution_response(client, tag).await?;
                }
                Response::TransactionStart(tag) => {
                    pgwire::api::query::send_execution_response(client, tag).await?;
                    transaction_status = transaction_status.to_in_transaction_state();
                }
                Response::TransactionEnd(tag) => {
                    pgwire::api::query::send_execution_response(client, tag).await?;
                    transaction_status = transaction_status.to_idle_state();
                }
                Response::Error(e) => {
                    client
                        .feed(PgWireBackendMessage::ErrorResponse((*e).into()))
                        .await?;
                    transaction_status = transaction_status.to_error_state();
                }
                Response::CopyIn(result) => {
                    pgwire::api::copy::send_copy_in_response(client, result).await?;
                    client.set_state(PgWireConnectionState::CopyInProgress(false));
                }
                Response::CopyOut(result) => {
                    pgwire::api::copy::send_copy_out_response(client, result).await?;
                    client.set_state(PgWireConnectionState::CopyInProgress(false));
                }
                Response::CopyBoth(result) => {
                    pgwire::api::copy::send_copy_both_response(client, result).await?;
                    client.set_state(PgWireConnectionState::CopyInProgress(false));
                }
            }
        }
    }

    if !matches!(client.state(), PgWireConnectionState::CopyInProgress(_)) {
        client.set_state(PgWireConnectionState::ReadyForQuery);
        client.set_transaction_status(transaction_status);
        pgwire::api::query::send_ready_for_query(client, transaction_status).await?;
    }

    Ok(())
}

async fn on_execute_with_tx_status_fix<H, C>(
    handler: &H,
    suspended_portals: &Mutex<HashMap<String, SuspendedPortalState>>,
    client: &mut C,
    message: pgwire::messages::extendedquery::Execute,
) -> PgWireResult<()>
where
    H: ExtendedQueryHandler,
    C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
    C::PortalStore: PortalStore<Statement = H::Statement>,
    C::Error: Debug,
    PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
{
    if !matches!(client.state(), PgWireConnectionState::ReadyForQuery) {
        return Err(PgWireError::NotReadyForQuery);
    }
    let mut transaction_status = client.transaction_status();

    client.set_state(PgWireConnectionState::QueryInProgress);

    let portal_name = message.name.as_deref().unwrap_or(pgwire::api::DEFAULT_NAME);
    let max_rows = if message.max_rows <= 0 {
        0
    } else {
        message.max_rows as usize
    };

    if let Some(portal) = client.portal_store().get_portal(portal_name) {
        if let Some((command_tag, chunk, still_suspended, total_rows_sent)) =
            take_suspended_rows(suspended_portals, portal_name, max_rows).await
        {
            for row in chunk {
                client.feed(PgWireBackendMessage::DataRow(row)).await?;
            }

            if still_suspended {
                client
                    .send(PgWireBackendMessage::PortalSuspended(
                        pgwire::messages::extendedquery::PortalSuspended::new(),
                    ))
                    .await?;
            } else {
                let tag = Tag::new(&command_tag).with_rows(total_rows_sent);
                client
                    .send(PgWireBackendMessage::CommandComplete(tag.into()))
                    .await?;
            }
        } else {
            match <H as ExtendedQueryHandler>::do_query(handler, client, portal.as_ref(), max_rows)
                .await?
            {
            Response::EmptyQuery => {
                client
                    .feed(PgWireBackendMessage::EmptyQueryResponse(
                        EmptyQueryResponse::new(),
                    ))
                    .await?;
            }
            Response::Query(results) => {
                if max_rows == 0 {
                    pgwire::api::query::send_query_response(client, results, false).await?;
                } else {
                    send_limited_query_response(
                        client,
                        suspended_portals,
                        portal_name,
                        results,
                        max_rows,
                    )
                    .await?;
                }
            }
            Response::Execution(tag) => {
                transaction_status = update_tx_status_after_execution(transaction_status, &tag);
                pgwire::api::query::send_execution_response(client, tag).await?;
            }
            Response::TransactionStart(tag) => {
                pgwire::api::query::send_execution_response(client, tag).await?;
                transaction_status = transaction_status.to_in_transaction_state();
            }
            Response::TransactionEnd(tag) => {
                pgwire::api::query::send_execution_response(client, tag).await?;
                transaction_status = transaction_status.to_idle_state();
            }
            Response::Error(err) => {
                client
                    .send(PgWireBackendMessage::ErrorResponse((*err).into()))
                    .await?;
                transaction_status = transaction_status.to_error_state();
            }
            Response::CopyIn(result) => {
                client.set_state(PgWireConnectionState::CopyInProgress(true));
                pgwire::api::copy::send_copy_in_response(client, result).await?;
            }
            Response::CopyOut(result) => {
                client.set_state(PgWireConnectionState::CopyInProgress(true));
                pgwire::api::copy::send_copy_out_response(client, result).await?;
            }
            Response::CopyBoth(result) => {
                client.set_state(PgWireConnectionState::CopyInProgress(true));
                pgwire::api::copy::send_copy_both_response(client, result).await?;
            }
            }
        }

        if !matches!(client.state(), PgWireConnectionState::CopyInProgress(_)) {
            client.set_state(PgWireConnectionState::ReadyForQuery);
            client.set_transaction_status(transaction_status);
        };

        Ok(())
    } else {
        Err(PgWireError::PortalNotFound(portal_name.to_owned()))
    }
}

#[derive(Debug)]
struct SuspendedPortalState {
    command_tag: String,
    remaining_rows: VecDeque<DataRow>,
    rows_sent_so_far: usize,
}

const DEFAULT_MAX_SUSPENDED_PORTALS: usize = 32;
const DEFAULT_MAX_SUSPENDED_PORTAL_BUFFER_ROWS: usize = 10_000;
const DEFAULT_MAX_SUSPENDED_PORTAL_BUFFER_BYTES: usize = 16 * 1024 * 1024;

fn max_suspended_portals() -> usize {
    std::env::var("PGTIKV_MAX_SUSPENDED_PORTALS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_SUSPENDED_PORTALS)
}

fn max_suspended_portal_buffer_rows() -> usize {
    std::env::var("PGTIKV_MAX_SUSPENDED_PORTAL_BUFFER_ROWS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_SUSPENDED_PORTAL_BUFFER_ROWS)
}

fn max_suspended_portal_buffer_bytes() -> usize {
    std::env::var("PGTIKV_MAX_SUSPENDED_PORTAL_BUFFER_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_SUSPENDED_PORTAL_BUFFER_BYTES)
}

async fn take_suspended_rows(
    suspended_portals: &Mutex<HashMap<String, SuspendedPortalState>>,
    portal_name: &str,
    max_rows: usize,
) -> Option<(String, Vec<DataRow>, bool, usize)> {
    let mut guard = suspended_portals.lock().await;
    let state = guard.get_mut(portal_name)?;

    let to_take = if max_rows == 0 {
        state.remaining_rows.len()
    } else {
        max_rows.min(state.remaining_rows.len())
    };

    let mut chunk = Vec::with_capacity(to_take);
    for _ in 0..to_take {
        if let Some(row) = state.remaining_rows.pop_front() {
            chunk.push(row);
        }
    }

    state.rows_sent_so_far = state.rows_sent_so_far.saturating_add(chunk.len());
    let still_suspended = !state.remaining_rows.is_empty();
    let command_tag = state.command_tag.clone();
    let total_rows_sent = state.rows_sent_so_far;

    if !still_suspended {
        guard.remove(portal_name);
    }

    Some((command_tag, chunk, still_suspended, total_rows_sent))
}

async fn send_limited_query_response<C>(
    client: &mut C,
    suspended_portals: &Mutex<HashMap<String, SuspendedPortalState>>,
    portal_name: &str,
    results: QueryResponse<'_>,
    max_rows: usize,
) -> PgWireResult<()>
where
    C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
    C::Error: Debug,
    PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
{
    let command_tag = results.command_tag().to_owned();
    let mut data_rows = results.data_rows();

    let mut rows_sent = 0usize;
    let mut remainder: VecDeque<DataRow> = VecDeque::new();
    let mut buffered_bytes: usize = 0;
    let max_buffered_rows = max_suspended_portal_buffer_rows();
    let max_buffered_bytes = max_suspended_portal_buffer_bytes();

    while let Some(row) = data_rows.next().await {
        let row = row?;
        if rows_sent < max_rows {
            rows_sent += 1;
            client.feed(PgWireBackendMessage::DataRow(row)).await?;
        } else {
            buffered_bytes = buffered_bytes.saturating_add(row.data.len());
            remainder.push_back(row);
            if remainder.len() > max_buffered_rows || buffered_bytes > max_buffered_bytes {
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_owned(),
                    "54000".to_owned(),
                    format!(
                        "portal suspension buffer exceeded (portal={portal_name}, max_rows={max_rows}, buffer_rows_limit={max_buffered_rows}, buffer_bytes_limit={max_buffered_bytes}); re-run with max_rows=0 or reduce result size; set PGTIKV_MAX_SUSPENDED_PORTAL_BUFFER_ROWS/BYTES to override"
                    ),
                ))));
            }
        }
    }

    if !remainder.is_empty() {
        let max_suspended = max_suspended_portals();
        let mut guard = suspended_portals.lock().await;
        if !guard.contains_key(portal_name) && guard.len() >= max_suspended {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_owned(),
                "54000".to_owned(),
                format!(
                    "too many suspended portals (portal={portal_name}, max_rows={max_rows}, suspended_portals_limit={max_suspended}); close portals to free resources or re-run with max_rows=0; set PGTIKV_MAX_SUSPENDED_PORTALS to override"
                ),
            ))));
        }
        guard.insert(
            portal_name.to_owned(),
            SuspendedPortalState {
                command_tag,
                remaining_rows: remainder,
                rows_sent_so_far: rows_sent,
            },
        );
        client
            .send(PgWireBackendMessage::PortalSuspended(
                pgwire::messages::extendedquery::PortalSuspended::new(),
            ))
            .await?;
    } else {
        let tag = Tag::new(&command_tag).with_rows(rows_sent);
        client
            .send(PgWireBackendMessage::CommandComplete(tag.into()))
            .await?;
    }

    Ok(())
}

pub struct PgServerParameterProvider;

impl ServerParameterProvider for PgServerParameterProvider {
    fn server_parameters<C: ClientInfo>(&self, _client: &C) -> Option<HashMap<String, String>> {
        let mut params = HashMap::new();
        params.insert("server_version".to_owned(), "16.0".to_owned());
        params.insert("server_version_num".to_owned(), "160000".to_owned());
        params.insert("server_encoding".to_owned(), "UTF8".to_owned());
        params.insert("client_encoding".to_owned(), "UTF8".to_owned());
        params.insert("DateStyle".to_owned(), "ISO, MDY".to_owned());
        params.insert("TimeZone".to_owned(), "UTC".to_owned());
        params.insert("standard_conforming_strings".to_owned(), "on".to_owned());
        Some(params)
    }
}

#[derive(Debug, Clone)]
pub struct CopyContext {
    pub table_name: String,
    pub columns: Vec<String>,
    pub column_types: Vec<Option<DataType>>,
    pub line_buffer: Vec<u8>,
    pub row_count: usize,
    pub started_txn: bool,
    pub reached_end_marker: bool,
}

/// A safety cap to prevent unbounded buffering if the client sends a single row without newlines.
/// This is a per-row cap (not a cap on the total COPY stream).
const MAX_COPY_FROM_STDIN_LINE_BYTES: usize = 32 * 1024 * 1024;

impl CopyContext {
    fn push_copy_data(&mut self, data: &[u8]) -> PgWireResult<Vec<Vec<u8>>> {
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut start = 0usize;

        for (idx, byte) in data.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }

            let mut line: Vec<u8> = Vec::new();
            if !self.line_buffer.is_empty() {
                line.extend_from_slice(&self.line_buffer);
                self.line_buffer.clear();
            }
            line.extend_from_slice(&data[start..idx]);
            if line.last() == Some(&b'\r') {
                line.pop();
            }

            if line.len() > MAX_COPY_FROM_STDIN_LINE_BYTES {
                return Err(copy_from_stdin_line_too_long_error());
            }

            lines.push(line);
            start = idx.saturating_add(1);
        }

        if start < data.len() {
            let remaining = &data[start..];
            let new_len = self.line_buffer.len().saturating_add(remaining.len());
            if new_len > MAX_COPY_FROM_STDIN_LINE_BYTES {
                return Err(copy_from_stdin_line_too_long_error());
            }
            self.line_buffer.extend_from_slice(remaining);
        }

        Ok(lines)
    }

    fn drain_final_line(&mut self) -> Option<Vec<u8>> {
        if self.line_buffer.is_empty() {
            return None;
        }
        let mut line = Vec::new();
        std::mem::swap(&mut line, &mut self.line_buffer);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(line)
    }
}

/// Parse username in format "tenant.user" or "tenant:user" into (keyspace, actual_user).
/// If no separator found, returns (None, username) - no keyspace override.
fn parse_tenant_username(username: &str) -> (Option<String>, String) {
    // Try dot separator first: "tenant_a.admin" -> keyspace=tenant_a, user=admin
    if let Some(pos) = username.find('.') {
        let tenant = &username[..pos];
        let user = &username[pos + 1..];
        if !tenant.is_empty() && !user.is_empty() {
            return (Some(tenant.to_string()), user.to_string());
        }
    }
    // Try colon separator: "tenant_a:admin" -> keyspace=tenant_a, user=admin
    if let Some(pos) = username.find(':') {
        let tenant = &username[..pos];
        let user = &username[pos + 1..];
        if !tenant.is_empty() && !user.is_empty() {
            return (Some(tenant.to_string()), user.to_string());
        }
    }
    // No separator or invalid format - use as-is without keyspace override
    (None, username.to_string())
}

/// Count the number of parameter placeholders ($1, $2, ...) in a SQL query.
/// Returns the maximum placeholder number found, which indicates how many parameters are expected.
fn count_sql_parameters(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut max_param = 0usize;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<u8>> = None;

    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(delim) = dollar_delim.as_ref() {
            let delim_len = delim.len();
            let matches =
                i + delim_len <= bytes.len() && &bytes[i..i + delim_len] == delim.as_slice();
            if matches {
                dollar_delim = None;
                i += delim_len;
            } else {
                i += 1;
            }
            continue;
        }

        let b = bytes[i];

        if b == b'\'' && !in_double_quote {
            if in_single_quote && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single_quote {
            if in_double_quote && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                i += 2;
                continue;
            }
            in_double_quote = !in_double_quote;
            i += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote {
            // SQL comments
            if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                i += 2;
                let mut depth = 1usize;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                        continue;
                    }
                    i += 1;
                }
                continue;
            }
        }

        if !in_single_quote && !in_double_quote && b == b'$' {
            // Prepared-statement placeholder: $1, $2, ...
            let mut j = i + 1;
            let mut saw_digit = false;
            let mut num = 0usize;
            while j < bytes.len() && bytes[j].is_ascii_digit() && j - i <= 10 {
                saw_digit = true;
                num = num
                    .saturating_mul(10)
                    .saturating_add((bytes[j] - b'0') as usize);
                j += 1;
            }
            if saw_digit {
                let before_ok = i == 0 || !is_ident_char_or_dollar(bytes[i - 1]);
                let after_ok = j == bytes.len() || !is_ident_char_or_dollar(bytes[j]);
                if before_ok && after_ok {
                    max_param = max_param.max(num);
                    i = j;
                    continue;
                }
            }

            // PostgreSQL dollar-quoted strings ($tag$...$tag$ or $$...$$)
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'$' {
                if !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'$' {
                dollar_delim = Some(bytes[i..=j].to_vec());
                i = j + 1;
                continue;
            }
        }

        i += 1;
    }

    max_param
}

fn is_ident_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

fn is_ident_char_or_dollar(b: u8) -> bool {
    is_ident_char(b) || b == b'$'
}

fn resolve_table_for_insert(table_name: &str, search_path: &[String]) -> String {
    // Strip quotes from table name (GORM uses quoted identifiers)
    let strip_quotes = |s: &str| -> String { s.trim_matches('"').to_string() };

    if table_name.contains('.') {
        // Split on . and strip quotes from each part
        let parts: Vec<&str> = table_name.splitn(2, '.').collect();
        if parts.len() == 2 {
            format!("{}.{}", strip_quotes(parts[0]), strip_quotes(parts[1]))
        } else {
            strip_quotes(table_name)
        }
    } else {
        let schema = search_path.first().map(|s| s.as_str()).unwrap_or("public");
        format!("{}.{}", schema, strip_quotes(table_name))
    }
}

fn normalize_copy_ident(token: &str) -> String {
    let token = token.trim();
    if token.starts_with('"') && token.ends_with('"') && token.len() >= 2 {
        token[1..token.len() - 1].replace("\"\"", "\"")
    } else {
        token.to_lowercase()
    }
}

fn resolve_copy_columns(
    schema: &TableSchema,
    columns: &[String],
    relation_name: &str,
) -> PgWireResult<(Vec<String>, Vec<Option<DataType>>)> {
    if columns.is_empty() {
        let resolved: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        let types: Vec<Option<DataType>> = schema
            .columns
            .iter()
            .map(|c| Some(c.data_type.clone()))
            .collect();
        return Ok((resolved, types));
    }

    let mut resolved_columns: Vec<String> = Vec::with_capacity(columns.len());
    let mut column_types: Vec<Option<DataType>> = Vec::with_capacity(columns.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(columns.len());

    for col in columns {
        let normalized = normalize_copy_ident(col);
        let Some(def) = schema.columns.iter().find(|c| c.name == normalized) else {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "42703".to_string(),
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    normalized, relation_name
                ),
            ))));
        };

        if !seen.insert(def.name.clone()) {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "42701".to_string(),
                format!(
                    "column \"{}\" specified more than once",
                    def.name
                ),
            ))));
        }

        resolved_columns.push(def.name.clone());
        column_types.push(Some(def.data_type.clone()));
    }

    Ok((resolved_columns, column_types))
}

fn count_placeholders_in_expr(expr: &Expr) -> usize {
    match expr {
        Expr::Value(sqlparser::ast::Value::Placeholder(p)) => {
            if p.starts_with('$') && p[1..].chars().all(|c| c.is_ascii_digit()) {
                1
            } else {
                0
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            count_placeholders_in_expr(left) + count_placeholders_in_expr(right)
        }
        Expr::UnaryOp { expr, .. } => count_placeholders_in_expr(expr),
        Expr::Nested(inner) => count_placeholders_in_expr(inner),
        Expr::Function(f) => f.args.iter().fold(0, |acc, arg| {
            acc + match arg {
                sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) => {
                    count_placeholders_in_expr(e)
                }
                sqlparser::ast::FunctionArg::Named {
                    arg: sqlparser::ast::FunctionArgExpr::Expr(e),
                    ..
                } => count_placeholders_in_expr(e),
                _ => 0,
            }
        }),
        Expr::Cast { expr, .. } => count_placeholders_in_expr(expr),
        _ => 0,
    }
}

fn infer_types_from_expr(
    expr: &Expr,
    col_types: &std::collections::HashMap<String, DataType>,
    types: &mut Vec<Type>,
) {
    match expr {
        Expr::BinaryOp { left, op, right } => match op {
            sqlparser::ast::BinaryOperator::Eq
            | sqlparser::ast::BinaryOperator::NotEq
            | sqlparser::ast::BinaryOperator::Lt
            | sqlparser::ast::BinaryOperator::LtEq
            | sqlparser::ast::BinaryOperator::Gt
            | sqlparser::ast::BinaryOperator::GtEq => {
                if let (
                    Expr::Identifier(ident),
                    Expr::Value(sqlparser::ast::Value::Placeholder(p)),
                ) = (left.as_ref(), right.as_ref())
                {
                    if let Some(idx) = extract_placeholder_index(p) {
                        let col_name = ident.value.to_lowercase();
                        if let Some(col_type) = col_types.get(&col_name) {
                            if idx < types.len() {
                                types[idx] = datatype_to_pgtype(Some(col_type));
                            }
                        }
                    }
                } else if let (
                    Expr::Value(sqlparser::ast::Value::Placeholder(p)),
                    Expr::Identifier(ident),
                ) = (left.as_ref(), right.as_ref())
                {
                    if let Some(idx) = extract_placeholder_index(p) {
                        let col_name = ident.value.to_lowercase();
                        if let Some(col_type) = col_types.get(&col_name) {
                            if idx < types.len() {
                                types[idx] = datatype_to_pgtype(Some(col_type));
                            }
                        }
                    }
                } else if let (
                    Expr::CompoundIdentifier(parts),
                    Expr::Value(sqlparser::ast::Value::Placeholder(p)),
                ) = (left.as_ref(), right.as_ref())
                {
                    if let Some(idx) = extract_placeholder_index(p) {
                        if let Some(last) = parts.last() {
                            let col_name = last.value.to_lowercase();
                            if let Some(col_type) = col_types.get(&col_name) {
                                if idx < types.len() {
                                    types[idx] = datatype_to_pgtype(Some(col_type));
                                }
                            }
                        }
                    }
                }
                infer_types_from_expr(left, col_types, types);
                infer_types_from_expr(right, col_types, types);
            }
            sqlparser::ast::BinaryOperator::And | sqlparser::ast::BinaryOperator::Or => {
                infer_types_from_expr(left, col_types, types);
                infer_types_from_expr(right, col_types, types);
            }
            _ => {}
        },
        Expr::Nested(inner) => infer_types_from_expr(inner, col_types, types),
        Expr::InList {
            expr: left_expr,
            list,
            ..
        } => {
            if let Expr::Identifier(ident) = left_expr.as_ref() {
                let col_name = ident.value.to_lowercase();
                if let Some(col_type) = col_types.get(&col_name) {
                    for item in list {
                        if let Expr::Value(sqlparser::ast::Value::Placeholder(p)) = item {
                            if let Some(idx) = extract_placeholder_index(p) {
                                if idx < types.len() {
                                    types[idx] = datatype_to_pgtype(Some(col_type));
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn extract_placeholder_index(p: &str) -> Option<usize> {
    if p.starts_with('$') && p[1..].chars().all(|c| c.is_ascii_digit()) {
        p[1..].parse::<usize>().ok().map(|n| n - 1)
    } else {
        None
    }
}

fn extract_placeholder_index_from_expr(expr: &sqlparser::ast::Expr) -> Option<usize> {
    match expr {
        sqlparser::ast::Expr::Value(sqlparser::ast::Value::Placeholder(p)) => {
            extract_placeholder_index(p)
        }
        _ => None,
    }
}

fn parse_startup_options(options: &str) -> Vec<(String, String)> {
    let tokens: Vec<&str> = options.split_whitespace().collect();
    let mut settings = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i] == "-c" {
            if let Some(kv) = tokens.get(i + 1) {
                if let Some((key, value)) = kv.split_once('=') {
                    settings.push((key.to_string(), value.to_string()));
                }
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    settings
}

fn client_min_messages_rank(level: &str) -> Option<u8> {
    match level.to_ascii_lowercase().as_str() {
        "debug5" => Some(1),
        "debug4" => Some(2),
        "debug3" => Some(3),
        "debug2" => Some(4),
        "debug1" => Some(5),
        "debug" => Some(4),
        "log" => Some(6),
        "info" => Some(7),
        "notice" => Some(8),
        "warning" => Some(9),
        "error" => Some(10),
        "fatal" => Some(11),
        "panic" => Some(12),
        _ => None,
    }
}

fn client_allows_notice(client_min_messages: Option<String>) -> bool {
    let notice_rank = client_min_messages_rank("notice").unwrap_or(8);
    let min_rank = client_min_messages
        .as_deref()
        .and_then(client_min_messages_rank)
        .unwrap_or(notice_rank);
    notice_rank >= min_rank
}

async fn send_notices_and_get_last_response<C>(
    client: &mut C,
    client_min_messages: Option<String>,
    results: crate::sql::ExecuteResults,
) -> PgWireResult<Response<'static>>
where
    C: Sink<PgWireBackendMessage> + Unpin + Send + Sync,
    C::Error: Debug,
    PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
{
    let allow_notice = client_allows_notice(client_min_messages);
    let mut last: Option<Response<'static>> = None;
    for result in results.into_vec() {
        match result {
            ExecuteResult::Notice { message } => {
                if allow_notice {
                    let notice = NoticeResponse::from(ErrorInfo::new(
                        "NOTICE".to_string(),
                        "00000".to_string(),
                        message,
                    ));
                    client
                        .send(PgWireBackendMessage::NoticeResponse(notice))
                        .await?;
                }
            }
            other => {
                last = Some(result_to_response(other)?);
            }
        }
    }
    Ok(last.unwrap_or(Response::EmptyQuery))
}

fn infer_parameter_types(sql: &str, param_count: usize) -> Vec<Type> {
    // Default to TEXT: drivers can encode any value to TEXT, server does implicit conversion.
    // UNKNOWN (OID 705) breaks pgx/GORM which cannot encode time.Time to unknown type.
    let mut types = vec![Type::TEXT; param_count];
    if param_count == 0 {
        return types;
    }

    // Use ASCII-only case normalization to keep byte offsets stable. Full Unicode uppercasing can
    // change byte length and make `pos` invalid for slicing, potentially panicking on non-ASCII SQL.
    let sql_upper = sql.to_ascii_uppercase();
    let bytes = sql.as_bytes();

    let mut placeholder_positions: Vec<(usize, usize)> = Vec::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<u8>> = None;

    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(delim) = dollar_delim.as_ref() {
            let delim_len = delim.len();
            if i + delim_len <= bytes.len() && &bytes[i..i + delim_len] == delim.as_slice() {
                dollar_delim = None;
                i += delim_len;
            } else {
                i += 1;
            }
            continue;
        }

        let b = bytes[i];

        if b == b'\'' && !in_double_quote {
            if in_single_quote && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single_quote {
            if in_double_quote && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                i += 2;
                continue;
            }
            in_double_quote = !in_double_quote;
            i += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote && b == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                if let Ok(num) = std::str::from_utf8(&bytes[i + 1..j])
                    .unwrap_or("0")
                    .parse::<usize>()
                {
                    placeholder_positions.push((i, num));
                }
                i = j;
                continue;
            }

            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'$' {
                if !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'$' {
                dollar_delim = Some(bytes[i..=j].to_vec());
                i = j + 1;
                continue;
            }
        }

        i += 1;
    }

    for (pos, param_num) in placeholder_positions {
        if param_num == 0 || param_num > param_count {
            continue;
        }
        let param_idx = param_num - 1;

        let before = &sql_upper[..pos];
        let before_trimmed = before.trim_end();

        if before_trimmed.ends_with("LIMIT") {
            types[param_idx] = Type::INT8;
            continue;
        }

        if before_trimmed.ends_with("OFFSET") {
            types[param_idx] = Type::INT8;
            continue;
        }

        if before_trimmed.ends_with("FIRST") || before_trimmed.ends_with("NEXT") {
            let keyword_start = if before_trimmed.ends_with("FIRST") {
                before_trimmed.len().saturating_sub(5)
            } else {
                before_trimmed.len().saturating_sub(4)
            };
            let even_before = before_trimmed[..keyword_start].trim_end();
            if even_before.ends_with("FETCH") {
                types[param_idx] = Type::INT8;
                continue;
            }
        }
    }

    types
}

#[allow(dead_code)]
fn find_keyword_outside_strings(query: &str, keyword: &str) -> Option<usize> {
    let bytes = query.as_bytes();
    let kw = keyword.as_bytes();
    if kw.is_empty() || bytes.len() < kw.len() {
        return None;
    }

    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<u8>> = None;

    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(delim) = dollar_delim.as_ref() {
            let delim_len = delim.len();
            let matches =
                i + delim_len <= bytes.len() && &bytes[i..i + delim_len] == delim.as_slice();
            if matches {
                dollar_delim = None;
                i += delim_len;
            } else {
                i += 1;
            }
            continue;
        }

        let b = bytes[i];

        if b == b'\'' && !in_double_quote {
            if in_single_quote && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single_quote {
            if in_double_quote && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                i += 2;
                continue;
            }
            in_double_quote = !in_double_quote;
            i += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote && b == b'$' {
            // Skip placeholders like $1 and keep scanning.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                i = j;
                continue;
            }

            // Track dollar-quoted strings ($tag$...$tag$ or $$...$$)
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'$' {
                if !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'$' {
                dollar_delim = Some(bytes[i..=j].to_vec());
                i = j + 1;
                continue;
            }
        }

        if !in_single_quote && !in_double_quote && i + kw.len() <= bytes.len() {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let after_ok = i + kw.len() == bytes.len() || !is_ident_char(bytes[i + kw.len()]);
            if before_ok && after_ok {
                let mut matched = true;
                for (j, kw_b) in kw.iter().enumerate() {
                    if bytes[i + j].to_ascii_uppercase() != kw_b.to_ascii_uppercase() {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    return Some(i);
                }
            }
        }

        i += 1;
    }

    None
}

fn normalize_sql_ident(ident: &sqlparser::ast::Ident) -> String {
    if ident.quote_style.is_some() {
        ident.value.clone()
    } else {
        ident.value.to_lowercase()
    }
}

fn split_object_name_for_catalog(name: &ObjectName) -> Option<(Option<String>, String)> {
    match name.0.len() {
        1 => Some((None, normalize_sql_ident(&name.0[0]))),
        2 => Some((
            Some(normalize_sql_ident(&name.0[0])),
            normalize_sql_ident(&name.0[1]),
        )),
        _ => None,
    }
}

fn expr_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(normalize_sql_ident(ident)),
        Expr::CompoundIdentifier(parts) => parts.last().map(normalize_sql_ident),
        _ => None,
    }
}

fn expr_referenced_column_type<'a>(
    schema: &'a crate::types::TableSchema,
    expr: &Expr,
) -> Option<&'a DataType> {
    let col_name = expr_column_name(expr)?;
    schema
        .columns
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(&col_name))
        .map(|c| &c.data_type)
}

async fn resolve_table_schema_for_object_name(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &ObjectName,
    search_path: &[String],
) -> Option<crate::types::TableSchema> {
    let (schema_opt, name) = split_object_name_for_catalog(table_name)?;

    async fn infer_view_schema(
        store: &TikvStore,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        full_name: &str,
    ) -> Option<crate::types::TableSchema> {
        let view_def = store.get_view(txn, db_id, full_name).await.ok()??;
        let view_query = view_def.query;
        with_view_inference_stack(async move {
            let _guard = ViewInferenceGuard::push(full_name.to_string())?;
            let parsed = crate::sql::parse_sql(&view_query).ok()?;
            let stmt = parsed.into_iter().next()?;
            let Statement::Query(q) = stmt else {
                return None;
            };
            let ctes: HashMap<String, TableSchema> = HashMap::new();
            let cols =
                infer_query_output_columns_with_txn(store, txn, db_id, search_path, &q, &ctes)
                    .await?;
            Some(schema_from_inferred_columns(full_name.to_string(), &cols))
        })
        .await
    }

    if let Some(schema) = schema_opt {
        let full = format!("{}.{}", schema, name);
        if let Some(schema) = crate::sql::get_information_schema_schema(&full) {
            return Some(schema);
        }
        if let Some(schema) =
            Box::pin(infer_view_schema(store, txn, db_id, search_path, &full)).await
        {
            return Some(schema);
        }
        return store.get_schema(txn, db_id, &full).await.ok().flatten();
    }

    if let Some(schema) = crate::sql::get_information_schema_schema(&name) {
        return Some(schema);
    }

    for schema in search_path {
        let full = format!("{}.{}", schema, name);
        if let Some(schema) =
            Box::pin(infer_view_schema(store, txn, db_id, search_path, &full)).await
        {
            return Some(schema);
        }
        if let Ok(Some(s)) = store.get_schema(txn, db_id, &full).await {
            return Some(s);
        }
    }

    // As a last resort, try the default schema even if it's not present in the session search_path.
    let default_schema = search_path.first().map(String::as_str).unwrap_or("public");
    let full = format!("{}.{}", default_schema, name);
    if let Some(schema) = Box::pin(infer_view_schema(store, txn, db_id, search_path, &full)).await {
        return Some(schema);
    }
    store.get_schema(txn, db_id, &full).await.ok().flatten()
}

async fn infer_returning_fields_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    table_name: &ObjectName,
    returning: &[SelectItem],
) -> Option<Vec<FieldInfo>> {
    let schema =
        resolve_table_schema_for_object_name(store, txn, db_id, table_name, search_path).await?;

    let mut fields = Vec::new();
    for item in returning {
        match item {
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                fields.extend(schema.columns.iter().map(|c| {
                    FieldInfo::new(
                        c.name.clone(),
                        None,
                        None,
                        datatype_to_pgtype(Some(&c.data_type)),
                        FieldFormat::Text,
                    )
                }));
            }
            SelectItem::UnnamedExpr(expr) => {
                let name = expr_column_name(expr).unwrap_or_else(|| "?column?".to_string());
                fields.push(FieldInfo::new(
                    name,
                    None,
                    None,
                    datatype_to_pgtype(expr_referenced_column_type(&schema, expr)),
                    FieldFormat::Text,
                ));
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                fields.push(FieldInfo::new(
                    normalize_sql_ident(alias),
                    None,
                    None,
                    datatype_to_pgtype(expr_referenced_column_type(&schema, expr)),
                    FieldFormat::Text,
                ));
            }
        }
    }

    Some(fields)
}

async fn infer_returning_fields_from_statement(
    store: &Arc<TikvStore>,
    session: &mut Session,
    stmt: &Statement,
) -> Option<Vec<FieldInfo>> {
    let (table_name, returning) = match stmt {
        Statement::Insert {
            table_name,
            returning: Some(items),
            ..
        } => (table_name, items),
        Statement::Update {
            table,
            returning: Some(items),
            ..
        } => match &table.relation {
            TableFactor::Table { name, .. } => (name, items),
            _ => return None,
        },
        Statement::Delete {
            from,
            returning: Some(items),
            ..
        } => {
            let first = from.first()?;
            match &first.relation {
                TableFactor::Table { name, .. } => (name, items),
                _ => return None,
            }
        }
        _ => return None,
    };

    let search_path = session.search_path().to_vec();
    let search_path = search_path.as_slice();
    let db_id = session.current_database_id();

    if let Some(txn) = session.get_mut_txn() {
        infer_returning_fields_with_txn(
            store.as_ref(),
            txn,
            db_id,
            search_path,
            table_name,
            returning,
        )
        .await
    } else {
        let mut temp_txn = store.begin().await.ok()?;
        infer_returning_fields_with_txn(
            store.as_ref(),
            &mut temp_txn,
            db_id,
            search_path,
            table_name,
            returning,
        )
        .await
    }
}

#[derive(Debug, Clone)]
struct InferredColumn {
    name: String,
    data_type: DataType,
}

#[derive(Debug, Clone)]
struct SourceSchema {
    alias: String,
    schema: TableSchema,
}

fn stub_describe_field() -> Vec<FieldInfo> {
    vec![FieldInfo::new(
        "column".to_string(),
        None,
        None,
        Type::TEXT,
        FieldFormat::Text,
    )]
}

fn select_item_output_name(item: &SelectItem) -> String {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
        SelectItem::UnnamedExpr(expr) => expr_output_name(expr),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => "*".to_string(),
    }
}

fn expr_output_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(id) => id.value.clone(),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(|p| p.value.clone())
            .unwrap_or_else(|| "?column?".to_string()),
        Expr::Function(f) => {
            if let Some(last_ident) = f.name.0.last() {
                last_ident.value.to_lowercase()
            } else {
                "?column?".to_string()
            }
        }
        Expr::Case { .. } => "case".to_string(),
        Expr::Cast { data_type, .. } => {
            use sqlparser::ast::DataType as SqlDataType;
            match data_type {
                SqlDataType::Int(_) | SqlDataType::Integer(_) => "int4".to_string(),
                SqlDataType::BigInt(_) => "int8".to_string(),
                SqlDataType::SmallInt(_) => "int2".to_string(),
                SqlDataType::Text => "text".to_string(),
                SqlDataType::Varchar(_) | SqlDataType::CharVarying(_) => "varchar".to_string(),
                SqlDataType::Boolean => "bool".to_string(),
                SqlDataType::Float(_) | SqlDataType::Real => "float4".to_string(),
                SqlDataType::Double | SqlDataType::DoublePrecision => "float8".to_string(),
                SqlDataType::Numeric(_) | SqlDataType::Decimal(_) => "numeric".to_string(),
                SqlDataType::Timestamp(_, _) => "timestamp".to_string(),
                SqlDataType::Date => "date".to_string(),
                SqlDataType::Uuid => "uuid".to_string(),
                SqlDataType::JSON => "json".to_string(),
                _ => data_type.to_string().to_lowercase(),
            }
        }
        Expr::Substring { .. } => "substring".to_string(),
        Expr::Trim { .. } => "btrim".to_string(),
        Expr::Position { .. } => "position".to_string(),
        Expr::Extract { .. } => "extract".to_string(),
        Expr::Subquery(_) => "subquery".to_string(),
        Expr::Nested(inner) => expr_output_name(inner),
        _ => "?column?".to_string(),
    }
}

fn inferred_columns_to_fields(cols: Vec<InferredColumn>) -> Vec<FieldInfo> {
    cols.into_iter()
        .map(|c| {
            FieldInfo::new(
                c.name,
                None,
                None,
                datatype_to_pgtype(Some(&c.data_type)),
                FieldFormat::Text,
            )
        })
        .collect()
}

fn schema_from_inferred_columns(name: String, cols: &[InferredColumn]) -> TableSchema {
    let columns = cols
        .iter()
        .map(|c| ColumnDef {
            name: c.name.clone(),
            data_type: c.data_type.clone(),
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        })
        .collect();
    TableSchema::new(name, 0, columns, Vec::new())
}

fn apply_column_aliases(schema: &mut TableSchema, aliases: &[Ident]) {
    for (idx, ident) in aliases.iter().enumerate() {
        if let Some(col) = schema.columns.get_mut(idx) {
            col.name = normalize_sql_ident(ident);
        }
    }
}

fn base_table_name(full: &str) -> &str {
    full.rsplit('.').next().unwrap_or(full)
}

async fn infer_query_output_columns(
    store: &Arc<TikvStore>,
    session: &mut Session,
    query: &Query,
) -> Option<Vec<InferredColumn>> {
    let search_path = session.search_path().to_vec();
    let search_path = search_path.as_slice();
    let db_id = session.current_database_id();
    let outer_ctes: HashMap<String, TableSchema> = HashMap::new();

    if let Some(txn) = session.get_mut_txn() {
        infer_query_output_columns_with_txn(
            store.as_ref(),
            txn,
            db_id,
            search_path,
            query,
            &outer_ctes,
        )
        .await
    } else {
        let mut temp_txn = store.begin().await.ok()?;
        let cols = infer_query_output_columns_with_txn(
            store.as_ref(),
            &mut temp_txn,
            db_id,
            search_path,
            query,
            &outer_ctes,
        )
        .await;
        let _ = temp_txn.rollback().await;
        cols
    }
}

async fn build_cte_schemas_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    query: &Query,
    outer_ctes: &HashMap<String, TableSchema>,
) -> Option<HashMap<String, TableSchema>> {
    let with = query.with.as_ref()?;
    if with.recursive {
        return None;
    }

    let mut ctes = outer_ctes.clone();
    for cte in &with.cte_tables {
        let cte_name = normalize_sql_ident(&cte.alias.name);
        let mut cols = Box::pin(infer_query_output_columns_with_txn(
            store,
            txn,
            db_id,
            search_path,
            &cte.query,
            &ctes,
        ))
        .await?;

        if !cte.alias.columns.is_empty() {
            for (idx, ident) in cte.alias.columns.iter().enumerate() {
                if let Some(col) = cols.get_mut(idx) {
                    col.name = normalize_sql_ident(ident);
                }
            }
        }

        ctes.insert(
            cte_name.clone(),
            schema_from_inferred_columns(cte_name, &cols),
        );
    }
    Some(ctes)
}

async fn infer_query_output_columns_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    query: &Query,
    outer_ctes: &HashMap<String, TableSchema>,
) -> Option<Vec<InferredColumn>> {
    let ctes = build_cte_schemas_with_txn(store, txn, db_id, search_path, query, outer_ctes)
        .await
        .unwrap_or_else(|| outer_ctes.clone());

    infer_setexpr_output_columns_with_txn(store, txn, db_id, search_path, &query.body, &ctes).await
}

async fn infer_setexpr_output_columns_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    body: &SetExpr,
    ctes: &HashMap<String, TableSchema>,
) -> Option<Vec<InferredColumn>> {
    match body {
        SetExpr::Select(select) => {
            infer_select_output_columns_with_txn(store, txn, db_id, search_path, select, ctes).await
        }
        SetExpr::Values(values) => infer_values_output_columns(values),
        SetExpr::SetOperation { left, .. } => {
            Box::pin(infer_setexpr_output_columns_with_txn(
                store,
                txn,
                db_id,
                search_path,
                left,
                ctes,
            ))
            .await
        }
        _ => None,
    }
}

fn infer_values_output_columns(values: &Values) -> Option<Vec<InferredColumn>> {
    let first = values.rows.first()?;
    let ctx = TypeContext::empty();
    let mut inferrer = TypeInferrer::new(ctx);

    Some(
        first
            .iter()
            .enumerate()
            .map(|(idx, expr)| InferredColumn {
                name: format!("column{}", idx + 1),
                data_type: inferrer.infer(expr).unwrap_or(DataType::Text),
            })
            .collect(),
    )
}

async fn collect_sources_from_table_with_joins(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    twj: &TableWithJoins,
    ctes: &HashMap<String, TableSchema>,
    out: &mut Vec<SourceSchema>,
) -> Option<()> {
    collect_sources_from_table_factor(store, txn, db_id, search_path, &twj.relation, ctes, out)
        .await?;
    for join in &twj.joins {
        collect_sources_from_table_factor(
            store,
            txn,
            db_id,
            search_path,
            &join.relation,
            ctes,
            out,
        )
        .await?;
    }
    Some(())
}

async fn collect_sources_from_table_factor(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    factor: &TableFactor,
    ctes: &HashMap<String, TableSchema>,
    out: &mut Vec<SourceSchema>,
) -> Option<()> {
    fn infer_generate_series_schema(
        args: &[FunctionArg],
        alias_name: &str,
        table_alias: Option<&sqlparser::ast::TableAlias>,
    ) -> Option<TableSchema> {
        if args.len() < 2 {
            return None;
        }

        fn extract_expr(arg: &FunctionArg) -> Option<&Expr> {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(expr),
                FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => Some(expr),
                _ => None,
            }
        }

        let start_expr = extract_expr(&args[0])?;
        let stop_expr = extract_expr(&args[1])?;

        let mut inferrer = TypeInferrer::new(TypeContext::empty());
        let start_type = inferrer.infer(start_expr).ok()?;
        let stop_type = inferrer.infer(stop_expr).ok()?;

        let data_type = match (&start_type, &stop_type) {
            (DataType::Int32, DataType::Int32) => DataType::Int32,
            (DataType::Int64, DataType::Int64) => DataType::Int64,
            (DataType::Int32, DataType::Int64) | (DataType::Int64, DataType::Int32) => {
                DataType::Int64
            }
            (DataType::Float64, DataType::Float64) => DataType::Float64,
            (DataType::Numeric { .. }, DataType::Numeric { .. }) => DataType::Numeric {
                precision: None,
                scale: None,
            },
            (DataType::Date, DataType::Date) => DataType::TimestampTz,
            (DataType::Timestamp, DataType::Timestamp)
            | (DataType::TimestampTz, DataType::Timestamp)
            | (DataType::Timestamp, DataType::TimestampTz)
            | (DataType::TimestampTz, DataType::TimestampTz) => DataType::Timestamp,
            _ => DataType::Text,
        };

        let col_name = if let Some(ta) = table_alias {
            if !ta.columns.is_empty() {
                normalize_sql_ident(&ta.columns[0])
            } else {
                alias_name.to_string()
            }
        } else {
            "generate_series".to_string()
        };

        Some(TableSchema {
            table_id: 0,
            name: "generate_series".to_string(),
            columns: vec![ColumnDef {
                name: col_name,
                data_type,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            version: 1,
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        })
    }

    fn infer_extension_table_function_schema(
        search_path: &[String],
        schema_opt: Option<&str>,
        func_name: &str,
    ) -> Option<TableSchema> {
        let in_extensions_schema = match schema_opt {
            Some(schema) => schema.eq_ignore_ascii_case(crate::extensions::EXTENSIONS_SCHEMA),
            None => search_path
                .iter()
                .any(|s| s.eq_ignore_ascii_case(crate::extensions::EXTENSIONS_SCHEMA)),
        };
        if !in_extensions_schema {
            return None;
        }
        crate::extensions::http::table_function_schema(func_name)
    }

    match factor {
        TableFactor::Table {
            name, alias, args, ..
        } => {
            let (schema_opt, obj_name_norm) = split_object_name_for_catalog(name)?;
            let alias_name = alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_else(|| name.0.last().map(|i| i.value.clone()).unwrap_or_default());
            if alias_name.is_empty() {
                return None;
            }

            let mut schema = if let Some(args) = args {
                if obj_name_norm.eq_ignore_ascii_case("generate_series") {
                    infer_generate_series_schema(args, &alias_name, alias.as_ref())?
                } else {
                    infer_extension_table_function_schema(
                        search_path,
                        schema_opt.as_deref(),
                        &obj_name_norm,
                    )?
                }
            } else if schema_opt.is_none() {
                match ctes.get(&obj_name_norm) {
                    Some(cte_schema) => cte_schema.clone(),
                    None => {
                        resolve_table_schema_for_object_name(store, txn, db_id, name, search_path)
                            .await?
                    }
                }
            } else {
                resolve_table_schema_for_object_name(store, txn, db_id, name, search_path).await?
            };

            if let Some(alias) = alias {
                if !alias.columns.is_empty() {
                    apply_column_aliases(&mut schema, &alias.columns);
                }
            }

            out.push(SourceSchema {
                alias: alias_name,
                schema,
            });
            Some(())
        }
        TableFactor::Derived {
            subquery, alias, ..
        } => {
            let alias = alias.as_ref()?;
            let alias_name = alias.name.value.clone();
            if alias_name.is_empty() {
                return None;
            }

            let cols = infer_query_output_columns_with_txn(
                store,
                txn,
                db_id,
                search_path,
                subquery.as_ref(),
                ctes,
            );
            let mut cols = Box::pin(cols).await?;

            if !alias.columns.is_empty() {
                for (idx, ident) in alias.columns.iter().enumerate() {
                    if let Some(col) = cols.get_mut(idx) {
                        col.name = normalize_sql_ident(ident);
                    }
                }
            }

            out.push(SourceSchema {
                alias: alias_name.clone(),
                schema: schema_from_inferred_columns(alias_name, &cols),
            });
            Some(())
        }
        _ => None,
    }
}

async fn infer_select_output_columns_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    select: &Select,
    ctes: &HashMap<String, TableSchema>,
) -> Option<Vec<InferredColumn>> {
    let mut sources = Vec::new();
    for twj in &select.from {
        collect_sources_from_table_with_joins(
            store,
            txn,
            db_id,
            search_path,
            twj,
            ctes,
            &mut sources,
        )
        .await?;
    }

    let join_wildcard_plan = if sources.is_empty() {
        None
    } else {
        let schema_refs: Vec<&TableSchema> = sources.iter().map(|s| &s.schema).collect();
        crate::sql::wildcard::build_join_wildcard_plan(select, &schema_refs)
    };

    let mut ctx = TypeContext::empty();
    for src in &sources {
        ctx.add_table(&src.alias, &src.schema);
    }
    let mut inferrer = TypeInferrer::new(ctx);

    let mut out_cols = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(_) => {
                if sources.is_empty() {
                    return None;
                }
                if let Some(plan) = join_wildcard_plan.as_ref().filter(|p| p.any_merge) {
                    out_cols.extend(plan.columns.iter().map(|c| InferredColumn {
                        name: c.name.clone(),
                        data_type: c.data_type.clone(),
                    }));
                } else {
                    for src in &sources {
                        out_cols.extend(src.schema.columns.iter().map(|c| InferredColumn {
                            name: c.name.clone(),
                            data_type: c.data_type.clone(),
                        }));
                    }
                }
            }
            SelectItem::QualifiedWildcard(obj, _) => {
                if sources.is_empty() {
                    return None;
                }
                let target = obj.0.last().map(|i| i.value.as_str())?;
                let src = sources.iter().find(|s| {
                    s.alias.eq_ignore_ascii_case(target)
                        || base_table_name(&s.schema.name).eq_ignore_ascii_case(target)
                })?;
                out_cols.extend(src.schema.columns.iter().map(|c| InferredColumn {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                }));
            }
            SelectItem::UnnamedExpr(expr) => {
                out_cols.push(InferredColumn {
                    name: select_item_output_name(item),
                    data_type: inferrer.infer(expr).unwrap_or(DataType::Text),
                });
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                out_cols.push(InferredColumn {
                    name: alias.value.clone(),
                    data_type: inferrer.infer(expr).unwrap_or(DataType::Text),
                });
            }
        }
    }

    Some(out_cols)
}

async fn infer_result_fields_from_query_ast(
    store: &Arc<TikvStore>,
    session: &mut Session,
    query: &str,
) -> Vec<FieldInfo> {
    let query_trimmed = query.trim();
    if query_trimmed.is_empty() {
        return vec![];
    }

    let parsed_stmt = crate::sql::parse_sql(query_trimmed)
        .ok()
        .and_then(|stmts| stmts.into_iter().next());
    let query_upper = query_trimmed.to_uppercase();

    let is_select_str = query_upper.starts_with("SELECT") || query_upper.starts_with("WITH");
    let has_returning_str = query_upper.contains("RETURNING");
    let should_infer = matches!(
        parsed_stmt,
        Some(Statement::Query(_))
            | Some(Statement::Insert {
                returning: Some(_),
                ..
            })
            | Some(Statement::Update {
                returning: Some(_),
                ..
            })
            | Some(Statement::Delete {
                returning: Some(_),
                ..
            })
    ) || (parsed_stmt.is_none() && (is_select_str || has_returning_str));

    if !should_infer {
        return vec![];
    }

    if let Some(ref stmt) = parsed_stmt {
        if let Some(fields) = infer_returning_fields_from_statement(store, session, stmt).await {
            return fields;
        }
    }

    // Only infer SELECT (Statement::Query) metadata here; RETURNING is handled above.
    let is_select = matches!(parsed_stmt, Some(Statement::Query(_)))
        || (parsed_stmt.is_none() && is_select_str);
    if !is_select {
        return stub_describe_field();
    }

    let stmt = match parsed_stmt {
        Some(stmt) => stmt,
        None => return stub_describe_field(),
    };

    match stmt {
        Statement::Query(q) => match infer_query_output_columns(store, session, &q).await {
            Some(cols) => inferred_columns_to_fields(cols),
            None => stub_describe_field(),
        },
        _ => stub_describe_field(),
    }
}

fn strip_leading_whitespace_and_comments(query: &str) -> Option<&str> {
    let bytes = query.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        // Line comment: -- ... \n
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() {
                let is_newline = bytes[i] == b'\n';
                i += 1;
                if is_newline {
                    break;
                }
            }
            continue;
        }

        // Block comment (supports nesting): /* ... */
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            let mut depth = 1usize;
            while i < bytes.len() && depth > 0 {
                if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                    continue;
                }
                if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            if depth > 0 {
                return None;
            }
            continue;
        }

        break;
    }

    Some(&query[i..])
}

pub struct DynamicPgHandler {
    client_pool: Option<Arc<TikvClientPool>>,
    pd_endpoints: Vec<String>,
    default_keyspace: Option<String>,
    executor: OnceCell<Arc<Executor>>,
    session: Mutex<Option<Session>>,
    connection_guard: OnceCell<observability::ConnectionGuard>,
    copy_context: Mutex<Option<CopyContext>>,
    suspended_portals: Mutex<HashMap<String, SuspendedPortalState>>,
    query_parser: Arc<TipgQueryParser>,
    connection_id: i32,
}

impl DynamicPgHandler {
    #[allow(dead_code)]
    pub fn new(pd_endpoints: Vec<String>, default_keyspace: Option<String>) -> Self {
        Self {
            client_pool: None,
            pd_endpoints,
            default_keyspace,
            executor: OnceCell::new(),
            session: Mutex::new(None),
            connection_guard: OnceCell::new(),
            copy_context: Mutex::new(None),
            suspended_portals: Mutex::new(HashMap::new()),
            query_parser: Arc::new(TipgQueryParser::new()),
            connection_id: CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn new_with_pool(
        client_pool: Arc<TikvClientPool>,
        default_keyspace: Option<String>,
    ) -> Self {
        Self {
            client_pool: Some(client_pool),
            pd_endpoints: Vec::new(),
            default_keyspace,
            executor: OnceCell::new(),
            session: Mutex::new(None),
            connection_guard: OnceCell::new(),
            copy_context: Mutex::new(None),
            suspended_portals: Mutex::new(HashMap::new()),
            query_parser: Arc::new(TipgQueryParser::new()),
            connection_id: CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    #[allow(dead_code)]
    pub fn connection_id(&self) -> i32 {
        self.connection_id
    }

    async fn infer_insert_parameter_types(
        &self,
        sql: &str,
        param_count: usize,
    ) -> Option<Vec<Type>> {
        if param_count == 0 {
            return Some(vec![]);
        }

        let parsed = crate::sql::parse_sql(sql).ok()?;
        let stmt = parsed.into_iter().next()?;

        let (table_name, columns, values_list): (String, Vec<String>, Vec<Vec<Expr>>) = match stmt {
            Statement::Insert {
                table_name,
                columns,
                source,
                ..
            } => {
                let table_name_str = table_name.to_string();
                let columns: Vec<String> = columns.iter().map(|c| c.value.clone()).collect();
                let values = match source.as_ref() {
                    Some(src) => src,
                    None => return None,
                };
                if let sqlparser::ast::SetExpr::Values(v) = values.body.as_ref() {
                    (table_name_str, columns, v.rows.clone())
                } else {
                    return None;
                }
            }
            _ => return None,
        };

        info!(
            "infer_insert_parameter_types: table={}, columns={:?}, param_count={}",
            table_name, columns, param_count
        );

        let executor = self.executor.get()?;
        let store = executor.store();

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut()?;
        let mut txn = store.begin().await.ok()?;

        let search_path = session.search_path();
        let resolved_table = resolve_table_for_insert(&table_name, search_path);

        info!(
            "infer_insert_parameter_types: resolved_table={}",
            resolved_table
        );

        let schema = store
            .get_schema(&mut txn, session.current_database_id(), &resolved_table)
            .await
            .ok()??;
        let _ = txn.rollback().await;

        info!(
            "infer_insert_parameter_types: got schema with {} columns",
            schema.columns.len()
        );

        let col_types: std::collections::HashMap<String, DataType> = schema
            .columns
            .iter()
            .map(|c| (c.name.to_lowercase(), c.data_type.clone()))
            .collect();

        let column_order: Vec<String> = if !columns.is_empty() {
            columns.iter().map(|c: &String| c.to_lowercase()).collect()
        } else {
            schema
                .columns
                .iter()
                .map(|c| c.name.to_lowercase())
                .collect()
        };

        let mut types = vec![Type::TEXT; param_count];

        if let Some(first_row) = values_list.first() {
            let mut param_idx = 0usize;
            for (col_idx, expr) in first_row.iter().enumerate() {
                let col_name = column_order.get(col_idx)?;
                let col_type = col_types.get(col_name);

                let placeholders = count_placeholders_in_expr(expr);
                for _ in 0..placeholders {
                    if param_idx < param_count {
                        types[param_idx] = datatype_to_pgtype(col_type);
                        param_idx += 1;
                    }
                }
            }
        }

        Some(types)
    }

    async fn infer_update_parameter_types(
        &self,
        sql: &str,
        param_count: usize,
    ) -> Option<Vec<Type>> {
        if param_count == 0 {
            return Some(vec![]);
        }

        let parsed = crate::sql::parse_sql(sql).ok()?;
        let stmt = parsed.into_iter().next()?;

        let (table_name, assignments) = match stmt {
            Statement::Update {
                table, assignments, ..
            } => {
                let table_name = match &table.relation {
                    sqlparser::ast::TableFactor::Table { name, .. } => name.to_string(),
                    _ => return None,
                };
                (table_name, assignments)
            }
            _ => return None,
        };

        info!(
            "infer_update_parameter_types: table={}, assignments={}, param_count={}",
            table_name,
            assignments.len(),
            param_count
        );

        let executor = self.executor.get()?;
        let store = executor.store();

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut()?;
        let mut txn = store.begin().await.ok()?;

        let search_path = session.search_path();
        let resolved_table = resolve_table_for_insert(&table_name, search_path);

        info!(
            "infer_update_parameter_types: resolved_table={}",
            resolved_table
        );

        let schema = store
            .get_schema(&mut txn, session.current_database_id(), &resolved_table)
            .await
            .ok()??;
        let _ = txn.rollback().await;

        info!(
            "infer_update_parameter_types: got schema with {} columns",
            schema.columns.len()
        );

        let col_types: std::collections::HashMap<String, DataType> = schema
            .columns
            .iter()
            .map(|c| (c.name.to_lowercase(), c.data_type.clone()))
            .collect();

        let mut types = vec![Type::TEXT; param_count];
        let mut param_idx = 0usize;

        for assignment in &assignments {
            let col_names: Vec<String> = assignment
                .id
                .iter()
                .map(|ident| ident.value.to_lowercase())
                .collect();

            if let Some(col_name) = col_names.last() {
                let col_type = col_types.get(col_name);
                let placeholders = count_placeholders_in_expr(&assignment.value);

                for _ in 0..placeholders {
                    if param_idx < param_count {
                        types[param_idx] = datatype_to_pgtype(col_type);
                        param_idx += 1;
                    }
                }
            }
        }

        info!(
            "infer_update_parameter_types: inferred {} types, remaining {} as TEXT",
            param_idx,
            param_count - param_idx
        );

        Some(types)
    }

    async fn infer_select_parameter_types(
        &self,
        sql: &str,
        param_count: usize,
    ) -> Option<Vec<Type>> {
        if param_count == 0 {
            return Some(vec![]);
        }

        let parsed = crate::sql::parse_sql(sql).ok()?;
        let stmt = parsed.into_iter().next()?;

        let (table_name, selection, limit_expr, offset_expr) = match stmt {
            Statement::Query(query) => {
                if let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() {
                    let table_name = select.from.first().and_then(|f| match &f.relation {
                        sqlparser::ast::TableFactor::Table { name, .. } => Some(name.to_string()),
                        _ => None,
                    })?;
                    (
                        table_name,
                        select.selection.clone(),
                        query.limit.clone(),
                        query.offset.clone(),
                    )
                } else {
                    return None;
                }
            }
            _ => return None,
        };

        info!(
            "infer_select_parameter_types: table={}, param_count={}",
            table_name, param_count
        );

        let executor = self.executor.get()?;
        let store = executor.store();

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut()?;
        let mut txn = store.begin().await.ok()?;

        let search_path = session.search_path();
        let resolved_table = resolve_table_for_insert(&table_name, search_path);

        let schema = store
            .get_schema(&mut txn, session.current_database_id(), &resolved_table)
            .await
            .ok()??;
        let _ = txn.rollback().await;

        let col_types: std::collections::HashMap<String, DataType> = schema
            .columns
            .iter()
            .map(|c| (c.name.to_lowercase(), c.data_type.clone()))
            .collect();

        let mut types = vec![Type::TEXT; param_count];

        // Infer types from WHERE clause
        if let Some(ref sel) = selection {
            infer_types_from_expr(sel, &col_types, &mut types);
        }

        // LIMIT placeholder should be INT8
        if let Some(ref limit) = limit_expr {
            if let Some(idx) = extract_placeholder_index_from_expr(limit) {
                if idx < types.len() {
                    types[idx] = Type::INT8;
                }
            }
        }

        // OFFSET placeholder should be INT8
        if let Some(ref offset) = offset_expr {
            if let Some(idx) = extract_placeholder_index_from_expr(&offset.value) {
                if idx < types.len() {
                    types[idx] = Type::INT8;
                }
            }
        }

        info!("infer_select_parameter_types: inferred types={:?}", types);

        Some(types)
    }

    async fn infer_result_fields_from_query(&self, query: &str) -> Vec<FieldInfo> {
        // Get the executor if initialized
        let executor = match self.executor.get() {
            Some(exec) => exec,
            None => {
                // Executor not initialized yet, return stub
                return stub_describe_field();
            }
        };

        // Get session lock
        let mut session_guard = self.session.lock().await;

        // Check if session exists
        if session_guard.is_none() {
            // No session yet, return stub
            return stub_describe_field();
        }

        let store = executor.store();
        let session = session_guard.as_mut().unwrap();
        infer_result_fields_from_query_ast(&store, session, query).await
    }

    async fn init_executor(
        &self,
        keyspace: Option<String>,
        username: Option<String>,
        is_superuser: bool,
        database: String,
    ) -> Result<(), String> {
        let effective_keyspace = keyspace
            .or_else(|| self.default_keyspace.clone())
            .unwrap_or_else(|| "default".to_string());

        let tenant_obs = observability::registry().tenant(&effective_keyspace);
        if self.connection_guard.get().is_none() {
            let _ = self.connection_guard.set(tenant_obs.connection_open());
        }

        let store = if let Some(pool) = &self.client_pool {
            pool.get_client(Some(effective_keyspace.clone()))
                .await
                .map_err(|e| format!("Failed to get client from pool: {}", e))?
        } else {
            let s = TikvStore::new_with_keyspace(
                self.pd_endpoints.clone(),
                Some(effective_keyspace.clone()),
            )
            .await
            .map_err(|e| format!("Failed to connect to TiKV: {}", e))?;
            Arc::new(s)
        };

        let executor = Arc::new(Executor::new(
            store.clone(),
            effective_keyspace.clone(),
            tenant_obs.clone(),
        ));

        let database_name = database.trim();
        let database_name = if database_name.is_empty() {
            "postgres"
        } else {
            database_name
        };
        let database_name = database_name.to_ascii_lowercase();

        let mut db_txn = store.begin().await.map_err(|e| e.to_string())?;
        let database_id = match store
            .get_database_id(&mut db_txn, &database_name)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(id) => id,
            None => {
                db_txn.rollback().await.ok();
                return Err(format!("database \"{}\" does not exist", database_name));
            }
        };
        db_txn.rollback().await.ok();

        let session = match username {
            Some(user) => Session::new_with_user_and_database(
                store,
                tenant_obs,
                user,
                is_superuser,
                self.connection_id,
                database_id,
                database_name,
            ),
            None => Session::new_with_database(
                store,
                tenant_obs,
                self.connection_id,
                database_id,
                database_name,
            ),
        };

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

    fn parse_copy_command(query: &str) -> Option<(String, Vec<String>)> {
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
        let re =
            regex::Regex::new(r"(?i)^COPY\s+(?:(\w+)\.)?(\w+)\s*\(([^)]+)\)\s+FROM\s+stdin")
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

    fn parse_copy_to_command(query: &str) -> Option<(String, Vec<String>)> {
        let query = strip_leading_whitespace_and_comments(query)?;
        let query_upper = query.to_uppercase();
        // COPY TO STDOUT must start at statement start (after leading whitespace/comments).
        if !query_upper.starts_with("COPY") || !query_upper.contains("TO") {
            return None;
        }
        if !query_upper.contains("STDOUT") {
            return None;
        }

        // COPY [schema.]table (col1, col2) TO STDOUT
        let re =
            regex::Regex::new(r"(?i)^COPY\s+(?:(\w+)\.)?(\w+)\s*\(([^)]+)\)\s+TO\s+STDOUT").ok()?;
        if let Some(caps) = re.captures(query) {
            let schema = caps.get(1).map(|m| m.as_str().to_string());
            let table = caps.get(2)?.as_str().to_string();
            let table_name = match schema {
                Some(s) => format!("{}.{}", s, table),
                None => table,
            };
            let columns: Vec<String> = caps
                .get(3)?
                .as_str()
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            return Some((table_name, columns));
        }

        // COPY [schema.]table TO STDOUT (no columns)
        let re2 = regex::Regex::new(r"(?i)^COPY\s+(?:(\w+)\.)?(\w+)\s+TO\s+STDOUT").ok()?;
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

    async fn handle_copy_to_stdout<'a, C>(
        &self,
        client: &mut C,
        table_name: &str,
        columns: &[String],
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

        let rows = match result {
            crate::sql::ExecuteResult::Select { rows, .. } => rows,
            _ => {
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    "COPY TO requires a table".to_string(),
                ))));
            }
        };

        drop(session_guard);

        let col_count = if let Some(first) = rows.first() {
            first.values.len()
        } else {
            1
        };
        let column_formats: Vec<i16> = vec![0; col_count];
        let copy_resp = CopyResponse::new(0, col_count, column_formats);
        pgwire::api::copy::send_copy_out_response(client, copy_resp).await?;

        let mut buf = Vec::with_capacity(4096);
        for row in &rows {
            buf.clear();
            super::copy_format::encode_row(&row.values, &mut buf);
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

    fn ensure_auth_bootstrapped(bootstrap_result: Result<(), anyhow::Error>) -> Result<(), String> {
        bootstrap_result.map_err(|e| format!("Failed to bootstrap auth: {}", e))
    }

    async fn authenticate_user(
        &self,
        keyspace: &Option<String>,
        username: &str,
        password: &str,
    ) -> Result<(bool, bool), String> {
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
        let bootstrap_result = async {
            let mut txn = store.begin().await?;
            auth_manager.bootstrap(&mut txn).await?;
            txn.commit().await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;

        Self::ensure_auth_bootstrapped(bootstrap_result)?;

        let mut txn = store
            .begin()
            .await
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        match auth_manager
            .authenticate(&mut txn, username, password)
            .await
        {
            Ok(Some(user)) => {
                txn.commit()
                    .await
                    .map_err(|e| format!("Failed to commit: {}", e))?;
                Ok((true, user.is_superuser))
            }
            Ok(None) => {
                txn.rollback().await.ok();
                Ok((false, false))
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(format!("Authentication error: {}", e))
            }
        }
    }
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

                if let Some(raw_user) = client.metadata().get(METADATA_USER).cloned() {
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
                }

                client.set_state(PgWireConnectionState::AuthenticationInProgress);
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
                    .unwrap_or_else(|| "admin".to_string());
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
                            if let Err(e) = self
                                .init_executor(
                                    keyspace.clone(),
                                    Some(actual_user.clone()),
                                    is_superuser,
                                    database,
                                )
                                .await
                            {
                                let error_info =
                                    ErrorInfo::new("FATAL".to_owned(), "XX000".to_owned(), e);
                                client
                                    .feed(PgWireBackendMessage::ErrorResponse(ErrorResponse::from(
                                        error_info,
                                    )))
                                    .await?;
                                client.close().await?;
                                return Ok(());
                            }

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

                            pgwire::api::auth::finish_authentication(
                                client,
                                &PgServerParameterProvider,
                            )
                            .await?;
                            debug!(
                                "Authentication successful for user '{}' with keyspace {:?}",
                                actual_user, keyspace
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
                            client
                                .feed(PgWireBackendMessage::ErrorResponse(ErrorResponse::from(
                                    error_info,
                                )))
                                .await?;
                            client.close().await?;
                        }
                    }
                    Err(e) => {
                        let error_info = ErrorInfo::new("FATAL".to_owned(), "XX000".to_owned(), e);
                        client
                            .feed(PgWireBackendMessage::ErrorResponse(ErrorResponse::from(
                                error_info,
                            )))
                            .await?;
                        client.close().await?;
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

        if let Some((table_name, columns)) = Self::parse_copy_to_command(query) {
            debug!(
                "COPY TO STDOUT: table={}, columns={:?}",
                table_name, columns
            );
            return self
                .handle_copy_to_stdout(client, &table_name, &columns)
                .await;
        }

        if let Some((table_name, columns)) = Self::parse_copy_command(query) {
            debug!(
                "COPY FROM STDIN: table={}, columns={:?}",
                table_name, columns
            );

            let (resolved_table, resolved_columns, column_types, col_count, started_txn) = {
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
                )
            };

            let mut ctx = self.copy_context.lock().await;
            *ctx = Some(CopyContext {
                table_name: resolved_table,
                columns: resolved_columns,
                column_types,
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

        set_connection_id(session.connection_id());

        match executor.execute(session, query).await {
            Ok(results) => {
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

        let (parse_res, table_name, started_txn) = {
            let mut ctx_guard = self.copy_context.lock().await;
            let Some(ctx) = ctx_guard.as_mut() else {
                return Ok(());
            };

            let table_name = ctx.table_name.clone();
            let started_txn = ctx.started_txn;

            let parse_res = (|| -> PgWireResult<Vec<Vec<(String, Value)>>> {
                if ctx.reached_end_marker {
                    return Ok(Vec::new());
                }

                let lines = ctx.push_copy_data(copy_data.data.as_ref())?;
                if lines.is_empty() {
                    return Ok(Vec::new());
                }

                let mut rows_to_insert: Vec<Vec<(String, Value)>> = Vec::with_capacity(lines.len());
                for line_bytes in lines {
                    if ctx.reached_end_marker {
                        break;
                    }
                    if line_bytes.as_slice() == b"\\." {
                        ctx.reached_end_marker = true;
                        ctx.line_buffer.clear();
                        break;
                    }

                    let line = String::from_utf8_lossy(&line_bytes);
                    let values: Vec<&str> = line.split('\t').collect();

                    if values.len() != ctx.columns.len() {
                        return Err(copy_row_column_mismatch_error(
                            values.len(),
                            ctx.columns.len(),
                        ));
                    }

                    let mut col_values: Vec<(String, Value)> =
                        Vec::with_capacity(ctx.columns.len());
                    for ((col_name, col_type), val) in ctx
                        .columns
                        .iter()
                        .zip(ctx.column_types.iter())
                        .zip(values.iter())
                    {
                        let value = if *val == "\\N" {
                            Value::Null
                        } else if let Some(dt) = col_type.as_ref() {
                            executor.parse_value_for_copy(val, dt).map_err(|e| {
                                PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "22P02".to_string(),
                                    e.to_string(),
                                )))
                            })?
                        } else {
                            Value::Text(val.to_string())
                        };
                        col_values.push((col_name.clone(), value));
                    }
                    rows_to_insert.push(col_values);
                }

                Ok(rows_to_insert)
            })();

            (parse_res, table_name, started_txn)
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

        let insert_res: PgWireResult<()> = async {
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
                for col_values in rows_to_insert {
                    executor
                        .execute_copy_insert(session, &table_name, col_values)
                        .await
                        .map_err(|e| {
                            error!("COPY insert error: {}", e);
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "XX000".to_string(),
                                e.to_string(),
                            )))
                        })?;
                }

                Ok::<(), PgWireError>(())
            })
            .await?;

            Ok(())
        }
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
                let line = String::from_utf8_lossy(&final_line_bytes);
                let values: Vec<&str> = line.split('\t').collect();

                if values.len() != ctx.columns.len() {
                    rollback_autocommit_or_mark_failed(session, ctx.started_txn).await;
                    return Err(copy_row_column_mismatch_error(
                        values.len(),
                        ctx.columns.len(),
                    ));
                }

                let mut col_values: Vec<(String, Value)> = Vec::with_capacity(ctx.columns.len());
                for ((col_name, col_type), val) in ctx
                    .columns
                    .iter()
                    .zip(ctx.column_types.iter())
                    .zip(values.iter())
                {
                    let value = if *val == "\\N" {
                        Value::Null
                    } else if let Some(dt) = col_type.as_ref() {
                        executor.parse_value_for_copy(val, dt).map_err(|e| {
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "22P02".to_string(),
                                e.to_string(),
                            )))
                        })?
                    } else {
                        Value::Text(val.to_string())
                    };
                    col_values.push((col_name.clone(), value));
                }

                let savepoints = session.savepoints();
                let insert_res = crate::txn::with_savepoints(savepoints, async {
                    executor
                        .execute_copy_insert(session, &ctx.table_name, col_values)
                        .await
                        .map_err(|e| {
                            error!("COPY insert error: {}", e);
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "XX000".to_string(),
                                e.to_string(),
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

            ctx.row_count
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
    type Statement = String;
    type QueryParser = TipgQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
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
            let portal = Portal::try_new(&message, statement)?;
            client.portal_store().put_portal(Arc::new(portal));
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
        let query = &portal.statement.statement;
        debug!("Extended query: {}", query);

        let final_query = substitute_parameters(query, portal)?;
        debug!("Final query after substitution: {}", final_query);

        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "XX000".to_string(),
                "Session not initialized".to_string(),
            )))
        })?;

        set_connection_id(session.connection_id());

        match executor.execute(session, &final_query).await {
            Ok(results) => Ok(send_notices_and_get_last_response(
                client,
                session.show_setting_value("client_min_messages"),
                results,
            )
            .await?),
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
        let param_count = count_sql_parameters(&stmt.statement);
        let mut param_types: Vec<Type> = stmt.parameter_types.clone();

        info!(
            "do_describe_statement: sql={}, param_count={}, initial_types={:?}",
            stmt.statement.chars().take(100).collect::<String>(),
            param_count,
            param_types
        );

        if param_types.len() < param_count || param_types.iter().any(|t| *t == Type::UNKNOWN) {
            let inferred = if let Some(insert_types) = self
                .infer_insert_parameter_types(&stmt.statement, param_count)
                .await
            {
                info!(
                    "do_describe_statement: inferred INSERT types={:?}",
                    insert_types
                );
                insert_types
            } else if let Some(update_types) = self
                .infer_update_parameter_types(&stmt.statement, param_count)
                .await
            {
                info!(
                    "do_describe_statement: inferred UPDATE types={:?}",
                    update_types
                );
                update_types
            } else if let Some(select_types) = self
                .infer_select_parameter_types(&stmt.statement, param_count)
                .await
            {
                info!(
                    "do_describe_statement: inferred SELECT types={:?}",
                    select_types
                );
                select_types
            } else {
                let fallback = infer_parameter_types(&stmt.statement, param_count);
                info!("do_describe_statement: fallback types={:?}", fallback);
                fallback
            };
            for i in param_types.len()..param_count {
                param_types.push(inferred[i].clone());
            }
            for i in 0..param_types.len().min(inferred.len()) {
                if param_types[i] == Type::UNKNOWN && inferred[i] != Type::UNKNOWN {
                    param_types[i] = inferred[i].clone();
                }
            }
        }

        info!("do_describe_statement: final_types={:?}", param_types);

        let query_for_inference = if param_count == 0 {
            stmt.statement.clone()
        } else {
            let mut values: Vec<String> = Vec::with_capacity(param_count);
            for i in 0..param_count {
                let t = param_types.get(i).cloned().unwrap_or(Type::UNKNOWN);
                values.push(dummy_sql_expr_for_param_type(&t));
            }
            substitute_placeholders_outside_strings_and_dollar(&stmt.statement, &values)
        };
        let fields = self
            .infer_result_fields_from_query(&query_for_inference)
            .await;
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
        let final_query = substitute_parameters(&portal.statement.statement, portal)?;
        let fields = self.infer_result_fields_from_query(&final_query).await;
        Ok(DescribePortalResponse::new(fields))
    }
}

#[cfg(test)]
fn replace_placeholders_for_inference(query: &str) -> String {
    let mut result = String::with_capacity(query.len());
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<char>> = None;
    let chars: Vec<char> = query.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if let Some(ref delim) = dollar_delim {
            if i + delim.len() <= chars.len() && chars[i..i + delim.len()] == delim[..] {
                result.extend(delim);
                i += delim.len();
                dollar_delim = None;
            } else {
                result.push(chars[i]);
                i += 1;
            }
            continue;
        }

        let c = chars[i];

        if c == '\'' && !in_double_quote {
            if in_single_quote && i + 1 < chars.len() && chars[i + 1] == '\'' {
                result.push('\'');
                result.push('\'');
                i += 2;
                continue;
            }
            in_single_quote = !in_single_quote;
            result.push(c);
            i += 1;
            continue;
        } else if c == '"' && !in_single_quote {
            if in_double_quote && i + 1 < chars.len() && chars[i + 1] == '"' {
                result.push('"');
                result.push('"');
                i += 2;
                continue;
            }
            in_double_quote = !in_double_quote;
            result.push(c);
            i += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote && c == '$' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                result.push('1');
                i = j;
                continue;
            }

            // Handle PostgreSQL dollar-quoted strings ($tag$ ... $tag$ or $$ ... $$)
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '$' {
                if !(chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    break;
                }
                j += 1;
            }
            if j < chars.len() && chars[j] == '$' {
                let delim: Vec<char> = chars[i..=j].to_vec();
                result.extend(&delim);
                dollar_delim = Some(delim);
                i = j + 1;
                continue;
            }
        }

        result.push(c);
        i += 1;
    }

    result
}

fn substitute_placeholders_outside_strings_and_dollar(query: &str, values: &[String]) -> String {
    let bytes = query.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(query.len());
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<u8>> = None;
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(ref delim) = dollar_delim {
            let delim_len = delim.len();
            if i + delim_len <= bytes.len() && &bytes[i..i + delim_len] == delim.as_slice() {
                out.extend_from_slice(delim);
                i += delim_len;
                dollar_delim = None;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
            continue;
        }

        let b = bytes[i];

        if b == b'\'' && !in_double_quote {
            if in_single_quote && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                out.push(b'\'');
                out.push(b'\'');
                i += 2;
                continue;
            }
            in_single_quote = !in_single_quote;
            out.push(b);
            i += 1;
            continue;
        }

        if b == b'"' && !in_single_quote {
            if in_double_quote && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                out.push(b'"');
                out.push(b'"');
                i += 2;
                continue;
            }
            in_double_quote = !in_double_quote;
            out.push(b);
            i += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote {
            // SQL comments
            if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                out.push(b'-');
                out.push(b'-');
                i += 2;
                while i < bytes.len() {
                    out.push(bytes[i]);
                    let is_newline = bytes[i] == b'\n';
                    i += 1;
                    if is_newline {
                        break;
                    }
                }
                continue;
            }
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                out.push(b'/');
                out.push(b'*');
                i += 2;
                let mut depth = 1usize;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                        out.push(b'/');
                        out.push(b'*');
                        depth += 1;
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        out.push(b'*');
                        out.push(b'/');
                        depth -= 1;
                        i += 2;
                        continue;
                    }
                    out.push(bytes[i]);
                    i += 1;
                }
                continue;
            }
        }

        if !in_single_quote && !in_double_quote && b == b'$' {
            // Prepared-statement placeholder: $1, $2, ...
            let mut j = i + 1;
            let mut saw_digit = false;
            let mut num = 0usize;
            while j < bytes.len() && bytes[j].is_ascii_digit() && j - i <= 10 {
                saw_digit = true;
                num = num
                    .saturating_mul(10)
                    .saturating_add((bytes[j] - b'0') as usize);
                j += 1;
            }
            if saw_digit {
                let before_ok = i == 0 || !is_ident_char_or_dollar(bytes[i - 1]);
                let after_ok = j == bytes.len() || !is_ident_char_or_dollar(bytes[j]);
                if before_ok && after_ok {
                    if num >= 1 && num <= values.len() {
                        out.extend_from_slice(values[num - 1].as_bytes());
                    } else {
                        out.extend_from_slice(&bytes[i..j]);
                    }
                    i = j;
                    continue;
                }
            }

            // PostgreSQL dollar-quoted strings ($tag$ ... $tag$ or $$ ... $$)
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'$' {
                if !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'$' {
                let delim = bytes[i..=j].to_vec();
                out.extend_from_slice(&delim);
                dollar_delim = Some(delim);
                i = j + 1;
                continue;
            }
        }

        out.push(b);
        i += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| query.to_string())
}

fn dummy_sql_expr_for_param_type(param_type: &Type) -> String {
    match param_type {
        t if *t == Type::BOOL => "NULL::bool".to_string(),
        t if *t == Type::INT2 => "NULL::int2".to_string(),
        t if *t == Type::INT4 => "NULL::int4".to_string(),
        t if *t == Type::INT8 => "NULL::int8".to_string(),
        t if *t == Type::FLOAT4 => "NULL::float4".to_string(),
        t if *t == Type::FLOAT8 => "NULL::float8".to_string(),
        t if *t == Type::TEXT || *t == Type::VARCHAR => "NULL::text".to_string(),
        t if *t == Type::TIMESTAMP => "NULL::timestamp".to_string(),
        t if *t == Type::TIMESTAMPTZ => "NULL::timestamptz".to_string(),
        t if *t == Type::UUID => "NULL::uuid".to_string(),
        t if *t == Type::DATE => "NULL::date".to_string(),
        t if *t == Type::BYTEA => "NULL::bytea".to_string(),
        t if *t == Type::JSON => "NULL::json".to_string(),
        t if *t == Type::JSONB => "NULL::jsonb".to_string(),
        t if *t == Type::NUMERIC => "NULL::numeric".to_string(),
        _ => "NULL".to_string(),
    }
}

fn substitute_parameters(query: &str, portal: &Portal<String>) -> PgWireResult<String> {
    let mut values: Vec<String> = Vec::with_capacity(portal.parameter_len());

    fn quote_sql_string_literal(value: &str) -> String {
        format!("'{}'", value.replace("'", "''"))
    }

    for i in 0..portal.parameter_len() {
        let param_type = portal
            .statement
            .parameter_types
            .get(i)
            .cloned()
            .unwrap_or(Type::UNKNOWN);

        let param = portal
            .parameters
            .get(i)
            .ok_or_else(|| PgWireError::ParameterIndexOutOfBound(i))?;

        let Some(param_bytes) = param.as_ref() else {
            values.push("NULL".to_string());
            continue;
        };

        let invalid_param = |message: String| -> PgWireError {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "22P02".to_string(),
                format!(
                    "invalid input syntax for parameter ${} ({}): {}",
                    i + 1,
                    param_type.name(),
                    message
                ),
            )))
        };

        let value_str = if portal.parameter_format.is_binary(i) {
            match &param_type {
                t if *t == Type::BOOL => match portal.parameter::<bool>(i, &param_type)? {
                    Some(v) => {
                        if v {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        }
                    }
                    None => "NULL".to_string(),
                },
                t if *t == Type::INT2 => match portal.parameter::<i16>(i, &param_type)? {
                    Some(v) => v.to_string(),
                    None => "NULL".to_string(),
                },
                t if *t == Type::INT4 => match portal.parameter::<i32>(i, &param_type)? {
                    Some(v) => v.to_string(),
                    None => "NULL".to_string(),
                },
                t if *t == Type::INT8 => match portal.parameter::<i64>(i, &param_type)? {
                    Some(v) => v.to_string(),
                    None => "NULL".to_string(),
                },
                t if *t == Type::FLOAT4 => match portal.parameter::<f32>(i, &param_type)? {
                    Some(v) => v.to_string(),
                    None => "NULL".to_string(),
                },
                t if *t == Type::FLOAT8 => match portal.parameter::<f64>(i, &param_type)? {
                    Some(v) => v.to_string(),
                    None => "NULL".to_string(),
                },
                t if *t == Type::TIMESTAMPTZ => {
                    use chrono::{DateTime, Utc};
                    match portal.parameter::<DateTime<Utc>>(i, &param_type)? {
                        Some(ts) => format!("'{}'", ts.format("%Y-%m-%d %H:%M:%S%.6f%:z")),
                        None => "NULL".to_string(),
                    }
                }
                t if *t == Type::TIMESTAMP => {
                    use chrono::NaiveDateTime;
                    match portal.parameter::<NaiveDateTime>(i, &param_type)? {
                        Some(ts) => format!("'{}'", ts.format("%Y-%m-%d %H:%M:%S%.6f")),
                        None => "NULL".to_string(),
                    }
                }
                t if *t == Type::UUID => {
                    let uuid = uuid::Uuid::from_slice(param_bytes.as_ref())
                        .map_err(|e| invalid_param(e.to_string()))?;
                    format!("{}::uuid", quote_sql_string_literal(&uuid.to_string()))
                }
                t if *t == Type::BYTEA => {
                    let hex = hex::encode(param_bytes.as_ref());
                    let repr = format!("\\x{}", hex);
                    format!("{}::bytea", quote_sql_string_literal(&repr))
                }
                t if *t == Type::TEXT => {
                    let s = std::str::from_utf8(param_bytes.as_ref())
                        .map_err(|e| invalid_param(e.to_string()))?;
                    quote_sql_string_literal(s)
                }
                t if *t == Type::JSON => {
                    let s = std::str::from_utf8(param_bytes.as_ref())
                        .map_err(|e| invalid_param(e.to_string()))?;
                    quote_sql_string_literal(s)
                }
                // Type::UNKNOWN (OID 705) - pgx/GORM sends binary unknown when type is not inferred.
                // Treat as text - decode UTF-8 and quote. If not valid UTF-8, try as integer.
                t if *t == Type::UNKNOWN => {
                    if let Ok(s) = std::str::from_utf8(param_bytes.as_ref()) {
                        // Try to parse as integer first (common case for LIMIT $1)
                        if let Ok(v) = s.trim().parse::<i64>() {
                            v.to_string()
                        } else {
                            quote_sql_string_literal(s)
                        }
                    } else if param_bytes.len() == 8 {
                        // Try as big-endian i64 (binary integer)
                        let arr: [u8; 8] = param_bytes.as_ref().try_into().unwrap();
                        i64::from_be_bytes(arr).to_string()
                    } else if param_bytes.len() == 4 {
                        // Try as big-endian i32 (binary integer)
                        let arr: [u8; 4] = param_bytes.as_ref().try_into().unwrap();
                        i32::from_be_bytes(arr).to_string()
                    } else {
                        // Fallback: hex encode as bytea
                        let hex = hex::encode(param_bytes.as_ref());
                        format!("'\\x{}'::bytea", hex)
                    }
                }
                _ => {
                    return Err(invalid_param(format!(
                        "unsupported binary parameter type {}",
                        param_type.name()
                    )));
                }
            }
        } else {
            let raw = std::str::from_utf8(param_bytes.as_ref())
                .map_err(|e| invalid_param(e.to_string()))?;

            let trimmed = raw.trim();

            match &param_type {
                t if *t == Type::BOOL => {
                    let lower = trimmed.to_ascii_lowercase();
                    match lower.as_str() {
                        "t" | "true" | "1" => "true".to_string(),
                        "f" | "false" | "0" => "false".to_string(),
                        _ => return Err(invalid_param(format!("\"{}\"", raw))),
                    }
                }
                t if *t == Type::INT2 => trimmed
                    .parse::<i16>()
                    .map(|v| v.to_string())
                    .map_err(|e| invalid_param(e.to_string()))?,
                t if *t == Type::INT4 => trimmed
                    .parse::<i32>()
                    .map(|v| v.to_string())
                    .map_err(|e| invalid_param(e.to_string()))?,
                t if *t == Type::INT8 => trimmed
                    .parse::<i64>()
                    .map(|v| v.to_string())
                    .map_err(|e| invalid_param(e.to_string()))?,
                t if *t == Type::FLOAT4 => {
                    let v = trimmed
                        .parse::<f32>()
                        .map_err(|e| invalid_param(e.to_string()))?;
                    if !v.is_finite() {
                        return Err(invalid_param(format!(
                            "non-finite FLOAT4 is not supported: \"{}\"",
                            raw
                        )));
                    }
                    v.to_string()
                }
                t if *t == Type::FLOAT8 => {
                    let v = trimmed
                        .parse::<f64>()
                        .map_err(|e| invalid_param(e.to_string()))?;
                    if !v.is_finite() {
                        return Err(invalid_param(format!(
                            "non-finite FLOAT8 is not supported: \"{}\"",
                            raw
                        )));
                    }
                    v.to_string()
                }
                t if *t == Type::UUID => format!("{}::uuid", quote_sql_string_literal(raw)),
                t if *t == Type::BYTEA => format!("{}::bytea", quote_sql_string_literal(raw)),
                _ => quote_sql_string_literal(raw),
            }
        };

        values.push(value_str);
    }

    Ok(substitute_placeholders_outside_strings_and_dollar(
        query, &values,
    ))
}

#[allow(dead_code)]
fn infer_result_fields(query: &str) -> Vec<FieldInfo> {
    let query_upper = query.to_uppercase();
    if query_upper.starts_with("SELECT") {
        vec![FieldInfo::new(
            "column".to_string(),
            None,
            None,
            Type::TEXT,
            FieldFormat::Text,
        )]
    } else {
        vec![]
    }
}

pub struct DynamicHandlerFactory {
    handler: Arc<DynamicPgHandler>,
}

impl DynamicHandlerFactory {
    #[allow(dead_code)]
    pub fn new(pd_endpoints: Vec<String>, default_keyspace: Option<String>) -> Self {
        Self {
            handler: Arc::new(DynamicPgHandler::new(pd_endpoints, default_keyspace)),
        }
    }

    pub fn new_with_pool(
        client_pool: Arc<TikvClientPool>,
        default_keyspace: Option<String>,
    ) -> Self {
        Self {
            handler: Arc::new(DynamicPgHandler::new_with_pool(
                client_pool,
                default_keyspace,
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

// Keep the old HandlerFactory for backward compatibility (static executor)
#[allow(dead_code)]
pub struct HandlerFactory {
    handler: Arc<PgHandler>,
}

impl HandlerFactory {
    #[allow(dead_code)]
    pub fn new(executor: Arc<Executor>) -> Self {
        Self {
            handler: Arc::new(PgHandler::new(executor)),
        }
    }
}

#[allow(dead_code)]
pub struct PgHandler {
    executor: Arc<Executor>,
    session: Mutex<Session>,
    copy_context: Mutex<Option<CopyContext>>,
    suspended_portals: Mutex<HashMap<String, SuspendedPortalState>>,
    query_parser: Arc<TipgQueryParser>,
    connection_id: i32,
}

impl PgHandler {
    #[allow(dead_code)]
    pub fn new(executor: Arc<Executor>) -> Self {
        let store = executor.store();
        let observability = executor.observability().clone();
        let connection_id = CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            executor,
            session: Mutex::new(Session::new_with_database(
                store,
                observability,
                connection_id,
                1,
                "postgres".to_string(),
            )),
            copy_context: Mutex::new(None),
            suspended_portals: Mutex::new(HashMap::new()),
            query_parser: Arc::new(TipgQueryParser::new()),
            connection_id,
        }
    }

    #[allow(dead_code)]
    async fn infer_result_fields_from_query(&self, query: &str) -> Vec<FieldInfo> {
        let mut session_guard = self.session.lock().await;
        let store = self.executor.store();
        infer_result_fields_from_query_ast(&store, &mut session_guard, query).await
    }

    #[allow(dead_code)]
    fn parse_copy_command(query: &str) -> Option<(String, Vec<String>)> {
        let query = strip_leading_whitespace_and_comments(query)?;
        let query_upper = query.to_uppercase();
        if !query_upper.starts_with("COPY")
            || !query_upper.contains("FROM")
            || !query_upper.contains("STDIN")
        {
            return None;
        }

        let re =
            regex::Regex::new(r"(?i)^COPY\s+(?:(\w+)\.)?(\w+)\s*\(([^)]+)\)\s+FROM\s+stdin")
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
}

#[async_trait]
impl StartupHandler for PgHandler {
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
        if let PgWireFrontendMessage::Startup(ref startup) = message {
            pgwire::api::auth::save_startup_parameters_to_metadata(client, startup);
            pgwire::api::auth::finish_authentication(client, &PgServerParameterProvider).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl SimpleQueryHandler for PgHandler {
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

        if let Some((table_name, columns)) = Self::parse_copy_command(query) {
            debug!(
                "COPY command detected: table={}, columns={:?}",
                table_name, columns
            );

            let (resolved_table, resolved_columns, column_types, col_count, started_txn) = {
                let mut session = self.session.lock().await;
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
                        match self
                            .executor
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

                        let mut found: Option<(String, TableSchema)> = None;
                        for schema_ident in schemas {
                            let resolved_table = format!("{}.{}", schema_ident, table_ident);
                            match self
                                .executor
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
                )
            };

            let mut ctx = self.copy_context.lock().await;
            *ctx = Some(CopyContext {
                table_name: resolved_table,
                columns: resolved_columns,
                column_types,
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

        let mut session = self.session.lock().await;

        set_connection_id(session.connection_id());

        match self.executor.execute(&mut session, query).await {
            Ok(results) => {
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
                Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    sqlstate_for_executor_error(&e).to_string(),
                    e.to_string(),
                ))))
            }
        }
    }
}

#[async_trait]
impl CopyHandler for PgHandler {
    async fn on_copy_data<C>(&self, _client: &mut C, copy_data: CopyData) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let (parse_res, table_name, started_txn) = {
            let mut ctx_guard = self.copy_context.lock().await;
            let Some(ctx) = ctx_guard.as_mut() else {
                return Ok(());
            };

            let table_name = ctx.table_name.clone();
            let started_txn = ctx.started_txn;

            let parse_res = (|| -> PgWireResult<Vec<Vec<(String, Value)>>> {
                if ctx.reached_end_marker {
                    return Ok(Vec::new());
                }

                let lines = ctx.push_copy_data(copy_data.data.as_ref())?;
                if lines.is_empty() {
                    return Ok(Vec::new());
                }

                let mut rows_to_insert: Vec<Vec<(String, Value)>> = Vec::with_capacity(lines.len());
                for line_bytes in lines {
                    if ctx.reached_end_marker {
                        break;
                    }
                    if line_bytes.as_slice() == b"\\." {
                        ctx.reached_end_marker = true;
                        ctx.line_buffer.clear();
                        break;
                    }

                    let line = String::from_utf8_lossy(&line_bytes);
                    let values: Vec<&str> = line.split('\t').collect();

                    if values.len() != ctx.columns.len() {
                        return Err(copy_row_column_mismatch_error(
                            values.len(),
                            ctx.columns.len(),
                        ));
                    }

                    let mut col_values: Vec<(String, Value)> =
                        Vec::with_capacity(ctx.columns.len());
                    for ((col_name, col_type), val) in ctx
                        .columns
                        .iter()
                        .zip(ctx.column_types.iter())
                        .zip(values.iter())
                    {
                        let value = if *val == "\\N" {
                            Value::Null
                        } else if let Some(dt) = col_type.as_ref() {
                            self.executor.parse_value_for_copy(val, dt).map_err(|e| {
                                PgWireError::UserError(Box::new(ErrorInfo::new(
                                    "ERROR".to_string(),
                                    "22P02".to_string(),
                                    e.to_string(),
                                )))
                            })?
                        } else {
                            Value::Text(val.to_string())
                        };
                        col_values.push((col_name.clone(), value));
                    }
                    rows_to_insert.push(col_values);
                }

                Ok(rows_to_insert)
            })();

            (parse_res, table_name, started_txn)
        };

        let rows_to_insert = match parse_res {
            Ok(rows) => rows,
            Err(e) => {
                let mut ctx_guard = self.copy_context.lock().await;
                *ctx_guard = None;
                drop(ctx_guard);

                let mut session = self.session.lock().await;
                rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
                return Err(e);
            }
        };

        let inserted_count = rows_to_insert.len();
        if inserted_count == 0 {
            return Ok(());
        }

        let insert_res: PgWireResult<()> = async {
            let mut session = self.session.lock().await;

            if session.is_transaction_failed() {
                return Err(in_failed_sql_transaction_pgwire_error());
            }

            let savepoints = session.savepoints();
            crate::txn::with_savepoints(savepoints, async {
                for col_values in rows_to_insert {
                    self.executor
                        .execute_copy_insert(&mut session, &table_name, col_values)
                        .await
                        .map_err(|e| {
                            error!("COPY insert error: {}", e);
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "XX000".to_string(),
                                e.to_string(),
                            )))
                        })?;
                }

                Ok::<(), PgWireError>(())
            })
            .await?;

            Ok(())
        }
        .await;

        if let Err(e) = insert_res {
            let mut session = self.session.lock().await;
            rollback_autocommit_or_mark_failed(&mut session, started_txn).await;
            drop(session);

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
            let mut session = self.session.lock().await;

            if session.is_transaction_failed() {
                return Err(in_failed_sql_transaction_pgwire_error());
            }

            if let Some(final_line_bytes) = ctx.drain_final_line() {
                if final_line_bytes.as_slice() == b"\\." {
                    ctx.reached_end_marker = true;
                } else if !ctx.reached_end_marker {
                let line = String::from_utf8_lossy(&final_line_bytes);
                let values: Vec<&str> = line.split('\t').collect();

                if values.len() != ctx.columns.len() {
                    rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn).await;
                    return Err(copy_row_column_mismatch_error(
                        values.len(),
                        ctx.columns.len(),
                    ));
                }

                let mut col_values: Vec<(String, Value)> = Vec::with_capacity(ctx.columns.len());
                for ((col_name, col_type), val) in ctx
                    .columns
                    .iter()
                    .zip(ctx.column_types.iter())
                    .zip(values.iter())
                {
                    let value = if *val == "\\N" {
                        Value::Null
                    } else if let Some(dt) = col_type.as_ref() {
                        self.executor.parse_value_for_copy(val, dt).map_err(|e| {
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "22P02".to_string(),
                                e.to_string(),
                            )))
                        })?
                    } else {
                        Value::Text(val.to_string())
                    };
                    col_values.push((col_name.clone(), value));
                }

                let savepoints = session.savepoints();
                let insert_res = crate::txn::with_savepoints(savepoints, async {
                    self.executor
                        .execute_copy_insert(&mut session, &ctx.table_name, col_values)
                        .await
                        .map_err(|e| {
                            error!("COPY insert error: {}", e);
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "XX000".to_string(),
                                e.to_string(),
                            )))
                        })
                })
                .await;

                if let Err(e) = insert_res {
                    rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn).await;
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

            ctx.row_count
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
            let mut session = self.session.lock().await;
            rollback_autocommit_or_mark_failed(&mut session, ctx.started_txn).await;
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
impl ExtendedQueryHandler for PgHandler {
    type Statement = String;
    type QueryParser = TipgQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
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
            let portal = Portal::try_new(&message, statement)?;
            client.portal_store().put_portal(Arc::new(portal));
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
        let query = &portal.statement.statement;
        debug!("Extended query: {}", query);

        let final_query = substitute_parameters(query, portal)?;
        debug!("Final query after substitution: {}", final_query);

        let mut session = self.session.lock().await;

        set_connection_id(session.connection_id());

        match self.executor.execute(&mut session, &final_query).await {
            Ok(results) => Ok(send_notices_and_get_last_response(
                client,
                session.show_setting_value("client_min_messages"),
                results,
            )
            .await?),
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
        let param_count = count_sql_parameters(&stmt.statement);
        let mut param_types: Vec<Type> = stmt.parameter_types.clone();

        if param_types.len() < param_count || param_types.iter().any(|t| *t == Type::UNKNOWN) {
            let inferred = infer_parameter_types(&stmt.statement, param_count);
            for i in param_types.len()..param_count {
                param_types.push(inferred[i].clone());
            }
            for i in 0..param_types.len().min(inferred.len()) {
                if param_types[i] == Type::UNKNOWN && inferred[i] != Type::UNKNOWN {
                    param_types[i] = inferred[i].clone();
                }
            }
        }

        let query_for_inference = if param_count == 0 {
            stmt.statement.clone()
        } else {
            let mut values: Vec<String> = Vec::with_capacity(param_count);
            for i in 0..param_count {
                let t = param_types.get(i).cloned().unwrap_or(Type::UNKNOWN);
                values.push(dummy_sql_expr_for_param_type(&t));
            }
            substitute_placeholders_outside_strings_and_dollar(&stmt.statement, &values)
        };
        let fields = self
            .infer_result_fields_from_query(&query_for_inference)
            .await;
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
        let final_query = substitute_parameters(&portal.statement.statement, portal)?;
        let fields = self.infer_result_fields_from_query(&final_query).await;
        Ok(DescribePortalResponse::new(fields))
    }
}

impl PgWireServerHandlers for HandlerFactory {
    type StartupHandler = PgHandler;
    type SimpleQueryHandler = PgHandler;
    type ExtendedQueryHandler = PgHandler;
    type CopyHandler = PgHandler;
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

fn datatype_to_pgtype(dt: Option<&DataType>) -> Type {
    match dt {
        Some(DataType::Boolean) => Type::BOOL,
        Some(DataType::Int32) => Type::INT4,
        Some(DataType::Int64) => Type::INT8,
        Some(DataType::Float64) => Type::FLOAT8,
        Some(DataType::Timestamp) => Type::TIMESTAMP,
        Some(DataType::TimestampTz) => Type::TIMESTAMPTZ,
        Some(DataType::Date) => Type::DATE,
        Some(DataType::Interval) => Type::INTERVAL,
        Some(DataType::Uuid) => Type::UUID,
        Some(DataType::Bytes) => Type::BYTEA,
        Some(DataType::Json) => Type::JSON,
        Some(DataType::Jsonb) => Type::JSONB,
        Some(DataType::Time) => Type::TIME,
        Some(DataType::Numeric { .. }) => Type::NUMERIC,
        Some(DataType::Vector(_))
        | Some(DataType::Array(_))
        | Some(DataType::Text)
        | Some(DataType::UserDefined(_))
        | None => Type::TEXT,
    }
}

fn result_to_response(result: ExecuteResult) -> PgWireResult<Response<'static>> {
    match result {
        ExecuteResult::Select {
            columns,
            column_types,
            rows,
            timezone,
        } => {
            let tz = crate::types::timestamp::TimeZoneSpec::parse(timezone.as_ref());
            let inferred_types: Vec<Type> = if let Some(types) = column_types.as_ref() {
                types
                    .iter()
                    .map(|dt| datatype_to_pgtype(Some(dt)))
                    .collect()
            } else if let Some(first_row) = rows.first() {
                first_row
                    .values
                    .iter()
                    .map(|v| {
                        let dt = v.data_type();
                        datatype_to_pgtype(dt.as_ref())
                    })
                    .collect()
            } else {
                vec![Type::TEXT; columns.len()]
            };

            let fixed_columns: Vec<String> = if columns.len() == 1 && columns[0] == "?column?" {
                if let Some(first_row) = rows.first() {
                    if let Some(first_val) = first_row.values.first() {
                        match first_val {
                            Value::Text(s) if s.starts_with("PostgreSQL") => {
                                vec!["version".to_string()]
                            }
                            Value::Text(s) if s == "postgres" || !s.contains(' ') => {
                                vec!["?column?".to_string()]
                            }
                            _ => columns,
                        }
                    } else {
                        columns
                    }
                } else {
                    columns
                }
            } else {
                columns
            };

            let fields: Vec<FieldInfo> = fixed_columns
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let pg_type = inferred_types.get(i).cloned().unwrap_or(Type::TEXT);
                    FieldInfo::new(name.clone(), None, None, pg_type, FieldFormat::Text)
                })
                .collect();

            let fields = Arc::new(fields);

            let internal_types: Vec<DataType> = if let Some(types) = column_types.as_ref() {
                types.clone()
            } else if let Some(first) = rows.first() {
                first
                    .values
                    .iter()
                    .map(|v| v.data_type().unwrap_or(DataType::Text))
                    .collect()
            } else {
                vec![DataType::Text; fixed_columns.len()]
            };

            let mut data_rows: Vec<PgWireResult<DataRow>> = Vec::new();
            for row in rows {
                let mut encoder = DataRowEncoder::new(fields.clone());
                for (i, value) in row.values.iter().enumerate() {
                    let col_type = internal_types.get(i);
                    encode_value(&mut encoder, value, col_type, tz)?;
                }
                data_rows.push(encoder.finish());
            }

            let row_stream = stream::iter(data_rows);
            let results = QueryResponse::new(fields, row_stream);

            Ok(Response::Query(results))
        }

        ExecuteResult::CreateTable { .. } => Ok(Response::Execution(Tag::new("CREATE TABLE"))),

        ExecuteResult::DropTable { .. } => Ok(Response::Execution(Tag::new("DROP TABLE"))),

        ExecuteResult::TruncateTable { .. } => Ok(Response::Execution(Tag::new("TRUNCATE TABLE"))),

        ExecuteResult::CreateIndex { .. } => Ok(Response::Execution(Tag::new("CREATE INDEX"))),

        ExecuteResult::DropIndex { .. } => Ok(Response::Execution(Tag::new("DROP INDEX"))),

        ExecuteResult::CreateView { .. } => Ok(Response::Execution(Tag::new("CREATE VIEW"))),

        ExecuteResult::DropView { .. } => Ok(Response::Execution(Tag::new("DROP VIEW"))),

        ExecuteResult::CreateMaterializedView { .. } => {
            Ok(Response::Execution(Tag::new("CREATE MATERIALIZED VIEW")))
        }

        ExecuteResult::DropMaterializedView { .. } => {
            Ok(Response::Execution(Tag::new("DROP MATERIALIZED VIEW")))
        }

        ExecuteResult::RefreshMaterializedView { .. } => {
            Ok(Response::Execution(Tag::new("REFRESH MATERIALIZED VIEW")))
        }

        ExecuteResult::CreateProcedure { .. } => {
            Ok(Response::Execution(Tag::new("CREATE PROCEDURE")))
        }

        ExecuteResult::DropProcedure { .. } => Ok(Response::Execution(Tag::new("DROP PROCEDURE"))),

        ExecuteResult::CreateFunction { .. } => {
            Ok(Response::Execution(Tag::new("CREATE FUNCTION")))
        }

        ExecuteResult::DropFunction { .. } => Ok(Response::Execution(Tag::new("DROP FUNCTION"))),

        ExecuteResult::CreateTrigger { .. } => Ok(Response::Execution(Tag::new("CREATE TRIGGER"))),

        ExecuteResult::DropTrigger { .. } => Ok(Response::Execution(Tag::new("DROP TRIGGER"))),

        ExecuteResult::CreateExtension { .. } => {
            Ok(Response::Execution(Tag::new("CREATE EXTENSION")))
        }

        ExecuteResult::DropExtension { .. } => Ok(Response::Execution(Tag::new("DROP EXTENSION"))),

        ExecuteResult::Call => Ok(Response::Execution(Tag::new("CALL"))),

        ExecuteResult::AlterTable { .. } => Ok(Response::Execution(Tag::new("ALTER TABLE"))),

        ExecuteResult::AlterSequence { .. } => Ok(Response::Execution(Tag::new("ALTER SEQUENCE"))),

        ExecuteResult::AlterFunction { .. } => Ok(Response::Execution(Tag::new("ALTER FUNCTION"))),

        ExecuteResult::AlterIndex { .. } => Ok(Response::Execution(Tag::new("ALTER INDEX"))),

        ExecuteResult::Insert { affected_rows } => Ok(Response::Execution(
            Tag::new("INSERT")
                .with_oid(0)
                .with_rows(affected_rows as usize),
        )),

        ExecuteResult::Delete { affected_rows } => Ok(Response::Execution(
            Tag::new("DELETE").with_rows(affected_rows as usize),
        )),

        ExecuteResult::Update { affected_rows } => Ok(Response::Execution(
            Tag::new("UPDATE").with_rows(affected_rows as usize),
        )),

        ExecuteResult::ShowTables { tables } => {
            let fields = vec![FieldInfo::new(
                "table_name".to_string(),
                None,
                None,
                Type::TEXT,
                FieldFormat::Text,
            )];
            let fields = Arc::new(fields);

            let mut data_rows: Vec<PgWireResult<DataRow>> = Vec::new();
            for table in tables {
                let mut encoder = DataRowEncoder::new(fields.clone());
                encoder.encode_field(&table)?;
                data_rows.push(encoder.finish());
            }

            let row_stream = stream::iter(data_rows);
            let results = QueryResponse::new(fields, row_stream);

            Ok(Response::Query(results))
        }

        ExecuteResult::Describe { schema } => {
            let fields = vec![
                FieldInfo::new(
                    "column_name".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Text,
                ),
                FieldInfo::new(
                    "data_type".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Text,
                ),
                FieldInfo::new(
                    "nullable".to_string(),
                    None,
                    None,
                    Type::BOOL,
                    FieldFormat::Text,
                ),
                FieldInfo::new(
                    "primary_key".to_string(),
                    None,
                    None,
                    Type::BOOL,
                    FieldFormat::Text,
                ),
                FieldInfo::new(
                    "default".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Text,
                ),
            ];
            let fields = Arc::new(fields);

            let mut data_rows: Vec<PgWireResult<DataRow>> = Vec::new();
            for col in &schema.columns {
                let mut encoder = DataRowEncoder::new(fields.clone());
                encoder.encode_field(&col.name)?;
                encoder.encode_field(&col.data_type.to_string())?;
                encoder.encode_field(&col.nullable)?;
                encoder.encode_field(&col.primary_key)?;

                let default_val = if col.is_serial {
                    Some("SERIAL (AUTO_INC)".to_string())
                } else {
                    col.default_expr.clone()
                };
                encoder.encode_field(&default_val)?;

                data_rows.push(encoder.finish());
            }

            let row_stream = stream::iter(data_rows);
            let results = QueryResponse::new(fields, row_stream);

            Ok(Response::Query(results))
        }

        ExecuteResult::CommandComplete { tag } => Ok(Response::Execution(Tag::new(tag))),

        ExecuteResult::TransactionStart { tag } => Ok(Response::TransactionStart(Tag::new(tag))),

        ExecuteResult::TransactionEnd { tag } => Ok(Response::TransactionEnd(Tag::new(tag))),

        ExecuteResult::Empty => Ok(Response::EmptyQuery),

        ExecuteResult::Notice { .. } => Ok(Response::EmptyQuery),

        ExecuteResult::CreateRole => Ok(Response::Execution(Tag::new("CREATE ROLE"))),

        ExecuteResult::AlterRole => Ok(Response::Execution(Tag::new("ALTER ROLE"))),

        ExecuteResult::DropRole => Ok(Response::Execution(Tag::new("DROP ROLE"))),

        ExecuteResult::Grant => Ok(Response::Execution(Tag::new("GRANT"))),

        ExecuteResult::Revoke => Ok(Response::Execution(Tag::new("REVOKE"))),

        ExecuteResult::Skipped { message } => {
            tracing::warn!("SKIPPED: {}", message);
            let fields = vec![FieldInfo::new(
                "warning".to_string(),
                None,
                None,
                Type::TEXT,
                FieldFormat::Text,
            )];
            let fields = Arc::new(fields);
            let mut encoder = DataRowEncoder::new(fields.clone());
            encoder.encode_field(&format!("SKIPPED: {}", message))?;
            let data_rows = vec![encoder.finish()];
            let row_stream = stream::iter(data_rows);
            Ok(Response::Query(QueryResponse::new(fields, row_stream)))
        }
    }
}

fn encode_value(
    encoder: &mut DataRowEncoder,
    value: &Value,
    col_type: Option<&DataType>,
    tz: crate::types::timestamp::TimeZoneSpec,
) -> PgWireResult<()> {
    match value {
        Value::Null => encoder.encode_field(&None::<String>),
        Value::Boolean(b) => encoder.encode_field(b),
        Value::Int32(i) => encoder.encode_field(i),
        Value::Int64(i) => {
            // Check if this Int64 should be interpreted as a timestamp based on column type
            // This handles the case where timestamps were incorrectly stored as Int64
            if matches!(
                col_type,
                Some(DataType::Timestamp) | Some(DataType::TimestampTz)
            ) {
                // Treat as timestamp - reuse the timestamp encoding logic
                use chrono::{DateTime, Utc};
                const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;
                const MAX_REASONABLE_UNIX_MS: i64 = 10_000_000_000_000;

                let (seconds, micros) = if i.abs() > MAX_REASONABLE_UNIX_MS {
                    let pg_micros = i;
                    let unix_secs = pg_micros.div_euclid(1_000_000) + PG_EPOCH_UNIX_SECS;
                    let micros = pg_micros.rem_euclid(1_000_000) as u32;
                    (unix_secs, micros)
                } else {
                    let secs = i.div_euclid(1000);
                    let millis = i.rem_euclid(1000) as u32;
                    (secs, millis * 1000)
                };

                let nanos = micros * 1000;
                if let Some(dt) = DateTime::<Utc>::from_timestamp(seconds, nanos) {
                    let is_timestamptz = matches!(col_type, Some(DataType::TimestampTz));
                    if is_timestamptz {
                        encoder.encode_field(&tz.format_timestamptz(dt, micros))
                    } else if micros == 0 {
                        encoder.encode_field(&dt.format("%Y-%m-%d %H:%M:%S").to_string())
                    } else {
                        encoder.encode_field(&dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
                    }
                } else {
                    encoder.encode_field(&"1970-01-01 00:00:00".to_string())
                }
            } else {
                encoder.encode_field(i)
            }
        }
        Value::Float64(f) => encoder.encode_field(f),
        Value::Text(s) => encoder.encode_field(s),
        Value::Bytes(b) => encoder.encode_field(&format!("\\x{}", hex::encode(b))),
        Value::Timestamp(ts) => {
            use chrono::{DateTime, Utc};

            // Detect timestamp format:
            // - Unix epoch milliseconds: typical values 1.0e12 to 2.5e12 (years 2001-2049)
            // - PostgreSQL epoch microseconds: typical values 0 to 1.6e15 (years 2000-2050)
            // If value is > 1e13 (year 2286 in Unix ms), assume it's PG epoch microseconds.
            // PostgreSQL epoch is 2000-01-01 00:00:00 UTC = 946684800 seconds since Unix epoch.
            const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;
            const MAX_REASONABLE_UNIX_MS: i64 = 10_000_000_000_000; // year ~2286

            let (seconds, micros) = if ts.abs() > MAX_REASONABLE_UNIX_MS {
                // Likely PostgreSQL epoch microseconds - convert to Unix seconds
                let pg_micros = ts;
                let unix_secs = pg_micros.div_euclid(1_000_000) + PG_EPOCH_UNIX_SECS;
                let micros = pg_micros.rem_euclid(1_000_000) as u32;
                (unix_secs, micros)
            } else {
                // Unix epoch milliseconds (our standard format)
                let secs = ts.div_euclid(1000);
                let millis = ts.rem_euclid(1000) as u32;
                (secs, millis * 1000)
            };

            let nanos = micros * 1000;
            if let Some(dt) = DateTime::<Utc>::from_timestamp(seconds, nanos) {
                let is_timestamptz = matches!(col_type, Some(DataType::TimestampTz));

                if is_timestamptz {
                    encoder.encode_field(&tz.format_timestamptz(dt, micros))
                } else if micros == 0 {
                    encoder.encode_field(&dt.format("%Y-%m-%d %H:%M:%S").to_string())
                } else {
                    encoder.encode_field(&dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
                }
            } else {
                // Fallback: encode as ISO string if all else fails
                encoder.encode_field(&format!("1970-01-01 00:00:00"))
            }
        }
        Value::Interval(iv) => encoder.encode_field(&iv.to_string()),
        Value::Uuid(bytes) => {
            let uuid = uuid::Uuid::from_bytes(*bytes);
            encoder.encode_field(&uuid.to_string())
        }
        Value::Array(elems) => {
            fn needs_array_quotes(s: &str) -> bool {
                s.is_empty()
                    || s.eq_ignore_ascii_case("NULL")
                    || s.chars()
                        .any(|c| c.is_whitespace() || matches!(c, '{' | '}' | ',' | '"' | '\\'))
            }

            fn escape_array_element(s: &str) -> String {
                let mut out = String::with_capacity(s.len());
                for ch in s.chars() {
                    match ch {
                        '\\' => out.push_str("\\\\"),
                        '"' => out.push_str("\\\""),
                        other => out.push(other),
                    }
                }
                out
            }

            fn encode_array(elems: &[Value]) -> String {
                let mut parts = Vec::with_capacity(elems.len());
                for elem in elems {
                    let part = match elem {
                        Value::Null => "NULL".to_string(),
                        Value::Array(nested) => encode_array(nested),
                        other => {
                            let s = match other {
                                Value::Text(t) => t.clone(),
                                v => v.to_string(),
                            };
                            if needs_array_quotes(&s) {
                                format!("\"{}\"", escape_array_element(&s))
                            } else {
                                s
                            }
                        }
                    };
                    parts.push(part);
                }
                format!("{{{}}}", parts.join(","))
            }

            encoder.encode_field(&encode_array(elems))
        }
        Value::Json(s) => encoder.encode_field(s),
        Value::Jsonb(s) => {
            fn write_jsonb_pg(out: &mut String, val: &serde_json::Value) {
                match val {
                    serde_json::Value::Null => out.push_str("null"),
                    serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                    serde_json::Value::Number(n) => out.push_str(&n.to_string()),
                    serde_json::Value::String(s) => {
                        // Delegate escaping to serde_json.
                        if let Ok(escaped) = serde_json::to_string(s) {
                            out.push_str(&escaped);
                        } else {
                            out.push_str("\"\"");
                        }
                    }
                    serde_json::Value::Array(arr) => {
                        out.push('[');
                        for (idx, item) in arr.iter().enumerate() {
                            if idx > 0 {
                                out.push_str(", ");
                            }
                            write_jsonb_pg(out, item);
                        }
                        out.push(']');
                    }
                    serde_json::Value::Object(obj) => {
                        use std::cmp::Ordering;
                        let mut items: Vec<(&String, &serde_json::Value)> = obj.iter().collect();
                        // PostgreSQL jsonb key ordering: length first, then binary (byte) order.
                        items.sort_by(|(k1, _), (k2, _)| match k1.len().cmp(&k2.len()) {
                            Ordering::Equal => k1.cmp(k2),
                            other => other,
                        });

                        out.push('{');
                        for (idx, (k, v)) in items.into_iter().enumerate() {
                            if idx > 0 {
                                out.push_str(", ");
                            }
                            if let Ok(key) = serde_json::to_string(k) {
                                out.push_str(&key);
                            } else {
                                out.push_str("\"\"");
                            }
                            out.push_str(": ");
                            write_jsonb_pg(out, v);
                        }
                        out.push('}');
                    }
                }
            }

            match serde_json::from_str::<serde_json::Value>(s) {
                Ok(val) => {
                    let mut formatted = String::new();
                    write_jsonb_pg(&mut formatted, &val);
                    encoder.encode_field(&formatted)
                }
                Err(_) => encoder.encode_field(s),
            }
        }
        Value::Vector(vec) => {
            // Encode as text: [1,2,3] (compact format for integers, decimals for floats)
            let vec_str = format!(
                "[{}]",
                vec.iter()
                    .map(|f| {
                        // Format as integer if whole number, otherwise as float
                        if f.fract() == 0.0 && f.is_finite() {
                            format!("{}", *f as i64)
                        } else {
                            f.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            );
            encoder.encode_field(&vec_str)
        }
        Value::Time(micros) => {
            let total_secs = micros / 1_000_000;
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            let frac = micros % 1_000_000;
            if frac > 0 {
                encoder.encode_field(&format!("{:02}:{:02}:{:02}.{:06}", hours, mins, secs, frac))
            } else {
                encoder.encode_field(&format!("{:02}:{:02}:{:02}", hours, mins, secs))
            }
        }
        Value::Date(days) => {
            let s =
                crate::types::date::format_date_days(*days).unwrap_or_else(|_| days.to_string());
            encoder.encode_field(&s)
        }
        Value::Numeric(d) => encoder.encode_field(&d.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Buf;
    use bytes::Bytes;
    use pgwire::api::portal::Format;
    use pgwire::api::stmt::NoopQueryParser;
    use pgwire::api::stmt::QueryParser;
    use pgwire::api::DefaultClient;
    use pgwire::messages::response::CommandComplete;
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    #[derive(Default)]
    struct RecordingSink {
        messages: Vec<PgWireBackendMessage>,
    }

    impl Sink<PgWireBackendMessage> for RecordingSink {
        type Error = PgWireError;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: PgWireBackendMessage) -> Result<(), Self::Error> {
            self.get_mut().messages.push(item);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn test_column(name: &str, data_type: DataType) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            data_type,
            nullable: false,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        }
    }

    fn test_schema(name: &str, columns: Vec<ColumnDef>) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            table_id: 0,
            columns,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    #[tokio::test]
    async fn extended_query_parse_rejects_invalid_sql() {
        let parser = TipgQueryParser::new();
        let err = parser.parse_sql("SELCT 1", &[]).await.unwrap_err();

        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "42601");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn extended_query_parse_allows_executor_handled_ddl() {
        // `CREATE DATABASE` is handled via the executor's string-based path (not sqlparser-rs).
        let parser = TipgQueryParser::new();
        parser
            .parse_sql("CREATE DATABASE test_db", &[])
            .await
            .expect("CREATE DATABASE should be accepted at Parse");
    }

    fn encode_value_to_string(value: &Value, col_type: Option<&DataType>) -> String {
        let fields = vec![FieldInfo::new(
            "col".to_string(),
            None,
            None,
            Type::TEXT,
            FieldFormat::Text,
        )];
        let fields = Arc::new(fields);
        let mut encoder = DataRowEncoder::new(fields);
        let tz = crate::types::timestamp::TimeZoneSpec::parse("UTC");
        encode_value(&mut encoder, value, col_type, tz).unwrap();
        let row = encoder.finish().unwrap();

        assert_eq!(row.field_count, 1);
        let mut data = row.data.clone();
        let len = data.get_i32();
        assert!(len >= 0);
        let bytes = data.copy_to_bytes(len as usize);
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn test_sqlstate_for_executor_error() {
        let failed = anyhow::Error::new(InFailedSqlTransaction);
        assert_eq!(sqlstate_for_executor_error(&failed), "25P02");

        let other = anyhow::anyhow!("boom");
        assert_eq!(sqlstate_for_executor_error(&other), "XX000");
    }

    #[test]
    fn test_update_tx_status_after_execution_clears_error_on_rollback_to_savepoint() {
        let status = TransactionStatus::Error;
        let tag = Tag::new("ROLLBACK");
        assert_eq!(
            update_tx_status_after_execution(status, &tag),
            TransactionStatus::Transaction
        );
    }

    #[test]
    fn test_update_tx_status_after_execution_keeps_status_for_other_commands() {
        let status = TransactionStatus::Error;
        let tag = Tag::new("SET");
        assert_eq!(
            update_tx_status_after_execution(status, &tag),
            TransactionStatus::Error
        );
    }

    #[derive(Debug)]
    struct TestClient {
        inner: DefaultClient<String>,
        sent: Vec<PgWireBackendMessage>,
    }

    impl TestClient {
        fn new() -> Self {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            Self {
                inner: DefaultClient::new(addr, false),
                sent: Vec::new(),
            }
        }
    }

    impl ClientInfo for TestClient {
        fn socket_addr(&self) -> SocketAddr {
            self.inner.socket_addr
        }

        fn is_secure(&self) -> bool {
            self.inner.is_secure
        }

        fn state(&self) -> PgWireConnectionState {
            self.inner.state
        }

        fn set_state(&mut self, new_state: PgWireConnectionState) {
            self.inner.state = new_state;
        }

        fn transaction_status(&self) -> TransactionStatus {
            self.inner.transaction_status
        }

        fn set_transaction_status(&mut self, new_status: TransactionStatus) {
            self.inner.transaction_status = new_status;
        }

        fn metadata(&self) -> &HashMap<String, String> {
            &self.inner.metadata
        }

        fn metadata_mut(&mut self) -> &mut HashMap<String, String> {
            &mut self.inner.metadata
        }
    }

    impl ClientPortalStore for TestClient {
        type PortalStore = pgwire::api::store::MemPortalStore<String>;

        fn portal_store(&self) -> &Self::PortalStore {
            &self.inner.portal_store
        }
    }

    impl Sink<PgWireBackendMessage> for TestClient {
        type Error = PgWireError;

        fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: PgWireBackendMessage) -> Result<(), Self::Error> {
            self.get_mut().sent.push(item);
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug)]
    struct StubExtendedQueryHandler {
        query_parser: Arc<NoopQueryParser>,
        rows: usize,
    }

    impl StubExtendedQueryHandler {
        fn new() -> Self {
            Self::new_with_rows(5)
        }

        fn new_with_rows(rows: usize) -> Self {
            Self {
                query_parser: Arc::new(NoopQueryParser::new()),
                rows,
            }
        }

        fn select_range_response(rows: usize) -> Response<'static> {
            let fields = Arc::new(vec![FieldInfo::new(
                "n".to_owned(),
                None,
                None,
                Type::INT4,
                FieldFormat::Text,
            )]);

            let row_fields = fields.clone();
            let row_stream = stream::iter(0..rows).map(move |v| {
                let mut encoder = DataRowEncoder::new(row_fields.clone());
                let value = i32::try_from(v).unwrap_or(i32::MAX);
                encoder.encode_field(&value)?;
                encoder.finish()
            });

            Response::Query(QueryResponse::new(fields, row_stream))
        }
    }

    #[async_trait]
    impl ExtendedQueryHandler for StubExtendedQueryHandler {
        type Statement = String;
        type QueryParser = NoopQueryParser;

        fn query_parser(&self) -> Arc<Self::QueryParser> {
            self.query_parser.clone()
        }

        async fn do_query<'a, 'b: 'a, C>(
            &'b self,
            _client: &mut C,
            _portal: &'a Portal<Self::Statement>,
            _max_rows: usize,
        ) -> PgWireResult<Response<'a>>
        where
            C: ClientInfo + Unpin + Send + Sync,
        {
            Ok(Self::select_range_response(self.rows))
        }

        async fn do_describe_statement<C>(
            &self,
            _client: &mut C,
            _target: &StoredStatement<Self::Statement>,
        ) -> PgWireResult<DescribeStatementResponse>
        where
            C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
            C::PortalStore: PortalStore<Statement = Self::Statement>,
            C::Error: Debug,
            PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
        {
            Ok(DescribeStatementResponse::new(vec![], vec![]))
        }

        async fn do_describe_portal<C>(
            &self,
            _client: &mut C,
            _target: &Portal<Self::Statement>,
        ) -> PgWireResult<DescribePortalResponse>
        where
            C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
            C::PortalStore: PortalStore<Statement = Self::Statement>,
            C::Error: Debug,
            PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
        {
            Ok(DescribePortalResponse::new(vec![]))
        }
    }

    fn decode_single_text_field(row: &DataRow) -> String {
        let mut data = row.data.clone();
        let len = data.get_i32();
        assert!(len >= 0);
        let bytes = data.copy_to_bytes(len as usize);
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    #[tokio::test]
    async fn execute_honors_max_rows_and_suspends_portal() {
        let handler = StubExtendedQueryHandler::new();
        let suspended = Mutex::new(HashMap::<String, SuspendedPortalState>::new());

        let statement = Arc::new(StoredStatement::new(
            "stmt".to_owned(),
            "SELECT 1".to_owned(),
            vec![],
        ));
        let bind = pgwire::messages::extendedquery::Bind::new(
            Some("portal".to_owned()),
            Some("stmt".to_owned()),
            vec![],
            vec![],
            vec![],
        );
        let portal = Portal::try_new(&bind, statement).expect("portal");

        let mut client = TestClient::new();
        client.set_state(PgWireConnectionState::ReadyForQuery);
        client
            .portal_store()
            .put_portal(Arc::new(portal.clone()));

        on_execute_with_tx_status_fix(
            &handler,
            &suspended,
            &mut client,
            pgwire::messages::extendedquery::Execute::new(Some("portal".to_owned()), 2),
        )
        .await
        .expect("execute 1");

        let msgs = std::mem::take(&mut client.sent);
        assert_eq!(msgs.len(), 3);
        assert!(matches!(msgs[2], PgWireBackendMessage::PortalSuspended(_)));
        assert_eq!(
            msgs.iter()
                .filter_map(|m| match m {
                    PgWireBackendMessage::DataRow(r) => Some(decode_single_text_field(r)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["0", "1"]
        );

        on_execute_with_tx_status_fix(
            &handler,
            &suspended,
            &mut client,
            pgwire::messages::extendedquery::Execute::new(Some("portal".to_owned()), 2),
        )
        .await
        .expect("execute 2");

        let msgs = std::mem::take(&mut client.sent);
        assert_eq!(msgs.len(), 3);
        assert!(matches!(msgs[2], PgWireBackendMessage::PortalSuspended(_)));
        assert_eq!(
            msgs.iter()
                .filter_map(|m| match m {
                    PgWireBackendMessage::DataRow(r) => Some(decode_single_text_field(r)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["2", "3"]
        );

        on_execute_with_tx_status_fix(
            &handler,
            &suspended,
            &mut client,
            pgwire::messages::extendedquery::Execute::new(Some("portal".to_owned()), 2),
        )
        .await
        .expect("execute 3");

        let msgs = std::mem::take(&mut client.sent);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[1], PgWireBackendMessage::CommandComplete(_)));
        let PgWireBackendMessage::CommandComplete(complete) = &msgs[1] else {
            panic!("expected CommandComplete");
        };
        assert_eq!(complete.tag, "SELECT 5");
        assert_eq!(
            msgs.iter()
                .filter_map(|m| match m {
                    PgWireBackendMessage::DataRow(r) => Some(decode_single_text_field(r)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["4"]
        );
    }

    #[tokio::test]
    async fn execute_errors_when_suspended_portal_count_exceeds_limit() {
        let max_suspended = max_suspended_portals();
        let handler = StubExtendedQueryHandler::new_with_rows(2);
        let suspended = Mutex::new(HashMap::<String, SuspendedPortalState>::new());

        let statement = Arc::new(StoredStatement::new(
            "stmt".to_owned(),
            "SELECT 1".to_owned(),
            vec![],
        ));

        let mut client = TestClient::new();
        client.set_state(PgWireConnectionState::ReadyForQuery);

        for i in 0..max_suspended {
            let portal_name = format!("portal_{i}");
            let bind = pgwire::messages::extendedquery::Bind::new(
                Some(portal_name.clone()),
                Some("stmt".to_owned()),
                vec![],
                vec![],
                vec![],
            );
            let portal = Portal::try_new(&bind, statement.clone()).expect("portal");
            client.portal_store().put_portal(Arc::new(portal));

            on_execute_with_tx_status_fix(
                &handler,
                &suspended,
                &mut client,
                pgwire::messages::extendedquery::Execute::new(Some(portal_name.clone()), 1),
            )
            .await
            .expect("execute should suspend");

            let msgs = std::mem::take(&mut client.sent);
            assert!(msgs.iter().any(|m| matches!(m, PgWireBackendMessage::DataRow(_))));
            assert!(msgs
                .iter()
                .any(|m| matches!(m, PgWireBackendMessage::PortalSuspended(_))));
        }

        assert_eq!(suspended.lock().await.len(), max_suspended);

        let portal_name = "portal_over_limit".to_owned();
        let bind = pgwire::messages::extendedquery::Bind::new(
            Some(portal_name.clone()),
            Some("stmt".to_owned()),
            vec![],
            vec![],
            vec![],
        );
        let portal = Portal::try_new(&bind, statement).expect("portal");
        client.portal_store().put_portal(Arc::new(portal));

        let err = on_execute_with_tx_status_fix(
            &handler,
            &suspended,
            &mut client,
            pgwire::messages::extendedquery::Execute::new(Some(portal_name.clone()), 1),
        )
        .await
        .expect_err("expected suspended portal count limit error");

        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "54000");
                assert!(info.message.contains("too many suspended portals"));
            }
            other => panic!("expected user error, got {other:?}"),
        }

        // The server should not retain suspended portal rows after failing.
        let guard = suspended.lock().await;
        assert_eq!(guard.len(), max_suspended);
        assert!(!guard.contains_key(&portal_name));
    }

    #[tokio::test]
    async fn execute_max_rows_zero_returns_all_rows() {
        let handler = StubExtendedQueryHandler::new();
        let suspended = Mutex::new(HashMap::<String, SuspendedPortalState>::new());

        let statement = Arc::new(StoredStatement::new(
            "stmt".to_owned(),
            "SELECT 1".to_owned(),
            vec![],
        ));
        let bind = pgwire::messages::extendedquery::Bind::new(
            Some("portal".to_owned()),
            Some("stmt".to_owned()),
            vec![],
            vec![],
            vec![],
        );
        let portal = Portal::try_new(&bind, statement).expect("portal");

        let mut client = TestClient::new();
        client.set_state(PgWireConnectionState::ReadyForQuery);
        client.portal_store().put_portal(Arc::new(portal));

        on_execute_with_tx_status_fix(
            &handler,
            &suspended,
            &mut client,
            pgwire::messages::extendedquery::Execute::new(Some("portal".to_owned()), 0),
        )
        .await
        .expect("execute");

        let msgs = std::mem::take(&mut client.sent);
        assert!(matches!(msgs.last(), Some(PgWireBackendMessage::CommandComplete(_))));
        assert_eq!(
            msgs.iter()
                .filter_map(|m| match m {
                    PgWireBackendMessage::DataRow(r) => Some(decode_single_text_field(r)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["0", "1", "2", "3", "4"]
        );
    }

    #[tokio::test]
    async fn execute_errors_when_suspension_buffer_exceeds_limit() {
        let max_buffered_rows = max_suspended_portal_buffer_rows();
        let max_rows = 2usize;
        let handler = StubExtendedQueryHandler::new_with_rows(max_rows + max_buffered_rows + 1);
        let suspended = Mutex::new(HashMap::<String, SuspendedPortalState>::new());

        let statement = Arc::new(StoredStatement::new(
            "stmt".to_owned(),
            "SELECT 1".to_owned(),
            vec![],
        ));
        let bind = pgwire::messages::extendedquery::Bind::new(
            Some("portal".to_owned()),
            Some("stmt".to_owned()),
            vec![],
            vec![],
            vec![],
        );
        let portal = Portal::try_new(&bind, statement).expect("portal");

        let mut client = TestClient::new();
        client.set_state(PgWireConnectionState::ReadyForQuery);
        client.portal_store().put_portal(Arc::new(portal));

        let err = on_execute_with_tx_status_fix(
            &handler,
            &suspended,
            &mut client,
            pgwire::messages::extendedquery::Execute::new(Some("portal".to_owned()), max_rows as i32),
        )
        .await
        .expect_err("expected buffer limit error");

        match err {
            PgWireError::UserError(info) => {
                assert!(info.message.contains("portal suspension buffer exceeded"));
            }
            other => panic!("expected user error, got {other:?}"),
        }

        // The server should not retain suspended portal rows after failing.
        assert!(suspended.lock().await.is_empty());

        let data_rows = client
            .sent
            .iter()
            .filter(|m| matches!(m, PgWireBackendMessage::DataRow(_)))
            .count();
        assert_eq!(data_rows, max_rows);
        assert!(!client
            .sent
            .iter()
            .any(|m| matches!(m, PgWireBackendMessage::PortalSuspended(_))));
        assert!(!client
            .sent
            .iter()
            .any(|m| matches!(m, PgWireBackendMessage::CommandComplete(_))));
    }

    #[test]
    fn test_infer_wildcard_multiway_natural_join_dedups_columns() {
        let stmts =
            crate::sql::parse_sql("SELECT * FROM a NATURAL JOIN b NATURAL JOIN c").expect("parse");
        let stmt = stmts.first().expect("stmt");
        let Statement::Query(query) = stmt else {
            panic!("expected query");
        };
        let SetExpr::Select(select) = query.body.as_ref() else {
            panic!("expected select");
        };

        let schema_a = test_schema(
            "a",
            vec![
                test_column("id", DataType::Int32),
                test_column("a1", DataType::Text),
            ],
        );
        let schema_b = test_schema("b", vec![test_column("b1", DataType::Text)]);
        let schema_c = test_schema(
            "c",
            vec![
                test_column("id", DataType::Int32),
                test_column("c1", DataType::Text),
            ],
        );
        let sources = vec![
            SourceSchema {
                alias: "a".to_string(),
                schema: schema_a,
            },
            SourceSchema {
                alias: "b".to_string(),
                schema: schema_b,
            },
            SourceSchema {
                alias: "c".to_string(),
                schema: schema_c,
            },
        ];

        let schema_refs: Vec<&TableSchema> = sources.iter().map(|s| &s.schema).collect();
        let plan =
            crate::sql::wildcard::build_join_wildcard_plan(select, &schema_refs).expect("plan");
        assert!(plan.any_merge);
        let names: Vec<String> = plan.columns.into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["id", "a1", "b1", "c1"]);
    }

    #[test]
    fn test_infer_wildcard_multiway_using_join_dedups_columns() {
        let stmts = crate::sql::parse_sql("SELECT * FROM a JOIN b USING (id) JOIN c USING (id)")
            .expect("parse");
        let stmt = stmts.first().expect("stmt");
        let Statement::Query(query) = stmt else {
            panic!("expected query");
        };
        let SetExpr::Select(select) = query.body.as_ref() else {
            panic!("expected select");
        };

        let schema_a = test_schema(
            "a",
            vec![
                test_column("id", DataType::Int32),
                test_column("a1", DataType::Text),
            ],
        );
        let schema_b = test_schema(
            "b",
            vec![
                test_column("id", DataType::Int32),
                test_column("b1", DataType::Text),
            ],
        );
        let schema_c = test_schema(
            "c",
            vec![
                test_column("id", DataType::Int32),
                test_column("c1", DataType::Text),
            ],
        );
        let sources = vec![
            SourceSchema {
                alias: "a".to_string(),
                schema: schema_a,
            },
            SourceSchema {
                alias: "b".to_string(),
                schema: schema_b,
            },
            SourceSchema {
                alias: "c".to_string(),
                schema: schema_c,
            },
        ];

        let schema_refs: Vec<&TableSchema> = sources.iter().map(|s| &s.schema).collect();
        let plan =
            crate::sql::wildcard::build_join_wildcard_plan(select, &schema_refs).expect("plan");
        assert!(plan.any_merge);
        let names: Vec<String> = plan.columns.into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["id", "a1", "b1", "c1"]);
    }

    #[test]
    fn test_infer_wildcard_natural_join_common_cols_is_case_sensitive() {
        let stmts = crate::sql::parse_sql("SELECT * FROM a NATURAL JOIN b").expect("parse");
        let stmt = stmts.first().expect("stmt");
        let Statement::Query(query) = stmt else {
            panic!("expected query");
        };
        let SetExpr::Select(select) = query.body.as_ref() else {
            panic!("expected select");
        };

        let schema_a = test_schema("a", vec![test_column("Foo", DataType::Int32)]);
        let schema_b = test_schema("b", vec![test_column("foo", DataType::Int32)]);
        let sources = vec![
            SourceSchema {
                alias: "a".to_string(),
                schema: schema_a,
            },
            SourceSchema {
                alias: "b".to_string(),
                schema: schema_b,
            },
        ];

        let schema_refs: Vec<&TableSchema> = sources.iter().map(|s| &s.schema).collect();
        let plan =
            crate::sql::wildcard::build_join_wildcard_plan(select, &schema_refs).expect("plan");
        assert!(!plan.any_merge);
        let names: Vec<String> = plan.columns.into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["Foo", "foo"]);
    }

    #[test]
    fn test_parse_tenant_username_dot() {
        let (ks, user) = parse_tenant_username("tenant_a.admin");
        assert_eq!(ks, Some("tenant_a".to_string()));
        assert_eq!(user, "admin");
    }

    #[test]
    fn test_parse_tenant_username_colon() {
        let (ks, user) = parse_tenant_username("tenant_b:postgres");
        assert_eq!(ks, Some("tenant_b".to_string()));
        assert_eq!(user, "postgres");
    }

    #[test]
    fn test_parse_tenant_username_no_separator() {
        let (ks, user) = parse_tenant_username("admin");
        assert_eq!(ks, None);
        assert_eq!(user, "admin");
    }

    #[test]
    fn test_parse_tenant_username_empty_parts() {
        let (ks, user) = parse_tenant_username(".admin");
        assert_eq!(ks, None);
        assert_eq!(user, ".admin");

        let (ks, user) = parse_tenant_username("tenant.");
        assert_eq!(ks, None);
        assert_eq!(user, "tenant.");
    }

    #[test]
    fn test_parse_tenant_username_multiple_dots() {
        let (ks, user) = parse_tenant_username("prod.tenant_a.admin");
        assert_eq!(ks, Some("prod".to_string()));
        assert_eq!(user, "tenant_a.admin");
    }

    #[test]
    fn test_parse_tenant_username_multiple_colons() {
        let (ks, user) = parse_tenant_username("prod:tenant_a:admin");
        assert_eq!(ks, Some("prod".to_string()));
        assert_eq!(user, "tenant_a:admin");
    }

    #[test]
    fn test_parse_tenant_username_mixed_separators() {
        let (ks, user) = parse_tenant_username("tenant.user:name");
        assert_eq!(ks, Some("tenant".to_string()));
        assert_eq!(user, "user:name");

        let (ks, user) = parse_tenant_username("tenant:user.name");
        assert_eq!(ks, Some("tenant:user".to_string()));
        assert_eq!(user, "name");
    }

    #[test]
    fn test_parse_tenant_username_special_chars() {
        let (ks, user) = parse_tenant_username("tenant-1.user_name");
        assert_eq!(ks, Some("tenant-1".to_string()));
        assert_eq!(user, "user_name");

        let (ks, user) = parse_tenant_username("my_tenant:pg-admin");
        assert_eq!(ks, Some("my_tenant".to_string()));
        assert_eq!(user, "pg-admin");
    }

    #[test]
    fn test_parse_tenant_username_numbers() {
        let (ks, user) = parse_tenant_username("tenant123.user456");
        assert_eq!(ks, Some("tenant123".to_string()));
        assert_eq!(user, "user456");
    }

    #[test]
    fn test_parse_tenant_username_empty_string() {
        let (ks, user) = parse_tenant_username("");
        assert_eq!(ks, None);
        assert_eq!(user, "");
    }

    #[test]
    fn test_parse_tenant_username_only_separator() {
        let (ks, user) = parse_tenant_username(".");
        assert_eq!(ks, None);
        assert_eq!(user, ".");

        let (ks, user) = parse_tenant_username(":");
        assert_eq!(ks, None);
        assert_eq!(user, ":");
    }

    #[test]
    fn test_parse_tenant_username_unicode() {
        let (ks, user) = parse_tenant_username("租户.用户");
        assert_eq!(ks, Some("租户".to_string()));
        assert_eq!(user, "用户");
    }

    #[test]
    fn test_parse_tenant_username_whitespace() {
        let (ks, user) = parse_tenant_username("tenant .user");
        assert_eq!(ks, Some("tenant ".to_string()));
        assert_eq!(user, "user");

        let (ks, user) = parse_tenant_username("tenant. user");
        assert_eq!(ks, Some("tenant".to_string()));
        assert_eq!(user, " user");
    }

    #[test]
    fn test_auth_bootstrap_transport_errors_do_not_authenticate() {
        let err = anyhow::anyhow!("gRPC transport error: connection reset");
        assert!(DynamicPgHandler::ensure_auth_bootstrapped(Err(err)).is_err());

        let err = anyhow::anyhow!("transport error");
        assert!(DynamicPgHandler::ensure_auth_bootstrapped(Err(err)).is_err());
    }

    #[test]
    fn test_parse_tenant_username_long_names() {
        let long_tenant = "a".repeat(100);
        let long_user = "b".repeat(100);
        let input = format!("{}.{}", long_tenant, long_user);
        let (ks, user) = parse_tenant_username(&input);
        assert_eq!(ks, Some(long_tenant));
        assert_eq!(user, long_user);
    }

    #[test]
    fn test_parse_copy_command_basic() {
        let result = DynamicPgHandler::parse_copy_command("COPY users (id, name) FROM stdin");
        assert_eq!(
            result,
            Some((
                "users".to_string(),
                vec!["id".to_string(), "name".to_string()]
            ))
        );
    }

    #[test]
    fn test_parse_copy_command_no_columns() {
        let result = DynamicPgHandler::parse_copy_command("COPY users FROM stdin");
        assert_eq!(result, Some(("users".to_string(), vec![])));
    }

    #[test]
    fn test_parse_copy_command_with_public_schema() {
        let result =
            DynamicPgHandler::parse_copy_command("COPY public.users (id, name) FROM stdin");
        assert_eq!(
            result,
            Some((
                "public.users".to_string(),
                vec!["id".to_string(), "name".to_string()]
            ))
        );
    }

    #[test]
    fn test_parse_copy_command_case_insensitive() {
        let result = DynamicPgHandler::parse_copy_command("copy USERS (ID, NAME) from STDIN");
        assert_eq!(
            result,
            Some((
                "USERS".to_string(),
                vec!["ID".to_string(), "NAME".to_string()]
            ))
        );
    }

    #[test]
    fn test_parse_copy_command_not_copy() {
        assert_eq!(
            DynamicPgHandler::parse_copy_command("SELECT * FROM users"),
            None
        );
        assert_eq!(
            DynamicPgHandler::parse_copy_command("INSERT INTO users VALUES (1)"),
            None
        );
    }

    #[test]
    fn test_parse_copy_command_copy_keyword_inside_string_literal() {
        assert_eq!(
            DynamicPgHandler::parse_copy_command("SELECT 'COPY users FROM stdin' AS s;"),
            None
        );
    }

    #[test]
    fn test_parse_copy_command_copy_keyword_inside_comment() {
        assert_eq!(
            DynamicPgHandler::parse_copy_command("/* COPY users FROM stdin */ SELECT 1;"),
            None
        );
        assert_eq!(
            DynamicPgHandler::parse_copy_command("-- COPY users FROM stdin\nSELECT 1;"),
            None
        );
    }

    #[test]
    fn test_parse_copy_command_copy_to() {
        assert_eq!(
            DynamicPgHandler::parse_copy_command("COPY users TO stdout"),
            None
        );
    }

    #[test]
    fn test_parse_copy_to_command_basic() {
        let result = DynamicPgHandler::parse_copy_to_command("COPY users TO STDOUT");
        assert_eq!(result, Some(("users".to_string(), vec![])));
    }

    #[test]
    fn test_parse_copy_to_command_with_columns() {
        let result = DynamicPgHandler::parse_copy_to_command("COPY users (id, name) TO STDOUT");
        assert_eq!(
            result,
            Some((
                "users".to_string(),
                vec!["id".to_string(), "name".to_string()]
            ))
        );
    }

    #[test]
    fn test_parse_copy_to_command_with_schema() {
        let result = DynamicPgHandler::parse_copy_to_command("COPY myschema.users TO STDOUT");
        assert_eq!(result, Some(("myschema.users".to_string(), vec![])));
    }

    #[test]
    fn test_parse_copy_to_command_not_stdout() {
        assert_eq!(
            DynamicPgHandler::parse_copy_to_command("COPY users TO '/tmp/file'"),
            None
        );
    }

    #[test]
    fn test_parse_copy_to_command_from_stdin() {
        assert_eq!(
            DynamicPgHandler::parse_copy_to_command("COPY users FROM stdin"),
            None
        );
    }

    #[test]
    fn test_parse_copy_command_many_columns() {
        let result = DynamicPgHandler::parse_copy_command(
            "COPY orders (id, user_id, product, quantity, price, created_at) FROM stdin",
        );
        assert_eq!(
            result,
            Some((
                "orders".to_string(),
                vec![
                    "id".to_string(),
                    "user_id".to_string(),
                    "product".to_string(),
                    "quantity".to_string(),
                    "price".to_string(),
                    "created_at".to_string()
                ]
            ))
        );
    }

    #[test]
    fn test_replace_placeholders_basic() {
        assert_eq!(
            replace_placeholders_for_inference("SELECT * FROM users WHERE id = $1"),
            "SELECT * FROM users WHERE id = 1"
        );
        assert_eq!(
            replace_placeholders_for_inference("SELECT * FROM users WHERE id = $1 AND name = $2"),
            "SELECT * FROM users WHERE id = 1 AND name = 1"
        );
    }

    #[test]
    fn test_replace_placeholders_preserves_string_literals() {
        assert_eq!(
            replace_placeholders_for_inference(
                "SELECT * FROM users WHERE email = '$100bill@example.com'"
            ),
            "SELECT * FROM users WHERE email = '$100bill@example.com'"
        );
        assert_eq!(
            replace_placeholders_for_inference("SELECT '${10}' AS template"),
            "SELECT '${10}' AS template"
        );
        assert_eq!(
            replace_placeholders_for_inference(
                "SELECT * FROM t WHERE a = $1 AND b = 'contains $2 inside'"
            ),
            "SELECT * FROM t WHERE a = 1 AND b = 'contains $2 inside'"
        );
    }

    #[test]
    fn test_replace_placeholders_preserves_double_quoted_identifiers() {
        assert_eq!(
            replace_placeholders_for_inference(r#"SELECT * FROM "table$1" WHERE id = $1"#),
            r#"SELECT * FROM "table$1" WHERE id = 1"#
        );
    }

    #[test]
    fn test_replace_placeholders_handles_escaped_single_quotes() {
        assert_eq!(
            replace_placeholders_for_inference("SELECT 'it''s $1' AS msg, $1 AS v"),
            "SELECT 'it''s $1' AS msg, 1 AS v"
        );
    }

    #[test]
    fn test_replace_placeholders_preserves_dollar_quoted_strings() {
        assert_eq!(
            replace_placeholders_for_inference("SELECT $$ $1 $$ AS body, $1 AS v"),
            "SELECT $$ $1 $$ AS body, 1 AS v"
        );
        assert_eq!(
            replace_placeholders_for_inference("SELECT $tag$ $1 $tag$ AS body, $1 AS v"),
            "SELECT $tag$ $1 $tag$ AS body, 1 AS v"
        );
    }

    #[test]
    fn test_replace_placeholders_high_numbers() {
        assert_eq!(
            replace_placeholders_for_inference("SELECT $1, $10, $100, $999"),
            "SELECT 1, 1, 1, 1"
        );
    }

    #[test]
    fn test_count_sql_parameters_ignores_dollar_quoted_strings() {
        assert_eq!(count_sql_parameters("SELECT $$ $99 $$, $1;"), 1);
        assert_eq!(count_sql_parameters("SELECT $tag$ $2 $tag$, $1;"), 1);
        assert_eq!(count_sql_parameters("SELECT $$ $100 $$, $2;"), 2);
        assert_eq!(count_sql_parameters("SELECT 'it''s $10', $2;"), 2);
        assert_eq!(count_sql_parameters(r#"SELECT "table$5", $1;"#), 1);
        assert_eq!(count_sql_parameters("SELECT $1, $10;"), 10);
    }

    #[test]
    fn test_count_sql_parameters_ignores_comments_and_identifier_tokens() {
        assert_eq!(count_sql_parameters("SELECT 1 /* $10 */;"), 0);
        assert_eq!(count_sql_parameters("SELECT 1 -- $10\n;"), 0);
        assert_eq!(count_sql_parameters("SELECT a$1 FROM t WHERE id = $1;"), 1);
        assert_eq!(count_sql_parameters("SELECT 1 /* $10 */ , $2;"), 2);
        assert_eq!(
            count_sql_parameters("SELECT /* outer /* $10 */ inner */ $1;"),
            1
        );
    }

    #[test]
    fn test_find_keyword_outside_strings_ignores_dollar_quoted_strings() {
        let query = "INSERT INTO t VALUES (1) $$ RETURNING $$ RETURNING id";
        let pos = find_keyword_outside_strings(query, "RETURNING").unwrap();
        assert_eq!(pos, query.rfind("RETURNING").unwrap());

        let query = "SELECT $tag$RETURNING$tag$ RETURNING";
        let pos = find_keyword_outside_strings(query, "RETURNING").unwrap();
        assert_eq!(pos, query.rfind("RETURNING").unwrap());

        let query = "SELECT RETURNINGX RETURNING";
        let pos = find_keyword_outside_strings(query, "RETURNING").unwrap();
        assert_eq!(pos, query.rfind("RETURNING").unwrap());
    }

    #[test]
    fn test_substitute_placeholders_preserves_dollar_quoted_strings() {
        let values = vec!["111".to_string()];
        assert_eq!(
            substitute_placeholders_outside_strings_and_dollar(
                "SELECT $$ $1 $$ AS body, $1 AS v",
                &values
            ),
            "SELECT $$ $1 $$ AS body, 111 AS v"
        );

        assert_eq!(
            substitute_placeholders_outside_strings_and_dollar(
                "SELECT 'it''s $1' AS msg, $1",
                &values
            ),
            "SELECT 'it''s $1' AS msg, 111"
        );
    }

    #[test]
    fn test_substitute_placeholders_handles_multi_digit_numbers() {
        let values = (1..=10).map(|i| i.to_string()).collect::<Vec<_>>();
        assert_eq!(
            substitute_placeholders_outside_strings_and_dollar("SELECT $10, $1", &values),
            "SELECT 10, 1"
        );

        assert_eq!(
            substitute_placeholders_outside_strings_and_dollar("SELECT '${10}', $1", &values),
            "SELECT '${10}', 1"
        );

        assert_eq!(
            substitute_placeholders_outside_strings_and_dollar("SELECT $$ $10 $$, $10", &values),
            "SELECT $$ $10 $$, 10"
        );
    }

    #[test]
    fn test_substitute_placeholders_ignores_comments_and_identifier_tokens() {
        let values = vec!["42".to_string()];
        assert_eq!(
            substitute_placeholders_outside_strings_and_dollar("SELECT 1 /* $1 */ , $1;", &values),
            "SELECT 1 /* $1 */ , 42;"
        );
        assert_eq!(
            substitute_placeholders_outside_strings_and_dollar("SELECT 1 -- $1\n, $1;", &values),
            "SELECT 1 -- $1\n, 42;"
        );
        assert_eq!(
            substitute_placeholders_outside_strings_and_dollar(
                "SELECT a$1 FROM t WHERE id = $1;",
                &values
            ),
            "SELECT a$1 FROM t WHERE id = 42;"
        );
        assert_eq!(
            substitute_placeholders_outside_strings_and_dollar(
                "SELECT /* outer /* $1 */ inner */ $1;",
                &values
            ),
            "SELECT /* outer /* $1 */ inner */ 42;"
        );
    }

    #[test]
    fn test_result_to_response_transaction_start_is_not_empty_query() {
        let resp = result_to_response(ExecuteResult::TransactionStart { tag: "BEGIN" }).unwrap();
        match resp {
            Response::TransactionStart(tag) => {
                let complete = CommandComplete::from(tag);
                assert_eq!(complete.tag, "BEGIN");
            }
            _ => panic!("expected TransactionStart"),
        }
    }

    #[test]
    fn test_result_to_response_transaction_end_is_not_empty_query() {
        let resp = result_to_response(ExecuteResult::TransactionEnd { tag: "COMMIT" }).unwrap();
        match resp {
            Response::TransactionEnd(tag) => {
                let complete = CommandComplete::from(tag);
                assert_eq!(complete.tag, "COMMIT");
            }
            _ => panic!("expected TransactionEnd"),
        }
    }

    #[test]
    fn test_result_to_response_command_complete_is_execution() {
        let resp = result_to_response(ExecuteResult::CommandComplete { tag: "SET" }).unwrap();
        match resp {
            Response::Execution(tag) => {
                let complete = CommandComplete::from(tag);
                assert_eq!(complete.tag, "SET");
            }
            _ => panic!("expected Execution"),
        }
    }

    #[test]
    fn test_result_to_response_empty_is_empty_query() {
        let resp = result_to_response(ExecuteResult::Empty).unwrap();
        assert!(matches!(resp, Response::EmptyQuery));
    }

    #[tokio::test]
    async fn test_extended_query_notice_emits_notice_response() {
        let mut client = RecordingSink::default();
        let results = crate::sql::ExecuteResults(vec![
            ExecuteResult::Notice {
                message: "table \"flow3_notice_test\" does not exist, skipping".to_string(),
            },
            ExecuteResult::CommandComplete { tag: "DROP TABLE" },
        ]);

        let resp = send_notices_and_get_last_response(&mut client, None, results)
            .await
            .unwrap();

        match resp {
            Response::Execution(tag) => {
                let complete = CommandComplete::from(tag);
                assert_eq!(complete.tag, "DROP TABLE");
            }
            _ => panic!("expected Execution"),
        }

        assert_eq!(client.messages.len(), 1);
        match &client.messages[0] {
            PgWireBackendMessage::NoticeResponse(notice) => {
                assert!(notice
                    .fields
                    .iter()
                    .any(|(code, value)| *code == b'S' && value == "NOTICE"));
                assert!(notice
                    .fields
                    .iter()
                    .any(|(code, value)| *code == b'C' && value == "00000"));
                assert!(notice.fields.iter().any(|(code, value)| {
                    *code == b'M' && value.contains("does not exist, skipping")
                }));
            }
            other => panic!("expected NoticeResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_extended_query_notice_respects_client_min_messages() {
        let mut client = RecordingSink::default();
        let results = crate::sql::ExecuteResults(vec![
            ExecuteResult::Notice {
                message: "test notice".to_string(),
            },
            ExecuteResult::CommandComplete { tag: "DROP TABLE" },
        ]);

        let _resp = send_notices_and_get_last_response(
            &mut client,
            Some("warning".to_string()),
            results,
        )
        .await
        .unwrap();

        assert!(client.messages.is_empty());
    }

    #[test]
    fn test_infer_parameter_types_limit() {
        let types = infer_parameter_types("SELECT * FROM users LIMIT $1", 1);
        assert_eq!(types, vec![Type::INT8]);
    }

    #[test]
    fn test_infer_parameter_types_offset() {
        let types = infer_parameter_types("SELECT * FROM users OFFSET $1", 1);
        assert_eq!(types, vec![Type::INT8]);
    }

    #[test]
    fn test_infer_parameter_types_limit_offset() {
        let types = infer_parameter_types("SELECT * FROM users LIMIT $1 OFFSET $2", 2);
        assert_eq!(types, vec![Type::INT8, Type::INT8]);
    }

    #[test]
    fn test_infer_parameter_types_fetch() {
        let types = infer_parameter_types("SELECT * FROM users FETCH FIRST $1 ROWS ONLY", 1);
        assert_eq!(types, vec![Type::INT8]);

        let types = infer_parameter_types("SELECT * FROM users FETCH NEXT $1 ROWS ONLY", 1);
        assert_eq!(types, vec![Type::INT8]);
    }

    #[test]
    fn test_infer_parameter_types_where_clause_defaults_to_text() {
        let types = infer_parameter_types("SELECT * FROM users WHERE id = $1", 1);
        assert_eq!(types, vec![Type::TEXT]);
    }

    #[test]
    fn test_infer_parameter_types_mixed() {
        let types =
            infer_parameter_types("SELECT * FROM users WHERE id = $1 LIMIT $2 OFFSET $3", 3);
        assert_eq!(types, vec![Type::TEXT, Type::INT8, Type::INT8]);
    }

    #[test]
    fn test_infer_parameter_types_preserves_string_literals() {
        let types = infer_parameter_types("SELECT 'LIMIT $1' FROM users LIMIT $1", 1);
        assert_eq!(types, vec![Type::INT8]);
    }

    #[test]
    fn test_infer_parameter_types_preserves_dollar_quoted() {
        let types = infer_parameter_types("SELECT $$ LIMIT $1 $$ FROM users LIMIT $1", 1);
        assert_eq!(types, vec![Type::INT8]);
    }

    #[test]
    fn test_infer_parameter_types_case_insensitive() {
        let types = infer_parameter_types("SELECT * FROM users limit $1", 1);
        assert_eq!(types, vec![Type::INT8]);

        let types = infer_parameter_types("SELECT * FROM users Offset $1", 1);
        assert_eq!(types, vec![Type::INT8]);
    }

    #[test]
    fn test_infer_parameter_types_no_params() {
        let types = infer_parameter_types("SELECT * FROM users", 0);
        assert!(types.is_empty());
    }

    #[test]
    fn test_infer_parameter_types_non_ascii_does_not_panic() {
        let types = infer_parameter_types("SELECT 'ııı' FROM users LIMIT $1", 1);
        assert_eq!(types, vec![Type::INT8]);
    }

    #[test]
    fn test_substitute_parameters_text_always_quoted() {
        let stmt = Arc::new(StoredStatement::new(
            "stmt".to_string(),
            "SELECT $1::text".to_string(),
            vec![Type::TEXT],
        ));
        let mut portal: Portal<String> = Portal::default();
        portal.name = "portal".to_string();
        portal.statement = stmt;
        portal.parameter_format = Format::UnifiedText;
        portal.parameters = vec![Some(Bytes::from_static(b"001"))];
        portal.result_column_format = Format::UnifiedText;

        assert_eq!(
            substitute_parameters("SELECT $1::text", &portal).unwrap(),
            "SELECT '001'::text"
        );
    }

    #[test]
    fn test_substitute_parameters_unknown_text_format_always_quoted() {
        let stmt = Arc::new(StoredStatement::new(
            "stmt".to_string(),
            "SELECT $1::text".to_string(),
            vec![],
        ));
        let mut portal: Portal<String> = Portal::default();
        portal.name = "portal".to_string();
        portal.statement = stmt;
        portal.parameter_format = Format::UnifiedText;
        portal.parameters = vec![Some(Bytes::from_static(b"001"))];
        portal.result_column_format = Format::UnifiedText;

        assert_eq!(
            substitute_parameters("SELECT $1::text", &portal).unwrap(),
            "SELECT '001'::text"
        );
    }

    #[test]
    fn test_substitute_parameters_escapes_single_quotes() {
        let stmt = Arc::new(StoredStatement::new(
            "stmt".to_string(),
            "SELECT $1".to_string(),
            vec![Type::TEXT],
        ));
        let mut portal: Portal<String> = Portal::default();
        portal.name = "portal".to_string();
        portal.statement = stmt;
        portal.parameter_format = Format::UnifiedText;
        portal.parameters = vec![Some(Bytes::from_static(b"O'Reilly"))];
        portal.result_column_format = Format::UnifiedText;

        assert_eq!(
            substitute_parameters("SELECT $1", &portal).unwrap(),
            "SELECT 'O''Reilly'"
        );
    }

    #[test]
    fn test_substitute_parameters_int4_text_format_renders_number() {
        let stmt = Arc::new(StoredStatement::new(
            "stmt".to_string(),
            "SELECT $1".to_string(),
            vec![Type::INT4],
        ));
        let mut portal: Portal<String> = Portal::default();
        portal.name = "portal".to_string();
        portal.statement = stmt;
        portal.parameter_format = Format::UnifiedText;
        portal.parameters = vec![Some(Bytes::from_static(b"42"))];
        portal.result_column_format = Format::UnifiedText;

        assert_eq!(
            substitute_parameters("SELECT $1", &portal).unwrap(),
            "SELECT 42"
        );
    }

    #[test]
    fn test_substitute_parameters_int4_text_format_invalid_errors() {
        let stmt = Arc::new(StoredStatement::new(
            "stmt".to_string(),
            "SELECT $1".to_string(),
            vec![Type::INT4],
        ));
        let mut portal: Portal<String> = Portal::default();
        portal.name = "portal".to_string();
        portal.statement = stmt;
        portal.parameter_format = Format::UnifiedText;
        portal.parameters = vec![Some(Bytes::from_static(b"not-a-number"))];
        portal.result_column_format = Format::UnifiedText;

        assert!(substitute_parameters("SELECT $1", &portal).is_err());
    }

    #[test]
    fn test_substitute_parameters_uuid_binary_format_renders_uuid_literal() {
        let stmt = Arc::new(StoredStatement::new(
            "stmt".to_string(),
            "SELECT $1".to_string(),
            vec![Type::UUID],
        ));
        let uuid =
            uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("valid uuid");
        let mut portal: Portal<String> = Portal::default();
        portal.name = "portal".to_string();
        portal.statement = stmt;
        portal.parameter_format = Format::UnifiedBinary;
        portal.parameters = vec![Some(Bytes::copy_from_slice(uuid.as_bytes()))];
        portal.result_column_format = Format::UnifiedText;

        assert_eq!(
            substitute_parameters("SELECT $1", &portal).unwrap(),
            "SELECT '550e8400-e29b-41d4-a716-446655440000'::uuid"
        );
    }

    #[test]
    fn test_encode_value_timestamp_negative_millis() {
        let col_type = DataType::Timestamp;
        assert_eq!(
            encode_value_to_string(&Value::Timestamp(-1), Some(&col_type)),
            "1969-12-31 23:59:59.999000"
        );
        assert_eq!(
            encode_value_to_string(&Value::Timestamp(-1001), Some(&col_type)),
            "1969-12-31 23:59:58.999000"
        );
    }

    #[test]
    fn test_encode_value_int64_as_timestamp_negative_millis() {
        let col_type = DataType::Timestamp;
        assert_eq!(
            encode_value_to_string(&Value::Int64(-1), Some(&col_type)),
            "1969-12-31 23:59:59.999000"
        );
    }
}
