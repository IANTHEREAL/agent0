//! Unified cast function with context-dependent rules.
//!
//! `CastContext` controls which conversions are allowed:
//! - `Explicit`: CAST(x AS type) — most permissive
//! - `Assignment`: INSERT/UPDATE column coercion — medium
//! - `Implicit`: Comparison coercion — strictest

use super::coercion::comparison_target_type;
use crate::sql::error::SqlError;
use crate::types::{DataType, Value};
use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use std::str::FromStr;

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
        (Value::Text(_), DataType::Text | DataType::Name | DataType::UserDefined(_)) => true,
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
        // ===== To Text / Name =====
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
        (Value::Int64(n), DataType::Int32) => i32::try_from(n)
            .map(Value::Int32)
            .map_err(|_| anyhow!("integer out of range: {}", n)),
        (Value::Float64(f), DataType::Int32) => match context {
            CastContext::Explicit => {
                let rounded = round_half_away_from_zero(f);
                if f.is_nan() || rounded < (i32::MIN as f64) || rounded > (i32::MAX as f64) {
                    return Err(anyhow!("integer out of range"));
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
                i32::try_from(n)
                    .map(Value::Int32)
                    .map_err(|_| anyhow!("integer out of range: {}", n))
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
                        .ok_or_else(|| anyhow!("numeric value out of range for integer"))
                }
                CastContext::Assignment | CastContext::Implicit => d
                    .to_i32()
                    .map(Value::Int32)
                    .ok_or_else(|| anyhow!("numeric value out of range for integer")),
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
                return Err(anyhow!("bigint out of range"));
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
                        .ok_or_else(|| anyhow!("numeric value out of range for bigint"))
                }
                CastContext::Assignment | CastContext::Implicit => d
                    .to_i64()
                    .map(Value::Int64)
                    .ok_or_else(|| anyhow!("numeric value out of range for bigint")),
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
                CastContext::Explicit => d
                    .to_f64()
                    .map(Value::Float64)
                    .ok_or_else(|| anyhow!("numeric value out of range for double precision")),
                CastContext::Assignment | CastContext::Implicit => {
                    Ok(Value::Float64(d.to_f64().unwrap_or(f64::NAN)))
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
            crate::types::date::parse_date_days(&s).map(Value::Date)
        }
        (Value::Timestamp(ts), DataType::Date) => {
            crate::types::date::timestamp_millis_to_date_days(ts).map(Value::Date)
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
        (Value::Date(days), DataType::Timestamp) => {
            crate::types::date::date_days_to_timestamp_millis(days).map(Value::Timestamp)
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
        (Value::Jsonb(s), DataType::Json) => Ok(Value::Json(s)),
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

        // ===== Numeric =====
        (Value::Text(s), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::from_str(s.trim()).map_err(|_| SqlError::InvalidInputSyntax {
                type_name: "numeric".into(),
                value: s,
            })?;
            if let Some(s) = scale {
                d.rescale(*s);
            }
            Ok(Value::Numeric(d))
        }
        (Value::Int32(n), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::from(n);
            if let Some(s) = scale {
                d.rescale(*s);
            }
            Ok(Value::Numeric(d))
        }
        (Value::Int64(n), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::from(n);
            if let Some(s) = scale {
                d.rescale(*s);
            }
            Ok(Value::Numeric(d))
        }
        (Value::Float64(f), DataType::Numeric { scale, .. }) => {
            let mut d = Decimal::try_from(f)
                .map_err(|_| anyhow!("invalid input for type numeric: \"{}\"", f))?;
            if let Some(s) = scale {
                d.rescale(*s);
            }
            Ok(Value::Numeric(d))
        }
        (Value::Numeric(d), DataType::Numeric { scale, .. }) => {
            let mut d = d;
            if let Some(s) = scale {
                d.rescale(*s);
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
            let trimmed = s.trim();
            if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
                return Err(SqlError::InvalidInputSyntax {
                    type_name: "vector".into(),
                    value: s,
                }
                .into());
            }
            let inner = &trimmed[1..trimmed.len() - 1];
            let elements: std::result::Result<Vec<f64>, _> = if inner.trim().is_empty() {
                Ok(Vec::new())
            } else {
                inner.split(',').map(|e| e.trim().parse::<f64>()).collect()
            };
            let vec = elements.map_err(|_| {
                anyhow::Error::from(SqlError::InvalidInputSyntax {
                    type_name: "vector".into(),
                    value: trimmed.to_string(),
                })
            })?;
            // dim == 0 means "any dimension" (bare `vector` without modifier).
            if *dim > 0 && vec.len() != *dim as usize {
                return Err(anyhow!(
                    "vector has wrong dimensions: expected {}, got {}",
                    dim,
                    vec.len()
                ));
            }
            Ok(Value::Vector(vec))
        }
        (Value::Vector(vec), DataType::Vector(dim)) => {
            if *dim > 0 && vec.len() != *dim as usize {
                return Err(anyhow!(
                    "vector has wrong dimensions: expected {}, got {}",
                    dim,
                    vec.len()
                ));
            }
            Ok(Value::Vector(vec))
        }

        // ===== regtype pseudo-type =====
        // Implements minimal ::regtype::text — strip schema qualification and
        // map short PostgreSQL aliases to their canonical display names.
        (Value::Text(s), DataType::UserDefined(ref udt)) if udt.eq_ignore_ascii_case("regtype") => {
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

/// Coerce two values to a common type for comparison.
///
/// Same-type and Null pairs are returned as-is (no cloning needed at call site
/// for same-type fast path). Cross-type pairs are cast via `CastContext::Implicit`.
pub(crate) fn coerce_pair(left: Value, right: Value) -> Result<(Value, Value)> {
    let lt = left.data_type();
    let rt = right.data_type();

    // Null or same type → return as-is
    if lt.is_none() || rt.is_none() || lt == rt {
        return Ok((left, right));
    }

    let lt = lt.unwrap();
    let rt = rt.unwrap();

    let target = comparison_target_type(&lt, &rt).ok_or_else(|| {
        anyhow!(
            "could not determine comparison type for {:?} and {:?}",
            lt,
            rt
        )
    })?;

    let cl = cast(left, &target, CastContext::Implicit)?;
    let cr = cast(right, &target, CastContext::Implicit)?;
    Ok((cl, cr))
}

/// Coerce a value to boolean via implicit cast.
///
/// Text values are parsed as booleans; non-text values pass through unchanged.
/// Replaces the old `coerce_text_literal_to_bool` which had a literal-only guard.
pub(crate) fn coerce_to_bool(val: Value) -> Result<Value> {
    match val {
        Value::Text(s) => cast(Value::Text(s), &DataType::Boolean, CastContext::Implicit),
        other => Ok(other),
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
    // 1. Remove double quotes: `"pg_catalog"."int4"` → `pg_catalog.int4`
    let stripped = s.replace('"', "");
    // 2. Take last dot-separated component: `pg_catalog.int4` → `int4`
    let name = stripped.rsplit('.').next().unwrap_or(&stripped);
    // 3. Map short aliases to display names (matches real PostgreSQL behavior)
    match name.to_lowercase().as_str() {
        "int2" | "smallint" => "smallint".to_string(),
        "int4" | "integer" | "int" | "serial" => "integer".to_string(),
        "int8" | "bigint" | "bigserial" => "bigint".to_string(),
        "float4" | "real" => "real".to_string(),
        "float8" => "double precision".to_string(),
        "bool" => "boolean".to_string(),
        "varchar" => "character varying".to_string(),
        "timestamp" => "timestamp without time zone".to_string(),
        "timestamptz" => "timestamp with time zone".to_string(),
        "time" | "timetz" => "time without time zone".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    // ---- Float64 → Int32 ----
    #[test]
    fn explicit_float_to_int32_rounds() {
        let r = cast(Value::Float64(2.7), &DataType::Int32, CastContext::Explicit).unwrap();
        assert_eq!(r, Value::Int32(3));
    }

    #[test]
    fn assignment_float_to_int32_rejects_fraction() {
        let r = cast(
            Value::Float64(2.7),
            &DataType::Int32,
            CastContext::Assignment,
        );
        assert!(r.is_err());
    }

    #[test]
    fn assignment_float_to_int32_accepts_whole() {
        let r = cast(
            Value::Float64(2.0),
            &DataType::Int32,
            CastContext::Assignment,
        )
        .unwrap();
        assert_eq!(r, Value::Int32(2));
    }

    // ---- Float64 → Int64 ----
    #[test]
    fn explicit_float_to_int64_rounds() {
        let r = cast(Value::Float64(2.7), &DataType::Int64, CastContext::Explicit).unwrap();
        assert_eq!(r, Value::Int64(3));
    }

    #[test]
    fn assignment_float_to_int64_falls_through() {
        let r = cast(
            Value::Float64(2.0),
            &DataType::Int64,
            CastContext::Assignment,
        );
        assert!(r.is_err());
    }

    // ---- Bool → Int32 ----
    #[test]
    fn explicit_bool_to_int32() {
        assert_eq!(
            cast(
                Value::Boolean(true),
                &DataType::Int32,
                CastContext::Explicit
            )
            .unwrap(),
            Value::Int32(1)
        );
        assert_eq!(
            cast(
                Value::Boolean(false),
                &DataType::Int32,
                CastContext::Explicit
            )
            .unwrap(),
            Value::Int32(0)
        );
    }

    #[test]
    fn assignment_bool_to_int32_rejected() {
        let r = cast(
            Value::Boolean(true),
            &DataType::Int32,
            CastContext::Assignment,
        );
        assert!(r.is_err());
    }

    // ---- Int → Bool ----
    #[test]
    fn explicit_int_to_bool() {
        assert_eq!(
            cast(Value::Int32(0), &DataType::Boolean, CastContext::Explicit).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            cast(Value::Int32(42), &DataType::Boolean, CastContext::Explicit).unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn assignment_int_to_bool_rejected() {
        let r = cast(Value::Int32(1), &DataType::Boolean, CastContext::Assignment);
        assert!(r.is_err());
    }

    // ---- Numeric → Int32 ----
    #[test]
    fn explicit_numeric_to_int32_rounds() {
        let d = Decimal::new(27, 1); // 2.7
        let r = cast(Value::Numeric(d), &DataType::Int32, CastContext::Explicit).unwrap();
        assert_eq!(r, Value::Int32(3));
    }

    #[test]
    fn assignment_numeric_to_int32_no_rounding() {
        // Assignment uses to_i32() directly which truncates toward zero
        let d = Decimal::new(27, 1); // 2.7 → truncates to 2
        let r = cast(Value::Numeric(d), &DataType::Int32, CastContext::Assignment).unwrap();
        assert_eq!(r, Value::Int32(2));

        let d2 = Decimal::new(20, 1); // 2.0
        let r2 = cast(
            Value::Numeric(d2),
            &DataType::Int32,
            CastContext::Assignment,
        )
        .unwrap();
        assert_eq!(r2, Value::Int32(2));
    }

    // ---- Numeric → Float64 ----
    #[test]
    fn explicit_numeric_to_float() {
        let d = Decimal::new(275, 2); // 2.75
        let r = cast(Value::Numeric(d), &DataType::Float64, CastContext::Explicit).unwrap();
        assert_eq!(r, Value::Float64(2.75));
    }

    #[test]
    fn assignment_numeric_to_float() {
        let d = Decimal::new(275, 2); // 2.75
        let r = cast(
            Value::Numeric(d),
            &DataType::Float64,
            CastContext::Assignment,
        )
        .unwrap();
        assert_eq!(r, Value::Float64(2.75));
    }

    // ---- Unknown target: pass-through vs error ----
    #[test]
    fn explicit_unknown_target_passes_through() {
        let r = cast(
            Value::Boolean(true),
            &DataType::Tsvector,
            CastContext::Explicit,
        )
        .unwrap();
        assert_eq!(r, Value::Boolean(true));
    }

    #[test]
    fn assignment_unknown_target_errors() {
        let r = cast(
            Value::Boolean(true),
            &DataType::Tsvector,
            CastContext::Assignment,
        );
        assert!(r.is_err());
    }

    // ---- Array: works in both contexts ----
    #[test]
    fn array_cast_both_contexts() {
        let arr = Value::Text("{1,2,3}".into());
        let target = DataType::Array(Box::new(DataType::Int32));

        let explicit = cast(arr.clone(), &target, CastContext::Explicit).unwrap();
        let assignment = cast(arr, &target, CastContext::Assignment).unwrap();

        let expected = Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]);
        assert_eq!(explicit, expected);
        assert_eq!(assignment, expected);
    }

    // ---- Vector: works in both contexts ----
    #[test]
    fn vector_cast_both_contexts() {
        let vec_val = Value::Text("[1.0,2.0,3.0]".into());
        let target = DataType::Vector(3);

        let explicit = cast(vec_val.clone(), &target, CastContext::Explicit).unwrap();
        let assignment = cast(vec_val, &target, CastContext::Assignment).unwrap();

        let expected = Value::Vector(vec![1.0, 2.0, 3.0]);
        assert_eq!(explicit, expected);
        assert_eq!(assignment, expected);
    }

    // ---- Null ----
    #[test]
    fn null_cast_any_context() {
        assert_eq!(
            cast(Value::Null, &DataType::Int32, CastContext::Explicit).unwrap(),
            Value::Null
        );
        assert_eq!(
            cast(Value::Null, &DataType::Int32, CastContext::Assignment).unwrap(),
            Value::Null
        );
    }

    // ---- Text conversions ----
    #[test]
    fn text_to_bool_both_contexts() {
        assert_eq!(
            cast(
                Value::Text("true".into()),
                &DataType::Boolean,
                CastContext::Explicit
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            cast(
                Value::Text("false".into()),
                &DataType::Boolean,
                CastContext::Assignment
            )
            .unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn text_to_bool_on_off() {
        assert_eq!(
            cast(
                Value::Text("on".into()),
                &DataType::Boolean,
                CastContext::Implicit
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            cast(
                Value::Text("OFF".into()),
                &DataType::Boolean,
                CastContext::Implicit
            )
            .unwrap(),
            Value::Boolean(false)
        );
    }

    // ---- coerce_pair ----
    #[test]
    fn coerce_pair_text_to_int32() {
        let (l, r) = coerce_pair(Value::Text("42".into()), Value::Int32(10)).unwrap();
        assert_eq!(l, Value::Int32(42));
        assert_eq!(r, Value::Int32(10));
    }

    #[test]
    fn coerce_pair_text_to_bool() {
        let (l, r) = coerce_pair(Value::Text("true".into()), Value::Boolean(false)).unwrap();
        assert_eq!(l, Value::Boolean(true));
        assert_eq!(r, Value::Boolean(false));
    }

    #[test]
    fn coerce_pair_text_to_date() {
        let (l, r) = coerce_pair(Value::Text("2024-01-01".into()), Value::Date(19723)).unwrap();
        // Both should be Date
        assert!(matches!(l, Value::Date(_)));
        assert_eq!(r, Value::Date(19723));
    }

    #[test]
    fn coerce_pair_text_parse_error() {
        let result = coerce_pair(Value::Text("abc".into()), Value::Int32(1));
        assert!(result.is_err());
    }

    #[test]
    fn coerce_pair_same_type_passthrough() {
        let (l, r) = coerce_pair(Value::Int32(1), Value::Int32(2)).unwrap();
        assert_eq!(l, Value::Int32(1));
        assert_eq!(r, Value::Int32(2));
    }

    #[test]
    fn coerce_pair_null_passthrough() {
        let (l, r) = coerce_pair(Value::Null, Value::Int32(42)).unwrap();
        assert_eq!(l, Value::Null);
        assert_eq!(r, Value::Int32(42));
    }

    #[test]
    fn coerce_pair_int32_int64_widening() {
        let (l, r) = coerce_pair(Value::Int32(1), Value::Int64(2)).unwrap();
        assert_eq!(l, Value::Int64(1));
        assert_eq!(r, Value::Int64(2));
    }

    #[test]
    fn coerce_pair_json_incomparable() {
        let result = coerce_pair(Value::Json("{}".into()), Value::Int32(1));
        assert!(result.is_err());
    }

    // ---- coerce_text_to_numeric ----
    #[test]
    fn coerce_text_to_numeric_int() {
        let v = coerce_text_to_numeric(Value::Text("42".into())).unwrap();
        assert_eq!(v, Value::Int64(42));
    }

    #[test]
    fn coerce_text_to_numeric_float() {
        let v = coerce_text_to_numeric(Value::Text("3.14".into())).unwrap();
        #[allow(clippy::approx_constant)]
        let expected = Value::Float64(3.14);
        assert_eq!(v, expected);
    }

    #[test]
    fn coerce_text_to_numeric_non_numeric_error() {
        let result = coerce_text_to_numeric(Value::Text("abc".into()));
        assert!(result.is_err());
    }

    #[test]
    fn coerce_text_to_numeric_passthrough_non_text() {
        let v = coerce_text_to_numeric(Value::Int32(42)).unwrap();
        assert_eq!(v, Value::Int32(42));
    }
}
