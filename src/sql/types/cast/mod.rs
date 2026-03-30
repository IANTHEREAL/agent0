//! Unified cast function with context-dependent rules.
//!
//! `CastContext` controls which conversions are allowed:
//! - `Explicit`: CAST(x AS type) — most permissive
//! - `Assignment`: INSERT/UPDATE column coercion — medium
//! - `Implicit`: Comparison coercion — strictest

use crate::model::{ColumnDef, DataType, Value};
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use sqlparser::ast::Expr;
use std::str::FromStr;

const MAX_RUNTIME_NUMERIC_SCALE: u32 = Decimal::MAX_SCALE;

fn is_regclass_udt(udt: &str) -> bool {
    udt.eq_ignore_ascii_case("regclass") || udt.eq_ignore_ascii_case("pg_catalog.regclass")
}

fn is_regtype_udt(udt: &str) -> bool {
    udt.eq_ignore_ascii_case("regtype") || udt.eq_ignore_ascii_case("pg_catalog.regtype")
}

/// Controls which type conversions are allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastContext {
    /// `CAST(x AS type)` — most permissive
    Explicit,
    /// INSERT/UPDATE column coercion — medium
    Assignment,
    /// Comparison coercion — strictest
    Implicit,
}

fn round_half_away_from_zero(n: f64) -> f64 {
    if n >= 0.0 {
        (n + 0.5).floor()
    } else {
        (n - 0.5).ceil()
    }
}

/// Convert a value to bytea. Public within crate because `cast_custom_type()` in expr/mod.rs
/// also needs this for `CAST(x AS BYTEA)` via custom type dispatch.
pub(crate) fn cast_to_bytea(v: Value) -> Result<Value> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Bytes(_) => Ok(v),
        Value::Text(s) => {
            if let Some(rest) = s.strip_prefix("\\x") {
                let bytes = hex::decode(rest).map_err(|e| SqlError::InvalidInputSyntax {
                    type_name: "bytea".into(),
                    value: e.to_string(),
                })?;
                Ok(Value::Bytes(bytes))
            } else {
                Ok(Value::Bytes(s.into_bytes()))
            }
        }
        other => Ok(Value::Bytes(other.to_string().into_bytes())),
    }
}

/// Check if a value's runtime type is already compatible with the target column type.
/// Used by Assignment context's catch-all to distinguish identity from invalid casts.
fn value_is_compatible_with_column_type(value: &Value, column_type: &DataType) -> bool {
    match (value, column_type) {
        (Value::Null, _) => true,
        (Value::Boolean(_), DataType::Boolean) => true,
        (Value::Int32(_), DataType::Int32) => true,
        (Value::Int64(_), DataType::Int64) => true,
        (Value::Float64(_), DataType::Float64) => true,
        (
            Value::Text(_),
            DataType::Text | DataType::Name | DataType::Varchar(_) | DataType::UserDefined(_),
        ) => true,
        (Value::Int32(_), DataType::UserDefined(udt)) if is_regclass_udt(udt) => true,
        (Value::Int64(_), DataType::UserDefined(udt)) if is_regclass_udt(udt) => true,
        (Value::Bytes(_), DataType::Bytes) => true,
        (Value::Timestamp(_), DataType::Timestamp | DataType::TimestampTz) => true,
        (Value::Interval(_), DataType::Interval) => true,
        (Value::Uuid(_), DataType::Uuid) => true,
        (Value::Array(_), DataType::Array(_)) => true,
        (Value::Vector(vec), DataType::Vector(dim)) => vec.len() == *dim as usize,
        (Value::Json(_), DataType::Json) => true,
        (Value::Jsonb(_), DataType::Jsonb) => true,
        (Value::Time(_), DataType::Time) => true,
        (Value::Date(_), DataType::Date) => true,
        (Value::Numeric(_), DataType::Numeric { .. }) => true,
        (Value::Tsvector(_), DataType::Tsvector) => true,
        (Value::Tsquery(_), DataType::Tsquery) => true,
        _ => false,
    }
}

