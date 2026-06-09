//! Miscellaneous helper functions for the typed expression evaluator.
//!
//! Contains function call dispatch, timezone evaluation, array indexing,
//! JSON operator mapping, and string conversion utilities.

use crate::model::{DataType, Row, Value};
use crate::sql::analyzer::types::*;
use crate::sql::error::SqlError;
use crate::sql::executor::{
    check_reserved_guc_reset, check_reserved_guc_write, session_auth_different_user_error_sync,
};
use crate::sql::query_context::{CurrentSettingLookup, QueryContext};
use anyhow::{anyhow, Result};
use chrono::Offset;
use std::sync::{Arc, OnceLock};

use super::eval_typed_expr;
use crate::sql::expr::functions::datetime::format_to_char_value;
use crate::sql::expr::functions::json::{
    format_jsonb_pg, render_json_text_pg_from_value, render_json_timestamptz_millis,
    render_jsonb_text_pg_from_value, value_to_json,
};

/// Process start time as epoch milliseconds, set once from main().
static POSTMASTER_START_TIME_MS: OnceLock<i64> = OnceLock::new();

/// Record the process start time. Must be called once at the top of main().
pub fn init_postmaster_start_time() {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as i64;
    let _ = POSTMASTER_START_TIME_MS.set(ms);
}

/// Return the recorded process start time (epoch millis).
fn postmaster_start_time_ms() -> i64 {
    *POSTMASTER_START_TIME_MS.get_or_init(|| {
        // Fallback for unit tests where main() is not called.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis() as i64
    })
}

pub(super) use crate::sql::expr::helpers::value_to_text;

fn eval_hash_input_arg(
    function_name: &str,
    expr: &TypedExpr,
    row: &Row,
    qctx: &QueryContext,
    allow_text: bool,
    allow_bytes: bool,
) -> Result<Option<Vec<u8>>> {
    let text_like = matches!(
        expr.data_type,
        DataType::Text | DataType::Name | DataType::Varchar(_)
    );
    let bytes = matches!(expr.data_type, DataType::Bytes);
    if !(allow_bytes && bytes || allow_text && text_like) {
        return Err(SqlError::FunctionNotFound(format!(
            "{}({})",
            function_name.to_lowercase(),
            expr.data_type.pg_display_name()
        ))
        .into());
    }

    let value = eval_typed_expr(expr, row, qctx)?;
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let timezone = effective_timezone(qctx);
    Ok(Some(
        crate::sql::expr::functions::encoding::hash_input_bytes_with_type(
            value,
            &expr.data_type,
            timezone.as_ref(),
        ),
    ))
}

pub(super) fn eval_hash_function_call(
    name: &str,
    args: &[TypedExpr],
    row: &Row,
    qctx: &QueryContext,
) -> Option<Result<Value>> {
    match name.to_ascii_uppercase().as_str() {
        "MD5" if args.len() == 1 => Some(
            match eval_hash_input_arg(name, &args[0], row, qctx, true, true) {
                Ok(Some(data)) => {
                    crate::sql::expr::functions::encoding::md5(vec![Value::Bytes(data)])
                }
                Ok(None) => Ok(Value::Null),
                Err(err) => Err(err),
            },
        ),
        "SHA256" if args.len() == 1 => Some(
            match eval_hash_input_arg(name, &args[0], row, qctx, false, true) {
                Ok(Some(data)) => {
                    crate::sql::expr::functions::encoding::sha256(vec![Value::Bytes(data)])
                }
                Ok(None) => Ok(Value::Null),
                Err(err) => Err(err),
            },
        ),
        "DIGEST" if args.len() == 2 => Some(
            match eval_hash_input_arg(name, &args[0], row, qctx, true, true) {
                Ok(data) => match eval_typed_expr(&args[1], row, qctx) {
                    Ok(algorithm) => match data {
                        Some(data) => crate::sql::expr::functions::encoding::digest(vec![
                            Value::Bytes(data),
                            algorithm,
                        ]),
                        None => Ok(Value::Null),
                    },
                    Err(err) => Err(err),
                },
                Err(err) => Err(err),
            },
        ),
        _ => None,
    }
}

pub(super) fn eval_json_function_call(
    name: &str,
    args: &[TypedExpr],
    row: &Row,
    qctx: &QueryContext,
) -> Option<Result<Value>> {
    match name.to_ascii_uppercase().as_str() {
        "TO_JSON" if args.len() == 1 => Some(eval_json_scalar_call(&args[0], row, qctx, false)),
        "TO_JSONB" if args.len() == 1 => Some(eval_json_scalar_call(&args[0], row, qctx, true)),
        "ROW_TO_JSON" if !args.is_empty() => Some(eval_row_to_json_call(args, row, qctx, false)),
        "JSON_BUILD_ARRAY" => Some(eval_json_build_array(args, row, qctx, false)),
        "JSONB_BUILD_ARRAY" => Some(eval_json_build_array(args, row, qctx, true)),
        "JSON_BUILD_OBJECT" => Some(eval_json_build_object(args, row, qctx, false)),
        "JSONB_BUILD_OBJECT" => Some(eval_json_build_object(args, row, qctx, true)),
        _ => None,
    }
}

