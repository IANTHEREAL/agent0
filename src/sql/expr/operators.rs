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

use crate::sql::error::SqlError;
use crate::types::Value;
use anyhow::{anyhow, Result};
use sqlparser::ast::BinaryOperator;

use super::numeric;

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

/// Evaluate a binary operator on two values.
pub fn eval_binary_op(left: Value, op: &BinaryOperator, right: Value) -> Result<Value> {
    match op {
        // Comparison - SQL three-valued logic: comparison with NULL returns NULL
        BinaryOperator::Eq => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? == 0))
            }
        }
        BinaryOperator::NotEq => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? != 0))
            }
        }
        BinaryOperator::Gt => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? > 0))
            }
        }
        BinaryOperator::Lt => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? < 0))
            }
        }
        BinaryOperator::GtEq => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? >= 0))
            }
        }
        BinaryOperator::LtEq => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(compare_values(&left, &right)? <= 0))
            }
        }

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

        // Arithmetic
        // PostgreSQL arithmetic operators are strict: NULL in => NULL out.
        BinaryOperator::Plus => {
            if left == Value::Null || right == Value::Null {
                Ok(Value::Null)
            } else {
                add_values(left, right)
            }
        }
        BinaryOperator::Minus => {
            if left == Value::Null || right == Value::Null {
                Ok(Value::Null)
            } else {
                sub_values(left, right)
            }
        }
        BinaryOperator::Multiply => {
            if left == Value::Null || right == Value::Null {
                Ok(Value::Null)
            } else {
                mul_values(left, right)
            }
        }
        BinaryOperator::Divide => {
            if left == Value::Null || right == Value::Null {
                Ok(Value::Null)
            } else {
                div_values(left, right)
            }
        }
        BinaryOperator::Modulo => {
            if left == Value::Null || right == Value::Null {
                Ok(Value::Null)
            } else {
                mod_values(left, right)
            }
        }

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

        BinaryOperator::PGRegexMatch => {
            let text = match &left {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let pattern = match &right {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            match regex::Regex::new(&pattern) {
                Ok(re) => Ok(Value::Boolean(re.is_match(&text))),
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        BinaryOperator::PGRegexIMatch => {
            let text = match &left {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let pattern = match &right {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let case_insensitive_pattern = format!("(?i){}", pattern);
            match regex::Regex::new(&case_insensitive_pattern) {
                Ok(re) => Ok(Value::Boolean(re.is_match(&text))),
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        BinaryOperator::PGRegexNotMatch => {
            let text = match &left {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let pattern = match &right {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            match regex::Regex::new(&pattern) {
                Ok(re) => Ok(Value::Boolean(!re.is_match(&text))),
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        BinaryOperator::PGRegexNotIMatch => {
            let text = match &left {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let pattern = match &right {
                Value::Text(s) => s.clone(),
                Value::Null => return Ok(Value::Null),
                v => v.to_string(),
            };
            let case_insensitive_pattern = format!("(?i){}", pattern);
            match regex::Regex::new(&case_insensitive_pattern) {
                Ok(re) => Ok(Value::Boolean(!re.is_match(&text))),
                Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
            }
        }

        // PostgreSQL JSONB existence operator: `jsonb ? text`
        // - For objects: key exists
        // - For arrays: string element exists at top-level
        // sqlparser 0.40 parses `?` as a custom operator.
        BinaryOperator::Custom(op) if op == "?" => {
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
        BinaryOperator::PGCustomBinaryOperator(op) if op.len() == 1 && op[0] == "?" => {
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
        BinaryOperator::PGCustomBinaryOperator(op) if op.len() == 1 && op[0] == "@@" => {
            super::super::fts::ts_match(&left, &right)
        }

        _ => Err(SqlError::Unsupported(format!("Unsupported binary operator: {:?}", op)).into()),
    }
}

// --- Arithmetic Helpers ---

fn add_interval_to_timestamp(ts_millis: i64, iv: &crate::types::IntervalValue) -> Result<i64> {
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
        result = result + delta;
    }

    Ok(result.timestamp_millis())
}

fn sub_interval_from_timestamp(ts_millis: i64, iv: &crate::types::IntervalValue) -> Result<i64> {
    let neg_iv = crate::types::IntervalValue::new(-iv.months, -iv.millis);
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

pub(super) fn parse_bool_pg(s: &str) -> Option<bool> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("true")
        || s.eq_ignore_ascii_case("t")
        || s.eq_ignore_ascii_case("yes")
        || s.eq_ignore_ascii_case("y")
        || s.eq_ignore_ascii_case("on")
        || s == "1"
    {
        Some(true)
    } else if s.eq_ignore_ascii_case("false")
        || s.eq_ignore_ascii_case("f")
        || s.eq_ignore_ascii_case("no")
        || s.eq_ignore_ascii_case("n")
        || s.eq_ignore_ascii_case("off")
        || s == "0"
    {
        Some(false)
    } else {
        None
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
            let ts = crate::types::date::date_days_to_timestamp_millis(days)?;
            Ok(Value::Timestamp(add_interval_to_timestamp(ts, &iv)?))
        }
        (Value::Interval(iv), Value::Date(days)) => {
            let ts = crate::types::date::date_days_to_timestamp_millis(days)?;
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
        _ => Err(SqlError::Unsupported("Unsupported types for addition".into()).into()),
    }
}

pub(super) fn sub_values(left: Value, right: Value) -> Result<Value> {
    if matches!(left, Value::Jsonb(_)) {
        return jsonb_subtract(left, right);
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
            crate::types::IntervalValue::from_millis(l - r),
        )),
        (Value::Timestamp(ts), Value::Interval(iv)) => {
            Ok(Value::Timestamp(sub_interval_from_timestamp(ts, &iv)?))
        }
        (Value::Date(days), Value::Interval(iv)) => {
            let ts = crate::types::date::date_days_to_timestamp_millis(days)?;
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
            _ => {}
        },
        Value::Int32(idx) => match &mut json_val {
            serde_json::Value::Array(arr) => {
                let len = arr.len() as i32;
                let idx = if idx < 0 { len + idx } else { idx };
                if idx >= 0 && (idx as usize) < arr.len() {
                    arr.remove(idx as usize);
                }
            }
            _ => {}
        },
        Value::Int64(idx) => match &mut json_val {
            serde_json::Value::Array(arr) => {
                let len = arr.len() as i64;
                let idx = if idx < 0 { len + idx } else { idx };
                if idx >= 0 && (idx as usize) < arr.len() {
                    arr.remove(idx as usize);
                }
            }
            _ => {}
        },
        other => {
            return Err(anyhow!(
                "unsupported right operand for jsonb subtraction: {:?}",
                other
            ))
        }
    }

    Ok(Value::Jsonb(json_val.to_string()))
}

fn mul_values(left: Value, right: Value) -> Result<Value> {
    let left = crate::sql::types::cast::coerce_text_to_numeric(left)?;
    let right = crate::sql::types::cast::coerce_text_to_numeric(right)?;

    if let (Some(l), Some(r)) = (
        numeric::NumericValue::from_value(&left),
        numeric::NumericValue::from_value(&right),
    ) {
        return Ok(numeric::numeric_mul(l, r)?.into_value());
    }

    fn mul_interval_by_int(
        iv: crate::types::IntervalValue,
        factor: i64,
    ) -> Result<crate::types::IntervalValue> {
        let months_i64 = i64::from(iv.months)
            .checked_mul(factor)
            .ok_or_else(|| anyhow!("interval months out of range"))?;
        let months =
            i32::try_from(months_i64).map_err(|_| anyhow!("interval months out of range"))?;
        let millis = iv
            .millis
            .checked_mul(factor)
            .ok_or_else(|| anyhow!("interval out of range"))?;
        Ok(crate::types::IntervalValue::new(months, millis))
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
                        Ok(Value::Interval(crate::types::IntervalValue::from_millis(
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
                        Ok(Value::Interval(crate::types::IntervalValue::from_millis(
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
    // Approximate PostgreSQL's default collation behavior for ASCII:
    // compare case-insensitively first, then order lowercase before uppercase.
    let left_fold = left.to_ascii_lowercase();
    let right_fold = right.to_ascii_lowercase();
    match left_fold.cmp(&right_fold) {
        std::cmp::Ordering::Equal => {}
        other => return other,
    }

    for (l, r) in left
        .as_bytes()
        .iter()
        .copied()
        .zip(right.as_bytes().iter().copied())
    {
        if l == r {
            continue;
        }

        let l_fold = l.to_ascii_lowercase();
        let r_fold = r.to_ascii_lowercase();
        if l_fold != r_fold {
            return l_fold.cmp(&r_fold);
        }

        let l_is_upper = l.is_ascii_uppercase();
        let r_is_upper = r.is_ascii_uppercase();
        if l_is_upper != r_is_upper {
            return if l_is_upper {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            };
        }

        return l.cmp(&r);
    }

    left.len().cmp(&right.len())
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
        _ => Err(anyhow!("Cannot compare values: {:?} vs {:?}", left, right)),
    }
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
        (Value::Jsonb(_), _) | (_, Value::Jsonb(_)) => {
            return Err(anyhow!(
                "could not identify an ordering operator for type jsonb"
            ))
        }
        (Value::Vector(_), _) | (_, Value::Vector(_)) => {
            return Err(anyhow!(
                "Vectors cannot be directly compared. Use vector distance functions instead."
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

    // Phase 4: Cross-type comparisons are rejected.
    //
    // The Analyzer must insert implicit casts so values are type-compatible
    // before runtime evaluation. Keeping coercion here would be a hidden
    // semantic fallback and a per-row overhead.
    Err(anyhow!("Cannot compare values: {:?} vs {:?}", left, right))
}

/// ORDER BY comparator with PostgreSQL-like NULLS FIRST/LAST semantics.
pub fn compare_order_by_values(
    left: &Value,
    right: &Value,
    asc: bool,
    nulls_first: bool,
) -> Result<std::cmp::Ordering> {
    match (left, right) {
        (Value::Null, Value::Null) => Ok(std::cmp::Ordering::Equal),
        (Value::Null, _) => {
            if nulls_first {
                Ok(std::cmp::Ordering::Less)
            } else {
                Ok(std::cmp::Ordering::Greater)
            }
        }
        (_, Value::Null) => {
            if nulls_first {
                Ok(std::cmp::Ordering::Greater)
            } else {
                Ok(std::cmp::Ordering::Less)
            }
        }
        _ => {
            let cmp = compare_values(left, right)?;
            if cmp == 0 {
                Ok(std::cmp::Ordering::Equal)
            } else if asc {
                if cmp > 0 {
                    Ok(std::cmp::Ordering::Greater)
                } else {
                    Ok(std::cmp::Ordering::Less)
                }
            } else if cmp > 0 {
                Ok(std::cmp::Ordering::Less)
            } else {
                Ok(std::cmp::Ordering::Greater)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bool_pg_accepts_on_off() {
        assert_eq!(parse_bool_pg("on"), Some(true));
        assert_eq!(parse_bool_pg("ON"), Some(true));
        assert_eq!(parse_bool_pg("  oN "), Some(true));

        assert_eq!(parse_bool_pg("off"), Some(false));
        assert_eq!(parse_bool_pg("OFF"), Some(false));
        assert_eq!(parse_bool_pg("\tOff\n"), Some(false));
    }

    #[test]
    fn test_parse_bool_pg_existing_variants() {
        assert_eq!(parse_bool_pg("true"), Some(true));
        assert_eq!(parse_bool_pg("t"), Some(true));
        assert_eq!(parse_bool_pg("yes"), Some(true));
        assert_eq!(parse_bool_pg("y"), Some(true));
        assert_eq!(parse_bool_pg("1"), Some(true));

        assert_eq!(parse_bool_pg("false"), Some(false));
        assert_eq!(parse_bool_pg("f"), Some(false));
        assert_eq!(parse_bool_pg("no"), Some(false));
        assert_eq!(parse_bool_pg("n"), Some(false));
        assert_eq!(parse_bool_pg("0"), Some(false));

        assert_eq!(parse_bool_pg("maybe"), None);
    }

    #[test]
    fn test_add_interval_to_timestamp_millis_min_does_not_panic() {
        let ts_millis = 0;
        let iv = crate::types::IntervalValue::new(0, i64::MIN);

        let err = add_interval_to_timestamp(ts_millis, &iv).unwrap_err();
        assert!(err.to_string().contains("Interval out of range"));
    }

    #[test]
    fn test_add_interval_to_timestamp_month_overflow_does_not_panic() {
        let ts_millis = 0;

        let iv = crate::types::IntervalValue::from_months(i32::MAX);
        let err = add_interval_to_timestamp(ts_millis, &iv).unwrap_err();
        assert!(err.to_string().contains("Date out of range"));

        let iv = crate::types::IntervalValue::from_months(i32::MIN);
        let err = add_interval_to_timestamp(ts_millis, &iv).unwrap_err();
        assert!(err.to_string().contains("Date out of range"));
    }

    #[test]
    fn test_mul_values_interval_by_int() {
        let iv = crate::types::IntervalValue::from_millis(1_000);

        assert_eq!(
            mul_values(Value::Int32(2), Value::Interval(iv)).unwrap(),
            Value::Interval(crate::types::IntervalValue::from_millis(2_000))
        );
        assert_eq!(
            mul_values(Value::Interval(iv), Value::Int64(3)).unwrap(),
            Value::Interval(crate::types::IntervalValue::from_millis(3_000))
        );

        let iv_months = crate::types::IntervalValue::from_months(12);
        assert_eq!(
            mul_values(Value::Interval(iv_months), Value::Int32(2)).unwrap(),
            Value::Interval(crate::types::IntervalValue::from_months(24))
        );
    }

    #[test]
    fn test_mul_values_interval_by_fractional_when_months_zero() {
        let iv = crate::types::IntervalValue::from_millis(1_000);
        assert_eq!(
            mul_values(Value::Interval(iv), Value::Float64(0.5)).unwrap(),
            Value::Interval(crate::types::IntervalValue::from_millis(500))
        );
    }

    #[test]
    fn test_mul_values_interval_fractional_rejects_months_component() {
        let iv = crate::types::IntervalValue::new(1, 0);
        let err = mul_values(Value::Interval(iv), Value::Float64(0.5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("fractional months"));
    }

    #[test]
    fn test_mul_values_interval_by_large_whole_float_overflow() {
        let iv = crate::types::IntervalValue::from_millis(1_000);
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
    fn test_compare_values_rejects_cross_type_without_analyzer_casts() {
        let err = compare_values(&Value::Text("42".into()), &Value::Int32(10))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Cannot compare values"));
    }
}
