use crate::sql::{ExecuteResult, Session};
use crate::types::DataType;
use futures::{Sink, SinkExt};
use pgwire::api::portal::Format;
use pgwire::api::results::Response;
// Re-exported for tests (via `use super::*`)
#[allow(unused_imports)]
use pgwire::api::results::{FieldFormat, FieldInfo};
#[allow(unused_imports)] // re-exported for tests (via `use super::*`)
use pgwire::api::Type;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::response::NoticeResponse;
use pgwire::messages::PgWireBackendMessage;
use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::atomic::AtomicI32;

// These imports are used by tests (via `use super::*`) and by dynamic.rs.
#[allow(unused_imports)]
use crate::types::{ColumnDef, TableSchema};
#[allow(unused_imports)]
use sqlparser::ast::{
    FunctionArg, FunctionArgExpr, Ident, ObjectName, Query, Select, SelectItem, SetExpr, Statement,
    TableFactor, TableWithJoins, Values,
};
#[allow(unused_imports)]
use std::collections::HashMap;
#[allow(unused_imports)]
use std::sync::Arc;

mod copy;
mod dynamic;
mod encode;
mod errors;
mod params;
mod portal;
mod prepared;
mod query_parser;
mod server_params;
mod tenant;

#[allow(unused_imports)] // re-exported for tests (via `use super::*`)
use encode::{datatype_to_pgtype, result_to_response, result_to_response_with_format};

pub use dynamic::DynamicHandlerFactory;
#[allow(unused_imports)]
pub use dynamic::DynamicPgHandler;
pub use query_parser::Db9QueryParser;
pub use server_params::PgServerParameterProvider;

/// Custom metadata key for storing the extracted keyspace
const METADATA_KEYSPACE: &str = "keyspace";
/// Custom metadata key for storing the actual username (after parsing tenant.user)
const METADATA_ACTUAL_USER: &str = "actual_user";
/// Custom metadata key for storing authenticated superuser status ("on"/"off")
const METADATA_AUTH_IS_SUPERUSER: &str = "auth_is_superuser";

/// Global atomic counter for generating unique connection IDs
static CONNECTION_ID_COUNTER: AtomicI32 = AtomicI32::new(1);

async fn rollback_autocommit_or_mark_failed(session: &mut Session, started_txn: bool) {
    if started_txn {
        let _ = session.rollback().await;
    } else {
        session.mark_transaction_failed();
    }
}

pub type CopyContext = copy::CopyContext;

fn normalize_copy_ident(token: &str) -> String {
    let token = token.trim();
    if token.starts_with('"') && token.ends_with('"') && token.len() >= 2 {
        token[1..token.len() - 1].replace("\"\"", "\"")
    } else {
        token.to_lowercase()
    }
}

fn resolve_copy_columns(
    schema: &crate::types::TableSchema,
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
                format!("column \"{}\" specified more than once", def.name),
            ))));
        }

        resolved_columns.push(def.name.clone());
        column_types.push(Some(def.data_type.clone()));
    }

    Ok((resolved_columns, column_types))
}

fn parse_startup_options(options: &str) -> Vec<(String, String)> {
    fn tokenize_options(options: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut quote: Option<char> = None;
        let mut chars = options.chars();

        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    } else {
                        current.push('\\');
                    }
                }
                '\'' | '"' => {
                    if quote == Some(ch) {
                        quote = None;
                    } else if quote.is_none() {
                        quote = Some(ch);
                    } else {
                        current.push(ch);
                    }
                }
                ch if ch.is_whitespace() => {
                    if quote.is_some() {
                        current.push(ch);
                    } else if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                ch => current.push(ch),
            }
        }

        if !current.is_empty() {
            tokens.push(current);
        }

        tokens
    }

    let tokens = tokenize_options(options);
    let mut settings = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i].as_str() == "-c" {
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

pub(super) fn client_allows_message(client_min_messages: Option<&str>, severity: &str) -> bool {
    let notice_rank = client_min_messages_rank("notice").unwrap_or(8);
    let severity_rank = client_min_messages_rank(severity).unwrap_or(notice_rank);
    let min_rank = client_min_messages
        .and_then(client_min_messages_rank)
        .unwrap_or(notice_rank);
    severity_rank >= min_rank
}

#[cfg(test)]
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
    send_notices_and_get_last_response_with_format(
        client,
        client_min_messages,
        results,
        &Format::UnifiedText,
    )
    .await
}

async fn send_notices_and_get_last_response_with_format<C>(
    client: &mut C,
    client_min_messages: Option<String>,
    results: crate::sql::ExecuteResults,
    result_format: &Format,
) -> PgWireResult<Response<'static>>
where
    C: Sink<PgWireBackendMessage> + Unpin + Send + Sync,
    C::Error: Debug,
    PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
{
    let client_min_messages = client_min_messages.as_deref();
    let mut last: Option<Response<'static>> = None;
    for result in results.into_vec() {
        match result {
            ExecuteResult::Notice { message, severity } => {
                if client_allows_message(client_min_messages, &severity) {
                    let notice = NoticeResponse::from(ErrorInfo::new(
                        severity,
                        "00000".to_string(),
                        message,
                    ));
                    client
                        .send(PgWireBackendMessage::NoticeResponse(notice))
                        .await?;
                }
            }
            other => {
                last = Some(result_to_response_with_format(other, result_format)?);
            }
        }
    }
    Ok(last.unwrap_or(Response::EmptyQuery))
}

#[cfg(test)]
mod tests;
