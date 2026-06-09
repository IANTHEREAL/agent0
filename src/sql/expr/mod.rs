//! Expression evaluation logic

pub mod bridge;
pub mod classify;
pub(crate) mod collation_aware;
pub mod compile;
pub mod functions;
pub(crate) mod helpers;
pub(crate) mod numeric;
pub(crate) mod operators;
pub mod static_eval;
pub(crate) mod traverse;
pub mod typed_eval;
pub mod typed_fold;
pub mod typed_rewrite;
pub mod typed_visit;

use crate::model::{IntervalValue, Value};
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use sqlparser::ast::JsonOperator;

pub(crate) const VERSION_STRING: &str = concat!(
    "PostgreSQL 16.0 (db9-server ",
    env!("CARGO_PKG_VERSION"),
    "-",
    env!("BUILD_GIT_HASH"),
    " ",
    env!("BUILD_DATE"),
    " on TiKV)"
);

const MAX_ARRAY_RECURSION_DEPTH: usize = 64;

pub(crate) fn like_match(
    s: &str,
    pattern: &str,
    escape_char: Option<&str>,
    case_insensitive: bool,
) -> Result<bool> {
    let escape_char = escape_char.and_then(|escape| escape.chars().next());
    like_match_impl(s, pattern, escape_char, case_insensitive)
}

pub(crate) fn simple_unicode_lower_char(ch: char) -> char {
    ch.to_lowercase().next().unwrap_or(ch)
}

pub(crate) fn simple_unicode_upper_char(ch: char) -> char {
    let mut mapped = ch.to_uppercase();
    match (mapped.next(), mapped.next()) {
        (Some(single), None) => single,
        _ => ch,
    }
}

fn like_char_eq(left: char, right: char, case_insensitive: bool) -> bool {
    if left == right {
        return true;
    }
    if !case_insensitive {
        return false;
    }
    simple_unicode_lower_char(left) == simple_unicode_lower_char(right)
}

fn like_match_impl(
    s: &str,
    pattern: &str,
    escape_char: Option<char>,
    case_insensitive: bool,
) -> Result<bool> {
    #[inline]
    fn next_char_at(s: &str, idx: usize) -> Option<(char, usize)> {
        let ch = s[idx..].chars().next()?;
        Some((ch, idx + ch.len_utf8()))
    }

    let mut s_idx = 0usize;
    let mut p_idx = 0usize;

    // Backtracking positions for the most recent '%'.
    let mut backtrack_p: Option<usize> = None;
    let mut backtrack_s: usize = 0;

    while s_idx < s.len() {
        if p_idx < pattern.len() {
            let (pc, p_next) = next_char_at(pattern, p_idx).expect("p_idx < len");

            if escape_char.is_some_and(|esc| pc == esc) {
                // Escape: treat the next pattern character as a literal.
                let Some((lit, p_after)) = next_char_at(pattern, p_next) else {
                    return Err(SqlError::InvalidEscapeString {
                        message: "LIKE pattern must not end with escape character".to_owned(),
                    }
                    .into());
                };
                if let Some((sc, s_next)) = next_char_at(s, s_idx) {
                    if like_char_eq(sc, lit, case_insensitive) {
                        s_idx = s_next;
                        p_idx = p_after;
                        continue;
                    }
                }
            } else if pc == '%' {
                // Collapse consecutive '%' and record the backtracking point.
                let mut p_after = p_next;
                while p_after < pattern.len() {
                    let (next_pc, next_next) =
                        next_char_at(pattern, p_after).expect("p_after < len");
                    if next_pc != '%' {
                        break;
                    }
                    p_after = next_next;
                }
                backtrack_p = Some(p_after);
                backtrack_s = s_idx;
                p_idx = p_after;
                continue;
            } else if pc == '_' {
                // Match any single character.
                if let Some((_sc, s_next)) = next_char_at(s, s_idx) {
                    s_idx = s_next;
                    p_idx = p_next;
                    continue;
                }
            } else if let Some((sc, s_next)) = next_char_at(s, s_idx) {
                if like_char_eq(sc, pc, case_insensitive) {
                    s_idx = s_next;
                    p_idx = p_next;
                    continue;
                }
            }
        }

        // Mismatch: if we have a previous '%', backtrack and let it consume one more character.
        if let Some(p_after_percent) = backtrack_p {
            if backtrack_s < s.len() {
                let (_sc, s_next) = next_char_at(s, backtrack_s).expect("backtrack_s < len");
                backtrack_s = s_next;
                s_idx = backtrack_s;
                p_idx = p_after_percent;
                continue;
            }
        }

        return Ok(false);
    }

    // String is consumed; the remaining pattern must be empty or all '%'.
    while p_idx < pattern.len() {
        let (pc, p_next) = next_char_at(pattern, p_idx).expect("p_idx < len");
        if escape_char.is_some_and(|esc| pc == esc) {
            return Ok(false);
        }
        if pc != '%' {
            return Ok(false);
        }
        p_idx = p_next;
    }

    Ok(true)
}

