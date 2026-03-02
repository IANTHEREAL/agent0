//! Miscellaneous helper functions for the typed expression evaluator.
//!
//! Contains function call dispatch, timezone evaluation, array indexing,
//! JSON operator mapping, and string conversion utilities.

use crate::model::{Row, Value};
use crate::sql::analyzer::types::*;
use crate::sql::error::SqlError;
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};

use super::eval_typed_expr;

pub(super) use crate::sql::expr::helpers::value_to_text;

/// Evaluate array indexing (1-based, PostgreSQL convention).
pub(super) fn eval_array_index(arr_val: Value, idx_val: Value) -> Result<Value> {
    if arr_val == Value::Null || idx_val == Value::Null {
        return Ok(Value::Null);
    }
    let arr = match arr_val {
        Value::Array(a) => a,
        _ => return Err(anyhow!("subscript requires array operand")),
    };
    let idx = match idx_val {
        Value::Int32(i) => i as i64,
        Value::Int64(i) => i,
        _ => return Err(anyhow!("array index must be integer")),
    };
    // PostgreSQL uses 1-based indexing
    let zero_based = idx - 1;
    if zero_based < 0 || zero_based >= arr.len() as i64 {
        Ok(Value::Null)
    } else {
        Ok(arr[zero_based as usize].clone())
    }
}

/// Dispatch a function call: check context-dependent builtins first, then
/// fall back to the global function registry.
///
/// Context-dependent functions (NOW, CURRENT_DATE, PG_BACKEND_PID, etc.)
/// read timestamps and session info from the explicit `QueryContext`.
pub(super) fn eval_function_call(
    name: &str,
    args: Vec<Value>,
    qctx: &QueryContext,
) -> Result<Value> {
    let func_name_upper = name.to_uppercase();
    let (schema_name, unqualified_name) = split_qualified_function_name(&func_name_upper);

    if schema_name.is_some_and(|schema| schema.eq_ignore_ascii_case("CRON")) {
        if let Some(result) =
            crate::sql::executor::try_execute_cron_scalar_function(unqualified_name, &args)
        {
            return result;
        }
    }

    if let Some(result) = crate::sql::executor::try_execute_bg_sql_function(unqualified_name, &args)
    {
        return result;
    }

    match func_name_upper.as_str() {
        "NOW" | "CURRENT_TIMESTAMP" | "STATEMENT_TIMESTAMP" | "TRANSACTION_TIMESTAMP" => {
            let precision = match args.first() {
                None => 6_u32,
                Some(Value::Int32(p)) => (*p).clamp(0, 6) as u32,
                Some(Value::Int64(p)) => (*p).clamp(0, 6) as u32,
                _ => 6_u32,
            };
            let ts = if func_name_upper == "STATEMENT_TIMESTAMP" {
                qctx.statement_timestamp_ms
            } else {
                qctx.transaction_timestamp_ms
            };
            let ts = crate::model::timestamp::truncate_timestamp_millis(ts, precision);
            return Ok(Value::Timestamp(ts));
        }
        "CURRENT_DATE" => {
            let days =
                crate::model::date::timestamp_millis_to_date_days(qctx.transaction_timestamp_ms)?;
            return Ok(Value::Date(days));
        }
        "PG_BACKEND_PID" => {
            // PostgreSQL exposes pg_backend_pid() as int4.
            // Internal connection identity is i64; cast intentionally truncates.
            return Ok(Value::Int32(qctx.connection_id as i32));
        }
        "CURRENT_DATABASE" => {
            return Ok(Value::Text(qctx.database_name.as_ref().to_string()));
        }
        "CURRENT_SCHEMA" => {
            return Ok(Value::Text(
                crate::session_context::current_search_path_first_schema(),
            ))
        }
        "CURRENT_USER" | "SESSION_USER" | "USER" => {
            return Ok(Value::Text(qctx.current_user.as_ref().to_string()));
        }
        "VERSION" => {
            return Ok(Value::Text(crate::sql::expr::VERSION_STRING.to_string()));
        }
        "CURRENT_SETTING" | "PG_CATALOG.CURRENT_SETTING" => {
            // Arg 0: setting name (text). NULL → NULL (strict function).
            let name = match args.first() {
                Some(Value::Text(s)) => s.to_lowercase(),
                Some(Value::Null) | None => return Ok(Value::Null),
                _ => return Err(anyhow!("current_setting requires text argument")),
            };
            let canonical =
                crate::sql::session::settings::SessionSettings::canonical_setting_name(&name);
            // Arg 1: missing_ok (boolean, default false).
            // Accept Boolean directly, coerce Text→Boolean (PG accepts 'true'/'false'),
            // NULL → NULL (strict), anything else → type error.
            let missing_ok = match args.get(1) {
                None => false,
                Some(Value::Boolean(b)) => *b,
                Some(Value::Null) => return Ok(Value::Null),
                Some(Value::Text(s)) => {
                    // Coerce text to boolean — propagate SqlError::InvalidInputSyntax directly.
                    match crate::sql::types::cast::cast(
                        Value::Text(s.clone()),
                        &crate::model::DataType::Boolean,
                        crate::sql::types::cast::CastContext::Implicit,
                    )? {
                        Value::Boolean(b) => b,
                        _ => false,
                    }
                }
                Some(_) => return Err(anyhow!("argument of current_setting must be type boolean")),
            };
            if let Some(ref snapshot) = qctx.settings_snapshot {
                return match snapshot.get(canonical) {
                    Some(v) => Ok(Value::Text(v.clone())),
                    None if missing_ok => Ok(Value::Null),
                    None => Err(anyhow!("unrecognized configuration parameter \"{}\"", name)),
                };
            }
            // No snapshot (unit tests, DDL contexts) — fall through to error
            return Err(anyhow!("unrecognized configuration parameter \"{}\"", name));
        }
        "SET_CONFIG" | "PG_CATALOG.SET_CONFIG" => return Ok(Value::Text(String::new())),
        "PG_GET_USERBYID" => return Ok(Value::Text("postgres".to_string())),
        "NEXTVAL" | "CURRVAL" | "SETVAL" | "LASTVAL" => {
            return Err(anyhow!(
                "{} is a sequence function and must be evaluated during execution",
                name
            ));
        }
        name if crate::sql::advisory_locks::is_advisory_lock_function(name) => {
            return Err(anyhow!("{} must be evaluated during execution", name));
        }
        "GENERATE_SERIES" => {
            return Err(anyhow!(
                "GENERATE_SERIES is a set-returning function, not supported in this context"
            ));
        }
        _ => {}
    }

    // Standard registry lookup.
    let registry = crate::sql::expr::functions::get_registry();
    match registry.get(unqualified_name) {
        Some(f) => f(args),
        None => Err(SqlError::Unsupported(format!("unknown function: {}", name)).into()),
    }
}