/// Unified cast function: convert `val` to `target` type under the given `context`.
///
/// Context-dependent behavior:
/// - Float64→Int32: Explicit rounds half-away-from-zero; Assignment rejects fractions
/// - Float64→Int64: Explicit rounds; Assignment falls to catch-all (error)
/// - Numeric→Int32/Int64: Explicit rounds then converts; Assignment converts directly (no rounding)
/// - Numeric→Float64: Explicit errors on overflow; Assignment uses NaN fallback
/// - Bool→Int32: Explicit allows (true→1, false→0); Assignment falls to catch-all (error)
/// - Int/Float/Numeric→Bool: Explicit allows (non-zero=true); Assignment falls to catch-all (error)
/// - Catch-all: Explicit passes through; Assignment checks compatibility then errors
pub(crate) fn cast(val: Value, target: &DataType, context: CastContext) -> Result<Value> {
    // NULL is castable to any type in all contexts.
    if val == Value::Null {
        return Ok(Value::Null);
    }

    match target {
        DataType::Varchar(n) => cast_to_varchar(&val, *n, context),
        DataType::Text | DataType::Name => cast_to_text(val),
        DataType::Boolean => cast_to_boolean(val, context),
        DataType::Int32 => cast_to_int32(val, context),
        DataType::Int64 => cast_to_int64(val, context),
        DataType::Float64 => cast_to_float64(val, context),
        DataType::Bytes => cast_to_bytea(val),
        DataType::Date
        | DataType::Time
        | DataType::Timestamp
        | DataType::TimestampTz
        | DataType::Interval => cast_to_temporal(val, target),
        DataType::Uuid => cast_to_uuid(val),
        DataType::Json | DataType::Jsonb => cast_to_json(val, target, context),
        DataType::Tsquery => cast_to_tsquery(val),
        DataType::Numeric { .. } => cast_to_numeric(val, target),
        DataType::Array(_) => cast_to_array(val, target, context),
        DataType::Vector(_) => cast_to_vector(val, target),
        DataType::UserDefined(u) if is_regclass_udt(u) => cast_to_regclass(val),
        DataType::UserDefined(u) if is_regtype_udt(u) => cast_to_regtype(val),
        // Unknown target: resolve to Text (PG defaults UNKNOWNOID → TEXT).
        DataType::Unknown => cast_to_text(val),
        _ => cast_catchall(val, target, context),
    }
}

fn cast_to_varchar(val: &Value, max_len: u64, context: CastContext) -> Result<Value> {
    let s = val.to_string();
    if max_len == 0 {
        // PostgreSQL bare VARCHAR has no typmod limit. We preserve the
        // type identity as Varchar(0) in metadata, but runtime coercion
        // must behave like unbounded character varying.
        return Ok(Value::Text(s));
    }
    match context {
        CastContext::Explicit => {
            let truncated: String = s.chars().take(max_len as usize).collect();
            Ok(Value::Text(truncated))
        }
        CastContext::Assignment => {
            // Postgres: error if value exceeds length (unless excess is all spaces)
            let trimmed = s.trim_end();
            if trimmed.chars().count() > max_len as usize {
                Err(SqlError::StringDataRightTruncation {
                    max_length: max_len,
                }
                .into())
            } else {
                Ok(Value::Text(s.chars().take(max_len as usize).collect()))
            }
        }
        CastContext::Implicit => Ok(Value::Text(s)),
    }
}

fn cast_to_text(val: Value) -> Result<Value> {
    match val {
        Value::Timestamp(ts) => {
            let formatted = crate::model::timestamp::format_timestamp_millis(ts, false)
                .unwrap_or_else(|_| ts.to_string());
            Ok(Value::Text(formatted))
        }
        Value::Jsonb(s) => Ok(Value::Text(crate::sql::jsonb::format_jsonb_pg_str(&s))),
        v => Ok(Value::Text(v.to_string())),
    }
}

fn cast_to_boolean(val: Value, context: CastContext) -> Result<Value> {
    match val {
        Value::Text(s) => match s.trim().to_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => Ok(Value::Boolean(true)),
            "false" | "f" | "no" | "n" | "off" | "0" => Ok(Value::Boolean(false)),
            _ => Err(SqlError::InvalidInputSyntax {
                type_name: "boolean".into(),
                value: s,
            }
            .into()),
        },
        // Int/Float/Numeric → Bool: Explicit only
        Value::Int32(n) if context == CastContext::Explicit => Ok(Value::Boolean(n != 0)),
        Value::Int64(n) if context == CastContext::Explicit => Ok(Value::Boolean(n != 0)),
        Value::Float64(n) if context == CastContext::Explicit => Ok(Value::Boolean(n != 0.0)),
        Value::Numeric(d) if context == CastContext::Explicit => Ok(Value::Boolean(!d.is_zero())),
        v => cast_catchall(v, &DataType::Boolean, context),
    }
}

