//! Binary and unary operator evaluation
//!
//! This module contains all operator evaluation logic including:
//! - Comparison operators (=, <>, <, >, <=, >=)
//! - Logical operators (AND, OR)
//! - Arithmetic operators (+, -, *, /, %)
//! - String concatenation (||)
//! - Regex operators (~, ~*, !~, !~*)
//! - JSONB operators (?, @>, <@)
//! - Array operators (&&)

use crate::model::Value;
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use bigdecimal::BigDecimal;
use dashmap::DashMap;
use serde_json::value::RawValue;
use sqlparser::ast::BinaryOperator;
use std::collections::BTreeMap;
use std::str::FromStr;

use super::numeric;

/// Process-wide cache for compiled regexes used by `~`, `~*`, `!~`, `!~*` operators.
/// Bounded: new entries are skipped (not cached) when the map is full.
const MAX_REGEX_CACHE_SIZE: usize = 256;

static REGEX_CACHE: std::sync::LazyLock<DashMap<String, regex::Regex>> =
    std::sync::LazyLock::new(DashMap::new);

fn get_or_compile_regex(pattern: &str) -> Result<regex::Regex> {
    if let Some(re) = REGEX_CACHE.get(pattern) {
        return Ok(re.clone());
    }
    let re = regex::Regex::new(pattern).map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;
    // Soft cap: skip insert when full. Under concurrency, len() is approximate
    // so the cache may transiently exceed MAX_REGEX_CACHE_SIZE — acceptable
    // since it's a memory budget hint, not a hard invariant.
    if REGEX_CACHE.len() < MAX_REGEX_CACHE_SIZE {
        REGEX_CACHE.insert(pattern.to_string(), re.clone());
    }
    Ok(re)
}

use super::helpers::value_to_text;

/// Evaluate PostgreSQL regex operators: `~`, `~*`, `!~`, `!~*`.
fn eval_regex_op(
    left: &Value,
    right: &Value,
    case_insensitive: bool,
    negate: bool,
) -> Result<Value> {
    let text = value_to_text(left);
    let pattern = value_to_text(right);
    let pattern = if case_insensitive {
        format!("(?i){}", pattern)
    } else {
        pattern
    };
    let re = get_or_compile_regex(&pattern)?;
    let matched = re.is_match(&text);
    Ok(Value::Boolean(if negate { !matched } else { matched }))
}