fn split_qualified_function_name(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once('.') {
        Some((schema, func)) => (Some(schema), func),
        None => (None, name),
    }
}

/// Evaluate TIMEZONE(zone, timestamp) with type-aware direction.
///
/// PostgreSQL semantics:
/// - TIMESTAMP AT TIME ZONE zone  → TIMESTAMPTZ: interpret as local, convert to UTC
/// - TIMESTAMPTZ AT TIME ZONE zone → TIMESTAMP: convert from UTC to local
pub(super) fn eval_timezone(
    tz_expr: &TypedExpr,
    ts_expr: &TypedExpr,
    row: &Row,
    qctx: &QueryContext,
) -> Result<Value> {
    let tz_val = eval_typed_expr(tz_expr, row, qctx)?;
    let ts_val = eval_typed_expr(ts_expr, row, qctx)?;

    if matches!(tz_val, Value::Null) || matches!(ts_val, Value::Null) {
        return Ok(Value::Null);
    }

    let tz_str = match &tz_val {
        Value::Text(s) => s.as_str(),
        _ => return Err(anyhow!("AT TIME ZONE requires text timezone argument")),
    };
    let offset_secs = crate::sql::timezone::parse_timezone_offset_seconds(tz_str)?;
    let offset_ms = i64::from(offset_secs) * 1000;

    let ts_millis = match ts_val {
        Value::Timestamp(ms) => ms,
        Value::Date(days) => crate::model::date::date_days_to_timestamp_millis(days)?,
        Value::Text(ref s) => match crate::sql::expr::parse_timestamp_string(s) {
            Ok(Value::Timestamp(ms)) => ms,
            _ => return Err(anyhow!("AT TIME ZONE requires timestamp, got text: {}", s)),
        },
        Value::Int64(ms) => ms,
        _ => return Err(anyhow!("AT TIME ZONE requires timestamp, got {:?}", ts_val)),
    };

    use crate::model::DataType;
    if matches!(ts_expr.data_type, DataType::TimestampTz) {
        // TIMESTAMPTZ → TIMESTAMP: convert from UTC to local
        Ok(Value::Timestamp(ts_millis + offset_ms))
    } else {
        // TIMESTAMP → TIMESTAMPTZ: interpret as local, convert to UTC
        Ok(Value::Timestamp(ts_millis - offset_ms))
    }
}

/// Map analyzer JsonAccessOp → sqlparser JsonOperator.
pub(super) fn to_sqlparser_json_op(op: &JsonAccessOp) -> sqlparser::ast::JsonOperator {
    match op {
        JsonAccessOp::Arrow => sqlparser::ast::JsonOperator::Arrow,
        JsonAccessOp::LongArrow => sqlparser::ast::JsonOperator::LongArrow,
        JsonAccessOp::HashArrow => sqlparser::ast::JsonOperator::HashArrow,
        JsonAccessOp::HashLongArrow => sqlparser::ast::JsonOperator::HashLongArrow,
        JsonAccessOp::HashMinus => sqlparser::ast::JsonOperator::HashMinus,
    }
}
