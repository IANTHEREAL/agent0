//! Sequence management -- async detection predicates, naming/ownership utils,
//! DDL handlers, sequence function replacement, and index helpers.

mod ddl;
mod eval;
pub(crate) mod index_helpers;
mod replace;
pub(crate) mod session;

// Re-exports to preserve the original `pub(crate)` surface.
pub(crate) use ddl::{execute_create_sequence, execute_drop_sequence};
pub(crate) use eval::eval_expr_with_sequences;
#[allow(unused_imports)]
pub(crate) use replace::replace_sequence_functions;
pub(crate) use session::SequenceSession;

use crate::model::{
    DataType, Row, SequenceBacking, SequenceDef, SequenceState, TableSchema, Value,
};
use crate::sql::error::SqlError;
use crate::sql::names;
use crate::sql::names::{function_name_upper, normalize_ident};
use crate::storage::TikvStore;
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, GeneratedAs, ObjectName, Value as SqlValue,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::sync::Arc;
use tikv_client::Transaction;

use super::expr::bridge::{eval_ast_expr_with_row, eval_const_ast_expr};

use super::ExecuteResult;

/// Evaluate an expression in sequence context (optional row/schema).
fn eval_seq_expr(expr: &Expr, row: Option<&Row>, schema: Option<&TableSchema>) -> Result<Value> {
    match (row, schema) {
        (Some(r), Some(s)) => {
            let alias = s.name.rsplit('.').next().unwrap_or(&s.name);
            eval_ast_expr_with_row(expr, r, s, alias)
        }
        _ => eval_const_ast_expr(expr),
    }
}

pub(crate) fn expr_uses_sequence_functions(expr: &Expr) -> bool {
    use core::ops::ControlFlow;
    use sqlparser::ast::visit_expressions;

    let mut found = false;
    let _ = visit_expressions(expr, |e| {
        if found {
            return ControlFlow::Break(());
        }

        if let Expr::Function(func) = e {
            let name = function_name_upper(func);
            if matches!(name.as_str(), "NEXTVAL" | "CURRVAL" | "SETVAL" | "LASTVAL")
                || (name == "PG_GET_SERIAL_SEQUENCE"
                    && is_pg_get_serial_sequence_function_name(&func.name))
            {
                found = true;
                return ControlFlow::Break(());
            }
        }

        ControlFlow::<()>::Continue(())
    });
    found
}

pub(crate) fn expr_uses_current_schema(expr: &Expr) -> bool {
    use core::ops::ControlFlow;
    use sqlparser::ast::visit_expressions;

    let mut found = false;
    let _ = visit_expressions(expr, |e| {
        if found {
            return ControlFlow::Break(());
        }

        if let Expr::Function(func) = e {
            let name = function_name_upper(func);
            if name == "CURRENT_SCHEMA" {
                found = true;
                return ControlFlow::Break(());
            }
        }

        ControlFlow::<()>::Continue(())
    });
    found
}

/// Check if expression needs async evaluation (sequence functions, current_schema, or potential user functions)
pub(crate) fn expr_needs_async_eval(expr: &Expr) -> bool {
    expr_uses_sequence_functions(expr)
        || expr_uses_pg_get_serial_sequence(expr)
        || expr_uses_current_schema(expr)
        || expr_may_have_user_function(expr)
}

fn expr_uses_pg_get_serial_sequence(expr: &Expr) -> bool {
    use core::ops::ControlFlow;
    use sqlparser::ast::visit_expressions;

    let mut found = false;
    let _ = visit_expressions(expr, |e| {
        if found {
            return ControlFlow::Break(());
        }

        if let Expr::Function(func) = e {
            if is_pg_get_serial_sequence_function_name(&func.name) {
                found = true;
                return ControlFlow::Break(());
            }
        }

        ControlFlow::<()>::Continue(())
    });
    found
}

fn is_pg_get_serial_sequence_function_name(name: &ObjectName) -> bool {
    match name.0.as_slice() {
        [func] => func.value.eq_ignore_ascii_case("pg_get_serial_sequence"),
        [schema, func] => {
            schema.value.eq_ignore_ascii_case("pg_catalog")
                && func.value.eq_ignore_ascii_case("pg_get_serial_sequence")
        }
        _ => false,
    }
}

