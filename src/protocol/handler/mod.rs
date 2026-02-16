use crate::sql::{ExecuteResult, Session};
use crate::types::DataType;
use futures::{Sink, SinkExt};
use pgwire::api::results::Response;
// Re-exported for tests (via `use super::*`)
#[allow(unused_imports)]
use pgwire::api::results::{FieldFormat, FieldInfo};
use pgwire::api::Type;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::response::NoticeResponse;
use pgwire::messages::PgWireBackendMessage;
use sqlparser::ast::Expr;
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
#[allow(unused_imports)]
use tikv_client::Transaction;

mod copy;
mod dynamic;
mod encode;
mod errors;
mod params;
mod portal;
mod query_parser;
mod schema_resolve;
mod server_params;
mod tenant;
mod type_infer;
mod view_infer;

use encode::{datatype_to_pgtype, result_to_response};

// Re-export items moved to sub-modules so that `use super::*` in tests/dynamic still works.
use type_infer::{infer_result_fields_from_query_ast, stub_describe_field};
// Re-export SourceSchema for tests (used via `use super::*`).
#[cfg(test)]
pub(self) use type_infer::infer_fs9_table_function_schema;
#[allow(unused_imports)] // used by dynamic.rs via `use super::*`
pub(self) use type_infer::SourceSchema;

pub use dynamic::DynamicHandlerFactory;
#[allow(unused_imports)]
pub use dynamic::DynamicPgHandler;
pub use query_parser::TipgQueryParser;
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

#[cfg(test)]
mod tests;