pub(crate) fn similar_to_match(s: &str, pattern: &str, escape_char: Option<&str>) -> Result<bool> {
    let escape_char = escape_char.and_then(|escape| escape.chars().next());
    let mut regex_pattern = String::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if escape_char.is_some_and(|escape| ch == escape) {
            // Treat escaped character as a literal.
            if let Some(next) = chars.next() {
                regex_pattern.push_str(&regex::escape(&next.to_string()));
            } else {
                return Ok(false);
            }
            continue;
        }

        match ch {
            '%' => regex_pattern.push_str(".*"),
            '_' => regex_pattern.push('.'),
            '\\' => regex_pattern.push_str("\\\\"),
            other => regex_pattern.push(other),
        }
    }

    let re = regex::Regex::new(&format!("^{}$", regex_pattern))
        .map_err(|e| crate::sql::expr::functions::regex::invalid_regular_expression_error(&e))?;
    Ok(re.is_match(s))
}

fn checked_interval_component(value: f64, field_name: &str) -> Result<i64> {
    let truncated = value.trunc();
    if !truncated.is_finite() || truncated < i64::MIN as f64 || truncated > i64::MAX as f64 {
        return Err(anyhow!("Interval {field_name} is out of range"));
    }
    Ok(truncated as i64)
}

fn checked_add_interval_millis(total: &mut i64, delta: i64, field_name: &str) -> Result<()> {
    *total = total
        .checked_add(delta)
        .ok_or_else(|| anyhow!("Interval {field_name} overflowed"))?;
    Ok(())
}

fn checked_add_interval_months(total: &mut i32, delta: i32, field_name: &str) -> Result<()> {
    *total = total
        .checked_add(delta)
        .ok_or_else(|| anyhow!("Interval {field_name} overflowed"))?;
    Ok(())
}

fn checked_fractional_interval_millis(
    value: f64,
    unit_millis: f64,
    field_name: &str,
) -> Result<i64> {
    checked_interval_component(value * unit_millis, field_name)
}

fn checked_truncated_interval_months(value: f64, field_name: &str) -> Result<i32> {
    if !value.is_finite() || value < i32::MIN as f64 || value > i32::MAX as f64 {
        return Err(anyhow!("Interval {field_name} is out of range"));
    }
    Ok(value.trunc() as i32)
}

fn checked_rounded_interval_months(value: f64, field_name: &str) -> Result<i32> {
    let rounded = value.round_ties_even();
    if !rounded.is_finite() || rounded < i32::MIN as f64 || rounded > i32::MAX as f64 {
        return Err(anyhow!("Interval {field_name} is out of range"));
    }
    Ok(rounded as i32)
}

// PostgreSQL stores intervals as months + time. Fractional months spill into the
// time bucket using a 30-day month, while fractional years round to whole months.
fn checked_fractional_month_interval(value: f64) -> Result<(i32, i64)> {
    const MONTH_MILLIS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;

    let months = checked_truncated_interval_months(value, "month")?;
    let millis = checked_fractional_interval_millis(value - months as f64, MONTH_MILLIS, "month")?;
    Ok((months, millis))
}

fn parse_interval_clock_component(token: &str) -> Result<i64> {
    let token = token.trim();
    let (sign, raw) = match token.as_bytes().first() {
        Some(b'-') => (-1_i64, &token[1..]),
        Some(b'+') => (1_i64, &token[1..]),
        _ => (1_i64, token),
    };
    let parts: Vec<&str> = raw.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return Err(anyhow!("Invalid interval time component: {token}"));
    }

    let hours: i64 = parts[0]
        .trim()
        .parse()
        .map_err(|_| anyhow!("Invalid interval time component: {token}"))?;
    let mins: i64 = parts[1]
        .trim()
        .parse()
        .map_err(|_| anyhow!("Invalid interval time component: {token}"))?;
    let secs: f64 = if parts.len() > 2 {
        parts[2]
            .trim()
            .parse()
            .map_err(|_| anyhow!("Invalid interval time component: {token}"))?
    } else {
        0.0
    };

    let total = hours
        .checked_mul(3_600_000)
        .and_then(|v| v.checked_add(mins.checked_mul(60_000)?))
        .and_then(|v| {
            v.checked_add(checked_fractional_interval_millis(secs, 1000.0, "second").ok()?)
        })
        .ok_or_else(|| anyhow!("Interval time component overflowed"))?;

    total
        .checked_mul(sign)
        .ok_or_else(|| anyhow!("Interval time component overflowed"))
}