/// Evaluate the JSONB `?` (existence) operator.
fn eval_json_exists(left: Value, right: Value) -> Result<Value> {
    let json_str = match left {
        Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let key = match right {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    Ok(Value::Boolean(super::super::jsonb::exists(&json_val, &key)))
}

/// Sort with fallible comparison. Propagates the first comparison error.
/// After the first error, remaining comparisons short-circuit to Equal
/// and the partially-sorted result is discarded.
///
/// Rust's `slice::sort_by` always terminates even when the comparator
/// returns Equal for error pairs.
pub fn sort_by_fallible<T>(
    items: &mut [T],
    mut cmp: impl FnMut(&T, &T) -> Result<std::cmp::Ordering>,
) -> Result<()> {
    let mut first_error: Option<anyhow::Error> = None;
    items.sort_by(|a, b| {
        if first_error.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match cmp(a, b) {
            Ok(ord) => ord,
            Err(e) => {
                first_error = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Returns true for SQL strict operators where any NULL input produces NULL output.
fn is_strict_op(op: &BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq
            | BinaryOperator::Plus
            | BinaryOperator::Minus
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Modulo
            | BinaryOperator::PGRegexMatch
            | BinaryOperator::PGRegexIMatch
            | BinaryOperator::PGRegexNotMatch
            | BinaryOperator::PGRegexNotIMatch
    )
}

/// Evaluate a binary operator on two values.
pub fn eval_binary_op(left: Value, op: &BinaryOperator, right: Value) -> Result<Value> {
    // SQL strict operators: any NULL input produces NULL output.
    // Excludes: And/Or (three-valued logic), StringConcat (array overloads are non-strict),
    // PGOverlap/@@/? (handled inline or delegated).
    if is_strict_op(op) && (matches!(left, Value::Null) || matches!(right, Value::Null)) {
        return Ok(Value::Null);
    }

    match op {
        // Comparison (NULL already handled above)
        BinaryOperator::Eq => Ok(Value::Boolean(compare_values(&left, &right)? == 0)),
        BinaryOperator::NotEq => Ok(Value::Boolean(compare_values(&left, &right)? != 0)),
        BinaryOperator::Gt => Ok(Value::Boolean(compare_values(&left, &right)? > 0)),
        BinaryOperator::Lt => Ok(Value::Boolean(compare_values(&left, &right)? < 0)),
        BinaryOperator::GtEq => Ok(Value::Boolean(compare_values(&left, &right)? >= 0)),
        BinaryOperator::LtEq => Ok(Value::Boolean(compare_values(&left, &right)? <= 0)),

        // Logical
        // SQL three-valued logic for boolean operators
        // https://www.postgresql.org/docs/current/functions-logical.html
        BinaryOperator::And => match (left, right) {
            (Value::Boolean(false), _) | (_, Value::Boolean(false)) => Ok(Value::Boolean(false)),
            (Value::Boolean(true), Value::Boolean(true)) => Ok(Value::Boolean(true)),
            (Value::Boolean(true), Value::Null) | (Value::Null, Value::Boolean(true)) => {
                Ok(Value::Null)
            }
            (Value::Null, Value::Null) => Ok(Value::Null),
            _ => Err(anyhow!("AND requires boolean operands")),
        },
        BinaryOperator::Or => match (left, right) {
            (Value::Boolean(true), _) | (_, Value::Boolean(true)) => Ok(Value::Boolean(true)),
            (Value::Boolean(false), Value::Boolean(false)) => Ok(Value::Boolean(false)),
            (Value::Boolean(false), Value::Null) | (Value::Null, Value::Boolean(false)) => {
                Ok(Value::Null)
            }
            (Value::Null, Value::Null) => Ok(Value::Null),
            _ => Err(anyhow!("OR requires boolean operands")),
        },

        // Arithmetic (NULL already handled above)
        BinaryOperator::Plus => add_values(left, right),
        BinaryOperator::Minus => sub_values(left, right),
        BinaryOperator::Multiply => mul_values(left, right),
        BinaryOperator::Divide => div_values(left, right),
        BinaryOperator::Modulo => mod_values(left, right),

        BinaryOperator::StringConcat => match (&left, &right) {
            (Value::Tsvector(_), _) | (_, Value::Tsvector(_)) => {
                super::super::fts::concat_tsvector(&left, &right)
            }
            (Value::Jsonb(l), Value::Jsonb(r)) => {
                let left_json: serde_json::Value =
                    serde_json::from_str(l).map_err(|e| anyhow!("Invalid JSONB: {}", e))?;
                let right_json: serde_json::Value =
                    serde_json::from_str(r).map_err(|e| anyhow!("Invalid JSONB: {}", e))?;

                let merged = match (left_json, right_json) {
                    (serde_json::Value::Object(mut lo), serde_json::Value::Object(ro)) => {
                        for (k, v) in ro {
                            lo.insert(k, v);
                        }
                        serde_json::Value::Object(lo)
                    }
                    (serde_json::Value::Array(mut la), serde_json::Value::Array(ra)) => {
                        la.extend(ra);
                        serde_json::Value::Array(la)
                    }
                    // PG behavior: object || array wraps object into array, prepended
                    (serde_json::Value::Object(lo), serde_json::Value::Array(mut ra)) => {
                        ra.insert(0, serde_json::Value::Object(lo));
                        serde_json::Value::Array(ra)
                    }
                    // PG behavior: array || object appends object to array
                    (serde_json::Value::Array(mut la), serde_json::Value::Object(ro)) => {
                        la.push(serde_json::Value::Object(ro));
                        serde_json::Value::Array(la)
                    }
                    // PG17 compatibility: array || scalar wraps scalar in array
                    (serde_json::Value::Array(mut la), scalar) => {
                        la.push(scalar);
                        serde_json::Value::Array(la)
                    }
                    // PG17 compatibility: scalar || array wraps scalar in array
                    (scalar, serde_json::Value::Array(mut ra)) => {
                        ra.insert(0, scalar);
                        serde_json::Value::Array(ra)
                    }
                    // Default: wrap both in array for other mixed types
                    (l, r) => serde_json::Value::Array(vec![l, r]),
                };

                Ok(Value::Jsonb(merged.to_string()))
            }
            (Value::Array(l), Value::Array(r)) => {
                let mut result = l.clone();
                result.extend(r.iter().cloned());
                Ok(Value::Array(result))
            }
            (Value::Array(arr), other) => {
                let mut result = arr.clone();
                result.push(other.clone());
                Ok(Value::Array(result))
            }
            (other, Value::Array(arr)) => {
                let mut result = vec![other.clone()];
                result.extend(arr.iter().cloned());
                Ok(Value::Array(result))
            }
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            _ => {
                let left_str = match left {
                    Value::Text(s) => s,
                    v => v.to_string(),
                };
                let right_str = match right {
                    Value::Text(s) => s,
                    v => v.to_string(),
                };
                Ok(Value::Text(format!("{}{}", left_str, right_str)))
            }
        },

        BinaryOperator::PGOverlap => match (&left, &right) {
            (Value::Array(l), Value::Array(r)) => {
                for lv in l {
                    for rv in r {
                        if compare_values(lv, rv)? == 0 {
                            return Ok(Value::Boolean(true));
                        }
                    }
                }
                Ok(Value::Boolean(false))
            }
            _ => Err(anyhow!("&& operator requires array operands")),
        },

        // Regex operators (NULL already handled above; compiled regex is cached)
        BinaryOperator::PGRegexMatch => eval_regex_op(&left, &right, false, false),
        BinaryOperator::PGRegexIMatch => eval_regex_op(&left, &right, true, false),
        BinaryOperator::PGRegexNotMatch => eval_regex_op(&left, &right, false, true),
        BinaryOperator::PGRegexNotIMatch => eval_regex_op(&left, &right, true, true),

        // PostgreSQL JSONB existence operator: `jsonb ? text`
        // - For objects: key exists
        // - For arrays: string element exists at top-level
        // sqlparser 0.40 parses `?` as a custom operator.
        BinaryOperator::Custom(op) if op == "?" => eval_json_exists(left, right),
        BinaryOperator::PGCustomBinaryOperator(op) if op.len() == 1 && op[0] == "?" => {
            eval_json_exists(left, right)
        }
        BinaryOperator::PGCustomBinaryOperator(op) if op.len() == 1 && op[0] == "@@" => {
            super::super::fts::ts_match(&left, &right)
        }

        _ => Err(SqlError::Unsupported(format!("Unsupported binary operator: {:?}", op)).into()),
    }
}

// --- Arithmetic Helpers ---

fn add_interval_to_timestamp(ts_millis: i64, iv: &crate::model::IntervalValue) -> Result<i64> {
    use chrono::{Datelike, Duration, TimeZone, Utc};

    let dt = Utc
        .timestamp_millis_opt(ts_millis)
        .single()
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;

    let mut result = dt;

    if iv.months != 0 {
        // NOTE: normalize months in O(1) and avoid i32 overflow.
        // `year * 12 + (month-1)` gives an absolute month index. We add the interval months in i64
        // then convert back using euclidean division so negative values work as expected.
        let abs_month0 = i64::from(result.year()) * 12 + (i64::from(result.month()) - 1);
        let abs_month = abs_month0 + i64::from(iv.months);
        let year_i64 = abs_month.div_euclid(12);
        let month_u32 = (abs_month.rem_euclid(12) + 1) as u32;

        let year = i32::try_from(year_i64)
            .map_err(|_| anyhow!("Date out of range after adding months"))?;

        let day = result.day().min(days_in_month(year, month_u32));

        result = result
            .with_year(year)
            .and_then(|d| d.with_month(month_u32))
            .and_then(|d| d.with_day(day))
            .ok_or_else(|| anyhow!("Date out of range after adding months"))?;
    }

    if iv.millis != 0 {
        let delta = Duration::try_milliseconds(iv.millis)
            .ok_or_else(|| anyhow!("Interval out of range"))?;
        result += delta;
    }

    Ok(result.timestamp_millis())
}

fn sub_interval_from_timestamp(ts_millis: i64, iv: &crate::model::IntervalValue) -> Result<i64> {
    let neg_iv = crate::model::IntervalValue::new(-iv.months, -iv.millis);
    add_interval_to_timestamp(ts_millis, &neg_iv)
}

pub(super) fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

pub(super) fn add_values(left: Value, right: Value) -> Result<Value> {
    let left = crate::sql::types::cast::coerce_text_to_numeric(left)?;
    let right = crate::sql::types::cast::coerce_text_to_numeric(right)?;

    if let (Some(l), Some(r)) = (
        numeric::NumericValue::from_value(&left),
        numeric::NumericValue::from_value(&right),
    ) {
        return Ok(numeric::numeric_add(l, r)?.into_value());
    }

    match (left, right) {
        (Value::Timestamp(ts), Value::Interval(iv)) => {
            Ok(Value::Timestamp(add_interval_to_timestamp(ts, &iv)?))
        }
        (Value::Interval(iv), Value::Timestamp(ts)) => {
            Ok(Value::Timestamp(add_interval_to_timestamp(ts, &iv)?))
        }
        (Value::Date(days), Value::Interval(iv)) => {
            let ts = crate::model::date::date_days_to_timestamp_millis(days)?;
            Ok(Value::Timestamp(add_interval_to_timestamp(ts, &iv)?))
        }
        (Value::Interval(iv), Value::Date(days)) => {
            let ts = crate::model::date::date_days_to_timestamp_millis(days)?;
            Ok(Value::Timestamp(add_interval_to_timestamp(ts, &iv)?))
        }
        (Value::Date(days), Value::Int32(n)) => Ok(Value::Date(days + n)),
        (Value::Int32(n), Value::Date(days)) => Ok(Value::Date(days + n)),
        (Value::Date(days), Value::Int64(n)) => {
            let result = (days as i64) + n;
            Ok(Value::Date(
                i32::try_from(result).map_err(|_| anyhow!("date out of range"))?,
            ))
        }
        (Value::Int64(n), Value::Date(days)) => {
            let result = (days as i64) + n;
            Ok(Value::Date(
                i32::try_from(result).map_err(|_| anyhow!("date out of range"))?,
            ))
        }
        (Value::Interval(l), Value::Interval(r)) => Ok(Value::Interval(l + r)),
        (Value::Vector(l), Value::Vector(r)) => {
            if l.len() != r.len() {
                return Err(anyhow!(
                    "cannot add vectors with different dimensions ({} and {})",
                    l.len(),
                    r.len()
                ));
            }
            Ok(Value::Vector(
                l.iter().zip(r.iter()).map(|(a, b)| a + b).collect(),
            ))
        }
        _ => Err(SqlError::Unsupported("Unsupported types for addition".into()).into()),
    }
}

pub(super) fn sub_values(left: Value, right: Value) -> Result<Value> {
    if matches!(left, Value::Jsonb(_)) {
        return jsonb_subtract(left, right);
    }

    if let (Value::Vector(l), Value::Vector(r)) = (&left, &right) {
        if l.len() != r.len() {
            return Err(anyhow!(
                "cannot subtract vectors with different dimensions ({} and {})",
                l.len(),
                r.len()
            ));
        }
        return Ok(Value::Vector(
            l.iter().zip(r.iter()).map(|(a, b)| a - b).collect(),
        ));
    }

    let left = crate::sql::types::cast::coerce_text_to_numeric(left)?;
    let right = crate::sql::types::cast::coerce_text_to_numeric(right)?;

    if let (Some(l), Some(r)) = (
        numeric::NumericValue::from_value(&left),
        numeric::NumericValue::from_value(&right),
    ) {
        return Ok(numeric::numeric_sub(l, r)?.into_value());
    }

    match (left, right) {
        (Value::Timestamp(l), Value::Timestamp(r)) => Ok(Value::Interval(
            crate::model::IntervalValue::from_millis(l - r),
        )),
        (Value::Timestamp(ts), Value::Date(days)) => {
            let date_ts = crate::model::date::date_days_to_timestamp_millis(days)?;
            Ok(Value::Interval(crate::model::IntervalValue::from_millis(
                ts - date_ts,
            )))
        }
        (Value::Date(days), Value::Timestamp(ts)) => {
            let date_ts = crate::model::date::date_days_to_timestamp_millis(days)?;
            Ok(Value::Interval(crate::model::IntervalValue::from_millis(
                date_ts - ts,
            )))
        }
        (Value::Timestamp(ts), Value::Interval(iv)) => {
            Ok(Value::Timestamp(sub_interval_from_timestamp(ts, &iv)?))
        }
        (Value::Date(days), Value::Interval(iv)) => {
            let ts = crate::model::date::date_days_to_timestamp_millis(days)?;
            Ok(Value::Timestamp(sub_interval_from_timestamp(ts, &iv)?))
        }
        (Value::Date(l), Value::Date(r)) => {
            let diff = (l as i64) - (r as i64);
            let days = i32::try_from(diff).map_err(|_| anyhow!("date difference out of range"))?;
            Ok(Value::Int32(days))
        }
        (Value::Date(days), Value::Int32(n)) => Ok(Value::Date(days - n)),
        (Value::Date(days), Value::Int64(n)) => {
            let result = (days as i64) - n;
            Ok(Value::Date(
                i32::try_from(result).map_err(|_| anyhow!("date out of range"))?,
            ))
        }
        (Value::Interval(l), Value::Interval(r)) => Ok(Value::Interval(l - r)),
        _ => Err(SqlError::Unsupported("Unsupported types for subtraction".into()).into()),
    }
}

fn jsonb_subtract(left: Value, right: Value) -> Result<Value> {
    let Value::Jsonb(json_str) = left else {
        return Err(anyhow!("jsonb subtraction requires jsonb left operand"));
    };
    if matches!(right, Value::Null) {
        return Ok(Value::Null);
    }

    let mut json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSONB: {}", e))?;

    match right {
        Value::Text(key) => match &mut json_val {
            serde_json::Value::Object(obj) => {
                obj.remove(&key);
            }
            serde_json::Value::Array(arr) => {
                arr.retain(|v| v.as_str() != Some(key.as_str()));
            }
            _ => {
                return Err(SqlError::InvalidParameterValue {
                    message: "cannot delete from scalar".into(),
                }
                .into());
            }
        },
        Value::Int32(idx) => {
            if let serde_json::Value::Array(arr) = &mut json_val {
                let len = arr.len() as i32;
                let idx = if idx < 0 { len + idx } else { idx };
                if idx >= 0 && (idx as usize) < arr.len() {
                    arr.remove(idx as usize);
                }
            } else {
                let msg = if matches!(json_val, serde_json::Value::Object(_)) {
                    "cannot delete from object using integer index"
                } else {
                    "cannot delete from scalar"
                };
                return Err(SqlError::InvalidParameterValue {
                    message: msg.into(),
                }
                .into());
            }
        }
        Value::Int64(idx) => {
            if let serde_json::Value::Array(arr) = &mut json_val {
                let len = arr.len() as i64;
                let idx = if idx < 0 { len + idx } else { idx };
                if idx >= 0 && (idx as usize) < arr.len() {
                    arr.remove(idx as usize);
                }
            } else {
                let msg = if matches!(json_val, serde_json::Value::Object(_)) {
                    "cannot delete from object using integer index"
                } else {
                    "cannot delete from scalar"
                };
                return Err(SqlError::InvalidParameterValue {
                    message: msg.into(),
                }
                .into());
            }
        }
        // PG17 compatibility: jsonb - text[] for multiple key deletion
        Value::Array(keys) => {
            if matches!(
                json_val,
                serde_json::Value::Null
                    | serde_json::Value::Bool(_)
                    | serde_json::Value::Number(_)
                    | serde_json::Value::String(_)
            ) {
                return Err(SqlError::InvalidParameterValue {
                    message: "cannot delete from scalar".into(),
                }
                .into());
            }
            if let serde_json::Value::Object(obj) = &mut json_val {
                for key_value in keys {
                    match key_value {
                        Value::Null => continue, // skip NULL keys per PG semantics
                        Value::Text(key) => {
                            obj.remove(&key);
                        }
                        _ => continue, // skip non-text elements per PG17 semantics
                    }
                }
            } else if let serde_json::Value::Array(arr) = &mut json_val {
                // Build a set of keys to remove for O(1) lookup
                let key_set: std::collections::HashSet<&str> = keys
                    .iter()
                    .filter_map(|k| {
                        if let Value::Text(s) = k {
                            Some(s.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                arr.retain(|elem| {
                    // Only remove string elements whose value is in the key set
                    // Non-string elements (numbers, bools, nulls, objects, arrays) always pass through
                    if let serde_json::Value::String(s) = elem {
                        !key_set.contains(s.as_str())
                    } else {
                        true
                    }
                });
            }
        }
        other => {
            return Err(SqlError::Unsupported(format!(
                "unsupported right operand for jsonb subtraction: {:?}",
                other
            ))
            .into())
        }
    }

    Ok(Value::Jsonb(json_val.to_string()))
}

fn mul_values(left: Value, right: Value) -> Result<Value> {
    match (&left, &right) {
        (Value::Vector(v), Value::Float64(s)) | (Value::Float64(s), Value::Vector(v)) => {
            return Ok(Value::Vector(v.iter().map(|x| x * s).collect()));
        }
        (Value::Vector(v), Value::Int32(s)) | (Value::Int32(s), Value::Vector(v)) => {
            let scalar = *s as f64;
            return Ok(Value::Vector(v.iter().map(|x| x * scalar).collect()));
        }
        (Value::Vector(v), Value::Int64(s)) | (Value::Int64(s), Value::Vector(v)) => {
            let scalar = *s as f64;
            return Ok(Value::Vector(v.iter().map(|x| x * scalar).collect()));
        }
        (Value::Vector(v), Value::Numeric(s)) | (Value::Numeric(s), Value::Vector(v)) => {
            use rust_decimal::prelude::ToPrimitive;
            let scalar = s
                .to_f64()
                .ok_or_else(|| anyhow!("numeric value out of range"))?;
            return Ok(Value::Vector(v.iter().map(|x| x * scalar).collect()));
        }
        _ => {}
    }

    let left = crate::sql::types::cast::coerce_text_to_numeric(left)?;
    let right = crate::sql::types::cast::coerce_text_to_numeric(right)?;

    if let (Some(l), Some(r)) = (
        numeric::NumericValue::from_value(&left),
        numeric::NumericValue::from_value(&right),
    ) {
        return Ok(numeric::numeric_mul(l, r)?.into_value());
    }

    fn mul_interval_by_int(
        iv: crate::model::IntervalValue,
        factor: i64,
    ) -> Result<crate::model::IntervalValue> {
        let months_i64 = i64::from(iv.months)
            .checked_mul(factor)
            .ok_or_else(|| anyhow!("interval months out of range"))?;
        let months =
            i32::try_from(months_i64).map_err(|_| anyhow!("interval months out of range"))?;
        let millis = iv
            .millis
            .checked_mul(factor)
            .ok_or_else(|| anyhow!("interval out of range"))?;
        Ok(crate::model::IntervalValue::new(months, millis))
    }

    match (left, right) {
        (Value::Interval(iv), factor) | (factor, Value::Interval(iv)) => {
            let Some(factor) = numeric::NumericValue::from_value(&factor) else {
                return Err(
                    SqlError::Unsupported("Unsupported types for multiplication".into()).into(),
                );
            };

            return match factor {
                numeric::NumericValue::Int32(n) => {
                    Ok(Value::Interval(mul_interval_by_int(iv, i64::from(n))?))
                }
                numeric::NumericValue::Int64(n) => Ok(Value::Interval(mul_interval_by_int(iv, n)?)),
                numeric::NumericValue::Decimal(d) => {
                    use rust_decimal::prelude::ToPrimitive;

                    if d.fract().is_zero() {
                        let n = d
                            .to_i64()
                            .ok_or_else(|| anyhow!("interval multiplier out of range"))?;
                        Ok(Value::Interval(mul_interval_by_int(iv, n)?))
                    } else if iv.months == 0 {
                        let f = d
                            .to_f64()
                            .ok_or_else(|| anyhow!("numeric value out of range"))?;
                        if !f.is_finite() {
                            return Err(anyhow!("interval multiplier out of range"));
                        }
                        let scaled = (iv.millis as f64) * f;
                        if !scaled.is_finite() {
                            return Err(anyhow!("interval out of range"));
                        }
                        let rounded = scaled.round();
                        if rounded < (i64::MIN as f64) || rounded > (i64::MAX as f64) {
                            return Err(anyhow!("interval out of range"));
                        }
                        Ok(Value::Interval(crate::model::IntervalValue::from_millis(
                            rounded as i64,
                        )))
                    } else {
                        Err(SqlError::Unsupported(
                            "Unsupported interval multiplication with fractional months".into(),
                        )
                        .into())
                    }
                }
                numeric::NumericValue::Float64(f) => {
                    if f.fract() == 0.0 {
                        // Range check before casting to avoid undefined behavior
                        if !f.is_finite() || f < (i64::MIN as f64) || f > (i64::MAX as f64) {
                            return Err(anyhow!("interval multiplier out of range"));
                        }
                        let n = f as i64;
                        Ok(Value::Interval(mul_interval_by_int(iv, n)?))
                    } else if iv.months == 0 {
                        if !f.is_finite() {
                            return Err(anyhow!("interval multiplier out of range"));
                        }
                        let scaled = (iv.millis as f64) * f;
                        if !scaled.is_finite() {
                            return Err(anyhow!("interval out of range"));
                        }
                        let rounded = scaled.round();
                        if rounded < (i64::MIN as f64) || rounded > (i64::MAX as f64) {
                            return Err(anyhow!("interval out of range"));
                        }
                        Ok(Value::Interval(crate::model::IntervalValue::from_millis(
                            rounded as i64,
                        )))
                    } else {
                        Err(SqlError::Unsupported(
                            "Unsupported interval multiplication with fractional months".into(),
                        )
                        .into())
                    }
                }
            };
        }
        _ => {}
    }

    Err(SqlError::Unsupported("Unsupported types for multiplication".into()).into())
}

fn div_values(left: Value, right: Value) -> Result<Value> {
    let left = crate::sql::types::cast::coerce_text_to_numeric(left)?;
    let right = crate::sql::types::cast::coerce_text_to_numeric(right)?;

    if let (Some(l), Some(r)) = (
        numeric::NumericValue::from_value(&left),
        numeric::NumericValue::from_value(&right),
    ) {
        return Ok(numeric::numeric_div(l, r)?.into_value());
    }

    Err(SqlError::Unsupported("Unsupported types for division".into()).into())
}

fn mod_values(left: Value, right: Value) -> Result<Value> {
    if let (Some(l), Some(r)) = (
        numeric::NumericValue::from_value(&left),
        numeric::NumericValue::from_value(&right),
    ) {
        return Ok(numeric::numeric_mod(l, r)?.into_value());
    }

    Err(SqlError::Unsupported("Unsupported types for modulo".into()).into())
}

fn compare_text_pg(left: &str, right: &str) -> std::cmp::Ordering {
    // PostgreSQL C/POSIX collation semantics: bytewise lexicographic ordering.
    left.as_bytes().cmp(right.as_bytes())
}

fn compare_float64_pg(left: f64, right: f64) -> std::cmp::Ordering {
    match (left.is_nan(), right.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => left
            .partial_cmp(&right)
            .expect("non-NaN floats must be comparable"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JsonbComparableValue {
    Null,
    String(String),
    Number(String),
    Bool(bool),
    Array(Vec<JsonbComparableValue>),
    Object(Vec<(String, JsonbComparableValue)>),
}

impl PartialOrd for JsonbComparableValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsonbComparableValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match compare_jsonb_value(self, other) {
            x if x < 0 => std::cmp::Ordering::Less,
            0 => std::cmp::Ordering::Equal,
            _ => std::cmp::Ordering::Greater,
        }
    }
}

/// Pre-parsed JSONB value for sort-key comparisons. Avoids re-parsing
/// during O(N log N) sort comparisons.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct JsonbSortKey(JsonbComparableValue);

impl JsonbSortKey {
    pub fn from_str(raw: &str) -> Result<Self> {
        Ok(Self(parse_jsonb_comparable_value(raw)?))
    }

    /// Estimated heap bytes owned by this sort key (recursive).
    pub fn estimate_heap_size(&self) -> usize {
        self.0.estimate_heap_size()
    }
}

impl JsonbComparableValue {
    /// Estimated heap bytes owned by this value (recursive).
    /// Counts String capacity, Vec overhead, and nested children.
    fn estimate_heap_size(&self) -> usize {
        match self {
            Self::Null | Self::Bool(_) => 0,
            Self::String(s) | Self::Number(s) => s.len(),
            Self::Array(items) => {
                std::mem::size_of::<Self>() * items.capacity()
                    + items.iter().map(|v| v.estimate_heap_size()).sum::<usize>()
            }
            Self::Object(entries) => {
                std::mem::size_of::<(String, Self)>() * entries.capacity()
                    + entries
                        .iter()
                        .map(|(k, v)| k.len() + v.estimate_heap_size())
                        .sum::<usize>()
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_counters {
    use std::cell::Cell;
    thread_local! {
        static PARSE_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    pub fn reset() {
        PARSE_COUNT.with(|c| c.set(0));
    }
    pub fn get() -> usize {
        PARSE_COUNT.with(|c| c.get())
    }
    pub(super) fn increment() {
        PARSE_COUNT.with(|c| c.set(c.get() + 1));
    }
}

pub(crate) fn parse_jsonb_comparable_value(raw: &str) -> Result<JsonbComparableValue> {
    #[cfg(test)]
    test_counters::increment();

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("Invalid JSONB: empty input"));
    }

    match trimmed.as_bytes()[0] {
        b'n' => {
            if trimmed == "null" {
                Ok(JsonbComparableValue::Null)
            } else {
                Err(anyhow!("Invalid JSONB: {}", trimmed))
            }
        }
        b't' => {
            if trimmed == "true" {
                Ok(JsonbComparableValue::Bool(true))
            } else {
                Err(anyhow!("Invalid JSONB: {}", trimmed))
            }
        }
        b'f' => {
            if trimmed == "false" {
                Ok(JsonbComparableValue::Bool(false))
            } else {
                Err(anyhow!("Invalid JSONB: {}", trimmed))
            }
        }
        b'"' => {
            let s: String =
                serde_json::from_str(trimmed).map_err(|e| anyhow!("Invalid JSONB: {}", e))?;
            Ok(JsonbComparableValue::String(s))
        }
        b'[' => {
            let values: Vec<Box<RawValue>> =
                serde_json::from_str(trimmed).map_err(|e| anyhow!("Invalid JSONB: {}", e))?;
            let mut out = Vec::with_capacity(values.len());
            for v in values {
                out.push(parse_jsonb_comparable_value(v.get())?);
            }
            Ok(JsonbComparableValue::Array(out))
        }
        b'{' => {
            let values: BTreeMap<String, Box<RawValue>> =
                serde_json::from_str(trimmed).map_err(|e| anyhow!("Invalid JSONB: {}", e))?;
            let mut out = Vec::with_capacity(values.len());
            for (k, v) in values {
                out.push((k, parse_jsonb_comparable_value(v.get())?));
            }
            Ok(JsonbComparableValue::Object(out))
        }
        b'-' | b'0'..=b'9' => {
            let _: serde_json::Number =
                serde_json::from_str(trimmed).map_err(|e| anyhow!("Invalid JSONB: {}", e))?;
            Ok(JsonbComparableValue::Number(trimmed.to_string()))
        }
        _ => Err(anyhow!("Invalid JSONB: {}", trimmed)),
    }
}

/// PostgreSQL jsonb type ordering priority.
/// PG order: Null < String < Number < Boolean < Array < Object.
fn jsonb_type_priority(v: &JsonbComparableValue) -> u8 {
    match v {
        JsonbComparableValue::Null => 0,
        JsonbComparableValue::String(_) => 1,
        JsonbComparableValue::Number(_) => 2,
        JsonbComparableValue::Bool(_) => 3,
        JsonbComparableValue::Array(_) => 4,
        JsonbComparableValue::Object(_) => 5,
    }
}

/// Compare JSON numbers with integer precision preserved whenever possible.
fn compare_jsonb_number(left: &str, right: &str) -> i8 {
    if let (Ok(li), Ok(ri)) = (left.parse::<i64>(), right.parse::<i64>()) {
        return li.cmp(&ri) as i8;
    }

    if let (Ok(lu), Ok(ru)) = (left.parse::<u64>(), right.parse::<u64>()) {
        return lu.cmp(&ru) as i8;
    }

    if let (Ok(li), Ok(ru)) = (left.parse::<i64>(), right.parse::<u64>()) {
        return if li < 0 {
            -1
        } else {
            (li as u64).cmp(&ru) as i8
        };
    }

    if let (Ok(lu), Ok(ri)) = (left.parse::<u64>(), right.parse::<i64>()) {
        return if ri < 0 {
            1
        } else {
            lu.cmp(&(ri as u64)) as i8
        };
    }

    match (BigDecimal::from_str(left), BigDecimal::from_str(right)) {
        (Ok(ld), Ok(rd)) => ld.cmp(&rd) as i8,
        // Unreachable in practice because parse_jsonb_comparable_value validates number syntax.
        _ => left.cmp(right) as i8,
    }
}

/// Compare two JSONB values using PostgreSQL ordering semantics.
fn compare_jsonb_value(left: &JsonbComparableValue, right: &JsonbComparableValue) -> i8 {
    let lp = jsonb_type_priority(left);
    let rp = jsonb_type_priority(right);
    if lp != rp {
        return lp.cmp(&rp) as i8;
    }
    match (left, right) {
        (JsonbComparableValue::Null, JsonbComparableValue::Null) => 0,
        (JsonbComparableValue::Bool(l), JsonbComparableValue::Bool(r)) => l.cmp(r) as i8,
        (JsonbComparableValue::Number(l), JsonbComparableValue::Number(r)) => {
            compare_jsonb_number(l, r)
        }
        (JsonbComparableValue::String(l), JsonbComparableValue::String(r)) => l.cmp(r) as i8,
        (JsonbComparableValue::Array(l), JsonbComparableValue::Array(r)) => {
            // Element-wise, then length.
            for (le, re) in l.iter().zip(r.iter()) {
                let c = compare_jsonb_value(le, re);
                if c != 0 {
                    return c;
                }
            }
            l.len().cmp(&r.len()) as i8
        }
        (JsonbComparableValue::Object(l), JsonbComparableValue::Object(r)) => {
            // PG: compare pair count, then key-by-key (sorted), then values.
            match l.len().cmp(&r.len()) {
                std::cmp::Ordering::Equal => {}
                o => return o as i8,
            }
            for ((lk, lv), (rk, rv)) in l.iter().zip(r.iter()) {
                match lk.cmp(rk) {
                    std::cmp::Ordering::Equal => {}
                    o => return o as i8,
                }
                let c = compare_jsonb_value(lv, rv);
                if c != 0 {
                    return c;
                }
            }
            0
        }
        _ => 0, // same type, already handled
    }
}

/// Compare two JSONB string values using PostgreSQL ordering semantics.
fn compare_jsonb_pg(left: &str, right: &str) -> Result<i8> {
    let lv = parse_jsonb_comparable_value(left)?;
    let rv = parse_jsonb_comparable_value(right)?;
    Ok(compare_jsonb_value(&lv, &rv))
}

/// Compare two values of the same type. Returns -1, 0, or 1.
fn compare_same_type(left: &Value, right: &Value) -> Result<i8> {
    match (left, right) {
        (Value::Int32(l), Value::Int32(r)) => Ok(l.cmp(r) as i8),
        (Value::Int64(l), Value::Int64(r)) => Ok(l.cmp(r) as i8),
        (Value::Float64(l), Value::Float64(r)) => Ok(compare_float64_pg(*l, *r) as i8),
        (Value::Text(l), Value::Text(r)) => Ok(compare_text_pg(l, r) as i8),
        (Value::Boolean(l), Value::Boolean(r)) => Ok(l.cmp(r) as i8),
        (Value::Timestamp(l), Value::Timestamp(r)) => Ok(l.cmp(r) as i8),
        (Value::Date(l), Value::Date(r)) => Ok(l.cmp(r) as i8),
        (Value::Uuid(l), Value::Uuid(r)) => Ok(l.cmp(r) as i8),
        (Value::Bytes(l), Value::Bytes(r)) => Ok(l.cmp(r) as i8),
        (Value::Numeric(l), Value::Numeric(r)) => Ok(l.cmp(r) as i8),
        (Value::Time(l), Value::Time(r)) => Ok(l.cmp(r) as i8),
        (Value::Array(l), Value::Array(r)) => {
            let min_len = l.len().min(r.len());
            for i in 0..min_len {
                let ord = compare_values(&l[i], &r[i])?;
                if ord != 0 {
                    return Ok(ord);
                }
            }
            Ok(l.len().cmp(&r.len()) as i8)
        }
        (Value::Vector(l), Value::Vector(r)) => {
            for (a, b) in l.iter().zip(r.iter()) {
                if a < b {
                    return Ok(-1);
                }
                if a > b {
                    return Ok(1);
                }
            }
            Ok(l.len().cmp(&r.len()) as i8)
        }
        (Value::Jsonb(l), Value::Jsonb(r)) => compare_jsonb_pg(l, r),
        (Value::Interval(l), Value::Interval(r)) => {
            // PostgreSQL normalizes intervals to total duration for comparison
            // using 30 days/month approximation.
            Ok(l.to_millis_approx().cmp(&r.to_millis_approx()) as i8)
        }
        _ => Err(anyhow!("Cannot compare values: {:?} vs {:?}", left, right)),
    }
}

fn compare_int64_and_float64(left: i64, right: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    if right.is_nan() {
        return Ordering::Less;
    }
    if right.is_infinite() {
        return if right.is_sign_positive() {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    if right == 0.0 {
        return left.cmp(&0);
    }

    let left_is_negative = left < 0;
    let right_is_negative = right.is_sign_negative();
    if left_is_negative != right_is_negative {
        return if left_is_negative {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }

    let magnitude_ordering = compare_uint64_and_positive_float64(left.unsigned_abs(), right.abs());
    if left_is_negative {
        magnitude_ordering.reverse()
    } else {
        magnitude_ordering
    }
}

fn compare_uint64_and_positive_float64(left: u64, right: f64) -> std::cmp::Ordering {
    let bits = right.to_bits();
    let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
    let mantissa = bits & ((1_u64 << 52) - 1);
    let (significand, exponent) = if exponent_bits == 0 {
        (mantissa, 1 - 1023 - 52)
    } else {
        ((1_u64 << 52) | mantissa, exponent_bits - 1023 - 52)
    };

    if exponent >= 0 {
        if exponent > 11 {
            return std::cmp::Ordering::Less;
        }
        let right_int = (significand as u128) << exponent as u32;
        return (left as u128).cmp(&right_int);
    }

    let shift = (-exponent) as u32;
    if shift > 63 {
        return if left == 0 {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        };
    }

    ((left as u128) << shift).cmp(&(significand as u128))
}

/// Compare two values. Returns:
/// - 0: equal
/// - 1: left > right
/// - -1: left < right
pub fn compare_values(left: &Value, right: &Value) -> Result<i8> {
    // Phase 1: Incomparable types (early error)
    match (left, right) {
        (Value::Json(_), _) | (_, Value::Json(_)) => {
            return Err(anyhow!(
                "could not identify a comparison function for type json"
            ))
        }
        _ => {}
    }

    // Phase 2: Null handling (PG null sort semantics)
    match (left, right) {
        (Value::Null, Value::Null) => return Ok(0),
        (Value::Null, _) => return Ok(-1),
        (_, Value::Null) => return Ok(1),
        _ => {}
    }

    // Phase 3: Same-type fast path (no cloning)
    // For Numeric, ignore scale differences — Decimal::cmp is scale-independent.
    if left.data_type() == right.data_type()
        || matches!((left, right), (Value::Numeric(_), Value::Numeric(_)))
    {
        return compare_same_type(left, right);
    }

    // Phase 4: Preserve exact mixed integer/float comparisons without lossy
    // implicit promotion to f64.
    match (left, right) {
        (Value::Int32(left), Value::Float64(right)) => {
            return Ok(compare_int64_and_float64(i64::from(*left), *right) as i8)
        }
        (Value::Float64(left), Value::Int32(right)) => {
            return Ok(compare_int64_and_float64(i64::from(*right), *left).reverse() as i8)
        }
        (Value::Int64(left), Value::Float64(right)) => {
            return Ok(compare_int64_and_float64(*left, *right) as i8)
        }
        (Value::Float64(left), Value::Int64(right)) => {
            return Ok(compare_int64_and_float64(*right, *left).reverse() as i8)
        }
        _ => {}
    }

    // Phase 5: Cross-type comparisons are rejected.
    //
    // The Analyzer must insert implicit casts so values are type-compatible
    // before runtime evaluation. Keeping coercion here would be a hidden
    // semantic fallback and a per-row overhead.
    Err(anyhow!("Cannot compare values: {:?} vs {:?}", left, right))
}

/// Shared NULL-sentinel + ASC/DESC ordering logic for ORDER BY comparators.
///
/// Handles NULLS FIRST/LAST, then delegates non-null comparison to `cmp_non_null`.
/// The closure receives two guaranteed-non-null values and returns a raw comparison
/// integer (negative = left < right, 0 = equal, positive = left > right).
fn compare_nullable(
    left: &Value,
    right: &Value,
    asc: bool,
    nulls_first: bool,
    cmp_non_null: impl FnOnce(&Value, &Value) -> Result<i32>,
) -> Result<std::cmp::Ordering> {
    use std::cmp::Ordering;
    match (left, right) {
        (Value::Null, Value::Null) => Ok(Ordering::Equal),
        (Value::Null, _) => Ok(if nulls_first {
            Ordering::Less
        } else {
            Ordering::Greater
        }),
        (_, Value::Null) => Ok(if nulls_first {
            Ordering::Greater
        } else {
            Ordering::Less
        }),
        _ => {
            let cmp = cmp_non_null(left, right)?;
            Ok(if cmp == 0 {
                Ordering::Equal
            } else if (cmp > 0) == asc {
                Ordering::Greater
            } else {
                Ordering::Less
            })
        }
    }
}

/// ORDER BY comparator with PostgreSQL-like NULLS FIRST/LAST semantics.
pub fn compare_order_by_values(
    left: &Value,
    right: &Value,
    asc: bool,
    nulls_first: bool,
) -> Result<std::cmp::Ordering> {
    compare_nullable(left, right, asc, nulls_first, |l, r| {
        Ok(compare_values(l, r)? as i32)
    })
}

/// ORDER BY comparator with collation support.
///
/// When `collation` is `Some`, text values are compared using the resolved
/// collation. For non-text values or `None` collation, falls back to
/// `compare_values()`.
pub fn compare_order_by_values_collated(
    left: &Value,
    right: &Value,
    asc: bool,
    nulls_first: bool,
    collation: Option<&crate::sql::collation::ResolvedCollation>,
) -> Result<std::cmp::Ordering> {
    compare_nullable(left, right, asc, nulls_first, |l, r| {
        if let (Some(coll), Value::Text(a), Value::Text(b)) = (collation, l, r) {
            crate::sql::collation::compare_with_resolved_collation(a, b, coll)
        } else {
            Ok(compare_values(l, r)? as i32)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_interval_to_timestamp_millis_min_does_not_panic() {
        let ts_millis = 0;
        let iv = crate::model::IntervalValue::new(0, i64::MIN);

        let err = add_interval_to_timestamp(ts_millis, &iv).unwrap_err();
        assert!(err.to_string().contains("Interval out of range"));
    }

    #[test]
    fn test_add_interval_to_timestamp_month_overflow_does_not_panic() {
        let ts_millis = 0;

        let iv = crate::model::IntervalValue::from_months(i32::MAX);
        let err = add_interval_to_timestamp(ts_millis, &iv).unwrap_err();
        assert!(err.to_string().contains("Date out of range"));

        let iv = crate::model::IntervalValue::from_months(i32::MIN);
        let err = add_interval_to_timestamp(ts_millis, &iv).unwrap_err();
        assert!(err.to_string().contains("Date out of range"));
    }

    #[test]
    fn test_mul_values_interval_by_int() {
        let iv = crate::model::IntervalValue::from_millis(1_000);

        assert_eq!(
            mul_values(Value::Int32(2), Value::Interval(iv)).unwrap(),
            Value::Interval(crate::model::IntervalValue::from_millis(2_000))
        );
        assert_eq!(
            mul_values(Value::Interval(iv), Value::Int64(3)).unwrap(),
            Value::Interval(crate::model::IntervalValue::from_millis(3_000))
        );

        let iv_months = crate::model::IntervalValue::from_months(12);
        assert_eq!(
            mul_values(Value::Interval(iv_months), Value::Int32(2)).unwrap(),
            Value::Interval(crate::model::IntervalValue::from_months(24))
        );
    }

    #[test]
    fn test_mul_values_interval_by_fractional_when_months_zero() {
        let iv = crate::model::IntervalValue::from_millis(1_000);
        assert_eq!(
            mul_values(Value::Interval(iv), Value::Float64(0.5)).unwrap(),
            Value::Interval(crate::model::IntervalValue::from_millis(500))
        );
    }

    #[test]
    fn test_mul_values_interval_fractional_rejects_months_component() {
        let iv = crate::model::IntervalValue::new(1, 0);
        let err = mul_values(Value::Interval(iv), Value::Float64(0.5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("fractional months"));
    }

    #[test]
    fn test_mul_values_interval_by_large_whole_float_overflow() {
        let iv = crate::model::IntervalValue::from_millis(1_000);
        // Test with float that's whole but exceeds i64::MAX
        let err = mul_values(Value::Interval(iv), Value::Float64(1e19))
            .unwrap_err()
            .to_string();
        assert!(err.contains("out of range"));

        // Test with large negative float
        let err = mul_values(Value::Interval(iv), Value::Float64(-1e19))
            .unwrap_err()
            .to_string();
        assert!(err.contains("out of range"));
    }

    #[test]
    fn test_compare_numeric_different_scales() {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        // 10.45 (scale=2) vs 10.4 (scale=1) — must not be equal
        let a = Value::Numeric(Decimal::from_str("10.45").unwrap());
        let b = Value::Numeric(Decimal::from_str("10.4").unwrap());
        assert_eq!(compare_values(&a, &b).unwrap(), 1); // 10.45 > 10.4

        // Equal values with different scales
        let c = Value::Numeric(Decimal::from_str("10.40").unwrap()); // scale=2
        let d = Value::Numeric(Decimal::from_str("10.4").unwrap()); // scale=1
        assert_eq!(compare_values(&c, &d).unwrap(), 0); // 10.40 == 10.4

        // Reverse direction
        assert_eq!(compare_values(&b, &a).unwrap(), -1); // 10.4 < 10.45
    }

    #[test]
    fn test_compare_text_pg_uses_bytewise_order_for_unicode() {
        assert_eq!(
            compare_values(&Value::Text("é".into()), &Value::Text("a".into())).unwrap(),
            1
        );
        assert_eq!(
            compare_values(&Value::Text("中".into()), &Value::Text("文".into())).unwrap(),
            -1
        );
        assert_eq!(
            compare_values(&Value::Text("É".into()), &Value::Text("é".into())).unwrap(),
            -1
        );
    }

    #[test]
    fn test_compare_order_by_text_default_collation_is_bytewise() {
        let mut values = vec![
            Value::Text("文".into()),
            Value::Text("a".into()),
            Value::Text("中".into()),
            Value::Text("é".into()),
            Value::Text("A".into()),
        ];
        sort_by_fallible(&mut values, |left, right| {
            compare_order_by_values_collated(left, right, true, true, None)
        })
        .unwrap();

        assert_eq!(
            values,
            vec![
                Value::Text("A".into()),
                Value::Text("a".into()),
                Value::Text("é".into()),
                Value::Text("中".into()),
                Value::Text("文".into()),
            ]
        );
    }

    #[test]
    fn test_compare_values_rejects_cross_type_without_analyzer_casts() {
        let err = compare_values(&Value::Text("42".into()), &Value::Int32(10))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Cannot compare values"));
    }

    #[test]
    fn test_compare_values_preserve_exact_mixed_int_float_ordering() {
        let large = Value::Int64(9_007_199_254_740_993);
        let rounded = Value::Float64(9_007_199_254_740_992.0);

        assert_eq!(compare_values(&large, &rounded).unwrap(), 1);
        assert_eq!(compare_values(&rounded, &large).unwrap(), -1);
        assert_eq!(
            compare_values(&Value::Int32(42), &Value::Float64(42.0)).unwrap(),
            0
        );
    }

    #[test]
    fn test_compare_values_mixed_int_float_follow_float_nan_ordering() {
        assert_eq!(
            compare_values(&Value::Int64(1), &Value::Float64(f64::NAN)).unwrap(),
            -1
        );
        assert_eq!(
            compare_values(&Value::Float64(f64::NAN), &Value::Int64(1)).unwrap(),
            1
        );
    }

    #[test]
    fn test_compare_jsonb_numeric_values_preserve_integer_precision() {
        let greater = Value::Jsonb("9007199254740993".into()); // 2^53 + 1
        let smaller = Value::Jsonb("9007199254740992".into()); // 2^53
        assert_eq!(compare_values(&greater, &smaller).unwrap(), 1);
        assert_eq!(compare_values(&smaller, &greater).unwrap(), -1);

        // Exercises mixed i64/u64 branch.
        let i64_max = Value::Jsonb("9223372036854775807".into());
        let i64_max_plus_one = Value::Jsonb("9223372036854775808".into());
        assert_eq!(compare_values(&i64_max_plus_one, &i64_max).unwrap(), 1);
    }

    #[test]
    fn test_compare_jsonb_numeric_values_preserve_high_precision_decimals() {
        let greater = Value::Jsonb("12345678901234567890.12345678901234567891".into());
        let smaller = Value::Jsonb("12345678901234567890.12345678901234567890".into());
        assert_eq!(compare_values(&greater, &smaller).unwrap(), 1);
        assert_eq!(compare_values(&smaller, &greater).unwrap(), -1);

        // `f64` would collapse both of these to the same value.
        let close_a = Value::Jsonb("9007199254740992.0000000000000000001".into());
        let close_b = Value::Jsonb("9007199254740992.0000000000000000002".into());
        assert_eq!(compare_values(&close_a, &close_b).unwrap(), -1);
    }

    #[test]
    fn test_regex_operator_variants_share_semantics() {
        let left = Value::Text("Hello".into());
        let right = Value::Text("HELLO".into());

        assert_eq!(
            eval_binary_op(left.clone(), &BinaryOperator::PGRegexMatch, right.clone()).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_binary_op(left.clone(), &BinaryOperator::PGRegexIMatch, right.clone()).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_binary_op(
                left.clone(),
                &BinaryOperator::PGRegexNotMatch,
                right.clone()
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval_binary_op(left, &BinaryOperator::PGRegexNotIMatch, right).unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn test_regex_operator_coerces_non_text_operands_to_text() {
        assert_eq!(
            eval_binary_op(
                Value::Int32(42),
                &BinaryOperator::PGRegexMatch,
                Value::Text("^42$".into()),
            )
            .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn test_regex_operator_invalid_pattern_surfaces_error() {
        let err = eval_binary_op(
            Value::Text("abc".into()),
            &BinaryOperator::PGRegexMatch,
            Value::Text("[".into()),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Invalid regex pattern"));
    }

    #[test]
    fn test_jsonb_comparable_value_ord_consistency() {
        // PG order: Null < String < Number < Boolean < Array < Object
        let null = parse_jsonb_comparable_value("null").unwrap();
        let string = parse_jsonb_comparable_value(r#""hello""#).unwrap();
        let number = parse_jsonb_comparable_value("42").unwrap();
        let boolean = parse_jsonb_comparable_value("true").unwrap();
        let array = parse_jsonb_comparable_value("[1,2]").unwrap();
        let object = parse_jsonb_comparable_value(r#"{"a":1}"#).unwrap();

        // Cross-type ordering
        assert!(null < string);
        assert!(string < number);
        assert!(number < boolean);
        assert!(boolean < array);
        assert!(array < object);

        // Same-type comparisons
        let num1 = parse_jsonb_comparable_value("1").unwrap();
        let num2 = parse_jsonb_comparable_value("2").unwrap();
        assert!(num1 < num2);
        assert_eq!(num1.cmp(&num1), std::cmp::Ordering::Equal);

        let str_a = parse_jsonb_comparable_value(r#""abc""#).unwrap();
        let str_z = parse_jsonb_comparable_value(r#""xyz""#).unwrap();
        assert!(str_a < str_z);
    }

    #[test]
    fn test_jsonb_sort_key_from_str_round_trip() {
        let k1 = JsonbSortKey::from_str("1").unwrap();
        let k2 = JsonbSortKey::from_str("2").unwrap();
        let k_str = JsonbSortKey::from_str(r#""hello""#).unwrap();
        let k_null = JsonbSortKey::from_str("null").unwrap();

        // Number ordering
        assert!(k1 < k2);
        // PG: null < string < number
        assert!(k_null < k_str);
        assert!(k_str < k1);
    }

    #[test]
    fn test_compare_nullable_null_sentinel_logic() {
        let non_null = Value::Int32(1);
        let null = Value::Null;

        // (Null, Null) → always Equal regardless of asc/nulls_first
        for asc in [true, false] {
            for nf in [true, false] {
                assert_eq!(
                    compare_nullable(&null, &null, asc, nf, |_, _| unreachable!()).unwrap(),
                    std::cmp::Ordering::Equal,
                );
            }
        }

        // (Null, non-null): nulls_first=true → Less, nulls_first=false → Greater
        assert_eq!(
            compare_nullable(&null, &non_null, true, true, |_, _| unreachable!()).unwrap(),
            std::cmp::Ordering::Less,
        );
        assert_eq!(
            compare_nullable(&null, &non_null, true, false, |_, _| unreachable!()).unwrap(),
            std::cmp::Ordering::Greater,
        );

        // (non-null, Null): nulls_first=true → Greater, nulls_first=false → Less
        assert_eq!(
            compare_nullable(&non_null, &null, true, true, |_, _| unreachable!()).unwrap(),
            std::cmp::Ordering::Greater,
        );
        assert_eq!(
            compare_nullable(&non_null, &null, true, false, |_, _| unreachable!()).unwrap(),
            std::cmp::Ordering::Less,
        );
    }

    #[test]
    fn test_compare_nullable_asc_desc_ordering() {
        let a = Value::Int32(1);
        let b = Value::Int32(2);

        // ASC: 1 < 2 → Less
        assert_eq!(
            compare_nullable(&a, &b, true, true, |l, r| Ok(compare_values(l, r)? as i32)).unwrap(),
            std::cmp::Ordering::Less,
        );
        // DESC: 1 < 2 → reversed → Greater
        assert_eq!(
            compare_nullable(&a, &b, false, true, |l, r| Ok(compare_values(l, r)? as i32)).unwrap(),
            std::cmp::Ordering::Greater,
        );
        // Equal values
        assert_eq!(
            compare_nullable(&a, &a, true, true, |l, r| Ok(compare_values(l, r)? as i32)).unwrap(),
            std::cmp::Ordering::Equal,
        );
    }
}