fn eval_json_scalar_call(
    arg: &TypedExpr,
    row: &Row,
    qctx: &QueryContext,
    jsonb: bool,
) -> Result<Value> {
    if let TypedExprKind::Row(items) = &arg.kind {
        return eval_row_items_to_json(items, row, qctx, jsonb);
    }

    let value = eval_typed_expr(arg, row, qctx)?;
    let rendered = if jsonb {
        render_jsonb_text_for_typed_value(&value, &arg.data_type, qctx)
    } else {
        render_json_text_for_typed_value(&value, &arg.data_type, qctx)
    }?;

    if jsonb {
        Ok(Value::Jsonb(crate::sql::jsonb::format_jsonb_pg_str(
            &rendered,
        )))
    } else {
        Ok(Value::Json(rendered))
    }
}

fn eval_row_to_json_call(
    args: &[TypedExpr],
    row: &Row,
    qctx: &QueryContext,
    jsonb: bool,
) -> Result<Value> {
    if let TypedExprKind::Row(items) = &args[0].kind {
        return eval_row_items_to_json(items, row, qctx, jsonb);
    }

    let value = eval_typed_expr(&args[0], row, qctx)?;
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    if let Value::Array(values) = &value {
        let element_type = match &args[0].data_type {
            crate::model::DataType::Array(inner) => Some(inner.as_ref()),
            _ => None,
        };
        let mut rendered_fields = Vec::with_capacity(values.len());
        for (index, value) in values.iter().enumerate() {
            let rendered = if let Some(data_type) = element_type {
                if jsonb {
                    render_jsonb_text_for_typed_value(value, data_type, qctx)
                } else {
                    render_json_text_for_typed_value(value, data_type, qctx)
                }
            } else if jsonb {
                render_jsonb_text_pg_from_value(value)
            } else {
                render_json_text_pg_from_value(value)
            }?;
            rendered_fields.push(format!(
                "{}:{}",
                serde_json::Value::String(format!("f{}", index + 1)),
                rendered
            ));
        }
        let raw = format!("{{{}}}", rendered_fields.join(","));
        return if jsonb {
            Ok(Value::Jsonb(crate::sql::jsonb::format_jsonb_pg_str(&raw)))
        } else {
            Ok(Value::Json(raw))
        };
    }

    let rendered = if jsonb {
        render_jsonb_text_for_typed_value(&value, &args[0].data_type, qctx)
    } else {
        render_json_text_for_typed_value(&value, &args[0].data_type, qctx)
    }?;

    if jsonb {
        Ok(Value::Jsonb(crate::sql::jsonb::format_jsonb_pg_str(
            &rendered,
        )))
    } else {
        Ok(Value::Json(rendered))
    }
}

fn eval_row_items_to_json(
    items: &[TypedExpr],
    row: &Row,
    qctx: &QueryContext,
    jsonb: bool,
) -> Result<Value> {
    let mut rendered_fields = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let value = eval_typed_expr(item, row, qctx)?;
        let rendered = if jsonb {
            render_jsonb_text_for_typed_value(&value, &item.data_type, qctx)
        } else {
            render_json_text_for_typed_value(&value, &item.data_type, qctx)
        }?;
        rendered_fields.push(format!(
            "{}:{}",
            serde_json::Value::String(format!("f{}", index + 1)),
            rendered
        ));
    }

    let raw = format!("{{{}}}", rendered_fields.join(","));
    if jsonb {
        Ok(Value::Jsonb(crate::sql::jsonb::format_jsonb_pg_str(&raw)))
    } else {
        Ok(Value::Json(raw))
    }
}

fn eval_json_build_array(
    args: &[TypedExpr],
    row: &Row,
    qctx: &QueryContext,
    jsonb: bool,
) -> Result<Value> {
    if jsonb {
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            let value = eval_typed_expr(arg, row, qctx)?;
            values.push(jsonb_value_for_typed_value(&value, &arg.data_type, qctx)?);
        }
        return Ok(Value::Jsonb(format_jsonb_pg(&serde_json::Value::Array(
            values,
        ))?));
    }

    let mut rendered = Vec::with_capacity(args.len());
    for arg in args {
        let value = eval_typed_expr(arg, row, qctx)?;
        let rendered_item = render_json_text_for_typed_value(&value, &arg.data_type, qctx)?;
        rendered.push(rendered_item);
    }

    Ok(Value::Json(format!("[{}]", rendered.join(", "))))
}