/// Check if expression contains any function call that might be a user-defined function.
/// This is a superset of `expr_uses_sequence_functions` - it matches ANY function call
/// that is not a known built-in pure function (like COALESCE, UPPER, etc.).
fn expr_may_have_user_function(expr: &Expr) -> bool {
    use core::ops::ControlFlow;
    use sqlparser::ast::visit_expressions;

    let mut found = false;
    let _ = visit_expressions(expr, |e| {
        if found {
            return ControlFlow::Break(());
        }

        if let Expr::Function(func) = e {
            let name = function_name_upper(func);
            // Skip known built-in functions that don't need async resolution.
            // Any function NOT in this list will trigger the user-function lookup path.
            if !is_known_builtin_function(&name) {
                found = true;
                return ControlFlow::Break(());
            }
        }

        ControlFlow::<()>::Continue(())
    });
    found
}

fn is_known_builtin_function(name: &str) -> bool {
    matches!(
        name,
        // Aggregate functions
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "ARRAY_AGG" | "STRING_AGG" | "BOOL_AND" | "BOOL_OR"
        // Window functions
        | "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "LEAD" | "LAG" | "FIRST_VALUE" | "LAST_VALUE" | "NTH_VALUE" | "NTILE"
        // Math functions
        | "ABS" | "CEIL" | "CEILING" | "FLOOR" | "ROUND" | "TRUNC" | "TRUNCATE" | "SQRT" | "CBRT"
        | "POWER" | "POW" | "EXP" | "LN" | "LOG" | "LOG10" | "SIGN" | "MOD" | "PI" | "RANDOM"
        | "DEGREES" | "RADIANS" | "SIN" | "COS" | "TAN" | "ASIN" | "ACOS" | "ATAN" | "ATAN2"
        | "GREATEST" | "LEAST" | "NULLIF" | "COALESCE"
        // String functions
        | "UPPER" | "LOWER" | "LENGTH" | "CHAR_LENGTH" | "CHARACTER_LENGTH" | "BIT_LENGTH" | "OCTET_LENGTH"
        | "CONCAT" | "CONCAT_WS" | "LEFT" | "RIGHT" | "SUBSTRING" | "SUBSTR"
        | "TRIM" | "LTRIM" | "RTRIM" | "BTRIM" | "LPAD" | "RPAD"
        | "REPLACE" | "REVERSE" | "REPEAT" | "SPLIT_PART" | "INITCAP" | "POSITION"
        | "STRPOS" | "OVERLAY" | "TRANSLATE" | "ASCII" | "CHR" | "ENCODE" | "DECODE"
        | "MD5" | "SHA256" | "DIGEST" | "QUOTE_LITERAL" | "QUOTE_IDENT" | "FORMAT"
        | "REGEXP_REPLACE" | "REGEXP_MATCHES" | "REGEXP_MATCH" | "REGEXP_SPLIT_TO_ARRAY"
        | "TO_HEX" | "STARTS_WITH" | "ENDS_WITH"
        // Date/time functions
        | "NOW" | "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME" | "LOCALTIME" | "LOCALTIMESTAMP"
        | "DATE_TRUNC" | "EXTRACT" | "DATE_PART" | "TO_CHAR" | "TO_DATE" | "TO_TIMESTAMP" | "TO_NUMBER"
        | "AGE" | "MAKE_DATE" | "MAKE_TIME" | "MAKE_TIMESTAMP" | "MAKE_TIMESTAMPTZ" | "MAKE_INTERVAL"
        | "CLOCK_TIMESTAMP" | "STATEMENT_TIMESTAMP" | "TRANSACTION_TIMESTAMP" | "TIMEOFDAY"
        | "ISFINITE" | "JUSTIFY_DAYS" | "JUSTIFY_HOURS" | "JUSTIFY_INTERVAL"
        // Type conversion
        | "CAST" | "TRY_CAST"
        // JSON functions
        | "JSON_BUILD_OBJECT" | "JSON_BUILD_ARRAY" | "JSONB_BUILD_OBJECT" | "JSONB_BUILD_ARRAY"
        | "JSON_OBJECT" | "JSON_ARRAY" | "JSON_AGG" | "JSONB_AGG" | "JSON_OBJECT_AGG" | "JSONB_OBJECT_AGG"
        | "JSON_ARRAY_LENGTH" | "JSONB_ARRAY_LENGTH" | "JSON_ARRAY_ELEMENTS" | "JSONB_ARRAY_ELEMENTS"
        | "JSON_ARRAY_ELEMENTS_TEXT" | "JSONB_ARRAY_ELEMENTS_TEXT"
        | "JSON_EACH" | "JSONB_EACH" | "JSON_EACH_TEXT" | "JSONB_EACH_TEXT"
        | "JSON_EXTRACT_PATH" | "JSONB_EXTRACT_PATH" | "JSON_EXTRACT_PATH_TEXT" | "JSONB_EXTRACT_PATH_TEXT"
        | "JSON_TYPEOF" | "JSONB_TYPEOF" | "JSON_STRIP_NULLS" | "JSONB_STRIP_NULLS"
        | "JSON_POPULATE_RECORD" | "JSONB_POPULATE_RECORD" | "JSON_POPULATE_RECORDSET" | "JSONB_POPULATE_RECORDSET"
        | "JSON_TO_RECORD" | "JSONB_TO_RECORD" | "JSON_TO_RECORDSET" | "JSONB_TO_RECORDSET"
        | "JSONB_SET" | "JSONB_INSERT" | "JSONB_PRETTY" | "ROW_TO_JSON" | "TO_JSON" | "TO_JSONB"
        // Array functions
        | "ARRAY_LENGTH" | "ARRAY_LOWER" | "ARRAY_UPPER" | "ARRAY_NDIMS" | "ARRAY_DIMS"
        | "ARRAY_POSITION" | "ARRAY_POSITIONS" | "ARRAY_PREPEND" | "ARRAY_APPEND" | "ARRAY_CAT"
        | "ARRAY_REMOVE" | "ARRAY_REPLACE" | "ARRAY_TO_STRING" | "STRING_TO_ARRAY" | "UNNEST"
        | "CARDINALITY" | "ARRAY_FILL"
        // UUID
        | "GEN_RANDOM_UUID" | "UUID_GENERATE_V4" | "UUIDV7"
        // Misc
        | "PG_TYPEOF" | "VERSION" | "CURRENT_USER" | "CURRENT_ROLE" | "SESSION_USER"
        | "PG_BACKEND_PID" | "PG_POSTMASTER_START_TIME" | "PG_CLIENT_ENCODING" | "PG_CATALOG" | "OBJ_DESCRIPTION" | "COL_DESCRIPTION"
        | "PG_GET_SERIAL_SEQUENCE"
        | "GENERATE_SERIES" | "GENERATE_SUBSCRIPTS"
        // These are handled specially but are built-in
        | "CURRENT_SCHEMA" | "NEXTVAL" | "CURRVAL" | "SETVAL" | "LASTVAL"
        // Vector functions (if supported)
        | "VECTOR_DIMS" | "VECTOR_NORM"
        // Vector distance functions (rewritten from <->, <#>, <=> operators by parser)
        | "L2_DISTANCE" | "INNER_PRODUCT" | "COSINE_DISTANCE"
    )
}

