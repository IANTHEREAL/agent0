//! Unified cast function with context-dependent rules.
//!
//! `CastContext` controls which conversions are allowed:
//! - `Explicit`: CAST(x AS type) — most permissive
//! - `Assignment`: INSERT/UPDATE column coercion — medium
//! - `Implicit`: Comparison coercion — strictest

use crate::model::{DataType, Value};
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use std::borrow::Cow;
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

    match (val, target) {
        // ===== To Varchar(n) =====
        (v, DataType::Varchar(max_len)) => {
            let s = v.to_string();
            if *max_len == 0 {
                // PostgreSQL bare VARCHAR has no typmod limit. We preserve the
                // type identity as Varchar(0) in metadata, but runtime coercion
                // must behave like unbounded character varying.
                return Ok(Value::Text(s));
            }
            match context {
                CastContext::Explicit => {
                    let truncated: String = s.chars().take(*max_len as usize).collect();
                    Ok(Value::Text(truncated))
                }
                CastContext::Assignment => {
                    // Postgres: error if value exceeds length (unless excess is all spaces)
                    let trimmed = s.trim_end();
                    if trimmed.chars().count() > *max_len as usize {
                        Err(SqlError::StringDataRightTruncation {
                            max_length: *max_len,
                        }
                        .into())
                    } else {
                        Ok(Value::Text(s.chars().take(*max_len as usize).collect()))
                    }
                }
                CastContext::Implicit => Ok(Value::Text(s)),
            }
        }

        // ===== To Text / Name =====
        (Value::Timestamp(ts), DataType::Text | DataType::Name) => {
            let formatted = crate::model::timestamp::format_timestamp_millis(ts, false)
                .unwrap_or_else(|_| ts.to_string());
            Ok(Value::Text(formatted))
        }
        (Value::Jsonb(s), DataType::Text | DataType::Name) => {
            Ok(Value::Text(crate::sql::jsonb::format_jsonb_pg_str(&s)))
        }
        (v, DataType::Text | DataType::Name) => Ok(Value::Text(v.to_string())),

        // ===== To Boolean =====
        (Value::Text(s), DataType::Boolean) => match s.trim().to_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => Ok(Value::Boolean(true)),
            "false" | "f" | "no" | "n" | "off" | "0" => Ok(Value::Boolean(false)),
            _ => Err(SqlError::InvalidInputSyntax {
                type_name: "boolean".into(),
                value: s,
            }
            .into()),
        },
        // Int/Float/Numeric → Bool: Explicit only
        (Value::Int32(n), DataType::Boolean) if context == CastContext::Explicit => {
            Ok(Value::Boolean(n != 0))
        }
        (Value::Int64(n), DataType::Boolean) if context == CastContext::Explicit => {
            Ok(Value::Boolean(n != 0))
        }
        (Value::Float64(n), DataType::Boolean) if context == CastContext::Explicit => {
            Ok(Value::Boolean(n != 0.0))
        }
        (Value::Numeric(d), DataType::Boolean) if context == CastContext::Explicit => {
            Ok(Value::Boolean(!d.is_zero()))
        }

        // ===== To Int32 =====
        (Value::Text(s), DataType::Int32) => {
            s.trim().parse::<i32>().map(Value::Int32).map_err(|_| {
                SqlError::InvalidInputSyntax {
                    type_name: "integer".into(),
                    value: s,
                }
                .into()
            })
        }
        (Value::Int64(n), DataType::Int32) => i32::try_from(n).map(Value::Int32).map_err(|_| {
            SqlError::NumericValueOutOfRange {
                message: "integer out of range".into(),
            }
            .into()
        }),
        (Value::Float64(f), DataType::Int32) => match context {
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
        (Value::Numeric(d), DataType::Int32) => {
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
        (Value::Boolean(b), DataType::Int32) if context == CastContext::Explicit => {
            Ok(Value::Int32(if b { 1 } else { 0 }))
        }

        // ===== To Int64 =====
        (Value::Text(s), DataType::Int64) => {
            s.trim().parse::<i64>().map(Value::Int64).map_err(|_| {
                SqlError::InvalidInputSyntax {
                    type_name: "bigint".into(),
                    value: s,
                }
                .into()
            })
        }
        (Value::Int32(n), DataType::Int64) => Ok(Value::Int64(n as i64)),
        // Float64 → Int64: Explicit only
        (Value::Float64(n), DataType::Int64) if context == CastContext::Explicit => {
            let rounded = round_half_away_from_zero(n);
            if n.is_nan() || rounded < (i64::MIN as f64) || rounded > (i64::MAX as f64) {
                return Err(SqlError::NumericValueOutOfRange {
                    message: "bigint out of range".into(),
                }
                .into());
            }
            Ok(Value::Int64(rounded as i64))
        }
        (Value::Numeric(d), DataType::Int64) => {
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

        // ===== To Float64 =====
        (Value::Text(s), DataType::Float64) => {
            s.trim().parse::<f64>().map(Value::Float64).map_err(|_| {
                SqlError::InvalidInputSyntax {
                    type_name: "double precision".into(),
                    value: s,
                }
                .into()
            })
        }
        (Value::Int32(n), DataType::Float64) => Ok(Value::Float64(n as f64)),
        (Value::Int64(n), DataType::Float64) => Ok(Value::Float64(n as f64)),
        (Value::Numeric(d), DataType::Float64) => {
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

        // ===== To Bytes =====
        (v, DataType::Bytes) => cast_to_bytea(v),

        // ===== Temporal =====
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
            crate::sql::value_coercion::parse_time_string(trimmed)
                .map(Value::Time)
                .ok_or_else(|| {
                    anyhow::Error::from(SqlError::InvalidInputSyntax {
                        type_name: "time".into(),
                        value: s,
                    })
                })
        }
        (Value::Time(micros), DataType::Time) => Ok(Value::Time(micros)),

        // ===== UUID =====
        (Value::Text(s), DataType::Uuid) => uuid::Uuid::parse_str(s.trim())
            .map(|u| Value::Uuid(*u.as_bytes()))
            .map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "uuid".into(),
                    value: s,
                })
            }),
        (Value::Uuid(bytes), DataType::Uuid) => Ok(Value::Uuid(bytes)),

        // ===== JSON =====
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

        // ===== Full-text search =====
        (Value::Text(s), DataType::Tsquery) => {
            crate::sql::fts::validate_tsquery_syntax(&s)?;
            Ok(Value::Tsquery(s))
        }
        (Value::Tsquery(s), DataType::Tsquery) => Ok(Value::Tsquery(s)),

        // ===== Numeric =====
        (Value::Text(s), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::from_str(s.trim()).map_err(|_| SqlError::InvalidInputSyntax {
                type_name: "numeric".into(),
                value: s,
            })?;
            if let Some(s) = scale {
                d.rescale((*s).min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }
        (Value::Int32(n), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::from(n);
            if let Some(s) = scale {
                d.rescale((*s).min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }
        (Value::Int64(n), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::from(n);
            if let Some(s) = scale {
                d.rescale((*s).min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }
        (Value::Float64(f), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::try_from(f).map_err(|_| SqlError::InvalidInputSyntax {
                type_name: "numeric".into(),
                value: f.to_string(),
            })?;
            if let Some(s) = scale {
                d.rescale((*s).min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }
        (Value::Numeric(d), DataType::Numeric { scale, .. }) => {
            let mut d = d;
            if let Some(s) = scale {
                d.rescale((*s).min(MAX_RUNTIME_NUMERIC_SCALE));
            }
            Ok(Value::Numeric(d))
        }

        // ===== Array =====
        (Value::Text(s), DataType::Array(elem_type)) => {
            let arr = crate::sql::value_coercion::parse_pg_array(&s).map_err(|_| {
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
        (Value::Array(elems), DataType::Array(elem_type)) => {
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

        // ===== Vector =====
        (Value::Text(s), DataType::Vector(dim)) => {
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
            if *dim as usize > MAX_VECTOR_DIMENSIONS {
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
            if *dim > 0 && vec.len() != *dim as usize {
                return Err(anyhow!("expected {} dimensions, not {}", dim, vec.len()));
            }
            Ok(Value::Vector(vec))
        }
        (Value::Vector(vec), DataType::Vector(dim)) => {
            if vec.is_empty() {
                return Err(anyhow!("vector must have at least 1 dimension"));
            }
            if *dim > 0 && vec.len() != *dim as usize {
                return Err(anyhow!("expected {} dimensions, not {}", dim, vec.len()));
            }
            Ok(Value::Vector(vec))
        }

        // ===== regclass pseudo-type =====
        // db9 stores catalog OIDs as Int64. For psql/JDBC compatibility queries
        // (e.g. d.classoid = 'pg_class'::regclass), accept numeric text,
        // well-known catalog table names, and integer values; normalize to Int64.
        // Also handles double-quoted schema-qualified names like "public"."table_name"
        // as generated by TypeORM and other ORMs.
        (Value::Text(s), DataType::UserDefined(ref udt)) if is_regclass_udt(udt) => {
            let trimmed = s.trim();
            // Try numeric OID first.
            if let Ok(n) = trimmed.parse::<i64>() {
                return Ok(Value::Int64(n));
            }
            // Handle double-quoted schema-qualified names: "schema"."table"
            // This format is used by TypeORM and other ORMs.
            let name_to_lookup: Cow<str> = if let Some(trimmed_stripped) = trimmed.strip_prefix('"')
            {
                // Parse "schema"."table" format
                // Find the closing quote of schema, then the dot, then the opening quote of table
                if let Some(first_close) = trimmed_stripped.find('"') {
                    let after_schema = &trimmed[first_close + 1..];
                    let after_schema = after_schema.trim_start();
                    if let Some(after_dot) = after_schema.strip_prefix('.') {
                        let after_dot = after_dot.trim_start();
                        if let Some(after_dot_stripped) = after_dot.strip_prefix('"') {
                            // Extract table name between quotes
                            if let Some(table_close) = after_dot_stripped.find('"') {
                                // Successfully parsed "schema"."table" - use just the table name
                                Cow::Borrowed(&after_dot_stripped[..table_close])
                            } else {
                                Cow::Borrowed(trimmed)
                            }
                        } else {
                            Cow::Borrowed(trimmed)
                        }
                    } else {
                        Cow::Borrowed(trimmed)
                    }
                } else {
                    Cow::Borrowed(trimmed)
                }
            } else {
                Cow::Borrowed(trimmed)
            };
            // Try well-known catalog relation names (with optional pg_catalog. prefix).
            // Prefix handling must be case-insensitive (PG_CATALOG.pg_class is valid).
            let normalized = name_to_lookup.to_lowercase();
            let name = normalized
                .strip_prefix("pg_catalog.")
                .unwrap_or(normalized.as_str());
            if let Some(oid) = crate::sql::catalog_oids::pg_catalog_relation_oid(name) {
                return Ok(Value::Int64(oid));
            }
            // Unknown names error - no synthetic OID generation (scope creep).
            Err(SqlError::InvalidInputSyntax {
                type_name: "regclass".into(),
                value: s,
            }
            .into())
        }
        (Value::Int32(n), DataType::UserDefined(ref udt)) if is_regclass_udt(udt) => {
            Ok(Value::Int64(n as i64))
        }
        (Value::Int64(n), DataType::UserDefined(ref udt)) if is_regclass_udt(udt) => {
            Ok(Value::Int64(n))
        }

        // ===== regtype pseudo-type =====
        // Implements minimal ::regtype::text — strip schema qualification and
        // map short PostgreSQL aliases to their canonical display names.
        (Value::Text(s), DataType::UserDefined(ref udt)) if is_regtype_udt(udt) => {
            Ok(Value::Text(normalize_regtype(&s)))
        }

        // ===== Catch-all =====
        (v, _) => match context {
            CastContext::Explicit => Ok(v),
            CastContext::Assignment | CastContext::Implicit => {
                if value_is_compatible_with_column_type(&v, target) {
                    Ok(v)
                } else {
                    Err(SqlError::InvalidCast {
                        from: v.type_display_name(),
                        to: target.clone(),
                    }
                    .into())
                }
            }
        },
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

#[cfg(test)]
mod tests;