fn eval_json_build_object(
    args: &[TypedExpr],
    row: &Row,
    qctx: &QueryContext,
    jsonb: bool,
) -> Result<Value> {
    if !args.len().is_multiple_of(2) {
        return Err(SqlError::InvalidParameterValue {
            message: "argument list must have even number of elements".into(),
        }
        .into());
    }

    if jsonb {
        let mut object = serde_json::Map::new();
        for (i, chunk) in args.chunks(2).enumerate() {
            let key = eval_typed_expr(&chunk[0], row, qctx)?;
            if matches!(key, Value::Null) {
                return Err(SqlError::InvalidParameterValue {
                    message: format!("argument {}: key must not be null", i * 2 + 1),
                }
                .into());
            }

            let key_str = match key {
                Value::Text(s) => s,
                v => v.to_string(),
            };
            let value = eval_typed_expr(&chunk[1], row, qctx)?;
            object.insert(
                key_str,
                jsonb_value_for_typed_value(&value, &chunk[1].data_type, qctx)?,
            );
        }
        return Ok(Value::Jsonb(format_jsonb_pg(&serde_json::Value::Object(
            object,
        ))?));
    }

    let mut rendered_pairs = Vec::with_capacity(args.len() / 2);
    for chunk in args.chunks(2) {
        let key = eval_typed_expr(&chunk[0], row, qctx)?;
        if matches!(key, Value::Null) {
            return Err(SqlError::NullValueNotAllowed {
                message: "null value not allowed for object key".into(),
            }
            .into());
        }

        let key_str = match key {
            Value::Text(s) => s,
            v => v.to_string(),
        };
        let value = eval_typed_expr(&chunk[1], row, qctx)?;
        let rendered_value = render_json_text_for_typed_value(&value, &chunk[1].data_type, qctx)?;
        let rendered_key = serde_json::to_string(&key_str).unwrap_or_else(|_| "\"\"".into());
        rendered_pairs.push(format!("{} : {}", rendered_key, rendered_value));
    }

    Ok(Value::Json(format!("{{{}}}", rendered_pairs.join(", "))))
}

fn jsonb_value_for_typed_value(
    value: &Value,
    data_type: &DataType,
    qctx: &QueryContext,
) -> Result<serde_json::Value> {
    match (value, data_type) {
        (Value::Timestamp(ts), DataType::TimestampTz) => Ok(serde_json::Value::String(
            render_json_timestamptz_millis(*ts, qctx.timezone.as_ref()),
        )),
        (Value::Array(values), DataType::Array(inner)) => Ok(serde_json::Value::Array(
            values
                .iter()
                .map(|value| jsonb_value_for_typed_value(value, inner, qctx))
                .collect::<Result<Vec<_>>>()?,
        )),
        _ => value_to_json(value),
    }
}

fn render_json_text_for_typed_value(
    value: &Value,
    data_type: &crate::model::DataType,
    qctx: &QueryContext,
) -> Result<String> {
    match (value, data_type) {
        (Value::Timestamp(ts), crate::model::DataType::TimestampTz) => Ok(
            serde_json::Value::String(render_json_timestamptz_millis(*ts, qctx.timezone.as_ref()))
                .to_string(),
        ),
        (Value::Array(values), crate::model::DataType::Array(inner)) => {
            let rendered = values
                .iter()
                .map(|value| render_json_text_for_typed_value(value, inner, qctx))
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("[{}]", rendered.join(",")))
        }
        _ => render_json_text_pg_from_value(value),
    }
}

