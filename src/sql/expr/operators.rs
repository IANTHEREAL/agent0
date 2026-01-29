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

use crate::types::Value;
use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use sqlparser::ast::BinaryOperator;
use std::str::FromStr;

use super::numeric;

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
        BinaryOperator::And => {
            let left = try_coerce_text_to_bool(left);
            let right = try_coerce_text_to_bool(right);
            match (left, right) {
                (Value::Boolean(false), _) | (_, Value::Boolean(false)) => Ok(Value::Boolean(false)),
                (Value::Boolean(true), Value::Boolean(true)) => Ok(Value::Boolean(true)),
                (Value::Boolean(true), Value::Null) | (Value::Null, Value::Boolean(true)) => {
                    Ok(Value::Null)
                }
                (Value::Null, Value::Null) => Ok(Value::Null),
                _ => Err(anyhow!("AND requires boolean operands")),
            }
        },
        BinaryOperator::Or => {
            let left = try_coerce_text_to_bool(left);
            let right = try_coerce_text_to_bool(right);
            match (left, right) {
                (Value::Boolean(true), _) | (_, Value::Boolean(true)) => Ok(Value::Boolean(true)),
                (Value::Boolean(false), Value::Boolean(false)) => Ok(Value::Boolean(false)),
                (Value::Boolean(false), Value::Null) | (Value::Null, Value::Boolean(false)) => {
                    Ok(Value::Null)
                }
                (Value::Null, Value::Null) => Ok(Value::Null),
                _ => Err(anyhow!("OR requires boolean operands")),
            }
        },

        // Arithmetic
        BinaryOperator::Plus => add_values(left, right),
        BinaryOperator::Minus => sub_values(left, right),
        BinaryOperator::Multiply => mul_values(left, right),
        BinaryOperator::Divide => div_values(left, right),
        BinaryOperator::Modulo => mod_values(left, right),

        BinaryOperator::StringConcat => match (&left, &right) {
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
                        if compare_values(lv, rv).unwrap_or(1) == 0 {
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

        _ => Err(anyhow!("Unsupported binary operator: {:?}", op)),
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
        let mut year = result.year();
        let mut month = result.month() as i32 + iv.months;

        while month > 12 {
            month -= 12;
            year += 1;
        }
        while month < 1 {
            month += 12;
            year -= 1;
        }

        let day = result.day().min(days_in_month(year, month as u32));

        result = result
            .with_year(year)
            .and_then(|d| d.with_month(month as u32))
            .and_then(|d| d.with_day(day))
            .ok_or_else(|| anyhow!("Date out of range after adding months"))?;
    }

    if iv.millis != 0 {
        result = result + Duration::milliseconds(iv.millis);
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

pub(super) fn try_coerce_text_to_numeric(v: Value) -> Value {
    match &v {
        Value::Text(s) => {
            if let Ok(i) = s.trim().parse::<i64>() {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    return Value::Int32(i as i32);
                }
                return Value::Int64(i);
            }
            if let Ok(f) = s.trim().parse::<f64>() {
                return Value::Float64(f);
            }
            v
        }
        _ => v,
    }
}

pub(super) fn parse_bool_pg(s: &str) -> Option<bool> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("true")
        || s.eq_ignore_ascii_case("t")
        || s.eq_ignore_ascii_case("yes")
        || s.eq_ignore_ascii_case("y")
        || s == "1"
    {
        Some(true)
    } else if s.eq_ignore_ascii_case("false")
        || s.eq_ignore_ascii_case("f")
        || s.eq_ignore_ascii_case("no")
        || s.eq_ignore_ascii_case("n")
        || s == "0"
    {
        Some(false)
    } else {
        None
    }
}

fn try_coerce_text_to_bool(v: Value) -> Value {
    match v {
        Value::Text(s) => parse_bool_pg(&s).map(Value::Boolean).unwrap_or(Value::Text(s)),
        other => other,
    }
}

pub(super) fn add_values(left: Value, right: Value) -> Result<Value> {
    let left = try_coerce_text_to_numeric(left);
    let right = try_coerce_text_to_numeric(right);

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
        _ => Err(anyhow!("Unsupported types for addition")),
    }
}

pub(super) fn sub_values(left: Value, right: Value) -> Result<Value> {
    if matches!(left, Value::Jsonb(_)) {
        return jsonb_subtract(left, right);
    }

    let left = try_coerce_text_to_numeric(left);
    let right = try_coerce_text_to_numeric(right);

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
        _ => Err(anyhow!("Unsupported types for subtraction")),
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
    let left = try_coerce_text_to_numeric(left);
    let right = try_coerce_text_to_numeric(right);

    if let (Some(l), Some(r)) = (
        numeric::NumericValue::from_value(&left),
        numeric::NumericValue::from_value(&right),
    ) {
        return Ok(numeric::numeric_mul(l, r)?.into_value());
    }

    Err(anyhow!("Unsupported types for multiplication"))
}