pub(crate) fn normalize_sequence_name(
    name: &ObjectName,
    search_path: &[String],
) -> Result<(String, String, String)> {
    let parts: Vec<String> = name.0.iter().map(normalize_ident).collect();
    if parts.is_empty() {
        return Err(anyhow!("Invalid sequence name"));
    }

    let (schema, seq_name) = match parts.as_slice() {
        [seq_name] => (
            names::default_schema(search_path).to_string(),
            seq_name.clone(),
        ),
        [schema, seq_name] => (schema.clone(), seq_name.clone()),
        _ => return Err(anyhow!("Invalid sequence name")),
    };

    Ok((
        schema.clone(),
        seq_name.clone(),
        format!("{}.{}", schema, seq_name),
    ))
}

const MAX_IDENTIFIER_BYTES: usize = 63;

fn truncate_utf8_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn build_implicit_sequence_name_with_label(
    table_name: &str,
    column_name: &str,
    label: &str,
) -> String {
    // Match PostgreSQL makeObjectName(): allocate bytes across name1/name2 by
    // trimming the longer component first, then clip on UTF-8 boundaries.
    let mut table_bytes = table_name.len();
    let mut column_bytes = column_name.len();
    let overhead = 2 + label.len(); // "_" between name1/name2 and "_label"
    let avail_bytes = MAX_IDENTIFIER_BYTES.saturating_sub(overhead);

    while table_bytes + column_bytes > avail_bytes {
        if table_bytes > column_bytes {
            table_bytes -= 1;
        } else {
            column_bytes -= 1;
        }
    }

    let table_part = truncate_utf8_to_bytes(table_name, table_bytes);
    let column_part = truncate_utf8_to_bytes(column_name, column_bytes);
    format!("{}_{}_{}", table_part, column_part, label)
}