fn cast_to_int32(val: Value, context: CastContext) -> Result<Value> {
    match val {
        Value::Text(s) => s.trim().parse::<i32>().map(Value::Int32).map_err(|_| {
            SqlError::InvalidInputSyntax {
                type_name: "integer".into(),
                value: s,
            }
            .into()
        }),
        Value::Int64(n) => i32::try_from(n).map(Value::Int32).map_err(|_| {
            SqlError::NumericValueOutOfRange {
                message: "integer out of range".into(),
            }
            .into()
        }),
        Value::Float64(f) => match context {
            CastContext::Explicit => {
                let rounded = round_half_away_from_zero(f);
                if f.is_nan() || rounded < (i32::MIN as f64) || rounded > (i32::MAX as f64) {
                    return Err(SqlError::NumericValueOutOfRange {
                        message: "integer out of range".into(),
                    }
                    .into());
                }
                Ok(Value::Int32(rounded as i32))
            }
            CastContext::Assignment | CastContext::Implicit => {
                if f.fract() != 0.0 {
                    return Err(SqlError::InvalidInputSyntax {
                        type_name: "integer".into(),
                        value: f.to_string(),
                    }
                    .into());
                }
                let n = f as i64;
                i32::try_from(n).map(Value::Int32).map_err(|_| {
                    SqlError::NumericValueOutOfRange {
                        message: "integer out of range".into(),
                    }
                    .into()
                })
            }
        },
        Value::Numeric(d) => {
            use rust_decimal::prelude::ToPrimitive;
            match context {
                CastContext::Explicit => {
                    use rust_decimal::RoundingStrategy;
                    d.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
                        .to_i32()
                        .map(Value::Int32)
                        .ok_or_else(|| {
                            SqlError::NumericValueOutOfRange {
                                message: "integer out of range".into(),
                            }
                            .into()
                        })
                }
                CastContext::Assignment | CastContext::Implicit => {
                    d.to_i32().map(Value::Int32).ok_or_else(|| {
                        SqlError::NumericValueOutOfRange {
                            message: "integer out of range".into(),
                        }
                        .into()
                    })
                }
            }
        }
        // Bool → Int32: Explicit only
        Value::Boolean(b) if context == CastContext::Explicit => {
            Ok(Value::Int32(if b { 1 } else { 0 }))
        }
        v => cast_catchall(v, &DataType::Int32, context),
    }
}

fn cast_to_int64(val: Value, context: CastContext) -> Result<Value> {
    match val {
        Value::Text(s) => s.trim().parse::<i64>().map(Value::Int64).map_err(|_| {
            SqlError::InvalidInputSyntax {
                type_name: "bigint".into(),
                value: s,
            }
            .into()
        }),
        Value::Int32(n) => Ok(Value::Int64(n as i64)),
        // Float64 → Int64: Explicit only
        Value::Float64(n) if context == CastContext::Explicit => {
            let rounded = round_half_away_from_zero(n);
            if n.is_nan() || rounded < (i64::MIN as f64) || rounded > (i64::MAX as f64) {
                return Err(SqlError::NumericValueOutOfRange {
                    message: "bigint out of range".into(),
                }
                .into());
            }
            Ok(Value::Int64(rounded as i64))
        }
        Value::Numeric(d) => {
            use rust_decimal::prelude::ToPrimitive;
            match context {
                CastContext::Explicit => {
                    use rust_decimal::RoundingStrategy;
                    d.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
                        .to_i64()
                        .map(Value::Int64)
                        .ok_or_else(|| {
                            SqlError::NumericValueOutOfRange {
                                message: "bigint out of range".into(),
                            }
                            .into()
                        })
                }
                CastContext::Assignment | CastContext::Implicit => {
                    d.to_i64().map(Value::Int64).ok_or_else(|| {
                        SqlError::NumericValueOutOfRange {
                            message: "bigint out of range".into(),
                        }
                        .into()
                    })
                }
            }
        }
        v => cast_catchall(v, &DataType::Int64, context),
    }
}

fn cast_to_float64(val: Value, context: CastContext) -> Result<Value> {
    match val {
        Value::Text(s) => s.trim().parse::<f64>().map(Value::Float64).map_err(|_| {
            SqlError::InvalidInputSyntax {
                type_name: "double precision".into(),
                value: s,
            }
            .into()
        }),
        Value::Int32(n) => Ok(Value::Float64(n as f64)),
        Value::Int64(n) => Ok(Value::Float64(n as f64)),
        Value::Numeric(d) => {
            use rust_decimal::prelude::ToPrimitive;
            match context {
                CastContext::Explicit => d.to_f64().map(Value::Float64).ok_or_else(|| {
                    SqlError::NumericValueOutOfRange {
                        message: "numeric value out of range for double precision".into(),
                    }
                    .into()
                }),
                CastContext::Assignment | CastContext::Implicit => {
                    d.to_f64().map(Value::Float64).ok_or_else(|| {
                        SqlError::NumericValueOutOfRange {
                            message: "numeric value out of range for double precision".into(),
                        }
                        .into()
                    })
                }
            }
        }
        v => cast_catchall(v, &DataType::Float64, context),
    }
}