fn interval_unit_matches(unit: &str, candidates: &[&str]) -> bool {
    candidates.contains(&unit)
}

pub(crate) fn parse_interval_value(s: &str) -> Result<IntervalValue> {
    let mut total_months = 0i32;
    let mut total_ms = 0i64;
    let s = s.trim();

    let lower = s.to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].contains(':') {
            let time = parse_interval_clock_component(tokens[i])?;
            checked_add_interval_millis(&mut total_ms, time, "time component")?;
            i += 1;
            continue;
        }

        let Some(num) = tokens[i].parse::<f64>().ok() else {
            return Err(anyhow!("Invalid interval syntax: {}", tokens[i]));
        };

        let unit = tokens.get(i + 1).copied().unwrap_or("");
        let consumed = match unit {
            "" => {
                let seconds = checked_fractional_interval_millis(num, 1000.0, "second")?;
                checked_add_interval_millis(&mut total_ms, seconds, "second")?;
                1
            }
            u if interval_unit_matches(u, &["year", "years"]) => {
                let months = checked_rounded_interval_months(num * 12.0, "year")?;
                checked_add_interval_months(&mut total_months, months, "year")?;
                2
            }
            u if interval_unit_matches(u, &["month", "months", "mon", "mons"]) => {
                let (months, millis) = checked_fractional_month_interval(num)?;
                checked_add_interval_months(&mut total_months, months, "month")?;
                checked_add_interval_millis(&mut total_ms, millis, "month")?;
                2
            }
            u if interval_unit_matches(u, &["week", "weeks"]) => {
                let weeks = checked_fractional_interval_millis(
                    num,
                    7.0 * 24.0 * 60.0 * 60.0 * 1000.0,
                    "week",
                )?;
                checked_add_interval_millis(&mut total_ms, weeks, "week")?;
                2
            }
            u if interval_unit_matches(u, &["day", "days"]) => {
                let days =
                    checked_fractional_interval_millis(num, 24.0 * 60.0 * 60.0 * 1000.0, "day")?;
                checked_add_interval_millis(&mut total_ms, days, "day")?;
                2
            }
            u if interval_unit_matches(u, &["hour", "hours"]) => {
                let hours = checked_fractional_interval_millis(num, 60.0 * 60.0 * 1000.0, "hour")?;
                checked_add_interval_millis(&mut total_ms, hours, "hour")?;
                2
            }
            u if interval_unit_matches(u, &["minute", "minutes", "min", "mins"]) => {
                let minutes = checked_fractional_interval_millis(num, 60.0 * 1000.0, "minute")?;
                checked_add_interval_millis(&mut total_ms, minutes, "minute")?;
                2
            }
            u if interval_unit_matches(u, &["second", "seconds", "sec", "secs"]) => {
                let seconds = checked_fractional_interval_millis(num, 1000.0, "second")?;
                checked_add_interval_millis(&mut total_ms, seconds, "second")?;
                2
            }
            u if interval_unit_matches(u, &["millisecond", "milliseconds", "ms"]) => {
                let millis = checked_interval_component(num, "millisecond")?;
                checked_add_interval_millis(&mut total_ms, millis, "millisecond")?;
                2
            }
            _ => return Err(anyhow!("Unknown interval unit: {}", unit)),
        };
        i += consumed;
    }

    Ok(IntervalValue::new(total_months, total_ms))
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn parse_interval_value_rejects_trailing_garbage() {
        let err = parse_interval_value("1 day garbage").unwrap_err();
        assert!(err.to_string().contains("Invalid interval syntax"));
    }

    #[test]
    fn parse_interval_value_rejects_prefix_matching_garbage_units() {
        for input in ["1 monkey", "2 daylight", "3 secondszzz", "5 weekend"] {
            let err = parse_interval_value(input).unwrap_err();
            assert!(
                err.to_string().contains("Unknown interval unit"),
                "expected unknown interval unit for {input}, got: {err}"
            );
        }
    }

    #[test]
    fn parse_interval_value_accepts_exact_pg_unit_tokens() {
        assert_eq!(
            parse_interval_value("1 mon").unwrap(),
            parse_interval_value("1 month").unwrap()
        );
        assert_eq!(
            parse_interval_value("1 mons").unwrap(),
            parse_interval_value("1 months").unwrap()
        );
        assert_eq!(
            parse_interval_value("1 min").unwrap(),
            parse_interval_value("1 minute").unwrap()
        );
        assert_eq!(
            parse_interval_value("1 secs").unwrap(),
            parse_interval_value("1 second").unwrap()
        );
        assert_eq!(
            parse_interval_value("1 milliseconds").unwrap(),
            parse_interval_value("1 ms").unwrap()
        );
    }

    #[test]
    fn eval_json_access_json_arrow_preserves_raw_order() {
        assert_eq!(
            eval_json_access(
                Value::Json(r#"{"b":2,"a":{"y":2,"x":1}}"#.into()),
                &JsonOperator::Arrow,
                Value::Text("a".into()),
            )
            .unwrap(),
            Value::Json(r#"{"y":2,"x":1}"#.into())
        );
    }

    #[test]
    fn eval_json_access_json_hash_long_arrow_preserves_raw_text_order() {
        assert_eq!(
            eval_json_access(
                Value::Json(r#"{"b":2,"a":{"y":2,"x":1}}"#.into()),
                &JsonOperator::HashLongArrow,
                Value::Text("{a}".into()),
            )
            .unwrap(),
            Value::Text(r#"{"y":2,"x":1}"#.into())
        );
    }

    #[test]
    fn eval_json_access_jsonb_long_arrow_uses_pg_jsonb_text() {
        assert_eq!(
            eval_json_access(
                Value::Jsonb(r#"{"b":2,"a":{"y":2,"x":1}}"#.into()),
                &JsonOperator::LongArrow,
                Value::Text("a".into()),
            )
            .unwrap(),
            Value::Text(r#"{"x": 1, "y": 2}"#.into())
        );
    }

    #[test]
    fn eval_json_access_jsonb_arrow_uses_pg_jsonb_text() {
        assert_eq!(
            eval_json_access(
                Value::Jsonb(r#"{"b":2,"a":{"y":2,"x":1}}"#.into()),
                &JsonOperator::Arrow,
                Value::Text("a".into()),
            )
            .unwrap(),
            Value::Jsonb(r#"{"x": 1, "y": 2}"#.into())
        );
        assert_eq!(
            eval_json_access(
                Value::Jsonb(r#"{"n":9007199254740993.123456789}"#.into()),
                &JsonOperator::Arrow,
                Value::Text("n".into()),
            )
            .unwrap(),
            Value::Jsonb("9007199254740993.123456789".into())
        );
        assert_eq!(
            eval_json_access(
                Value::Jsonb(r#"{"n":9007199254740993.123456789}"#.into()),
                &JsonOperator::LongArrow,
                Value::Text("n".into()),
            )
            .unwrap(),
            Value::Text("9007199254740993.123456789".into())
        );
        assert_eq!(
            eval_json_access(
                Value::Jsonb(r#"{"b":2,"a":{"y":2,"x":1}}"#.into()),
                &JsonOperator::HashArrow,
                Value::Text("{a}".into()),
            )
            .unwrap(),
            Value::Jsonb(r#"{"x": 1, "y": 2}"#.into())
        );
        assert_eq!(
            eval_json_access(
                Value::Jsonb(r#"{"n":9007199254740993.123456789,"drop":1}"#.into()),
                &JsonOperator::HashMinus,
                Value::Text("{drop}".into()),
            )
            .unwrap(),
            Value::Jsonb(r#"{"n": 9007199254740993.123456789}"#.into())
        );
        let err = eval_json_access(
            Value::Json(r#"{"n":1,"drop":2}"#.into()),
            &JsonOperator::HashMinus,
            Value::Text("{drop}".into()),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("#- requires jsonb operand on left"));
    }

    #[test]
    fn eval_json_access_jsonb_hash_long_arrow_supports_quoted_comma_key() {
        assert_eq!(
            eval_json_access(
                Value::Jsonb(r#"{"a,b":1}"#.into()),
                &JsonOperator::HashLongArrow,
                Value::Text("{\"a,b\"}".into()),
            )
            .unwrap(),
            Value::Text("1".into())
        );
    }

    #[test]
    fn eval_json_access_hash_long_arrow_null_path_returns_null() {
        assert_eq!(
            eval_json_access(
                Value::Jsonb(r#"{"a":1}"#.into()),
                &JsonOperator::HashLongArrow,
                Value::Null,
            )
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn parse_pg_text_array_literal_handles_quotes_escapes_and_sql_nulls() {
        assert_eq!(
            parse_pg_text_array_literal("{\"a,b\",\"a\\\"b\",NULL}").unwrap(),
            vec![Some("a,b".into()), Some("a\"b".into()), None]
        );
    }

    #[test]
    fn eval_json_access_array_contains_flattens_nested_elements() {
        let left = Value::Array(vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
        ]);
        let right = Value::Array(vec![Value::Int32(2), Value::Int32(3)]);

        let result = eval_json_access(left, &JsonOperator::AtArrow, right).unwrap();
        assert_eq!(result, Value::Boolean(true));
    }

    #[test]
    fn eval_json_access_array_contained_by_flattens_nested_elements() {
        let left = Value::Array(vec![Value::Int32(2), Value::Int32(3)]);
        let right = Value::Array(vec![
            Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
            Value::Array(vec![Value::Int32(3), Value::Int32(4)]),
        ]);

        let result = eval_json_access(left, &JsonOperator::ArrowAt, right).unwrap();
        assert_eq!(result, Value::Boolean(true));
    }

    #[test]
    fn eval_json_access_array_operators_do_not_match_null_elements() {
        assert_eq!(
            eval_json_access(
                Value::Array(vec![Value::Int32(1), Value::Null]),
                &JsonOperator::AtArrow,
                Value::Array(vec![Value::Null]),
            )
            .unwrap(),
            Value::Boolean(false)
        );
        assert!(!array_overlap_pg(&[Value::Null], &[Value::Null]).unwrap());
    }
}

pub(crate) fn parse_interval_string(s: &str) -> Result<Value> {
    Ok(Value::Interval(parse_interval_value(s)?))
}

pub(crate) fn parse_timestamp_string(s: &str) -> Result<Value> {
    let trimmed = s.trim();

    if trimmed.eq_ignore_ascii_case("infinity") {
        return Ok(Value::Timestamp(i64::MAX));
    }
    if trimmed.eq_ignore_ascii_case("-infinity") {
        return Ok(Value::Timestamp(i64::MIN));
    }

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Ok(Value::Timestamp(dt.timestamp_millis()));
    }

    // PostgreSQL accepts TIMESTAMPTZ inputs like:
    // - `YYYY-MM-DD HH:MM:SS[.ffffff]+HH:MM`
    // - `YYYY-MM-DD HH:MM:SS[.ffffff] +HH:MM`
    // and similar forms with `T` separators. Many ORMs/JDBC/JS stacks emit these.
    {
        use chrono::DateTime;

        // `chrono` supports `%:z` for `+HH:MM` and `%z` for `+HHMM`.
        // Try the common Postgres-style layouts that are not RFC3339 (missing `T`).
        let tz_formats = [
            "%Y-%m-%d %H:%M:%S%.f%:z",
            "%Y-%m-%d %H:%M:%S%:z",
            "%Y-%m-%d %H:%M:%S%.f %:z",
            "%Y-%m-%d %H:%M:%S %:z",
            "%Y-%m-%dT%H:%M:%S%.f%:z",
            "%Y-%m-%dT%H:%M:%S%:z",
            "%Y-%m-%dT%H:%M:%S%.f %:z",
            "%Y-%m-%dT%H:%M:%S %:z",
            "%Y-%m-%d %H:%M:%S%.f%z",
            "%Y-%m-%d %H:%M:%S%z",
            "%Y-%m-%d %H:%M:%S%.f %z",
            "%Y-%m-%d %H:%M:%S %z",
            "%Y-%m-%dT%H:%M:%S%.f%z",
            "%Y-%m-%dT%H:%M:%S%z",
            "%Y-%m-%dT%H:%M:%S%.f %z",
            "%Y-%m-%dT%H:%M:%S %z",
        ];

        for fmt in &tz_formats {
            if let Ok(dt) = DateTime::parse_from_str(trimmed, fmt) {
                return Ok(Value::Timestamp(dt.timestamp_millis()));
            }
        }

        // Also accept `+HH` / `-HH` offsets by normalizing them to `+HH:00`.
        if trimmed.len() >= 3 {
            let bytes = trimmed.as_bytes();
            let len = bytes.len();
            let sign = bytes[len - 3];
            let d1 = bytes[len - 2];
            let d2 = bytes[len - 1];
            if matches!(sign, b'+' | b'-') && d1.is_ascii_digit() && d2.is_ascii_digit() {
                let normalized = format!("{trimmed}:00");
                for fmt in &tz_formats {
                    if let Ok(dt) = DateTime::parse_from_str(&normalized, fmt) {
                        return Ok(Value::Timestamp(dt.timestamp_millis()));
                    }
                }
            }
        }
    }

    use chrono::{NaiveDateTime, TimeZone, Utc};
    let formats = [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d",
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d",
    ];
    for fmt in &formats {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s.trim(), fmt) {
            return Ok(Value::Timestamp(
                Utc.from_utc_datetime(&dt).timestamp_millis(),
            ));
        }
    }
    if let Ok(dt) = chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
        let datetime = dt
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow!("Failed to create datetime from date"))?;
        return Ok(Value::Timestamp(
            Utc.from_utc_datetime(&datetime).timestamp_millis(),
        ));
    }
    Err(anyhow!("Cannot parse timestamp: {}", s))
}

pub fn compare_values(left: &Value, right: &Value) -> Result<i8> {
    operators::compare_values(left, right)
}

pub fn compare_order_by_values(
    left: &Value,
    right: &Value,
    asc: bool,
    nulls_first: bool,
) -> anyhow::Result<std::cmp::Ordering> {
    operators::compare_order_by_values(left, right, asc, nulls_first)
}

pub fn compare_order_by_values_collated(
    left: &Value,
    right: &Value,
    asc: bool,
    nulls_first: bool,
    collation: Option<&crate::sql::collation::ResolvedCollation>,
) -> anyhow::Result<std::cmp::Ordering> {
    operators::compare_order_by_values_collated(left, right, asc, nulls_first, collation)
}

pub(crate) fn eval_json_access(
    left: Value,
    operator: &JsonOperator,
    right: Value,
) -> Result<Value> {
    // Handle @@ operator for full-text search (tsvector @@ tsquery)
    if matches!(operator, JsonOperator::AtAt) {
        return super::fts::ts_match(&left, &right);
    }

    // `@>`/`<@` are overloaded by PostgreSQL for both SQL arrays and JSONB.
    if let Value::Array(left_arr) = &left {
        match operator {
            JsonOperator::AtArrow => {
                let Value::Array(right_arr) = &right else {
                    return Err(anyhow!("@> on arrays requires array operand on right"));
                };
                return Ok(Value::Boolean(array_contains_pg(left_arr, right_arr)?));
            }
            JsonOperator::ArrowAt => {
                let Value::Array(right_arr) = &right else {
                    return Err(anyhow!("<@ on arrays requires array operand on right"));
                };
                return Ok(Value::Boolean(array_contains_pg(right_arr, left_arr)?));
            }
            _ => {
                return Err(SqlError::Unsupported(format!(
                    "Unsupported operator for arrays: {:?}",
                    operator
                ))
                .into());
            }
        }
    }

    let left_is_jsonb = matches!(&left, Value::Jsonb(_));
    let json_str = match left {
        Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
        Value::Null => return Ok(Value::Null),
        Value::Vector(v) => format!(
            "[{}]",
            v.iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => return Err(anyhow!("JSON operators require json/jsonb operand")),
    };

    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

    match operator {
        JsonOperator::AtArrow => {
            let right_str = match right {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                _ => return Err(anyhow!("@> requires json/jsonb operand on right")),
            };
            Ok(Value::Boolean(super::jsonb::contains_str(
                &json_str, &right_str,
            )?))
        }
        JsonOperator::ArrowAt => {
            let right_str = match right {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                _ => return Err(anyhow!("<@ requires json/jsonb operand on right")),
            };
            Ok(Value::Boolean(super::jsonb::contains_str(
                &right_str, &json_str,
            )?))
        }
        JsonOperator::HashArrow | JsonOperator::HashLongArrow | JsonOperator::HashMinus => {
            let Some(path) = json_path_from_value(&right)? else {
                return Ok(Value::Null);
            };
            if !left_is_jsonb && matches!(operator, JsonOperator::HashMinus) {
                return Err(anyhow!("#- requires jsonb operand on left"));
            }
            if !left_is_jsonb && !matches!(operator, JsonOperator::HashMinus) {
                return match operator {
                    JsonOperator::HashArrow => {
                        match functions::json::extract_json_path_raw(&json_str, &path) {
                            Some(raw) => Ok(Value::Json(raw.to_string())),
                            None => Ok(Value::Null),
                        }
                    }
                    JsonOperator::HashLongArrow => {
                        match functions::json::extract_json_path_raw(&json_str, &path) {
                            Some(raw) => Ok(functions::json::json_text_value_from_raw(raw)),
                            None => Ok(Value::Null),
                        }
                    }
                    _ => unreachable!(),
                };
            }
            if left_is_jsonb {
                return match operator {
                    JsonOperator::HashArrow => {
                        match functions::json::extract_json_path_raw(&json_str, &path) {
                            Some(raw) => Ok(functions::json::jsonb_value_from_raw(raw)),
                            None => Ok(Value::Null),
                        }
                    }
                    JsonOperator::HashLongArrow => {
                        match functions::json::extract_json_path_raw(&json_str, &path) {
                            Some(raw) => Ok(functions::json::jsonb_text_value_from_raw(raw)),
                            None => Ok(Value::Null),
                        }
                    }
                    JsonOperator::HashMinus => Ok(Value::Jsonb(
                        functions::json::delete_json_path_raw(&json_str, &path)?,
                    )),
                    _ => unreachable!(),
                };
            }
            match operator {
                JsonOperator::HashArrow => match json_get_path(&json_val, &path) {
                    Some(val) => Ok(Value::Jsonb(functions::json::format_jsonb_pg(val)?)),
                    None => Ok(Value::Null),
                },
                JsonOperator::HashLongArrow => match json_get_path(&json_val, &path) {
                    None => Ok(Value::Null),
                    Some(serde_json::Value::Null) => Ok(Value::Null),
                    Some(serde_json::Value::String(s)) => Ok(Value::Text(s.clone())),
                    Some(other) => Ok(Value::Text(functions::json::format_jsonb_pg(other)?)),
                },
                JsonOperator::HashMinus => unreachable!("json #- is rejected above"),
                _ => Err(SqlError::Unsupported(format!(
                    "Unsupported JSON operator: {:?}",
                    operator
                ))
                .into()),
            }
        }
        _ => {
            if !left_is_jsonb {
                if matches!(right, Value::Null) {
                    return Ok(Value::Null);
                }
                let step = json_access_step_from_value(&right)?;
                return match functions::json::extract_json_path_raw(&json_str, &[step]) {
                    None => Ok(Value::Null),
                    Some(raw) => match operator {
                        JsonOperator::Arrow => Ok(Value::Json(raw.to_string())),
                        JsonOperator::LongArrow => {
                            Ok(functions::json::json_text_value_from_raw(raw))
                        }
                        _ => Err(SqlError::Unsupported(format!(
                            "Unsupported JSON operator: {:?}",
                            operator
                        ))
                        .into()),
                    },
                };
            }

            if left_is_jsonb {
                let raw = match right {
                    Value::Text(key) => {
                        functions::json::extract_json_object_key_raw(&json_str, &key)
                    }
                    Value::Int32(idx) => {
                        functions::json::extract_json_array_index_raw(&json_str, i64::from(idx))
                    }
                    Value::Int64(idx) => {
                        functions::json::extract_json_array_index_raw(&json_str, idx)
                    }
                    Value::Null => return Ok(Value::Null),
                    _ => return Err(anyhow!("JSON key must be text or integer")),
                };

                return match raw {
                    None => Ok(Value::Null),
                    Some(raw) => match operator {
                        JsonOperator::Arrow => Ok(functions::json::jsonb_value_from_raw(raw)),
                        JsonOperator::LongArrow => {
                            Ok(functions::json::jsonb_text_value_from_raw(raw))
                        }
                        _ => Err(SqlError::Unsupported(format!(
                            "Unsupported JSON operator: {:?}",
                            operator
                        ))
                        .into()),
                    },
                };
            }

            let accessed = match right {
                Value::Text(key) => json_val.get(&key),
                Value::Int32(idx) => {
                    if let Some(arr) = json_val.as_array() {
                        let idx = if idx < 0 {
                            (arr.len() as i32 + idx) as usize
                        } else {
                            idx as usize
                        };
                        arr.get(idx)
                    } else {
                        None
                    }
                }
                Value::Int64(idx) => {
                    if let Some(arr) = json_val.as_array() {
                        let idx = if idx < 0 {
                            (arr.len() as i64 + idx) as usize
                        } else {
                            idx as usize
                        };
                        arr.get(idx)
                    } else {
                        None
                    }
                }
                Value::Null => return Ok(Value::Null),
                _ => return Err(anyhow!("JSON key must be text or integer")),
            };

            match accessed {
                None => Ok(Value::Null),
                Some(val) => match operator {
                    JsonOperator::Arrow => Ok(Value::Jsonb(functions::json::format_jsonb_pg(val)?)),
                    JsonOperator::LongArrow => match val {
                        serde_json::Value::Null => Ok(Value::Null),
                        serde_json::Value::Bool(b) => Ok(Value::Text(b.to_string())),
                        serde_json::Value::Number(n) => Ok(Value::Text(n.to_string())),
                        serde_json::Value::String(s) => Ok(Value::Text(s.clone())),
                        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                            Ok(Value::Text(functions::json::format_jsonb_pg(val)?))
                        }
                    },
                    _ => Err(SqlError::Unsupported(format!(
                        "Unsupported JSON operator: {:?}",
                        operator
                    ))
                    .into()),
                },
            }
        }
    }
}

pub(crate) fn array_contains_pg(container: &[Value], containee: &[Value]) -> Result<bool> {
    let mut container_leaves = Vec::new();
    let mut containee_leaves = Vec::new();
    collect_array_leaf_values(container, &mut container_leaves, 0)?;
    collect_array_leaf_values(containee, &mut containee_leaves, 0)?;

    for needle in containee_leaves {
        let mut found = false;
        for hay in &container_leaves {
            if array_operator_values_equal(hay, needle)? {
                found = true;
                break;
            }
        }
        if !found {
            return Ok(false);
        }
    }

    Ok(true)
}

pub(crate) fn array_overlap_pg(left: &[Value], right: &[Value]) -> Result<bool> {
    let mut left_leaves = Vec::new();
    let mut right_leaves = Vec::new();
    collect_array_leaf_values(left, &mut left_leaves, 0)?;
    collect_array_leaf_values(right, &mut right_leaves, 0)?;

    for left_value in left_leaves {
        for right_value in &right_leaves {
            if array_operator_values_equal(left_value, right_value)? {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

fn array_operator_values_equal(left: &Value, right: &Value) -> Result<bool> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(false);
    }
    Ok(compare_values(left, right)? == 0)
}

fn collect_array_leaf_values<'a>(
    values: &'a [Value],
    out: &mut Vec<&'a Value>,
    depth: usize,
) -> Result<()> {
    if depth >= MAX_ARRAY_RECURSION_DEPTH {
        return Err(anyhow!("array value is too deep"));
    }
    for value in values {
        match value {
            Value::Array(nested) => collect_array_leaf_values(nested, out, depth + 1)?,
            other => out.push(other),
        }
    }
    Ok(())
}

fn json_access_step_from_value(path: &Value) -> Result<String> {
    match path {
        Value::Text(s) => Ok(s.clone()),
        Value::Int32(idx) => Ok(idx.to_string()),
        Value::Int64(idx) => Ok(idx.to_string()),
        Value::Null => Err(anyhow!("JSON key must be text or integer")),
        _ => Err(anyhow!("JSON key must be text or integer")),
    }
}

fn parse_pg_text_array_literal(s: &str) -> Result<Vec<Option<String>>> {
    let trimmed = s.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        if inner.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut escape_next = false;
        let mut quoted = false;
        let mut saw_non_whitespace = false;

        for ch in inner.chars() {
            if escape_next {
                current.push(ch);
                escape_next = false;
                saw_non_whitespace = true;
                continue;
            }

            match ch {
                '\\' => {
                    escape_next = true;
                }
                '"' if !in_quotes && !saw_non_whitespace => {
                    in_quotes = true;
                    quoted = true;
                    saw_non_whitespace = true;
                }
                '"' if in_quotes => {
                    in_quotes = false;
                }
                ',' if !in_quotes => {
                    let element = if quoted {
                        Some(current.clone())
                    } else {
                        let trimmed = current.trim();
                        if trimmed.eq_ignore_ascii_case("NULL") {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    };
                    out.push(element);
                    current.clear();
                    quoted = false;
                    saw_non_whitespace = false;
                }
                other => {
                    if !quoted || !other.is_whitespace() || saw_non_whitespace {
                        current.push(other);
                        if !other.is_whitespace() {
                            saw_non_whitespace = true;
                        }
                    }
                }
            }
        }

        if escape_next || in_quotes {
            return Err(anyhow!("invalid text[] path literal"));
        }

        let element = if quoted {
            Some(current)
        } else {
            let trimmed = current.trim();
            if trimmed.eq_ignore_ascii_case("NULL") {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        out.push(element);
        return Ok(out);
    }
    Ok(vec![Some(trimmed.to_string())])
}

fn json_path_from_value(path: &Value) -> Result<Option<Vec<String>>> {
    match path {
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for value in arr {
                match value {
                    Value::Null => return Ok(None),
                    Value::Text(s) => out.push(s.clone()),
                    other => out.push(other.to_string()),
                }
            }
            Ok(Some(out))
        }
        Value::Text(s) => parse_pg_text_array_literal(s)
            .map(|parts| parts.into_iter().collect::<Option<Vec<_>>>()),
        Value::Null => Ok(None),
        other => Err(anyhow!(
            "JSON path must be text[] or array literal, got {:?}",
            other
        )),
    }
}

fn json_get_path<'a>(
    mut current: &'a serde_json::Value,
    path: &[String],
) -> Option<&'a serde_json::Value> {
    for key in path {
        match current {
            serde_json::Value::Object(obj) => {
                current = obj.get(key)?;
            }
            serde_json::Value::Array(arr) => {
                let idx: i64 = key.parse().ok()?;
                let idx = if idx < 0 {
                    (arr.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}