pub(crate) fn implicit_sequence_name(table_name: &str, column_name: &str) -> String {
    build_implicit_sequence_name_with_label(table_name, column_name, "seq")
}

pub(crate) fn implicit_sequence_name_with_suffix(
    table_name: &str,
    column_name: &str,
    suffix: u32,
) -> String {
    build_implicit_sequence_name_with_label(table_name, column_name, &format!("seq{}", suffix))
}

pub(crate) fn build_implicit_sequence_def(
    table_full_name: &str,
    column_name: &str,
    data_type: &DataType,
) -> SequenceDef {
    let (schema, table_name) = match table_full_name.split_once('.') {
        Some((schema, table)) if !schema.is_empty() && !table.is_empty() => {
            (schema.to_string(), table.to_string())
        }
        _ => ("public".to_string(), table_full_name.to_string()),
    };

    let max_value = match data_type {
        DataType::Int64 => i64::MAX,
        _ => i32::MAX as i64,
    };

    SequenceDef {
        oid: 0,
        schema,
        name: implicit_sequence_name(&table_name, column_name),
        start_value: 1,
        increment: 1,
        min_value: 1,
        max_value,
        cache_size: 1,
        is_cycled: false,
        owned_by: Some((table_full_name.to_string(), column_name.to_string())),
        owner: "postgres".to_string(),
        backing: SequenceBacking::Standalone(SequenceState {
            last_value: 1,
            is_called: false,
        }),
    }
}

const IDENTITY_ALWAYS_MARKER: &str = "NULL /* db9_identity_always */";
const IDENTITY_BY_DEFAULT_MARKER: &str = "NULL /* db9_identity_by_default */";
const SERIAL_DEFAULT_DROPPED_MARKER: &str = "NULL /* db9_serial_default_dropped */";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SerialDefaultBehavior<'a> {
    ImplicitSequence,
    ExplicitExpr(&'a str),
    ExplicitNull,
}

pub(crate) fn identity_default_marker(generated_as: &GeneratedAs) -> Option<&'static str> {
    match generated_as {
        GeneratedAs::Always => Some(IDENTITY_ALWAYS_MARKER),
        GeneratedAs::ByDefault => Some(IDENTITY_BY_DEFAULT_MARKER),
        _ => None,
    }
}

pub(crate) fn is_identity_default_marker(default_expr: Option<&str>) -> bool {
    matches!(
        default_expr.map(str::trim),
        Some(IDENTITY_ALWAYS_MARKER | IDENTITY_BY_DEFAULT_MARKER)
    )
}

#[cfg(test)]
pub(crate) fn serial_default_dropped_marker() -> &'static str {
    SERIAL_DEFAULT_DROPPED_MARKER
}

pub(crate) fn is_serial_default_dropped_marker(default_expr: Option<&str>) -> bool {
    matches!(
        default_expr.map(str::trim),
        Some(SERIAL_DEFAULT_DROPPED_MARKER)
    )
}

fn parse_default_expr(expr: &str) -> Option<Expr> {
    let sql = format!("SELECT {}", expr);
    let dialect = PostgreSqlDialect {};
    let ast = match Parser::parse_sql(&dialect, &sql) {
        Ok(ast) => ast,
        Err(_) => return None,
    };

    let Some(sqlparser::ast::Statement::Query(q)) = ast.into_iter().next() else {
        return None;
    };
    let sqlparser::ast::SetExpr::Select(s) = *q.body else {
        return None;
    };
    let Some(sqlparser::ast::SelectItem::UnnamedExpr(parsed_expr)) =
        s.projection.into_iter().next()
    else {
        return None;
    };

    Some(parsed_expr)
}

fn is_explicit_null_default_expr(expr: &str) -> bool {
    fn expr_is_explicit_null(expr: &Expr) -> bool {
        match expr {
            Expr::Nested(inner) => expr_is_explicit_null(inner),
            Expr::Value(SqlValue::Null) => true,
            Expr::Cast { expr, .. } | Expr::TryCast { expr, .. } => expr_is_explicit_null(expr),
            _ => false,
        }
    }

    if expr.trim().eq_ignore_ascii_case("NULL") {
        return true;
    }

    let Some(parsed_expr) = parse_default_expr(expr) else {
        return false;
    };

    expr_is_explicit_null(&parsed_expr)
}