fn render_jsonb_text_for_typed_value(
    value: &Value,
    data_type: &crate::model::DataType,
    qctx: &QueryContext,
) -> Result<String> {
    match (value, data_type) {
        (Value::Timestamp(ts), crate::model::DataType::TimestampTz) => Ok(
            serde_json::Value::String(render_json_timestamptz_millis(*ts, qctx.timezone.as_ref()))
                .to_string(),
        ),
        (Value::Array(values), crate::model::DataType::Array(inner)) => {
            let rendered = values
                .iter()
                .map(|value| render_jsonb_text_for_typed_value(value, inner, qctx))
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("[{}]", rendered.join(",")))
        }
        _ => render_jsonb_text_pg_from_value(value),
    }
}

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
            let days = crate::model::date::timestamp_millis_to_date_days(
                session_local_timestamp_millis(qctx.transaction_timestamp_ms, qctx)?,
            )?;
            return Ok(Value::Date(days));
        }
        "CURRENT_TIME" | "LOCALTIME" => {
            return Ok(Value::Time(current_time_micros(
                qctx.transaction_timestamp_ms,
                qctx,
            )?));
        }
        "LOCALTIMESTAMP" => {
            return Ok(Value::Timestamp(session_local_timestamp_millis(
                qctx.transaction_timestamp_ms,
                qctx,
            )?));
        }
        "PG_BACKEND_PID" => {
            // PostgreSQL exposes pg_backend_pid() as int4.
            // Internal connection identity is i64; cast intentionally truncates.
            return Ok(Value::Int32(qctx.connection_id as i32));
        }
        "PG_POSTMASTER_START_TIME" => {
            // Actual process start time, captured once on first access.
            // PostgreSQL JDBC driver calls this during connection setup.
            return Ok(Value::Timestamp(postmaster_start_time_ms()));
        }
        "CURRENT_DATABASE" => {
            return Ok(Value::Text(qctx.database_name.as_ref().to_string()));
        }
        "CURRENT_SCHEMA" => {
            return Ok(Value::Text(
                crate::session_context::current_search_path_first_schema(),
            ));
        }
        "CURRENT_SCHEMAS" => {
            // current_schemas(bool) → text[]
            // Strict function: NULL input → NULL output (PG parity).
            // true = include implicit schemas (pg_catalog), false = user schemas only
            let include_implicit = match args.first() {
                Some(Value::Boolean(b)) => *b,
                Some(Value::Null) | None => return Ok(Value::Null),
                Some(other) => {
                    let type_name = other
                        .data_type()
                        .map(|dt| dt.pg_display_name())
                        .unwrap_or_else(|| "unknown".to_string());
                    return Err(crate::sql::error::SqlError::FunctionNotFound(format!(
                        "current_schemas({})",
                        type_name
                    ))
                    .into());
                }
            };
            let schemas = crate::session_context::current_search_path_schemas(include_implicit);
            return Ok(Value::Array(schemas.into_iter().map(Value::Text).collect()));
        }
        "CURRENT_USER" | "SESSION_USER" | "USER" => {
            return Ok(Value::Text(qctx.current_user.as_ref().to_string()));
        }
        "AUTH.UID" => {
            // auth.uid() is a zero-argument function. Reject any arguments.
            if !args.is_empty() {
                return Err(SqlError::FunctionNotFound(format!(
                    "auth.uid({})",
                    args.iter()
                        .map(|a| a
                            .data_type()
                            .map(|dt| dt.pg_display_name())
                            .unwrap_or_else(|| "unknown".to_string()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .into());
            }
            // auth.uid() — returns the JWT subject (sub claim) from the trusted
            // auth pipeline. Reads from `auth.uid` first, falling back to
            // `request.jwt.claim.sub` (populated by P2-4 JWT pipeline). Both
            // namespaces are protected from client spoofing by the anti-spoofing
            // guard. Returns NULL when no JWT context is available.
            for key in &["auth.uid", "request.jwt.claim.sub"] {
                if let Some(v) = QueryContext::current_setting_snapshot(key) {
                    return Ok(Value::Text(v));
                }
                if let Some(ref snapshot) = qctx.settings_snapshot {
                    if let Some(v) = snapshot.get(*key) {
                        return Ok(Value::Text(v.clone()));
                    }
                }
            }
            return Ok(Value::Null);
        }
        "AUTH.JWT" => {
            // auth.jwt() is a zero-argument function. Reject any arguments.
            if !args.is_empty() {
                return Err(SqlError::FunctionNotFound(format!(
                    "auth.jwt({})",
                    args.iter()
                        .map(|a| a
                            .data_type()
                            .map(|dt| dt.pg_display_name())
                            .unwrap_or_else(|| "unknown".to_string()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .into());
            }
            // auth.jwt() — returns the full verified JWT claims as a JSON text
            // string. Reads from `request.jwt.claims` which is populated by the
            // trusted auth pipeline and protected by the anti-spoofing guard.
            // Returns NULL when no JWT context is available.
            let key = "request.jwt.claims";
            if let Some(v) = QueryContext::current_setting_snapshot(key) {
                return Ok(Value::Text(v));
            }
            if let Some(ref snapshot) = qctx.settings_snapshot {
                if let Some(v) = snapshot.get(key) {
                    return Ok(Value::Text(v.clone()));
                }
            }
            return Ok(Value::Null);
        }
        "AUTH.ROLE" => {
            // auth.role() is a zero-argument function. Reject any arguments.
            if !args.is_empty() {
                return Err(SqlError::FunctionNotFound(format!(
                    "auth.role({})",
                    args.iter()
                        .map(|a| a
                            .data_type()
                            .map(|dt| dt.pg_display_name())
                            .unwrap_or_else(|| "unknown".to_string()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .into());
            }
            // auth.role() — returns the current database role name for this
            // session. This is the effective role after any SET ROLE, equivalent
            // to current_user. Useful in RLS policies to distinguish between
            // anon and authenticated roles.
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
            match QueryContext::current_setting_lookup(qctx, canonical) {
                CurrentSettingLookup::Found(v) => return Ok(Value::Text(v)),
                CurrentSettingLookup::Rejected(err) => return Err(err.into()),
                CurrentSettingLookup::Missing if missing_ok => return Ok(Value::Null),
                CurrentSettingLookup::Missing | CurrentSettingLookup::NoSnapshot => {}
            }
            // No snapshot (unit tests, DDL contexts) — fall through to error
            return Err(anyhow!("unrecognized configuration parameter \"{}\"", name));
        }
        "SET_CONFIG" | "PG_CATALOG.SET_CONFIG" => {
            // PG parity: NULL name → error (SQLSTATE 22004).
            let name = match args.first() {
                Some(Value::Text(s)) => s.to_lowercase(),
                Some(Value::Null) | None => {
                    return Err(SqlError::NullValueNotAllowed {
                        message: "SET requires parameter name".to_string(),
                    }
                    .into());
                }
                _ => return Err(anyhow!("set_config requires text argument")),
            };
            // PG parity: NULL value → RESET (handled below after is_local).
            let value: Option<String> = match args.get(1) {
                Some(Value::Text(s)) => Some(s.clone()),
                Some(Value::Null) => None,
                Some(_) | None => return Err(anyhow!("set_config requires text argument")),
            };
            // PG parity: NULL is_local → false (session scope).
            let is_local = match args.get(2) {
                Some(Value::Boolean(b)) => *b,
                Some(Value::Null) => false,
                Some(Value::Text(s)) => {
                    match crate::sql::types::cast::cast(
                        Value::Text(s.clone()),
                        &crate::model::DataType::Boolean,
                        crate::sql::types::cast::CastContext::Implicit,
                    )? {
                        Value::Boolean(b) => b,
                        _ => false,
                    }
                }
                Some(_) | None => {
                    return Err(anyhow!("argument of set_config must be type boolean"));
                }
            };

            let canonical =
                crate::sql::session::settings::SessionSettings::canonical_setting_name(&name);
            if let Some(err) =
                crate::sql::session::settings::SessionSettings::rejected_public_guc_error(canonical)
            {
                return Err(err.into());
            }

            // NULL value → RESET: return boot-default value (PG parity).
            // Guard: reserved pseudo-GUCs must be rejected even on NULL reset.
            let Some(value) = value else {
                check_reserved_guc_reset(&name)?;
                // session_authorization resets to the session's login role,
                // not the current role after SET ROLE (PG parity).
                // Read from base snapshot which holds the authoritative
                // session_authorization set at statement start.
                let default_value = if canonical == "session_authorization" {
                    QueryContext::base_setting_snapshot("session_authorization").unwrap_or_else(
                        || {
                            QueryContext::current_user_name()
                                .map(|u| u.to_string())
                                .unwrap_or_default()
                        },
                    )
                } else {
                    // Read the effective reset default from the execution snapshot
                    // for tenant-configurable GUCs (e.g. statement_timeout) whose
                    // default can differ from the hardcoded boot default.
                    // These values are stored as _reset_default.<name> entries in
                    // the execution snapshot only (never in the public snapshot),
                    // so they are invisible to current_setting() while still
                    // available for NULL-reset resolution.
                    QueryContext::reset_default_from_snapshot(canonical).unwrap_or_else(|| {
                        crate::sql::session::settings::SessionSettings::boot_default_show_value(
                            canonical,
                        )
                    })
                };
                QueryContext::record_set_config_reset(canonical, is_local, &default_value);
                return Ok(Value::Text(default_value));
            };

            check_reserved_guc_write(&name)?;
            if canonical == "session_authorization" {
                // Use session_user (not current_user) for PG parity: SET ROLE
                // changes current_user but session_authorization checks the
                // authenticated login role.
                let session_user = QueryContext::base_setting_snapshot("session_authorization")
                    .unwrap_or_else(|| qctx.current_user.to_string());
                if value == session_user {
                    // Same-user set is a no-op; return the current value.
                    return Ok(Value::Text(session_user));
                }
                // Different user: produce PG-parity error
                // (role-not-found vs permission-denied).
                let store = qctx.store_ref.as_ref().map(|s| s.0.as_ref());
                return Err(session_auth_different_user_error_sync(&value, store));
            }
            let normalized_value = if canonical == "search_path" {
                if value.is_empty() {
                    String::new()
                } else {
                    let parsed = parse_search_path_guc_value(&value);
                    let normalized = normalize_search_path_entries(parsed)?;
                    crate::sql::session::settings::SessionSettings::format_search_path_show(
                        &normalized,
                    )
                }
            } else {
                crate::sql::session::settings::SessionSettings::validate_and_normalize_value(
                    canonical, &value,
                )?
            };
            // Always record the mutation: the runtime override gives same-statement
            // visibility to current_setting(), and the flush layer
            // (apply_pending_set_config_mutations) already drops is_local mutations
            // outside explicit transactions.  Runtime overrides are per-statement
            // scoped (fresh HashMap in with_scoped_query_context), so they cannot
            // leak to subsequent statements.
            QueryContext::record_set_config_mutation(canonical, &normalized_value, is_local);
            return Ok(Value::Text(normalized_value));
        }
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
    // Try fully-qualified name first (e.g. "SERVERLESS_FUNCTIONS.INVOKE"),
    // then fall back to unqualified (e.g. "UPPER").
    let registry = crate::sql::expr::functions::get_registry();
    match registry
        .get(func_name_upper.as_str())
        .or_else(|| registry.get(unqualified_name))
    {
        Some(f) => f(args),
        None => Err(SqlError::Unsupported(format!("unknown function: {}", name)).into()),
    }
}

fn local_naive_datetime_from_timestamptz_millis(
    ts_millis: i64,
    qctx: &QueryContext,
) -> Result<chrono::NaiveDateTime> {
    let timezone = effective_timezone(qctx);
    let tz = crate::model::timestamp::TimeZoneSpec::try_parse(timezone.as_ref())?;
    let utc = chrono::DateTime::from_timestamp_millis(ts_millis)
        .ok_or_else(|| anyhow!("invalid timestamp"))?;
    Ok(match tz {
        crate::model::timestamp::TimeZoneSpec::Fixed(offset) => {
            utc.with_timezone(&offset).naive_local()
        }
        crate::model::timestamp::TimeZoneSpec::Named(tz) => utc.with_timezone(&tz).naive_local(),
    })
}

fn local_naive_datetime_to_timestamptz_millis(
    naive: chrono::NaiveDateTime,
    qctx: &QueryContext,
) -> Result<i64> {
    let timezone = effective_timezone(qctx);
    let tz = crate::model::timestamp::TimeZoneSpec::try_parse(timezone.as_ref())?;
    tz.timestamp_millis_from_local_datetime(naive)
}

fn value_to_naive_datetime_with_context(
    value: &Value,
    data_type: &crate::model::DataType,
    qctx: &QueryContext,
) -> Result<chrono::NaiveDateTime> {
    if matches!(data_type, crate::model::DataType::TimestampTz) {
        return match value {
            Value::Timestamp(ts) => local_naive_datetime_from_timestamptz_millis(*ts, qctx),
            Value::Date(days) => crate::model::date::date_days_to_naive_date(*days)?
                .and_hms_opt(0, 0, 0)
                .ok_or_else(|| anyhow!("invalid date")),
            Value::Text(s) => match crate::sql::expr::parse_timestamp_string(s)? {
                Value::Timestamp(ts) => local_naive_datetime_from_timestamptz_millis(ts, qctx),
                _ => Err(anyhow!("invalid timestamptz text")),
            },
            Value::Null => Err(anyhow!("AGE cannot take NULL directly")),
            other => Err(anyhow!("AGE cannot convert {:?} to timestamp", other)),
        };
    }

    match value {
        Value::Timestamp(ts) => chrono::DateTime::from_timestamp_millis(*ts)
            .ok_or_else(|| anyhow!("invalid timestamp"))
            .map(|dt| dt.naive_utc()),
        Value::Date(days) => crate::model::date::date_days_to_naive_date(*days)?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow!("invalid date")),
        Value::Text(s) => match crate::sql::expr::parse_timestamp_string(s)? {
            Value::Timestamp(ts) => chrono::DateTime::from_timestamp_millis(ts)
                .ok_or_else(|| anyhow!("invalid timestamp"))
                .map(|dt| dt.naive_utc()),
            _ => Err(anyhow!("invalid timestamp text")),
        },
        Value::Null => Err(anyhow!("AGE cannot take NULL directly")),
        other => Err(anyhow!("AGE cannot convert {:?} to timestamp", other)),
    }
}

pub(super) fn eval_date_timestamptz(
    arg: &TypedExpr,
    row: &Row,
    qctx: &QueryContext,
) -> Result<Value> {
    let value = eval_typed_expr(arg, row, qctx)?;
    match value {
        Value::Null => Ok(Value::Null),
        Value::Timestamp(ts) => {
            let local = local_naive_datetime_from_timestamptz_millis(ts, qctx)?;
            Ok(Value::Date(crate::model::date::naive_date_to_days(
                local.date(),
            )?))
        }
        Value::Date(days) => Ok(Value::Date(days)),
        Value::Text(s) => match crate::sql::expr::parse_timestamp_string(&s)? {
            Value::Timestamp(ts) => {
                let local = local_naive_datetime_from_timestamptz_millis(ts, qctx)?;
                Ok(Value::Date(crate::model::date::naive_date_to_days(
                    local.date(),
                )?))
            }
            _ => Err(anyhow!("DATE() cannot convert text to timestamptz")),
        },
        other => Err(anyhow!("DATE() cannot convert {:?} to date", other)),
    }
}

pub(super) fn eval_age_with_timestamptz(
    args: &[TypedExpr],
    row: &Row,
    qctx: &QueryContext,
) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(anyhow!("AGE requires 1 or 2 arguments"));
    }

    let end_val = eval_typed_expr(&args[0], row, qctx)?;
    if matches!(end_val, Value::Null) {
        return Ok(Value::Null);
    }

    let start_val = if args.len() == 2 {
        let start_val = eval_typed_expr(&args[1], row, qctx)?;
        if matches!(start_val, Value::Null) {
            return Ok(Value::Null);
        }
        Some(start_val)
    } else {
        None
    };

    let age_args = match &start_val {
        Some(start_val) => vec![end_val.clone(), start_val.clone()],
        None => vec![end_val.clone()],
    };
    if let Some(interval) = crate::sql::expr::functions::datetime::age_infinite_interval(&age_args)?
    {
        return Ok(Value::Interval(interval));
    }

    let end_ts = value_to_naive_datetime_with_context(&end_val, &args[0].data_type, qctx)?;
    let start_ts = match start_val {
        Some(start_val) => {
            value_to_naive_datetime_with_context(&start_val, &args[1].data_type, qctx)?
        }
        None => local_naive_datetime_from_timestamptz_millis(qctx.transaction_timestamp_ms, qctx)?,
    };

    Ok(Value::Interval(
        crate::sql::expr::functions::datetime::age_interval(end_ts, start_ts)?,
    ))
}

pub(super) fn eval_date_part_with_timestamptz(
    field_expr: &TypedExpr,
    source_expr: &TypedExpr,
    row: &Row,
    qctx: &QueryContext,
    return_numeric: bool,
) -> Result<Value> {
    let field = match eval_typed_expr(field_expr, row, qctx)? {
        Value::Text(s) => s.trim().to_owned(),
        Value::Null => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let source = eval_typed_expr(source_expr, row, qctx)?;
    if matches!(source, Value::Null) {
        return Ok(Value::Null);
    }
    if matches!(source, Value::Timestamp(ts) if ts == i64::MAX || ts == i64::MIN) {
        return crate::sql::expr::functions::datetime::eval_date_part_common(
            vec![Value::Text(field), source],
            return_numeric,
        );
    }

    let local_dt = value_to_naive_datetime_with_context(&source, &source_expr.data_type, qctx)?;
    let ts = if field.eq_ignore_ascii_case("epoch") {
        local_naive_datetime_to_timestamptz_millis(local_dt, qctx)?
    } else {
        local_dt.and_utc().timestamp_millis()
    };

    crate::sql::expr::functions::datetime::eval_date_part_common(
        vec![Value::Text(field), Value::Timestamp(ts)],
        return_numeric,
    )
}

pub(super) fn eval_date_trunc_with_timestamptz(
    field_expr: &TypedExpr,
    source_expr: &TypedExpr,
    row: &Row,
    qctx: &QueryContext,
) -> Result<Value> {
    let field = match eval_typed_expr(field_expr, row, qctx)? {
        Value::Text(s) => s.trim().to_owned(),
        Value::Null => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let source = eval_typed_expr(source_expr, row, qctx)?;
    if matches!(source, Value::Null) {
        return Ok(Value::Null);
    }
    if matches!(source, Value::Timestamp(ts) if ts == i64::MAX || ts == i64::MIN) {
        return crate::sql::expr::functions::datetime::eval_date_trunc(vec![
            Value::Text(field),
            source,
        ]);
    }

    let local_dt = value_to_naive_datetime_with_context(&source, &source_expr.data_type, qctx)?;
    let local_ts = local_dt.and_utc().timestamp_millis();
    let truncated = crate::sql::expr::functions::datetime::eval_date_trunc(vec![
        Value::Text(field),
        Value::Timestamp(local_ts),
    ])?;
    let Value::Timestamp(truncated_local_ts) = truncated else {
        return Ok(truncated);
    };
    let truncated_local_dt = chrono::DateTime::from_timestamp_millis(truncated_local_ts)
        .ok_or_else(|| anyhow!("invalid timestamp"))?
        .naive_utc();
    Ok(Value::Timestamp(
        local_naive_datetime_to_timestamptz_millis(truncated_local_dt, qctx)?,
    ))
}

pub(super) fn eval_to_char_with_typed_args(
    value_expr: &TypedExpr,
    pattern_expr: &TypedExpr,
    row: &Row,
    qctx: &QueryContext,
) -> Result<Value> {
    let value = eval_typed_expr(value_expr, row, qctx)?;
    let pattern = match eval_typed_expr(pattern_expr, row, qctx)? {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        other => return Err(anyhow!("TO_CHAR format must be text, got {:?}", other)),
    };

    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }

    match format_to_char_value(
        &value,
        Some(&value_expr.data_type),
        Some(qctx.timezone.as_ref()),
        &pattern,
    )? {
        Some(text) => Ok(Value::Text(text)),
        None => Ok(Value::Null),
    }
}

fn split_qualified_function_name(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once('.') {
        Some((schema, func)) => (Some(schema), func),
        None => (None, name),
    }
}

fn parse_search_path_guc_value(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in s.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let schema = if token.starts_with('\"') && token.ends_with('\"') && token.len() >= 2 {
            token[1..token.len() - 1].to_string()
        } else {
            token.to_lowercase()
        };
        out.push(schema);
    }
    out
}

fn normalize_search_path_entries(mut entries: Vec<String>) -> Result<Vec<String>> {
    entries.retain(|s| !s.is_empty());
    // NOTE: No `default` keyword rewrite here. set_config() takes text values,
    // so 'default' is a literal schema name (PG parity). The DEFAULT keyword
    // rewrite belongs only in the SET statement path (guc.rs).
    for schema in &entries {
        if schema.contains('.') {
            return Err(anyhow!("schema name '{}' must not contain '.'", schema));
        }
    }
    if entries.is_empty() {
        entries.push("public".to_string());
    }
    Ok(entries)
}

pub(super) fn effective_timezone(qctx: &QueryContext) -> Arc<str> {
    qctx.timezone.clone()
}

fn session_local_timestamp_millis(ts_millis: i64, qctx: &QueryContext) -> Result<i64> {
    let timezone = effective_timezone(qctx);
    let tz = crate::model::timestamp::TimeZoneSpec::try_parse(timezone.as_ref())?;
    let utc = chrono::DateTime::from_timestamp_millis(ts_millis)
        .ok_or_else(|| anyhow!("invalid timestamp"))?;
    let offset_ms = match tz {
        crate::model::timestamp::TimeZoneSpec::Fixed(offset) => {
            i64::from(offset.local_minus_utc()) * 1000
        }
        crate::model::timestamp::TimeZoneSpec::Named(tz) => {
            i64::from(utc.with_timezone(&tz).offset().fix().local_minus_utc()) * 1000
        }
    };
    Ok(ts_millis + offset_ms)
}

fn current_time_micros(ts_millis: i64, qctx: &QueryContext) -> Result<i64> {
    let local_ts = session_local_timestamp_millis(ts_millis, qctx)?;
    Ok(local_ts.rem_euclid(24 * 60 * 60 * 1000) * 1000)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for #2294: schema-qualified builtins must resolve via
    /// qualified key in the function registry. Before the fix,
    /// `eval_function_call("serverless_functions.invoke", ...)` looked up
    /// only `INVOKE` (unqualified), missing `SERVERLESS_FUNCTIONS.INVOKE`.
    #[tokio::test]
    async fn schema_qualified_builtin_resolves_via_registry() {
        // Set up extension context (non-superuser) so the function is
        // dispatched but rejected at the permission check — proving the
        // qualified registry lookup succeeded.
        let err = crate::extensions::context::with_context(false, "test_ks", async {
            let qctx = QueryContext::for_tests();
            eval_function_call(
                "serverless_functions.invoke",
                vec![Value::Text("test_fn".into()), Value::Null],
                &qctx,
            )
        })
        .await
        .unwrap_err();

        // If the qualified lookup failed, we'd get "unknown function".
        // Instead we should get "permission denied" — the function was found
        // but the non-superuser guard rejected it.
        let msg = err.to_string();
        assert!(
            msg.contains("permission denied"),
            "expected PermissionDenied (function resolved), got: {msg}"
        );
    }

    /// Verify that unqualified function lookup still works (no regression
    /// from the qualified-first change).
    #[test]
    fn unqualified_builtin_still_resolves() {
        let registry = crate::sql::expr::functions::get_registry();
        assert!(
            registry.contains_key("UPPER"),
            "unqualified builtin UPPER must be in registry"
        );
    }
}
