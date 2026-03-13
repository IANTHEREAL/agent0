//! Miscellaneous helper functions for the typed expression evaluator.
//!
//! Contains function call dispatch, timezone evaluation, array indexing,
//! JSON operator mapping, and string conversion utilities.

use crate::model::{Row, Value};
use crate::sql::analyzer::types::*;
use crate::sql::error::SqlError;
use crate::sql::executor::{
    check_reserved_guc_reset, check_reserved_guc_write, session_auth_different_user_error_sync,
};
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};
use std::sync::OnceLock;

use super::eval_typed_expr;

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
            ))
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
            if let Some(v) = QueryContext::current_setting_snapshot(canonical) {
                return Ok(Value::Text(v));
            }
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
                    return Err(anyhow!("argument of set_config must be type boolean"))
                }
            };

            let canonical =
                crate::sql::session::settings::SessionSettings::canonical_setting_name(&name);

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