pub(crate) fn classify_serial_default(default_expr: Option<&str>) -> SerialDefaultBehavior<'_> {
    match default_expr {
        Some(expr) if is_identity_default_marker(Some(expr)) => {
            SerialDefaultBehavior::ImplicitSequence
        }
        Some(expr) if is_serial_default_dropped_marker(Some(expr)) => {
            SerialDefaultBehavior::ExplicitNull
        }
        Some(expr) if is_explicit_null_default_expr(expr) => SerialDefaultBehavior::ExplicitNull,
        Some(expr) => SerialDefaultBehavior::ExplicitExpr(expr),
        None => SerialDefaultBehavior::ImplicitSequence,
    }
}

pub(crate) fn find_owned_sequence_full_name(
    sequences: &[SequenceDef],
    table_full_name: &str,
    column_name: &str,
) -> Result<Option<String>> {
    let mut matches = sequences.iter().filter(|seq| {
        seq.owned_by
            .as_ref()
            .is_some_and(|(owned_table, owned_col)| {
                owned_table == table_full_name && owned_col == column_name
            })
    });

    let Some(first) = matches.next() else {
        return Ok(None);
    };

    if matches.next().is_some() {
        return Err(anyhow!(
            "Multiple sequences are owned by {}.{}",
            table_full_name,
            column_name
        ));
    }

    Ok(Some(first.full_name()))
}

pub(crate) fn resolve_serial_sequence_owned_by(
    sequences: &[SequenceDef],
    table_full_name: &str,
    column_name: &str,
) -> Result<Option<String>> {
    find_owned_sequence_full_name(sequences, table_full_name, column_name)
}

fn push_identifier_part(parts: &mut Vec<String>, raw: &str, quoted: bool) -> Result<()> {
    let trimmed = if quoted { raw } else { raw.trim() };
    if trimmed.is_empty() {
        return Err(anyhow!("invalid name syntax"));
    }
    if quoted {
        parts.push(trimmed.to_string());
    } else {
        parts.push(trimmed.to_lowercase());
    }
    Ok(())
}

fn parse_identifier_parts(token: &str) -> Result<Vec<String>> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("invalid name syntax"));
    }

    let mut parts: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut in_quotes = false;
    let mut part_quoted = false;

    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        buf.push('"');
                    } else {
                        in_quotes = false;
                    }
                } else {
                    in_quotes = true;
                    part_quoted = true;
                }
            }
            '.' if !in_quotes => {
                push_identifier_part(&mut parts, &buf, part_quoted)?;
                buf.clear();
                part_quoted = false;
            }
            _ => buf.push(ch),
        }
    }

    if in_quotes {
        return Err(anyhow!("invalid name syntax"));
    }
    push_identifier_part(&mut parts, &buf, part_quoted)?;
    Ok(parts)
}

fn parse_table_arg_identifier(table_arg: &str) -> Result<(Option<String>, String)> {
    let parts = parse_identifier_parts(table_arg)?;
    match parts.as_slice() {
        [table] => Ok((None, table.clone())),
        [schema, table] => Ok((Some(schema.clone()), table.clone())),
        _ => Err(anyhow!(
            "cross-database references are not implemented: {}",
            table_arg.trim()
        )),
    }
}

fn quote_pg_identifier_if_needed(ident: &str) -> String {
    crate::sql::quoting::quote_ident(ident)
}

pub(crate) fn format_serial_sequence_name(schema: &str, sequence_name: &str) -> String {
    format!(
        "{}.{}",
        quote_pg_identifier_if_needed(schema),
        quote_pg_identifier_if_needed(sequence_name)
    )
}

fn search_path_schemas(search_path: &[String]) -> Vec<&str> {
    let mut schemas: Vec<&str> = search_path
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !s.eq_ignore_ascii_case("$user"))
        .collect();
    if schemas.is_empty() {
        schemas.push("public");
    }
    schemas
}

