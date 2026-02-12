use crate::sql::expr::eval_expr;
use crate::sql::types::{TypeContext, TypeInferrer};
use crate::sql::{ExecuteResult, Session};
use crate::storage::TikvStore;
use crate::types::{ColumnDef, DataType, TableSchema, Value};
use futures::{Sink, SinkExt};
use pgwire::api::results::{FieldFormat, FieldInfo, Response};
use pgwire::api::Type;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::response::NoticeResponse;
use pgwire::messages::PgWireBackendMessage;
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, Ident, ObjectName, Query, Select, SelectItem, SetExpr,
    Statement, TableFactor, TableWithJoins, Values,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::future::Future;
use std::sync::atomic::AtomicI32;
use std::sync::Arc;
use tikv_client::Transaction;

mod copy;
mod dynamic;
mod encode;
mod errors;
mod params;
mod portal;
mod query_parser;
mod server_params;
mod tenant;

use encode::{datatype_to_pgtype, result_to_response};

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
    is_superuser: bool,
) -> Option<crate::types::TableSchema> {
    let (schema_opt, name) = split_object_name_for_catalog(table_name)?;

    async fn infer_view_schema(
        store: &TikvStore,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        full_name: &str,
        is_superuser: bool,
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
            let cols = infer_query_output_columns_with_txn(
                store,
                txn,
                db_id,
                search_path,
                &q,
                &ctes,
                is_superuser,
            )
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
        if let Some(schema) = Box::pin(infer_view_schema(
            store,
            txn,
            db_id,
            search_path,
            &full,
            is_superuser,
        ))
        .await
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
        if let Some(schema) = Box::pin(infer_view_schema(
            store,
            txn,
            db_id,
            search_path,
            &full,
            is_superuser,
        ))
        .await
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
    if let Some(schema) = Box::pin(infer_view_schema(
        store,
        txn,
        db_id,
        search_path,
        &full,
        is_superuser,
    ))
    .await
    {
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
    is_superuser: bool,
) -> Option<Vec<FieldInfo>> {
    let schema = resolve_table_schema_for_object_name(
        store,
        txn,
        db_id,
        table_name,
        search_path,
        is_superuser,
    )
    .await?;

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
    let is_superuser = session.is_superuser();

    if let Some(txn) = session.get_mut_txn() {
        infer_returning_fields_with_txn(
            store.as_ref(),
            txn,
            db_id,
            search_path,
            table_name,
            returning,
            is_superuser,
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
            is_superuser,
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

fn infer_system_table_function_schema(func_name: &str) -> Option<TableSchema> {
    fn col(name: &str, data_type: DataType) -> ColumnDef {
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

    let columns = match func_name.to_ascii_uppercase().as_str() {
        "_PGTIKV_SYS_EXPORT_DDL" => vec![
            col("object_type", DataType::Text),
            col("object_name", DataType::Text),
            col("ddl_sql", DataType::Text),
        ],
        "_PGTIKV_SYS_MIGRATIONS" => vec![
            col("name", DataType::Text),
            col("applied_at", DataType::Text),
            col("checksum", DataType::Text),
            col("sql_preview", DataType::Text),
        ],
        "_PGTIKV_SYS_RECORD_MIGRATION" => vec![
            col("name", DataType::Text),
            col("applied_at", DataType::Text),
            col("status", DataType::Text),
        ],
        "_PGTIKV_SYS_OBSERVABILITY" => vec![
            col("window_seconds", DataType::Int64),
            col("statement_count", DataType::Int64),
            col("txn_commit_count", DataType::Int64),
            col("error_count", DataType::Int64),
            col("qps", DataType::Float64),
            col("tps", DataType::Float64),
            col("latency_avg_ms", DataType::Float64),
            col("latency_p99_ms", DataType::Float64),
            col("active_connections", DataType::Int64),
        ],
        "_PGTIKV_SYS_QUERY_SAMPLES" => vec![
            col("query", DataType::Text),
            col("sample_count", DataType::Int64),
            col("error_count", DataType::Int64),
            col("latency_avg_ms", DataType::Float64),
            col("latency_p99_ms", DataType::Float64),
            col("latency_max_ms", DataType::Float64),
            col("last_seen_ms_ago", DataType::Int64),
        ],
        "_PGTIKV_SYS_TRIGGER_QUEUE_STATS" => vec![
            col("keyspace", DataType::Text),
            col("pending", DataType::Int64),
            col("processing", DataType::Int64),
            col("failed", DataType::Int64),
            col("dlq_count", DataType::Int64),
            col("avg_latency_ms", DataType::Float64),
            col("events_per_min", DataType::Int64),
        ],
        _ => return None,
    };

    Some(TableSchema {
        table_id: 0,
        name: func_name.to_string(),
        columns,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        version: 1,
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    })
}

async fn infer_extension_table_function_schema(
    search_path: &[String],
    schema_opt: Option<&str>,
    func_name: &str,
    args: &[FunctionArg],
    is_superuser: bool,
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

    if let Some(schema) = crate::extensions::http::table_function_schema(func_name) {
        return Some(schema);
    }

    if func_name.eq_ignore_ascii_case("fs9") {
        return infer_fs9_table_function_schema(args, is_superuser).await;
    }

    crate::extensions::fs::table_function_schema(func_name)
}

fn try_parse_fs9_mode_from_args(args: &[FunctionArg]) -> Option<crate::extensions::fs::Fs9Mode> {
    if args.is_empty() {
        return None;
    }

    let qc = crate::sql::query_context::QueryContext::from_task_locals();
    let path_expr = match &args[0] {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => e,
        _ => return None,
    };
    let path = match eval_expr(path_expr, None, None, &qc).ok()? {
        Value::Text(s) => s,
        _ => return None,
    };

    let mut format: Option<String> = None;
    let mut delimiter: Option<char> = None;
    let mut header: Option<bool> = None;
    let mut recursive: Option<bool> = None;
    let mut exclude: Option<String> = None;

    for arg in &args[1..] {
        match arg {
            FunctionArg::Named {
                name,
                arg: FunctionArgExpr::Expr(e),
                ..
            } => {
                let param_name = name.value.to_ascii_lowercase();
                let val = eval_expr(e, None, None, &qc).ok()?;
                match param_name.as_str() {
                    "format" => match val {
                        Value::Text(s) => format = Some(s),
                        _ => return None,
                    },
                    "delimiter" => match val {
                        Value::Text(s) => {
                            if s.chars().count() != 1 {
                                return None;
                            }
                            delimiter = s.chars().next();
                        }
                        _ => return None,
                    },
                    "header" => match val {
                        Value::Boolean(b) => header = Some(b),
                        Value::Text(s) if s.eq_ignore_ascii_case("true") => header = Some(true),
                        Value::Text(s) if s.eq_ignore_ascii_case("false") => header = Some(false),
                        _ => return None,
                    },
                    "recursive" => match val {
                        Value::Boolean(b) => recursive = Some(b),
                        Value::Text(s) if s.eq_ignore_ascii_case("true") => recursive = Some(true),
                        Value::Text(s) if s.eq_ignore_ascii_case("false") => {
                            recursive = Some(false)
                        }
                        _ => return None,
                    },
                    "exclude" => match val {
                        Value::Text(s) => exclude = Some(s),
                        _ => return None,
                    },
                    _ => return None,
                }
            }
            _ => return None,
        }
    }

    let mode = if path.ends_with('/') {
        crate::extensions::fs::Fs9Mode::Directory {
            path,
            recursive: recursive.unwrap_or(false),
            exclude,
        }
    } else if crate::extensions::fs::glob::is_glob_pattern(&path) {
        crate::extensions::fs::Fs9Mode::Glob {
            pattern: path,
            format,
            delimiter,
            header,
            exclude,
        }
    } else {
        crate::extensions::fs::Fs9Mode::File {
            path,
            format,
            delimiter,
            header,
        }
    };

    Some(mode)
}

async fn infer_fs9_table_function_schema(
    args: &[FunctionArg],
    is_superuser: bool,
) -> Option<TableSchema> {
    let fallback = crate::extensions::fs::table_function_schema("fs9")?;
    if !is_superuser {
        return Some(fallback);
    }

    let mode = match try_parse_fs9_mode_from_args(args) {
        Some(mode) => mode,
        None => return Some(fallback),
    };

    use crate::extensions::fs::backend::FsBackend;
    let backend = crate::extensions::fs::backend::local_backend();

    match mode {
        crate::extensions::fs::Fs9Mode::Directory { .. } => {
            Some(crate::extensions::fs::decoders::decode_directory(Vec::new()).schema)
        }
        crate::extensions::fs::Fs9Mode::File {
            path,
            format,
            delimiter,
            header,
        } => {
            if backend
                .stat(&path)
                .await
                .ok()
                .is_some_and(|info| info.is_dir)
            {
                return Some(crate::extensions::fs::decoders::decode_directory(Vec::new()).schema);
            }

            let fmt = crate::extensions::fs::decoders::detect_format(&path, format.as_deref());
            match fmt {
                "csv" | "tsv" => {
                    let delim = if fmt == "tsv" && delimiter.is_none() {
                        Some('\t')
                    } else {
                        delimiter
                    };
                    let data = match backend
                        .read_file(&path, crate::extensions::fs::MAX_BYTES_PER_FILE)
                        .await
                    {
                        Ok(data) => data,
                        Err(_) => return Some(fallback),
                    };
                    let decoded =
                        crate::extensions::fs::decoders::decode_csv(&data, &path, delim, header, 0)
                            .ok()?;
                    Some(decoded.schema)
                }
                "jsonl" | "ndjson" => {
                    Some(crate::extensions::fs::decoders::decode_jsonl(&[], &path, 0).schema)
                }
                _ => Some(crate::extensions::fs::decoders::decode_raw_text(&[], &path, 0).schema),
            }
        }
        crate::extensions::fs::Fs9Mode::Glob {
            pattern,
            format,
            delimiter,
            header,
            exclude,
        } => {
            let files = match crate::extensions::fs::glob::expand_glob(
                backend,
                &pattern,
                crate::extensions::fs::MAX_FILES_PER_GLOB,
                exclude.as_deref(),
            )
            .await
            {
                Ok(files) => files,
                Err(_) => return Some(fallback),
            };

            if files.is_empty() {
                return Some(
                    crate::extensions::fs::decoders::decode_raw_text(&[], &pattern, 0).schema,
                );
            }

            let first = files.get(0).cloned().unwrap_or_default();
            if first.is_empty() {
                return Some(fallback);
            }

            let fmt = crate::extensions::fs::decoders::detect_format(&first, format.as_deref());
            match fmt {
                "csv" | "tsv" => {
                    let delim = if fmt == "tsv" && delimiter.is_none() {
                        Some('\t')
                    } else {
                        delimiter
                    };
                    let data = match backend
                        .read_file(&first, crate::extensions::fs::MAX_BYTES_PER_FILE)
                        .await
                    {
                        Ok(data) => data,
                        Err(_) => return Some(fallback),
                    };
                    let decoded = crate::extensions::fs::decoders::decode_csv(
                        &data, &first, delim, header, 0,
                    )
                    .ok()?;
                    Some(decoded.schema)
                }
                "jsonl" | "ndjson" => {
                    Some(crate::extensions::fs::decoders::decode_jsonl(&[], &first, 0).schema)
                }
                _ => Some(crate::extensions::fs::decoders::decode_raw_text(&[], &first, 0).schema),
            }
        }
    }
}

async fn infer_query_output_columns(
    store: &Arc<TikvStore>,
    session: &mut Session,
    query: &Query,
) -> Option<Vec<InferredColumn>> {
    let search_path = session.search_path().to_vec();
    let search_path = search_path.as_slice();
    let db_id = session.current_database_id();
    let is_superuser = session.is_superuser();
    let outer_ctes: HashMap<String, TableSchema> = HashMap::new();

    if let Some(txn) = session.get_mut_txn() {
        infer_query_output_columns_with_txn(
            store.as_ref(),
            txn,
            db_id,
            search_path,
            query,
            &outer_ctes,
            is_superuser,
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
            is_superuser,
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
    is_superuser: bool,
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
            is_superuser,
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
    is_superuser: bool,
) -> Option<Vec<InferredColumn>> {
    let ctes = build_cte_schemas_with_txn(
        store,
        txn,
        db_id,
        search_path,
        query,
        outer_ctes,
        is_superuser,
    )
    .await
    .unwrap_or_else(|| outer_ctes.clone());

    infer_setexpr_output_columns_with_txn(
        store,
        txn,
        db_id,
        search_path,
        &query.body,
        &ctes,
        is_superuser,
    )
    .await
}

async fn infer_setexpr_output_columns_with_txn(
    store: &TikvStore,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    body: &SetExpr,
    ctes: &HashMap<String, TableSchema>,
    is_superuser: bool,
) -> Option<Vec<InferredColumn>> {
    match body {
        SetExpr::Select(select) => {
            infer_select_output_columns_with_txn(
                store,
                txn,
                db_id,
                search_path,
                select,
                ctes,
                is_superuser,
            )
            .await
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
                is_superuser,
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
                // INTENTIONAL: wire protocol encoding — Text OID is universally safe
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
    is_superuser: bool,
) -> Option<()> {
    collect_sources_from_table_factor(
        store,
        txn,
        db_id,
        search_path,
        &twj.relation,
        ctes,
        out,
        is_superuser,
    )
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
            is_superuser,
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
    is_superuser: bool,
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
            from_alias: None,
        })
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
                } else if let Some(sys_schema) = infer_system_table_function_schema(&obj_name_norm)
                {
                    sys_schema
                } else {
                    infer_extension_table_function_schema(
                        search_path,
                        schema_opt.as_deref(),
                        &obj_name_norm,
                        args,
                        is_superuser,
                    )
                    .await?
                }
            } else if schema_opt.is_none() {
                match ctes.get(&obj_name_norm) {
                    Some(cte_schema) => cte_schema.clone(),
                    None => {
                        resolve_table_schema_for_object_name(
                            store,
                            txn,
                            db_id,
                            name,
                            search_path,
                            is_superuser,
                        )
                        .await?
                    }
                }
            } else {
                resolve_table_schema_for_object_name(
                    store,
                    txn,
                    db_id,
                    name,
                    search_path,
                    is_superuser,
                )
                .await?
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
                is_superuser,
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
    is_superuser: bool,
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
            is_superuser,
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
                    // INTENTIONAL: wire protocol encoding — Text OID is universally safe
                    data_type: inferrer.infer(expr).unwrap_or(DataType::Text),
                });
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                out_cols.push(InferredColumn {
                    name: alias.value.clone(),
                    // INTENTIONAL: wire protocol encoding — Text OID is universally safe
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

#[cfg(test)]
mod tests;
