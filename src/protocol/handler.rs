use crate::auth::AuthManager;
use crate::observability;
use crate::pool::TikvClientPool;
use crate::sql::expr::set_connection_id;
use crate::sql::{ExecuteResult, Executor, Session};
use crate::storage::TikvStore;
use crate::types::{DataType, Value};
use async_trait::async_trait;
use futures::{stream, Sink, SinkExt};
use pgwire::api::auth::{ServerParameterProvider, StartupHandler};
use pgwire::api::copy::CopyHandler;
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    CopyResponse, DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat,
    FieldInfo, QueryResponse, Response, Tag,
};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::{
    ClientInfo, NoopErrorHandler, PgWireConnectionState, PgWireServerHandlers, Type,
    METADATA_DATABASE, METADATA_USER,
};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::copy::{CopyData, CopyDone, CopyFail};
use pgwire::messages::data::DataRow;
use pgwire::messages::response::{CommandComplete, ErrorResponse, NoticeResponse};
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use sqlparser::ast::{Expr, ObjectName, SelectItem, Statement, TableFactor};
use std::collections::HashMap;
use std::fmt::Debug;
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
    pub data_buffer: Vec<Vec<u8>>,
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
                max_param = max_param.max(num);
                i = j;
                continue;
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

fn resolve_table_for_insert(table_name: &str, search_path: &[String]) -> String {
    // Strip quotes from table name (GORM uses quoted identifiers)
    let strip_quotes = |s: &str| -> String {
        s.trim_matches('"').to_string()
    };
    
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
                sqlparser::ast::FunctionArg::Named { arg: sqlparser::ast::FunctionArgExpr::Expr(e), .. } => {
                    count_placeholders_in_expr(e)
                }
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
        Expr::BinaryOp { left, op, right } => {
            match op {
                sqlparser::ast::BinaryOperator::Eq
                | sqlparser::ast::BinaryOperator::NotEq
                | sqlparser::ast::BinaryOperator::Lt
                | sqlparser::ast::BinaryOperator::LtEq
                | sqlparser::ast::BinaryOperator::Gt
                | sqlparser::ast::BinaryOperator::GtEq => {
                    if let (Expr::Identifier(ident), Expr::Value(sqlparser::ast::Value::Placeholder(p))) = (left.as_ref(), right.as_ref()) {
                        if let Some(idx) = extract_placeholder_index(p) {
                            let col_name = ident.value.to_lowercase();
                            if let Some(col_type) = col_types.get(&col_name) {
                                if idx < types.len() {
                                    types[idx] = datatype_to_pgtype(Some(col_type));
                                }
                            }
                        }
                    } else if let (Expr::Value(sqlparser::ast::Value::Placeholder(p)), Expr::Identifier(ident)) = (left.as_ref(), right.as_ref()) {
                        if let Some(idx) = extract_placeholder_index(p) {
                            let col_name = ident.value.to_lowercase();
                            if let Some(col_type) = col_types.get(&col_name) {
                                if idx < types.len() {
                                    types[idx] = datatype_to_pgtype(Some(col_type));
                                }
                            }
                        }
                    } else if let (Expr::CompoundIdentifier(parts), Expr::Value(sqlparser::ast::Value::Placeholder(p))) = (left.as_ref(), right.as_ref()) {
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
            }
        }
        Expr::Nested(inner) => infer_types_from_expr(inner, col_types, types),
        Expr::InList { expr: left_expr, list, .. } => {
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

    if let Some(schema) = schema_opt {
        let full = format!("{}.{}", schema, name);
        return store.get_schema(txn, db_id, &full).await.ok().flatten();
    }

    for schema in search_path {
        let full = format!("{}.{}", schema, name);
        if let Ok(Some(s)) = store.get_schema(txn, db_id, &full).await {
            return Some(s);
        }
    }

    // As a last resort, try the default schema even if it's not present in the session search_path.
    let default_schema = search_path.first().map(String::as_str).unwrap_or("public");
    let full = format!("{}.{}", default_schema, name);
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
        infer_returning_fields_with_txn(store.as_ref(), txn, db_id, search_path, table_name, returning).await
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

pub struct DynamicPgHandler {
    client_pool: Option<Arc<TikvClientPool>>,
    pd_endpoints: Vec<String>,
    default_keyspace: Option<String>,
    executor: OnceCell<Arc<Executor>>,
    session: Mutex<Option<Session>>,
    connection_guard: OnceCell<observability::ConnectionGuard>,
    copy_context: Mutex<Option<CopyContext>>,
    query_parser: Arc<NoopQueryParser>,
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
            query_parser: Arc::new(NoopQueryParser::new()),
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
            query_parser: Arc::new(NoopQueryParser::new()),
            connection_id: CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn connection_id(&self) -> i32 {
        self.connection_id
    }

    async fn infer_insert_parameter_types(&self, sql: &str, param_count: usize) -> Option<Vec<Type>> {
        if param_count == 0 {
            return Some(vec![]);
        }

        let parsed = crate::sql::parse_sql(sql).ok()?;
        let stmt = parsed.into_iter().next()?;
        
        let (table_name, columns, values_list): (String, Vec<String>, Vec<Vec<Expr>>) = match stmt {
            Statement::Insert { table_name, columns, source, .. } => {
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

        info!("infer_insert_parameter_types: table={}, columns={:?}, param_count={}", table_name, columns, param_count);

        let executor = self.executor.get()?;
        let store = executor.store();
        
        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut()?;
        let mut txn = store.begin().await.ok()?;
        
        let search_path = session.search_path();
        let resolved_table = resolve_table_for_insert(&table_name, search_path);
        
        info!("infer_insert_parameter_types: resolved_table={}", resolved_table);
        
        let schema = store
            .get_schema(&mut txn, session.current_database_id(), &resolved_table)
            .await
            .ok()??;
        let _ = txn.rollback().await;
        
        info!("infer_insert_parameter_types: got schema with {} columns", schema.columns.len());
        
        let col_types: std::collections::HashMap<String, DataType> = schema
            .columns
            .iter()
            .map(|c| (c.name.to_lowercase(), c.data_type.clone()))
            .collect();
        
        let column_order: Vec<String> = if !columns.is_empty() {
            columns.iter().map(|c: &String| c.to_lowercase()).collect()
        } else {
            schema.columns.iter().map(|c| c.name.to_lowercase()).collect()
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

    async fn infer_update_parameter_types(&self, sql: &str, param_count: usize) -> Option<Vec<Type>> {
        if param_count == 0 {
            return Some(vec![]);
        }

        let parsed = crate::sql::parse_sql(sql).ok()?;
        let stmt = parsed.into_iter().next()?;
        
        let (table_name, assignments) = match stmt {
            Statement::Update { table, assignments, .. } => {
                let table_name = match &table.relation {
                    sqlparser::ast::TableFactor::Table { name, .. } => name.to_string(),
                    _ => return None,
                };
                (table_name, assignments)
            }
            _ => return None,
        };

        info!("infer_update_parameter_types: table={}, assignments={}, param_count={}", 
              table_name, assignments.len(), param_count);

        let executor = self.executor.get()?;
        let store = executor.store();
        
        let mut session_guard = self.session.lock().await;
        let session = session_guard.as_mut()?;
        let mut txn = store.begin().await.ok()?;
        
        let search_path = session.search_path();
        let resolved_table = resolve_table_for_insert(&table_name, search_path);
        
        info!("infer_update_parameter_types: resolved_table={}", resolved_table);
        
        let schema = store
            .get_schema(&mut txn, session.current_database_id(), &resolved_table)
            .await
            .ok()??;
        let _ = txn.rollback().await;
        
        info!("infer_update_parameter_types: got schema with {} columns", schema.columns.len());
        
        let col_types: std::collections::HashMap<String, DataType> = schema
            .columns
            .iter()
            .map(|c| (c.name.to_lowercase(), c.data_type.clone()))
            .collect();
        
        let mut types = vec![Type::TEXT; param_count];
        let mut param_idx = 0usize;
        
        for assignment in &assignments {
            let col_names: Vec<String> = assignment.id.iter()
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
        
        info!("infer_update_parameter_types: inferred {} types, remaining {} as TEXT", param_idx, param_count - param_idx);
        
        Some(types)
    }

    async fn infer_select_parameter_types(&self, sql: &str, param_count: usize) -> Option<Vec<Type>> {
        if param_count == 0 {
            return Some(vec![]);
        }

        let parsed = crate::sql::parse_sql(sql).ok()?;
        let stmt = parsed.into_iter().next()?;
        
        let (table_name, selection, limit_expr, offset_expr) = match stmt {
            Statement::Query(query) => {
                if let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() {
                    let table_name = select.from.first().and_then(|f| {
                        match &f.relation {
                            sqlparser::ast::TableFactor::Table { name, .. } => Some(name.to_string()),
                            _ => None,
                        }
                    })?;
                    (table_name, select.selection.clone(), query.limit.clone(), query.offset.clone())
                } else {
                    return None;
                }
            }
            _ => return None,
        };

        info!("infer_select_parameter_types: table={}, param_count={}", table_name, param_count);

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

        // Get the executor if initialized
        let executor = match self.executor.get() {
            Some(exec) => exec,
            None => {
                // Executor not initialized yet, return stub
                return vec![FieldInfo::new(
                    "column".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Text,
                )];
            }
        };

        // Get session lock
        let mut session_guard = self.session.lock().await;

        // Check if session exists
        if session_guard.is_none() {
            // No session yet, return stub
            return vec![FieldInfo::new(
                "column".to_string(),
                None,
                None,
                Type::TEXT,
                FieldFormat::Text,
            )];
        }

        let store = executor.store();
        let session = session_guard.as_mut().unwrap();

        if let Some(ref stmt) = parsed_stmt {
            if let Some(fields) = infer_returning_fields_from_statement(&store, session, stmt).await
            {
                return fields;
            }
        }

        // Only infer SELECT (Statement::Query) metadata here; RETURNING is handled above.
        let is_select = matches!(parsed_stmt, Some(Statement::Query(_)))
            || (parsed_stmt.is_none() && is_select_str);
        if !is_select {
            return vec![FieldInfo::new(
                "column".to_string(),
                None,
                None,
                Type::TEXT,
                FieldFormat::Text,
            )];
        }

        // Execute SELECT query with LIMIT 1 to get column metadata without side effects
        // Replace any parameter placeholders ($1, $2, etc.) with defaults for type inference
        let query_with_defaults = replace_placeholders_for_inference(query);
        let metadata_query = if query_upper.contains(" LIMIT ") {
            query_with_defaults
        } else {
            format!("{} LIMIT 1", query_with_defaults)
        };

        match executor
            .execute(session, &metadata_query)
            .await
            .map(|r| r.last())
        {
            Ok(crate::sql::ExecuteResult::Select {
                columns,
                column_types,
                rows,
            }) => {
                if let Some(types) = column_types {
                    columns
                        .iter()
                        .enumerate()
                        .map(|(i, name)| {
                            FieldInfo::new(
                                name.clone(),
                                None,
                                None,
                                datatype_to_pgtype(types.get(i)),
                                FieldFormat::Text,
                            )
                        })
                        .collect()
                } else if let Some(first_row) = rows.first() {
                    // Fall back to inferring from first row
                    columns
                        .iter()
                        .enumerate()
                        .map(|(i, name)| {
                            let pg_type = if let Some(value) = first_row.values.get(i) {
                                let dt = value.data_type();
                                datatype_to_pgtype(dt.as_ref())
                            } else {
                                Type::TEXT
                            };
                            FieldInfo::new(name.clone(), None, None, pg_type, FieldFormat::Text)
                        })
                        .collect()
                } else {
                    // No rows and no types, default to TEXT
                    columns
                        .iter()
                        .map(|name| {
                            FieldInfo::new(name.clone(), None, None, Type::TEXT, FieldFormat::Text)
                        })
                        .collect()
                }
            }
            _ => {
                // Query didn't return SELECT result, return stub
                vec![FieldInfo::new(
                    "column".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Text,
                )]
            }
        }
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
        let query_upper = query.to_uppercase();
        if !query_upper.contains("COPY")
            || !query_upper.contains("FROM")
            || !query_upper.contains("STDIN")
        {
            return None;
        }

        // Regex: COPY [public.]table_name (col1, col2, ...) FROM stdin
        let re = regex::Regex::new(r"(?i)COPY\s+(?:public\.)?(\w+)\s*\(([^)]+)\)\s+FROM\s+stdin")
            .ok()?;
        if let Some(caps) = re.captures(query) {
            let table_name = caps.get(1)?.as_str().to_string();
            let columns_str = caps.get(2)?.as_str();
            let columns: Vec<String> = columns_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            return Some((table_name, columns));
        }

        // Regex: COPY [public.]table_name FROM stdin (no column list)
        let re2 = regex::Regex::new(r"(?i)COPY\s+(?:public\.)?(\w+)\s+FROM\s+stdin").ok()?;
        if let Some(caps) = re2.captures(query) {
            let table_name = caps.get(1)?.as_str().to_string();
            return Some((table_name, vec![]));
        }

        None
    }

    fn parse_copy_to_command(query: &str) -> Option<(String, Vec<String>)> {
        let query_upper = query.to_uppercase();
        if !query_upper.contains("COPY") || !query_upper.contains("TO") {
            return None;
        }
        if !query_upper.contains("STDOUT") {
            return None;
        }

        // COPY [schema.]table (col1, col2) TO STDOUT
        let re =
            regex::Regex::new(r"(?i)COPY\s+(?:(\w+)\.)?(\w+)\s*\(([^)]+)\)\s+TO\s+STDOUT").ok()?;
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
        let re2 = regex::Regex::new(r"(?i)COPY\s+(?:(\w+)\.)?(\w+)\s+TO\s+STDOUT").ok()?;
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

        // Try bootstrap with error handling for gRPC transport issues
        let bootstrap_result = async {
            let mut txn = store.begin().await?;
            auth_manager.bootstrap(&mut txn).await?;
            txn.commit().await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;

        if let Err(e) = bootstrap_result {
            let err_str = e.to_string();
            if err_str.contains("gRPC") || err_str.contains("transport") {
                warn!("Auth bootstrap failed with transport error (TiKV issue), allowing connection: {}", e);
                return Ok((true, true));
            }
            return Err(format!("Failed to bootstrap auth: {}", e));
        }

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
                                        if let Err(e) = session.set_known_setting(
                                            &key.to_ascii_lowercase(),
                                            value,
                                        ) {
                                            warn!(
                                                "Failed to apply startup option {}: {}",
                                                key, e
                                            );
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

            let col_count = if columns.is_empty() {
                let mut session_guard = self.session.lock().await;
                let session = session_guard.as_mut().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        "Session not initialized".to_string(),
                    )))
                })?;
                let db_id = session.current_database_id();
                session.begin().await.ok();
                let count = if let Some(txn) = session.get_mut_txn() {
                    if let Ok(Some(schema)) = executor
                        .store()
                        .get_schema(txn, db_id, &table_name)
                        .await
                    {
                        schema.columns.len()
                    } else {
                        1
                    }
                } else {
                    1
                };
                session.rollback().await.ok();
                count
            } else {
                columns.len()
            };

            let mut ctx = self.copy_context.lock().await;
            *ctx = Some(CopyContext {
                table_name,
                columns,
                data_buffer: Vec::new(),
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
                Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    e.to_string(),
                ))))
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
        let mut ctx_guard = self.copy_context.lock().await;
        if let Some(ref mut ctx) = *ctx_guard {
            ctx.data_buffer.push(copy_data.data.to_vec());
        }
        Ok(())
    }

    async fn on_copy_done<C>(&self, client: &mut C, _done: CopyDone) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let executor = self.get_executor()?;

        let ctx_opt = {
            let mut ctx_guard = self.copy_context.lock().await;
            ctx_guard.take()
        };

        let row_count = if let Some(ctx) = ctx_opt {
            let mut session_guard = self.session.lock().await;
            let session = session_guard.as_mut().ok_or_else(|| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    "Session not initialized".to_string(),
                )))
            })?;

            session.begin().await.map_err(|e| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    e.to_string(),
                )))
            })?;

            let db_id = session.current_database_id();
            let schema = {
                let txn = session.get_mut_txn().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        "No transaction".to_string(),
                    )))
                })?;
                executor
                    .store()
                    .get_schema(txn, db_id, &ctx.table_name)
                    .await
                    .map_err(|e| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "XX000".to_string(),
                            e.to_string(),
                        )))
                    })?
                    .ok_or_else(|| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "42P01".to_string(),
                            format!("relation \"{}\" does not exist", ctx.table_name),
                        )))
                    })?
            };
            session.rollback().await.ok();

            let columns: Vec<String> = if ctx.columns.is_empty() {
                schema.columns.iter().map(|c| c.name.clone()).collect()
            } else {
                ctx.columns.clone()
            };

            let mut all_data = Vec::new();
            for chunk in &ctx.data_buffer {
                all_data.extend_from_slice(chunk);
            }

            let data_str = String::from_utf8_lossy(&all_data);
            let lines: Vec<&str> = data_str.lines().filter(|l| !l.is_empty()).collect();

            let mut count = 0usize;

            for line in lines {
                let values: Vec<&str> = line.split('\t').collect();

                if values.len() != columns.len() {
                    continue;
                }

                let mut col_values: Vec<(String, Value)> = Vec::new();
                for (col_name, val) in columns.iter().zip(values.iter()) {
                    let value = if *val == "\\N" {
                        Value::Null
                    } else {
                        let col_schema = schema.columns.iter().find(|c| c.name == *col_name);
                        if let Some(cs) = col_schema {
                            executor.parse_value_for_copy(val, &cs.data_type)
                        } else {
                            Value::Text(val.to_string())
                        }
                    };
                    col_values.push((col_name.clone(), value));
                }

                if let Err(e) = executor
                    .execute_copy_insert(session, &ctx.table_name, col_values)
                    .await
                {
                    error!("COPY insert error: {}", e);
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        e.to_string(),
                    ))));
                }
                count += 1;
            }

            count
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
        let mut ctx_guard = self.copy_context.lock().await;
        *ctx_guard = None;

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
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn do_query<'a, 'b: 'a, C>(
        &'b self,
        _client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let executor = self.get_executor()?;
        let query = &portal.statement.statement;
        debug!("Extended query: {}", query);

        let final_query = substitute_parameters(query, portal);
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
            Ok(results) => result_to_response(results.last()),
            Err(e) => {
                error!("Extended query execution error: {}", e);
                Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
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

        info!("do_describe_statement: sql={}, param_count={}, initial_types={:?}", stmt.statement.chars().take(100).collect::<String>(), param_count, param_types);
        
        if param_types.len() < param_count || param_types.iter().any(|t| *t == Type::UNKNOWN) {
            let inferred = if let Some(insert_types) = self.infer_insert_parameter_types(&stmt.statement, param_count).await {
                info!("do_describe_statement: inferred INSERT types={:?}", insert_types);
                insert_types
            } else if let Some(update_types) = self.infer_update_parameter_types(&stmt.statement, param_count).await {
                info!("do_describe_statement: inferred UPDATE types={:?}", update_types);
                update_types
            } else if let Some(select_types) = self.infer_select_parameter_types(&stmt.statement, param_count).await {
                info!("do_describe_statement: inferred SELECT types={:?}", select_types);
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

        let fields = self.infer_result_fields_from_query(&stmt.statement).await;
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
        let final_query = substitute_parameters(&portal.statement.statement, portal);
        let fields = self.infer_result_fields_from_query(&final_query).await;
        Ok(DescribePortalResponse::new(fields))
    }
}

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
                if num >= 1 && num <= values.len() {
                    out.extend_from_slice(values[num - 1].as_bytes());
                } else {
                    out.extend_from_slice(&bytes[i..j]);
                }
                i = j;
                continue;
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

fn substitute_parameters(query: &str, portal: &Portal<String>) -> String {
    let mut values: Vec<String> = Vec::with_capacity(portal.parameter_len());

    for i in 0..portal.parameter_len() {
        let param_type = portal
            .statement
            .parameter_types
            .get(i)
            .cloned()
            .unwrap_or(Type::UNKNOWN);

        let value_str = match &param_type {
            t if *t == Type::BOOL => portal
                .parameter::<bool>(i, &param_type)
                .ok()
                .flatten()
                .map(|v| if v { "true" } else { "false" }.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            t if *t == Type::INT2 => portal
                .parameter::<i16>(i, &param_type)
                .ok()
                .flatten()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            t if *t == Type::INT4 => portal
                .parameter::<i32>(i, &param_type)
                .ok()
                .flatten()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            t if *t == Type::INT8 => portal
                .parameter::<i64>(i, &param_type)
                .ok()
                .flatten()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            t if *t == Type::FLOAT4 => portal
                .parameter::<f32>(i, &param_type)
                .ok()
                .flatten()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            t if *t == Type::FLOAT8 => portal
                .parameter::<f64>(i, &param_type)
                .ok()
                .flatten()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            t if *t == Type::TIMESTAMP || *t == Type::TIMESTAMPTZ => {
                use chrono::{DateTime, NaiveDateTime, Utc};
                if portal.parameter_format.is_binary(i) {
                    if let Ok(Some(ts)) = portal.parameter::<DateTime<Utc>>(i, &param_type) {
                        format!("'{}'", ts.format("%Y-%m-%d %H:%M:%S%.6f%:z"))
                    } else if let Ok(Some(ts)) = portal.parameter::<NaiveDateTime>(i, &param_type) {
                        format!("'{}'", ts.format("%Y-%m-%d %H:%M:%S%.6f"))
                    } else {
                        "NULL".to_string()
                    }
                } else {
                    portal
                        .parameter::<String>(i, &Type::TEXT)
                        .ok()
                        .flatten()
                        .map(|v| format!("'{}'", v.replace("'", "''")))
                        .unwrap_or_else(|| "NULL".to_string())
                }
            }
            t if *t == Type::UNKNOWN => {
                if portal.parameter_format.is_binary(i) {
                    // Binary format - try to decode as common types
                    // NOTE: Do NOT try timestamp here - timestamps and i64 are both 8 bytes,
                    // and we can't reliably distinguish them without knowing the actual type.
                    // Timestamps should only be decoded when param_type is TIMESTAMP/TIMESTAMPTZ.
                    if let Ok(Some(v)) = portal.parameter::<i32>(i, &Type::INT4) {
                        v.to_string()
                    } else if let Ok(Some(v)) = portal.parameter::<i64>(i, &Type::INT8) {
                        v.to_string()
                    } else if let Ok(Some(v)) = portal.parameter::<bool>(i, &Type::BOOL) {
                        if v { "true" } else { "false" }.to_string()
                    } else if let Ok(Some(v)) = portal.parameter::<f64>(i, &Type::FLOAT8) {
                        v.to_string()
                    } else if let Ok(Some(v)) = portal.parameter::<String>(i, &Type::TEXT) {
                        format!("'{}'", v.replace("'", "''"))
                    } else {
                        "NULL".to_string()
                    }
                } else {
                    // Text format - read as string and auto-detect
                    portal
                        .parameter::<String>(i, &Type::TEXT)
                        .ok()
                        .flatten()
                        .map(|v| {
                            if v.parse::<i64>().is_ok() || v.parse::<f64>().is_ok() {
                                v
                            } else if v.eq_ignore_ascii_case("true")
                                || v.eq_ignore_ascii_case("false")
                            {
                                v.to_lowercase()
                            } else {
                                format!("'{}'", v.replace("'", "''"))
                            }
                        })
                        .unwrap_or_else(|| "NULL".to_string())
                }
            }
            _ => portal
                .parameter::<String>(i, &param_type)
                .ok()
                .flatten()
                .map(|v| {
                    if v.parse::<i64>().is_ok() || v.parse::<f64>().is_ok() {
                        v
                    } else if v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false") {
                        v.to_lowercase()
                    } else {
                        format!("'{}'", v.replace("'", "''"))
                    }
                })
                .unwrap_or_else(|| "NULL".to_string()),
        };

        values.push(value_str);
    }

    substitute_placeholders_outside_strings_and_dollar(query, &values)
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
    query_parser: Arc<NoopQueryParser>,
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
            query_parser: Arc::new(NoopQueryParser::new()),
            connection_id,
        }
    }

    #[allow(dead_code)]
    async fn infer_result_fields_from_query(&self, query: &str) -> Vec<FieldInfo> {
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

        // Get the session
        let mut session_guard = self.session.lock().await;

        let store = self.executor.store();
        if let Some(ref stmt) = parsed_stmt {
            if let Some(fields) =
                infer_returning_fields_from_statement(&store, &mut session_guard, stmt).await
            {
                return fields;
            }
        };

        // Only infer SELECT (Statement::Query) metadata here; RETURNING is handled above.
        let is_select = matches!(parsed_stmt, Some(Statement::Query(_)))
            || (parsed_stmt.is_none() && is_select_str);
        if !is_select {
            return vec![FieldInfo::new(
                "column".to_string(),
                None,
                None,
                Type::TEXT,
                FieldFormat::Text,
            )];
        }

        // Execute SELECT query with LIMIT 1 to get column metadata without side effects
        // Replace any parameter placeholders ($1, $2, etc.) with defaults for type inference
        let query_with_defaults = replace_placeholders_for_inference(query);
        let metadata_query = if query_upper.contains(" LIMIT ") {
            query_with_defaults
        } else {
            format!("{} LIMIT 1", query_with_defaults)
        };

        match self
            .executor
            .execute(&mut session_guard, &metadata_query)
            .await
            .map(|r| r.last())
        {
            Ok(crate::sql::ExecuteResult::Select {
                columns,
                column_types,
                rows,
            }) => {
                if let Some(types) = column_types {
                    columns
                        .iter()
                        .enumerate()
                        .map(|(i, name)| {
                            FieldInfo::new(
                                name.clone(),
                                None,
                                None,
                                datatype_to_pgtype(types.get(i)),
                                FieldFormat::Text,
                            )
                        })
                        .collect()
                } else if let Some(first_row) = rows.first() {
                    // Fall back to inferring from first row
                    columns
                        .iter()
                        .enumerate()
                        .map(|(i, name)| {
                            let pg_type = if let Some(value) = first_row.values.get(i) {
                                let dt = value.data_type();
                                datatype_to_pgtype(dt.as_ref())
                            } else {
                                Type::TEXT
                            };
                            FieldInfo::new(name.clone(), None, None, pg_type, FieldFormat::Text)
                        })
                        .collect()
                } else {
                    // No rows and no types, default to TEXT
                    columns
                        .iter()
                        .map(|name| {
                            FieldInfo::new(name.clone(), None, None, Type::TEXT, FieldFormat::Text)
                        })
                        .collect()
                }
            }
            _ => {
                // Query didn't return SELECT result, return stub
                vec![FieldInfo::new(
                    "column".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Text,
                )]
            }
        }
    }

    #[allow(dead_code)]
    fn parse_copy_command(query: &str) -> Option<(String, Vec<String>)> {
        let query_upper = query.to_uppercase();
        if !query_upper.contains("COPY")
            || !query_upper.contains("FROM")
            || !query_upper.contains("STDIN")
        {
            return None;
        }

        let re = regex::Regex::new(r"(?i)COPY\s+(?:public\.)?(\w+)\s*\(([^)]+)\)\s+FROM\s+stdin")
            .ok()?;
        if let Some(caps) = re.captures(query) {
            let table_name = caps.get(1)?.as_str().to_string();
            let columns_str = caps.get(2)?.as_str();
            let columns: Vec<String> = columns_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            return Some((table_name, columns));
        }

        let re2 = regex::Regex::new(r"(?i)COPY\s+(?:public\.)?(\w+)\s+FROM\s+stdin").ok()?;
        if let Some(caps) = re2.captures(query) {
            let table_name = caps.get(1)?.as_str().to_string();
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

            let col_count = if columns.is_empty() {
                let mut session = self.session.lock().await;
                let db_id = session.current_database_id();
                session.begin().await.ok();
                let count = if let Some(txn) = session.get_mut_txn() {
                    if let Ok(Some(schema)) = self
                        .executor
                        .store()
                        .get_schema(txn, db_id, &table_name)
                        .await
                    {
                        schema.columns.len()
                    } else {
                        1
                    }
                } else {
                    1
                };
                session.rollback().await.ok();
                count
            } else {
                columns.len()
            };

            let mut ctx = self.copy_context.lock().await;
            *ctx = Some(CopyContext {
                table_name,
                columns,
                data_buffer: Vec::new(),
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
                    "XX000".to_string(),
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
        let mut ctx_guard = self.copy_context.lock().await;
        if let Some(ref mut ctx) = *ctx_guard {
            ctx.data_buffer.push(copy_data.data.to_vec());
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

        let row_count = if let Some(ctx) = ctx_opt {
            let mut session = self.session.lock().await;
            session.begin().await.map_err(|e| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    e.to_string(),
                )))
            })?;

            let db_id = session.current_database_id();
            let schema = {
                let txn = session.get_mut_txn().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        "No transaction".to_string(),
                    )))
                })?;
                self.executor
                    .store()
                    .get_schema(txn, db_id, &ctx.table_name)
                    .await
                    .map_err(|e| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "XX000".to_string(),
                            e.to_string(),
                        )))
                    })?
                    .ok_or_else(|| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "42P01".to_string(),
                            format!("relation \"{}\" does not exist", ctx.table_name),
                        )))
                    })?
            };
            session.rollback().await.ok();

            let columns: Vec<String> = if ctx.columns.is_empty() {
                schema.columns.iter().map(|c| c.name.clone()).collect()
            } else {
                ctx.columns.clone()
            };

            let mut all_data = Vec::new();
            for chunk in &ctx.data_buffer {
                all_data.extend_from_slice(chunk);
            }

            let data_str = String::from_utf8_lossy(&all_data);
            let lines: Vec<&str> = data_str.lines().filter(|l| !l.is_empty()).collect();

            let mut count = 0usize;

            for line in lines {
                let values: Vec<&str> = line.split('\t').collect();

                if values.len() != columns.len() {
                    continue;
                }

                let mut col_values: Vec<(String, Value)> = Vec::new();
                for (col_name, val) in columns.iter().zip(values.iter()) {
                    let value = if *val == "\\N" {
                        Value::Null
                    } else {
                        let col_schema = schema.columns.iter().find(|c| c.name == *col_name);
                        if let Some(cs) = col_schema {
                            self.executor.parse_value_for_copy(val, &cs.data_type)
                        } else {
                            Value::Text(val.to_string())
                        }
                    };
                    col_values.push((col_name.clone(), value));
                }

                if let Err(e) = self
                    .executor
                    .execute_copy_insert(&mut session, &ctx.table_name, col_values)
                    .await
                {
                    error!("COPY insert error: {}", e);
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "XX000".to_string(),
                        e.to_string(),
                    ))));
                }
                count += 1;
            }

            count
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
        let mut ctx_guard = self.copy_context.lock().await;
        *ctx_guard = None;

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
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn do_query<'a, 'b: 'a, C>(
        &'b self,
        _client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let query = &portal.statement.statement;
        debug!("Extended query: {}", query);

        let final_query = substitute_parameters(query, portal);
        debug!("Final query after substitution: {}", final_query);

        let mut session = self.session.lock().await;

        set_connection_id(session.connection_id());

        match self.executor.execute(&mut session, &final_query).await {
            Ok(results) => result_to_response(results.last()),
            Err(e) => {
                error!("Extended query execution error: {}", e);
                Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
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

        let fields = self.infer_result_fields_from_query(&stmt.statement).await;
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
        let final_query = substitute_parameters(&portal.statement.statement, portal);
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
        } => {
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
                    encode_value(&mut encoder, value, col_type)?;
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
) -> PgWireResult<()> {
    match value {
        Value::Null => encoder.encode_field(&None::<String>),
        Value::Boolean(b) => encoder.encode_field(b),
        Value::Int32(i) => encoder.encode_field(i),
        Value::Int64(i) => {
            // Check if this Int64 should be interpreted as a timestamp based on column type
            // This handles the case where timestamps were incorrectly stored as Int64
            if matches!(col_type, Some(DataType::Timestamp) | Some(DataType::TimestampTz)) {
                // Treat as timestamp - reuse the timestamp encoding logic
                use chrono::{DateTime, Offset, Utc};
                const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;
                const MAX_REASONABLE_UNIX_MS: i64 = 10_000_000_000_000;
                
                let (seconds, micros) = if i.abs() > MAX_REASONABLE_UNIX_MS {
                    let pg_micros = i;
                    let unix_secs = (pg_micros / 1_000_000) + PG_EPOCH_UNIX_SECS;
                    let micros = (pg_micros % 1_000_000).unsigned_abs() as u32;
                    (unix_secs, micros)
                } else {
                    let secs = i / 1000;
                    let millis = (i % 1000).unsigned_abs() as u32;
                    (secs, millis * 1000)
                };
                
                let nanos = micros * 1000;
                if let Some(dt) = DateTime::<Utc>::from_timestamp(seconds, nanos) {
                    let is_timestamptz = matches!(col_type, Some(DataType::TimestampTz));
                    if is_timestamptz {
                        let local = dt.with_timezone(&chrono_tz::America::Los_Angeles);
                        let base = if micros == 0 {
                            local.format("%Y-%m-%d %H:%M:%S").to_string()
                        } else {
                            local.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
                        };
                        let offset_secs = local.offset().fix().local_minus_utc();
                        let sign = if offset_secs >= 0 { '+' } else { '-' };
                        let abs = offset_secs.unsigned_abs();
                        let hours = abs / 3600;
                        let minutes = (abs % 3600) / 60;
                        let tz = if minutes == 0 {
                            format!("{sign}{:02}", hours)
                        } else {
                            format!("{sign}{:02}:{:02}", hours, minutes)
                        };
                        encoder.encode_field(&format!("{base}{tz}"))
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
            use chrono::{DateTime, Offset, Utc};
            
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
                let unix_secs = (pg_micros / 1_000_000) + PG_EPOCH_UNIX_SECS;
                let micros = (pg_micros % 1_000_000).unsigned_abs() as u32;
                (unix_secs, micros)
            } else {
                // Unix epoch milliseconds (our standard format)
                let secs = ts / 1000;
                let millis = (ts % 1000).unsigned_abs() as u32;
                (secs, millis * 1000)
            };
            
            let nanos = micros * 1000;
            if let Some(dt) = DateTime::<Utc>::from_timestamp(seconds, nanos) {
                let is_timestamptz = matches!(col_type, Some(DataType::TimestampTz));

                if is_timestamptz {
                    let local = dt.with_timezone(&chrono_tz::America::Los_Angeles);
                    let base = if micros == 0 {
                        local.format("%Y-%m-%d %H:%M:%S").to_string()
                    } else {
                        local.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
                    };
                    let offset_secs = local.offset().fix().local_minus_utc();
                    let sign = if offset_secs >= 0 { '+' } else { '-' };
                    let abs = offset_secs.unsigned_abs();
                    let hours = abs / 3600;
                    let minutes = (abs % 3600) / 60;
                    let tz = if minutes == 0 {
                        format!("{sign}{:02}", hours)
                    } else {
                        format!("{sign}{:02}:{:02}", hours, minutes)
                    };
                    encoder.encode_field(&format!("{base}{tz}"))
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
                    || s.chars().any(|c| {
                        c.is_whitespace() || matches!(c, '{' | '}' | ',' | '"' | '\\')
                    })
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
            use serde::Serialize;

            struct PgJsonbFormatter;

            impl serde_json::ser::Formatter for PgJsonbFormatter {
                fn begin_array_value<W: ?Sized + std::io::Write>(
                    &mut self,
                    writer: &mut W,
                    first: bool,
                ) -> std::io::Result<()> {
                    if first {
                        Ok(())
                    } else {
                        writer.write_all(b", ")
                    }
                }

                fn begin_object_key<W: ?Sized + std::io::Write>(
                    &mut self,
                    writer: &mut W,
                    first: bool,
                ) -> std::io::Result<()> {
                    if first {
                        Ok(())
                    } else {
                        writer.write_all(b", ")
                    }
                }

                fn begin_object_value<W: ?Sized + std::io::Write>(
                    &mut self,
                    writer: &mut W,
                ) -> std::io::Result<()> {
                    writer.write_all(b": ")
                }
            }

            match serde_json::from_str::<serde_json::Value>(s) {
                Ok(val) => {
                    let mut buf = Vec::new();
                    let mut ser = serde_json::Serializer::with_formatter(&mut buf, PgJsonbFormatter);
                    if val.serialize(&mut ser).is_ok() {
                        if let Ok(formatted) = String::from_utf8(buf) {
                            return encoder.encode_field(&formatted);
                        }
                    }
                    encoder.encode_field(s)
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
    use pgwire::messages::response::CommandComplete;

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
                "users".to_string(),
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
}