fn cast_to_temporal(val: Value, target: &DataType) -> Result<Value> {
    match (val, target) {
        (Value::Text(s), DataType::Interval) => crate::sql::expr::parse_interval_string(&s)
            .map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "interval".into(),
                    value: s,
                })
            }),
        (Value::Text(s), DataType::Date) => {
            crate::model::date::parse_date_days(&s).map(Value::Date)
        }
        (Value::Timestamp(ts), DataType::Date) => {
            crate::model::date::timestamp_millis_to_date_days(ts).map(Value::Date)
        }
        (Value::Date(days), DataType::Date) => Ok(Value::Date(days)),
        (Value::Text(s), DataType::Timestamp | DataType::TimestampTz) => {
            crate::sql::expr::parse_timestamp_string(&s).map_err(|_| {
                let ty = match target {
                    DataType::TimestampTz => "timestamp with time zone",
                    _ => "timestamp",
                };
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: ty.into(),
                    value: s,
                })
            })
        }
        (Value::Timestamp(ts), DataType::Timestamp | DataType::TimestampTz) => {
            Ok(Value::Timestamp(ts))
        }
        (Value::Date(days), DataType::Timestamp | DataType::TimestampTz) => {
            crate::model::date::date_days_to_timestamp_millis(days).map(Value::Timestamp)
        }
        (Value::Text(s), DataType::Time) => {
            let trimmed = s.trim();
            parse_time_string(trimmed).map(Value::Time).ok_or_else(|| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "time".into(),
                    value: s,
                })
            })
        }
        (Value::Time(micros), DataType::Time) => Ok(Value::Time(micros)),
        (v, _) => cast_catchall(v, target, CastContext::Explicit),
    }
}

fn cast_to_uuid(val: Value) -> Result<Value> {
    match val {
        Value::Text(s) => uuid::Uuid::parse_str(s.trim())
            .map(|u| Value::Uuid(*u.as_bytes()))
            .map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "uuid".into(),
                    value: s,
                })
            }),
        Value::Uuid(bytes) => Ok(Value::Uuid(bytes)),
        v => cast_catchall(v, &DataType::Uuid, CastContext::Explicit),
    }
}

fn cast_to_json(val: Value, target: &DataType, context: CastContext) -> Result<Value> {
    match (val, target) {
        (Value::Text(s), DataType::Json) => {
            serde_json::from_str::<serde_json::Value>(&s).map_err(|e| {
                SqlError::InvalidInputSyntax {
                    type_name: "json".into(),
                    value: e.to_string(),
                }
            })?;
            Ok(Value::Json(s))
        }
        (Value::Text(s), DataType::Jsonb) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&s).map_err(|e| SqlError::InvalidInputSyntax {
                    type_name: "jsonb".into(),
                    value: e.to_string(),
                })?;
            Ok(Value::Jsonb(parsed.to_string()))
        }
        (Value::Json(s), DataType::Json) => Ok(Value::Json(s)),
        (Value::Json(s), DataType::Jsonb) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&s).map_err(|e| SqlError::InvalidInputSyntax {
                    type_name: "jsonb".into(),
                    value: e.to_string(),
                })?;
            Ok(Value::Jsonb(parsed.to_string()))
        }
        (Value::Jsonb(s), DataType::Json) => {
            Ok(Value::Json(crate::sql::jsonb::format_jsonb_pg_str(&s)))
        }
        (Value::Jsonb(s), DataType::Jsonb) => Ok(Value::Jsonb(s)),
        // Explicit: any value → JSON via to_string (existing behavior from cast_value_to_type)
        (v, DataType::Json) if context == CastContext::Explicit => {
            let s = match &v {
                Value::Text(s) => s.clone(),
                Value::Json(s) => s.clone(),
                Value::Jsonb(s) => s.clone(),
                other => other.to_string(),
            };
            serde_json::from_str::<serde_json::Value>(&s).map_err(|e| {
                SqlError::InvalidInputSyntax {
                    type_name: "json".into(),
                    value: e.to_string(),
                }
            })?;
            Ok(Value::Json(s))
        }
        (v, _) => cast_catchall(v, target, context),
    }
}

fn cast_to_tsquery(val: Value) -> Result<Value> {
    match val {
        Value::Text(s) => {
            crate::sql::fts::validate_tsquery_syntax(&s)?;
            Ok(Value::Tsquery(s))
        }
        Value::Tsquery(s) => Ok(Value::Tsquery(s)),
        v => cast_catchall(v, &DataType::Tsquery, CastContext::Explicit),
    }
}