pub(crate) async fn resolve_serial_sequence(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    table_arg: &str,
    column_arg: &str,
) -> Result<Option<String>> {
    let (schema_opt, table_name) = parse_table_arg_identifier(table_arg)?;

    let table_schema = match schema_opt {
        Some(schema_name) => {
            let resolved = names::ResolvedName::new(schema_name, table_name.clone())?;
            store
                .get_schema(txn, db_id, &resolved.full)
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(resolved.full.clone()))?
        }
        None => {
            let mut found = None;
            for schema_name in search_path_schemas(search_path) {
                let resolved =
                    names::ResolvedName::new(schema_name.to_string(), table_name.clone())?;
                if let Some(schema) = store.get_schema(txn, db_id, &resolved.full).await? {
                    found = Some(schema);
                    break;
                }
            }
            found.ok_or_else(|| SqlError::RelationNotFound(table_name.clone()))?
        }
    };

    if !table_schema.columns.iter().any(|c| c.name == column_arg) {
        let rel_name = table_schema
            .name
            .rsplit('.')
            .next()
            .unwrap_or(table_schema.name.as_str());
        return Err(anyhow!(
            "column \"{}\" of relation \"{}\" does not exist",
            column_arg,
            rel_name
        ));
    }

    let sequence_defs = store.list_sequences(txn, db_id).await?;
    resolve_serial_sequence_owned_by(&sequence_defs, &table_schema.name, column_arg)
}

fn eval_i64(expr: &Expr) -> Result<i64> {
    match super::expr::bridge::eval_const_ast_expr(expr)? {
        crate::model::Value::Int32(n) => Ok(n as i64),
        crate::model::Value::Int64(n) => Ok(n),
        crate::model::Value::Float64(n) => Ok(n as i64),
        crate::model::Value::Text(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| anyhow!("Expected integer, got {}", s)),
        other => Err(anyhow!("Expected integer, got {}", other)),
    }
}

fn parse_sequence_name_token(token: &str) -> Result<(Option<String>, String)> {
    fn push_part(parts: &mut Vec<String>, raw: &str, quoted: bool) -> Result<()> {
        let trimmed = if quoted { raw } else { raw.trim() };
        if trimmed.is_empty() {
            return Err(anyhow!("Invalid sequence name"));
        }
        if quoted {
            parts.push(trimmed.to_string());
        } else {
            parts.push(trimmed.to_lowercase());
        }
        Ok(())
    }

    let mut parts: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut in_quotes = false;
    let mut part_quoted = false;

    let mut chars = token.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        buf.push('"');
                    } else {
                        in_quotes = false;
                    }
                } else {
                    in_quotes = true;
                    part_quoted = true;
                }
            }
            '.' if !in_quotes => {
                push_part(&mut parts, &buf, part_quoted)?;
                buf.clear();
                part_quoted = false;
            }
            _ => buf.push(ch),
        }
    }

    if in_quotes {
        return Err(anyhow!("Unterminated quoted identifier in sequence name"));
    }
    push_part(&mut parts, &buf, part_quoted)?;

    if parts.len() >= 2 {
        Ok((
            Some(parts[parts.len() - 2].clone()),
            parts[parts.len() - 1].clone(),
        ))
    } else {
        Ok((None, parts[0].clone()))
    }
}

fn extract_arg_expr(args: &[FunctionArg], idx: usize) -> Result<&Expr> {
    match args.get(idx) {
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))) => Ok(expr),
        Some(_) => Err(SqlError::Unsupported("Unsupported function argument".into()).into()),
        None => Err(anyhow!("Missing function argument")),
    }
}

fn value_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int32(n) => Some(*n as i64),
        Value::Int64(n) => Some(*n),
        Value::Float64(n) => Some(*n as i64),
        Value::Text(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn split_schema_and_name(full: &str) -> (String, String) {
    match names::parse_full_name(full) {
        Ok((schema, name)) => (schema, name),
        Err(_) => ("public".to_string(), full.to_string()),
    }
}

pub(crate) async fn resolve_sequence_full_name_from_value(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    v: crate::model::Value,
) -> Result<String> {
    let crate::model::Value::Text(s) = v else {
        return Err(anyhow!("Sequence name must be text/regclass"));
    };

    let (schema_opt, seq_name) = parse_sequence_name_token(&s)?;
    if let Some(schema) = schema_opt {
        return Ok(names::ResolvedName::new(schema, seq_name)?.full);
    }

    let candidates: Vec<&str> = if search_path.is_empty() {
        vec!["public"]
    } else {
        search_path.iter().map(|s| s.as_str()).collect()
    };
    for schema in candidates {
        let resolved = names::ResolvedName::new(schema.to_string(), seq_name.clone())?;
        if store
            .get_sequence(txn, db_id, &resolved.full)
            .await?
            .is_some()
        {
            return Ok(resolved.full);
        }
    }

    Ok(names::ResolvedName::new(names::default_schema(search_path).to_string(), seq_name)?.full)
}

#[cfg(test)]
mod tests;