fn div_values(left: Value, right: Value) -> Result<Value> {
    let left = try_coerce_text_to_numeric(left);
    let right = try_coerce_text_to_numeric(right);

    if let (Some(l), Some(r)) = (
        numeric::NumericValue::from_value(&left),
        numeric::NumericValue::from_value(&right),
    ) {
        return Ok(numeric::numeric_div(l, r)?.into_value());
    }

    Err(anyhow!("Unsupported types for division"))
}

fn mod_values(left: Value, right: Value) -> Result<Value> {
    if let (Some(l), Some(r)) = (
        numeric::NumericValue::from_value(&left),
        numeric::NumericValue::from_value(&right),
    ) {
        return Ok(numeric::numeric_mod(l, r)?.into_value());
    }

    Err(anyhow!("Unsupported types for modulo"))
}

/// Compare two values. Returns:
/// - 0: equal
/// - 1: left > right
/// - -1: left < right
pub fn compare_values(left: &Value, right: &Value) -> Result<i8> {
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

    match (left, right) {
        (Value::Int32(l), Value::Int32(r)) => Ok(l.cmp(r) as i8),
        (Value::Int64(l), Value::Int64(r)) => Ok(l.cmp(r) as i8),
        (Value::Int32(l), Value::Int64(r)) => Ok((*l as i64).cmp(r) as i8),
        (Value::Int64(l), Value::Int32(r)) => Ok(l.cmp(&(*r as i64)) as i8),
        (Value::Float64(l), Value::Float64(r)) => Ok(compare_float64_pg(*l, *r) as i8),
        (Value::Text(l), Value::Text(r)) => Ok(compare_text_pg(l, r) as i8),
        (Value::Boolean(l), Value::Boolean(r)) => Ok(l.cmp(r) as i8),
        (Value::Boolean(l), Value::Text(t)) => match parse_bool_pg(t) {
            Some(r) => Ok(l.cmp(&r) as i8),
            None => Err(anyhow!("invalid input syntax for type boolean: \"{}\"", t)),
        },
        (Value::Text(t), Value::Boolean(r)) => match parse_bool_pg(t) {
            Some(l) => Ok(l.cmp(r) as i8),
            None => Err(anyhow!("invalid input syntax for type boolean: \"{}\"", t)),
        },
        (Value::Timestamp(l), Value::Timestamp(r)) => Ok(l.cmp(r) as i8),
        (Value::Date(l), Value::Date(r)) => Ok(l.cmp(r) as i8),
        (Value::Date(l), Value::Timestamp(r)) => {
            let l_ts = crate::types::date::date_days_to_timestamp_millis(*l)?;
            Ok(l_ts.cmp(r) as i8)
        }
        (Value::Timestamp(l), Value::Date(r)) => {
            let r_ts = crate::types::date::date_days_to_timestamp_millis(*r)?;
            Ok(l.cmp(&r_ts) as i8)
        }
        (Value::Timestamp(l), Value::Text(r)) => match super::parse_timestamp_string(r)? {
            Value::Timestamp(r_ts) => Ok(l.cmp(&r_ts) as i8),
            _ => Err(anyhow!("Cannot compare")),
        },
        (Value::Text(l), Value::Timestamp(r)) => match super::parse_timestamp_string(l)? {
            Value::Timestamp(l_ts) => Ok(l_ts.cmp(r) as i8),
            _ => Err(anyhow!("Cannot compare")),
        },
        (Value::Date(l), Value::Text(r)) => {
            let r_days = crate::types::date::parse_date_days(r)?;
            Ok(l.cmp(&r_days) as i8)
        }
        (Value::Text(l), Value::Date(r)) => {
            let l_days = crate::types::date::parse_date_days(l)?;
            Ok(l_days.cmp(r) as i8)
        }
        (Value::Uuid(l), Value::Uuid(r)) => Ok(l.cmp(r) as i8),
        (Value::Uuid(l), Value::Text(t)) => {
            if let Ok(r) = uuid::Uuid::parse_str(t) {
                Ok(l.cmp(r.as_bytes()) as i8)
            } else {
                Err(anyhow!("invalid input syntax for type uuid: \"{}\"", t))
            }
        }
        (Value::Text(t), Value::Uuid(r)) => {
            if let Ok(l) = uuid::Uuid::parse_str(t) {
                Ok(l.as_bytes().cmp(r) as i8)
            } else {
                Err(anyhow!("invalid input syntax for type uuid: \"{}\"", t))
            }
        }
        (Value::Bytes(l), Value::Bytes(r)) => Ok(l.cmp(r) as i8),
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
        (Value::Null, Value::Null) => Ok(0),
        (Value::Null, _) => Ok(-1),
        (_, Value::Null) => Ok(1),
        (Value::Text(t), Value::Int32(i)) => {
            if let Ok(n) = t.parse::<i32>() {
                Ok(n.cmp(i) as i8)
            } else {
                Ok(t.cmp(&i.to_string()) as i8)
            }
        }
        (Value::Int32(i), Value::Text(t)) => {
            if let Ok(n) = t.parse::<i32>() {
                Ok(i.cmp(&n) as i8)
            } else {
                Ok(i.to_string().cmp(t) as i8)
            }
        }
        (Value::Text(t), Value::Int64(i)) => {
            if let Ok(n) = t.parse::<i64>() {
                Ok(n.cmp(i) as i8)
            } else {
                Ok(t.cmp(&i.to_string()) as i8)
            }
        }
        (Value::Int64(i), Value::Text(t)) => {
            if let Ok(n) = t.parse::<i64>() {
                Ok(i.cmp(&n) as i8)
            } else {
                Ok(i.to_string().cmp(t) as i8)
            }
        }
        (Value::Text(t), Value::Float64(f)) => {
            if let Ok(n) = t.parse::<f64>() {
                Ok(compare_float64_pg(n, *f) as i8)
            } else {
                Err(anyhow!("Cannot compare"))
            }
        }
        (Value::Float64(f), Value::Text(t)) => {
            if let Ok(n) = t.parse::<f64>() {
                Ok(compare_float64_pg(*f, n) as i8)
            } else {
                Err(anyhow!("Cannot compare"))
            }
        }
        (Value::Int32(i), Value::Float64(f)) => Ok(compare_float64_pg(*i as f64, *f) as i8),
        (Value::Float64(f), Value::Int32(i)) => Ok(compare_float64_pg(*f, *i as f64) as i8),
        (Value::Int64(i), Value::Float64(f)) => Ok(compare_float64_pg(*i as f64, *f) as i8),
        (Value::Float64(f), Value::Int64(i)) => Ok(compare_float64_pg(*f, *i as f64) as i8),
        (Value::Numeric(l), Value::Numeric(r)) => Ok(l.cmp(r) as i8),
        (Value::Numeric(d), Value::Int32(i)) => Ok(d.cmp(&Decimal::from(*i)) as i8),
        (Value::Int32(i), Value::Numeric(d)) => Ok(Decimal::from(*i).cmp(d) as i8),
        (Value::Numeric(d), Value::Int64(i)) => Ok(d.cmp(&Decimal::from(*i)) as i8),
        (Value::Int64(i), Value::Numeric(d)) => Ok(Decimal::from(*i).cmp(d) as i8),
        (Value::Numeric(d), Value::Float64(f)) => {
            if let Some(fd) = Decimal::try_from(*f).ok() {
                Ok(d.cmp(&fd) as i8)
            } else {
                use rust_decimal::prelude::ToPrimitive;
                Ok(compare_float64_pg(d.to_f64().unwrap_or(f64::NAN), *f) as i8)
            }
        }
        (Value::Float64(f), Value::Numeric(d)) => {
            if let Some(fd) = Decimal::try_from(*f).ok() {
                Ok(fd.cmp(d) as i8)
            } else {
                use rust_decimal::prelude::ToPrimitive;
                Ok(compare_float64_pg(*f, d.to_f64().unwrap_or(f64::NAN)) as i8)
            }
        }
        (Value::Numeric(d), Value::Text(t)) => {
            if let Ok(td) = Decimal::from_str(t) {
                Ok(d.cmp(&td) as i8)
            } else {
                Err(anyhow!("Cannot compare numeric with non-numeric string"))
            }
        }
        (Value::Text(t), Value::Numeric(d)) => {
            if let Ok(td) = Decimal::from_str(t) {
                Ok(td.cmp(d) as i8)
            } else {
                Err(anyhow!("Cannot compare numeric with non-numeric string"))
            }
        }
        (Value::Json(_), _) | (_, Value::Json(_)) => Err(anyhow!(
            "could not identify a comparison function for type json"
        )),
        (Value::Jsonb(_), _) | (_, Value::Jsonb(_)) => Err(anyhow!(
            "could not identify an ordering operator for type jsonb"
        )),
        (Value::Vector(_), _) | (_, Value::Vector(_)) => Err(anyhow!(
            "Vectors cannot be directly compared. Use vector distance functions instead."
        )),
        _ => Err(anyhow!(
            "Cannot compare distinct types: {:?} vs {:?}",
            left,
            right
        )),
    }
}

/// ORDER BY comparator with PostgreSQL-like NULLS FIRST/LAST semantics.
pub fn compare_order_by_values(
    left: &Value,
    right: &Value,
    asc: bool,
    nulls_first: bool,
) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => {
            if nulls_first {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if nulls_first {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }
        _ => {
            let cmp = compare_values(left, right).unwrap_or(0);
            if cmp == 0 {
                std::cmp::Ordering::Equal
            } else if asc {
                if cmp > 0 {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            } else if cmp > 0 {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
    }
}