fn cast_to_numeric(val: Value, target: &DataType) -> Result<Value> {
    let scale = match target {
        DataType::Numeric { scale, .. } => *scale,
        _ => None,
    };
    match val {
        Value::Text(s) => {
            let mut d = Decimal::from_str(s.trim()).map_err(|_| SqlError::InvalidInputSyntax {
                type_name: "numeric".into(),
                value: s,
            })?;
            if let Some(s) = scale {
                d.rescale(s.min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }
        Value::Int32(n) => {
            let mut d = Decimal::from(n);
            if let Some(s) = scale {
                d.rescale(s.min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }
        Value::Int64(n) => {
            let mut d = Decimal::from(n);
            if let Some(s) = scale {
                d.rescale(s.min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }
        Value::Float64(f) => {
            let mut d = Decimal::try_from(f).map_err(|_| SqlError::InvalidInputSyntax {
                type_name: "numeric".into(),
                value: f.to_string(),
            })?;
            if let Some(s) = scale {
                d.rescale(s.min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }
        Value::Numeric(d) => {
            let mut d = d;
            if let Some(s) = scale {
                d.rescale(s.min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }
        v => cast_catchall(v, target, CastContext::Explicit),
    }
}

fn cast_to_array(val: Value, target: &DataType, context: CastContext) -> Result<Value> {
    let elem_type = match target {
        DataType::Array(elem_type) => elem_type,
        _ => unreachable!(),
    };
    match val {
        Value::Text(s) => {
            let arr = parse_pg_array(&s).map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "array".into(),
                    value: s,
                })
            })?;
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                if v == Value::Null {
                    out.push(Value::Null);
                } else {
                    out.push(cast(v, elem_type, context)?);
                }
            }
            Ok(Value::Array(out))
        }
        Value::Array(elems) => {
            let mut out = Vec::with_capacity(elems.len());
            for v in elems {
                if v == Value::Null {
                    out.push(Value::Null);
                } else {
                    out.push(cast(v, elem_type, context)?);
                }
            }
            Ok(Value::Array(out))
        }
        v => cast_catchall(v, target, context),
    }
}

fn cast_to_vector(val: Value, target: &DataType) -> Result<Value> {
    let dim = match target {
        DataType::Vector(dim) => *dim,
        _ => unreachable!(),
    };
    match val {
        Value::Text(s) => {
            const MAX_VECTOR_DIMENSIONS: usize = 16384;
            let trimmed = s.trim();
            if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
                return Err(SqlError::InvalidInputSyntax {
                    type_name: "vector".into(),
                    value: s,
                }
                .into());
            }
            // Reject modifier > MAX before parsing elements.
            if dim as usize > MAX_VECTOR_DIMENSIONS {
                return Err(anyhow!(
                    "vector cannot have more than {} dimensions",
                    MAX_VECTOR_DIMENSIONS
                ));
            }
            let inner = &trimmed[1..trimmed.len() - 1];
            if inner.trim().is_empty() {
                return Err(anyhow!("vector must have at least 1 dimension"));
            }
            // Count elements before allocating: single pass, early break at MAX+1.
            let elem_count = inner.split(',').take(MAX_VECTOR_DIMENSIONS + 1).count();
            if elem_count > MAX_VECTOR_DIMENSIONS {
                return Err(anyhow!(
                    "vector cannot have more than {} dimensions",
                    MAX_VECTOR_DIMENSIONS
                ));
            }
            let vec: Vec<f64> = inner
                .split(',')
                .map(|e| e.trim().parse::<f64>())
                .collect::<std::result::Result<Vec<f64>, _>>()
                .map_err(|_| {
                    anyhow::Error::from(SqlError::InvalidInputSyntax {
                        type_name: "vector".into(),
                        value: trimmed.to_string(),
                    })
                })?;
            // dim == 0 means "any dimension" (bare `vector` without modifier).
            if dim > 0 && vec.len() != dim as usize {
                return Err(anyhow!("expected {} dimensions, not {}", dim, vec.len()));
            }
            Ok(Value::Vector(vec))
        }
        Value::Vector(vec) => {
            if vec.is_empty() {
                return Err(anyhow!("vector must have at least 1 dimension"));
            }
            if dim > 0 && vec.len() != dim as usize {
                return Err(anyhow!("expected {} dimensions, not {}", dim, vec.len()));
            }
            Ok(Value::Vector(vec))
        }
        v => cast_catchall(v, target, CastContext::Explicit),
    }
}

fn cast_to_regclass(val: Value) -> Result<Value> {
    match val {
        // db9 stores catalog OIDs as Int64. For psql/JDBC compatibility queries
        // (e.g. d.classoid = 'pg_class'::regclass), accept numeric text,
        // well-known catalog table names, and integer values; normalize to Int64.
        Value::Text(s) => {
            let trimmed = s.trim();
            // Try numeric OID first.
            if let Ok(n) = trimmed.parse::<i64>() {
                return Ok(Value::Int64(n));
            }
            // Try well-known catalog relation names (with optional pg_catalog. prefix).
            // Prefix handling must be case-insensitive (PG_CATALOG.pg_class is valid).
            let normalized = trimmed.to_lowercase();
            let name = normalized
                .strip_prefix("pg_catalog.")
                .unwrap_or(normalized.as_str());
            if let Some(oid) = crate::sql::catalog_oids::pg_catalog_relation_oid(name) {
                return Ok(Value::Int64(oid));
            }
            Err(SqlError::InvalidInputSyntax {
                type_name: "regclass".into(),
                value: s,
            }
            .into())
        }
        Value::Int32(n) => Ok(Value::Int64(n as i64)),
        Value::Int64(n) => Ok(Value::Int64(n)),
        v => cast_catchall(
            v,
            &DataType::UserDefined("regclass".to_string()),
            CastContext::Explicit,
        ),
    }
}

fn cast_to_regtype(val: Value) -> Result<Value> {
    match val {
        // Implements minimal ::regtype::text — strip schema qualification and
        // map short PostgreSQL aliases to their canonical display names.
        Value::Text(s) => Ok(Value::Text(normalize_regtype(&s))),
        v => cast_catchall(
            v,
            &DataType::UserDefined("regtype".to_string()),
            CastContext::Explicit,
        ),
    }
}

fn cast_catchall(val: Value, target: &DataType, context: CastContext) -> Result<Value> {
    match context {
        CastContext::Explicit => Ok(val),
        CastContext::Assignment | CastContext::Implicit => {
            if value_is_compatible_with_column_type(&val, target) {
                Ok(val)
            } else {
                Err(SqlError::InvalidCast {
                    from: val.type_display_name(),
                    to: target.clone(),
                }
                .into())
            }
        }
    }
}

/// Coerce a Text value to a numeric type for arithmetic operations.
///
/// Tries Int64 first, then Float64. Non-text values pass through unchanged.
pub(crate) fn coerce_text_to_numeric(v: Value) -> Result<Value> {
    match &v {
        Value::Text(_) => {
            if let Ok(r) = cast(v.clone(), &DataType::Int64, CastContext::Implicit) {
                return Ok(r);
            }
            cast(v, &DataType::Float64, CastContext::Implicit)
        }
        _ => Ok(v),
    }
}

/// Normalize a PostgreSQL type name the way `::regtype::text` does:
/// strip double-quote delimiters, drop schema qualification, and map
/// internal short aliases to their canonical SQL display names.
fn normalize_regtype(s: &str) -> String {
    crate::sql::pg_types::canonical_regtype_name(s)
}

/// Coerce a value to match the expected column type
pub(crate) fn coerce_value_for_column(val: Value, col: &ColumnDef) -> Result<Value> {
    cast(val, &col.data_type, super::CastContext::Assignment)
}

/// Parse a PostgreSQL array literal string into a Vec<Value>
pub(crate) fn parse_pg_array(s: &str) -> Result<Vec<Value>> {
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return Err(anyhow!("Invalid array format"));
    }

    let inner = &s[1..s.len() - 1];
    if inner.is_empty() {
        return Ok(Vec::new());
    }

    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quoted_element = false;
    let mut escape_next = false;

    for c in inner.chars() {
        if escape_next {
            current.push(c);
            escape_next = false;
            continue;
        }

        match c {
            '\\' => escape_next = true,
            '"' => {
                if !in_quotes && current.trim().is_empty() {
                    quoted_element = true;
                }
                in_quotes = !in_quotes;
            }
            ',' if !in_quotes => {
                let val = parse_array_element(&current, quoted_element);
                result.push(val);
                current.clear();
                quoted_element = false;
            }
            _ => current.push(c),
        }
    }

    if !current.is_empty() || inner.ends_with(',') {
        let val = parse_array_element(&current, quoted_element);
        result.push(val);
    }

    Ok(result)
}

/// Parse a single array element string into a Value
fn parse_array_element(s: &str, quoted: bool) -> Value {
    let s = s.trim();
    if !quoted && s.eq_ignore_ascii_case("NULL") {
        return Value::Null;
    }

    if let Ok(i) = s.parse::<i32>() {
        return Value::Int32(i);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Value::Int64(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return Value::Float64(f);
    }
    if s.eq_ignore_ascii_case("true") {
        return Value::Boolean(true);
    }
    if s.eq_ignore_ascii_case("false") {
        return Value::Boolean(false);
    }

    Value::Text(s.to_string())
}

/// Infer the DataType from a Value
pub(crate) fn infer_data_type(value: &Value) -> DataType {
    match value {
        Value::Int32(_) => DataType::Int32,
        Value::Int64(_) => DataType::Int64,
        Value::Float64(_) => DataType::Float64,
        Value::Boolean(_) => DataType::Boolean,
        Value::Text(_) => DataType::Text,
        Value::Bytes(_) => DataType::Bytes,
        Value::Timestamp(_) => DataType::Timestamp,
        Value::Interval { .. } => DataType::Interval,
        Value::Time(_) => DataType::Time,
        Value::Date(_) => DataType::Date,
        Value::Uuid(_) => DataType::Uuid,
        Value::Vector(vec) => DataType::Vector(vec.len() as u32),
        Value::Json(_) => DataType::Json,
        Value::Jsonb(_) => DataType::Jsonb,
        Value::Array(_) => DataType::Text,
        Value::Null => DataType::Text,
        Value::Numeric(_) => DataType::Numeric {
            precision: None,
            scale: None,
        },
        Value::Tsvector(_) => DataType::Tsvector,
        Value::Tsquery(_) => DataType::Tsquery,
    }
}

/// Parse a time string (HH:MM or HH:MM:SS or HH:MM:SS.ffffff) into microseconds since midnight
pub(crate) fn parse_time_string(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }

    let hours: i64 = parts[0].parse().ok()?;
    let minutes: i64 = parts[1].parse().ok()?;

    let (seconds, micros) = if parts.len() == 3 {
        if let Some(dot_pos) = parts[2].find('.') {
            let secs: i64 = parts[2][..dot_pos].parse().ok()?;
            let frac_str = &parts[2][dot_pos + 1..];
            let padded = format!("{:0<6}", frac_str);
            let micros: i64 = padded[..6].parse().ok()?;
            (secs, micros)
        } else {
            let secs: i64 = parts[2].parse().ok()?;
            (secs, 0)
        }
    } else {
        (0, 0)
    };

    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) || !(0..=59).contains(&seconds) {
        return None;
    }

    Some(hours * 3_600_000_000 + minutes * 60_000_000 + seconds * 1_000_000 + micros)
}

/// Convert an internal `Value` to a SQL AST `Expr` for re-parsing
pub(crate) fn value_to_sql_expr(v: &Value) -> Expr {
    use sqlparser::ast::Value as SqlValue;
    match v {
        Value::Null => Expr::Value(SqlValue::Null),
        Value::Boolean(b) => Expr::Value(SqlValue::Boolean(*b)),
        Value::Int32(i) => Expr::Value(SqlValue::Number(i.to_string(), false)),
        Value::Int64(i) => Expr::Value(SqlValue::Number(i.to_string(), false)),
        Value::Float64(f) => Expr::Value(SqlValue::Number(format!("{:E}", f), false)),
        Value::Text(s) => Expr::Value(SqlValue::SingleQuotedString(s.clone())),
        Value::Bytes(b) => Expr::Value(SqlValue::SingleQuotedString(format!(
            "\\x{}",
            hex::encode(b)
        ))),
        Value::Timestamp(ts) => {
            let seconds = ts.div_euclid(1000);
            let millis = ts.rem_euclid(1000) as u32;
            let nanos = millis * 1_000_000;
            if chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, nanos).is_some() {
                let formatted = crate::model::timestamp::format_timestamp_millis(*ts, false)
                    .unwrap_or_else(|_| ts.to_string());
                Expr::TypedString {
                    data_type: sqlparser::ast::DataType::Timestamp(
                        None,
                        sqlparser::ast::TimezoneInfo::None,
                    ),
                    value: formatted,
                }
            } else {
                Expr::Value(SqlValue::Number(ts.to_string(), false))
            }
        }
        Value::Interval(iv) => {
            let mut parts = Vec::new();
            if iv.months != 0 {
                parts.push(format!("{} month", iv.months));
            }
            if iv.millis != 0 || parts.is_empty() {
                parts.push(format!("{} millisecond", iv.millis));
            }
            Expr::TypedString {
                data_type: sqlparser::ast::DataType::Interval,
                value: parts.join(" "),
            }
        }
        Value::Uuid(bytes) => {
            let uuid = uuid::Uuid::from_bytes(*bytes);
            Expr::Value(SqlValue::SingleQuotedString(uuid.to_string()))
        }
        Value::Array(elems) => {
            let elem_exprs: Vec<Expr> = elems.iter().map(value_to_sql_expr).collect();
            Expr::Array(sqlparser::ast::Array {
                elem: elem_exprs,
                named: true,
            })
        }
        Value::Vector(vec) => Expr::Value(SqlValue::SingleQuotedString(
            crate::model::format_vector_pg_text(vec),
        )),
        Value::Json(s) => Expr::Value(SqlValue::SingleQuotedString(s.clone())),
        Value::Jsonb(s) => Expr::Value(SqlValue::SingleQuotedString(s.clone())),
        Value::Date(days) => match crate::model::date::format_date_days(*days) {
            Ok(s) => Expr::TypedString {
                data_type: sqlparser::ast::DataType::Date,
                value: s,
            },
            Err(_) => Expr::Value(SqlValue::SingleQuotedString(days.to_string())),
        },
        Value::Time(micros) => {
            let total_secs = micros / 1_000_000;
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            Expr::Value(SqlValue::SingleQuotedString(format!(
                "{:02}:{:02}:{:02}",
                hours, mins, secs
            )))
        }
        Value::Numeric(d) => Expr::Value(SqlValue::Number(d.to_string(), false)),
        Value::Tsvector(s) | Value::Tsquery(s) => {
            Expr::Value(SqlValue::SingleQuotedString(s.clone()))
        }
    }
}

/// Parse a text string into a typed `Value` based on the target `DataType`.
///
/// This is the generic "text input function" dispatcher: given a plain (already
/// unescaped) string and a target type, it returns the corresponding typed
/// `Value`.  Used by COPY FROM parsing (after COPY-specific unescaping) and
/// potentially other text-to-Value conversion paths.
pub(crate) fn parse_typed_value(val: &str, data_type: &DataType) -> Result<Value> {
    let trimmed = val.trim();

    match data_type {
        DataType::Boolean => match trimmed.to_lowercase().as_str() {
            "t" | "true" | "1" | "yes" | "on" => Ok(Value::Boolean(true)),
            "f" | "false" | "0" | "no" | "off" => Ok(Value::Boolean(false)),
            _ => Err(SqlError::InvalidInputSyntax {
                type_name: "boolean".into(),
                value: val.to_string(),
            }
            .into()),
        },
        DataType::Int32 => trimmed.parse::<i32>().map(Value::Int32).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "integer".into(),
                value: val.to_string(),
            })
        }),
        DataType::Int64 => trimmed.parse::<i64>().map(Value::Int64).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "bigint".into(),
                value: val.to_string(),
            })
        }),
        DataType::Float64 => trimmed.parse::<f64>().map(Value::Float64).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "double precision".into(),
                value: val.to_string(),
            })
        }),
        DataType::Timestamp => crate::sql::expr::parse_timestamp_string(trimmed).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "timestamp".into(),
                value: val.to_string(),
            })
        }),
        DataType::TimestampTz => crate::sql::expr::parse_timestamp_string(trimmed).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "timestamp with time zone".into(),
                value: val.to_string(),
            })
        }),
        DataType::Date => crate::model::date::parse_date_days(trimmed)
            .map(Value::Date)
            .map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "date".into(),
                    value: val.to_string(),
                })
            }),
        DataType::Uuid => uuid::Uuid::parse_str(trimmed)
            .map(|u| Value::Uuid(*u.as_bytes()))
            .map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "uuid".into(),
                    value: val.to_string(),
                })
            }),
        DataType::Bytes => {
            // Accept both `\x` (standard PG hex literal) and `\\x` (COPY-escaped)
            // prefixes for bytea hex input.
            if let Some(hex_str) = val.strip_prefix("\\x") {
                Ok(hex::decode(hex_str)
                    .map(Value::Bytes)
                    .unwrap_or(Value::Bytes(val.as_bytes().to_vec())))
            } else {
                Ok(Value::Bytes(val.as_bytes().to_vec()))
            }
        }
        DataType::Time => parse_time_string(trimmed).map(Value::Time).ok_or_else(|| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "time".into(),
                value: val.to_string(),
            })
        }),
        DataType::Interval => crate::sql::expr::parse_interval_string(trimmed).map_err(|_| {
            anyhow::Error::from(SqlError::InvalidInputSyntax {
                type_name: "interval".into(),
                value: val.to_string(),
            })
        }),
        DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::UserDefined(_) => {
            Ok(Value::Text(val.to_string()))
        }
        DataType::Array(elem_type) => {
            let elements = parse_pg_array(trimmed).map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "array".into(),
                    value: val.to_string(),
                })
            })?;
            // Cast each element to the declared element type so that e.g.
            // UUID strings become Value::Uuid, not Value::Text.
            let mut typed = Vec::with_capacity(elements.len());
            for v in elements {
                if v == Value::Null {
                    typed.push(Value::Null);
                } else {
                    typed.push(cast(v, elem_type, CastContext::Assignment)?);
                }
            }
            Ok(Value::Array(typed))
        }
        DataType::Json => {
            serde_json::from_str::<serde_json::Value>(val).map_err(|e| {
                SqlError::InvalidInputSyntax {
                    type_name: "json".into(),
                    value: e.to_string(),
                }
            })?;
            Ok(Value::Json(val.to_string()))
        }
        DataType::Jsonb => {
            let parsed: serde_json::Value =
                serde_json::from_str(val).map_err(|e| SqlError::InvalidInputSyntax {
                    type_name: "jsonb".into(),
                    value: e.to_string(),
                })?;
            Ok(Value::Jsonb(parsed.to_string()))
        }
        DataType::Vector(_) => {
            if val.starts_with('[') && val.ends_with(']') {
                let inner = &val[1..val.len() - 1];
                let elements: std::result::Result<Vec<f64>, _> =
                    inner.split(',').map(|s| s.trim().parse::<f64>()).collect();
                elements.map(Value::Vector).map_err(|_| {
                    anyhow::Error::from(SqlError::InvalidInputSyntax {
                        type_name: "vector".into(),
                        value: val.to_string(),
                    })
                })
            } else {
                Err(SqlError::InvalidInputSyntax {
                    type_name: "vector".into(),
                    value: val.to_string(),
                }
                .into())
            }
        }
        DataType::Numeric { scale, .. } => {
            let mut d = Decimal::from_str(trimmed).map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "numeric".into(),
                    value: val.to_string(),
                })
            })?;
            if let Some(s) = scale {
                d.rescale((*s).min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }
        DataType::Tsvector => Ok(Value::Tsvector(val.to_string())),
        DataType::Tsquery => {
            crate::sql::fts::validate_tsquery_syntax(val)?;
            Ok(Value::Tsquery(val.to_string()))
        }
        DataType::Unknown => {
            unreachable!("DataType::Unknown must be resolved before reaching text parsing")
        }
    }
}

#[cfg(test)]
mod tests;
