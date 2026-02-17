//! Expression evaluation logic

pub mod bridge;
pub mod classify;
pub mod compile;
pub mod functions;
mod numeric;
pub(crate) mod operators;
pub mod static_eval;
pub mod typed_eval;
pub mod typed_fold;
pub mod typed_rewrite;
pub mod typed_visit;

use crate::sql::error::SqlError;
use crate::types::Value;
use anyhow::{anyhow, Result};
use sqlparser::ast::JsonOperator;

pub(crate) const VERSION_STRING: &str = concat!(
    "PostgreSQL 16.0 (db9 ",
    env!("CARGO_PKG_VERSION"),
    "-",
    env!("BUILD_GIT_HASH"),
    " ",
    env!("BUILD_DATE"),
    " on TiKV)"
);

pub(crate) fn like_match(
    s: &str,
    pattern: &str,
    escape_char: Option<char>,
    case_insensitive: bool,
) -> bool {
    if case_insensitive {
        return like_match_impl(&s.to_lowercase(), &pattern.to_lowercase(), escape_char);
    }
    like_match_impl(s, pattern, escape_char)
}

fn like_match_impl(s: &str, pattern: &str, escape_char: Option<char>) -> bool {
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
                if let Some((lit, p_after)) = next_char_at(pattern, p_next) {
                    if let Some((sc, s_next)) = next_char_at(s, s_idx) {
                        if sc == lit {
                            s_idx = s_next;
                            p_idx = p_after;
                            continue;
                        }
                    }
                } else if let Some((sc, s_next)) = next_char_at(s, s_idx) {
                    // Trailing escape char: match it literally.
                    let esc = escape_char.expect("checked is_some");
                    if sc == esc {
                        s_idx = s_next;
                        p_idx = p_next;
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
                if sc == pc {
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

        return false;
    }

    // String is consumed; the remaining pattern must be empty or all '%'.
    while p_idx < pattern.len() {
        let (pc, p_next) = next_char_at(pattern, p_idx).expect("p_idx < len");
        if escape_char.is_some_and(|esc| pc == esc) {
            return false;
        }
        if pc != '%' {
            return false;
        }
        p_idx = p_next;
    }

    true
}

pub(crate) fn similar_to_match(s: &str, pattern: &str, escape_char: Option<char>) -> Result<bool> {
    let escape = escape_char.unwrap_or('\\');
    let mut regex_pattern = String::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch == escape {
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
        .map_err(|e| anyhow!("Invalid SIMILAR TO pattern: {}", e))?;
    Ok(re.is_match(s))
}

pub(crate) fn parse_interval_string(s: &str) -> Result<Value> {
    use crate::types::IntervalValue;
    let s = s.trim().to_lowercase();
    let mut total_months: i32 = 0;
    let mut total_ms: i64 = 0;

    let parts: Vec<&str> = s.split_whitespace().collect();
    let mut i = 0;
    while i < parts.len() {
        if let Ok(num) = parts[i].parse::<i64>() {
            if i + 1 < parts.len() {
                let unit = parts[i + 1].trim_end_matches('s');
                match unit {
                    "day" => total_ms += num * 24 * 60 * 60 * 1000,
                    "hour" => total_ms += num * 60 * 60 * 1000,
                    "minute" | "min" => total_ms += num * 60 * 1000,
                    "second" | "sec" => total_ms += num * 1000,
                    "millisecond" | "ms" => total_ms += num,
                    "week" => total_ms += num * 7 * 24 * 60 * 60 * 1000,
                    "month" | "mon" => total_months += num as i32,
                    "year" => total_months += (num * 12) as i32,
                    _ => return Err(anyhow!("Unknown interval unit: {}", parts[i + 1])),
                };
                i += 2;
            } else {
                return Err(anyhow!("Interval number without unit"));
            }
        } else {
            i += 1;
        }
    }

    Ok(Value::Interval(IntervalValue::new(total_months, total_ms)))
}

pub(crate) fn parse_timestamp_string(s: &str) -> Result<Value> {
    let trimmed = s.trim();

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
                for r in right_arr {
                    let mut found = false;
                    for l in left_arr.iter() {
                        if compare_values(l, r)? == 0 {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        return Ok(Value::Boolean(false));
                    }
                }
                return Ok(Value::Boolean(true));
            }
            JsonOperator::ArrowAt => {
                let Value::Array(right_arr) = &right else {
                    return Err(anyhow!("<@ on arrays requires array operand on right"));
                };
                for l in left_arr {
                    let mut found = false;
                    for r in right_arr.iter() {
                        if compare_values(l, r)? == 0 {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        return Ok(Value::Boolean(false));
                    }
                }
                return Ok(Value::Boolean(true));
            }
            _ => {
                return Err(SqlError::Unsupported(format!(
                    "Unsupported operator for arrays: {:?}",
                    operator
                ))
                .into())
            }
        }
    }

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

    let mut json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

    match operator {
        JsonOperator::AtArrow => {
            let right_str = match right {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                _ => return Err(anyhow!("@> requires json/jsonb operand on right")),
            };
            let right_json: serde_json::Value = serde_json::from_str(&right_str)
                .map_err(|e| anyhow!("Invalid JSON on right side of @>: {}", e))?;
            Ok(Value::Boolean(super::jsonb::contains(
                &json_val,
                &right_json,
            )))
        }
        JsonOperator::ArrowAt => {
            let right_str = match right {
                Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
                Value::Null => return Ok(Value::Null),
                _ => return Err(anyhow!("<@ requires json/jsonb operand on right")),
            };
            let right_json: serde_json::Value = serde_json::from_str(&right_str)
                .map_err(|e| anyhow!("Invalid JSON on right side of <@: {}", e))?;
            Ok(Value::Boolean(super::jsonb::contains(
                &right_json,
                &json_val,
            )))
        }
        JsonOperator::HashArrow | JsonOperator::HashLongArrow | JsonOperator::HashMinus => {
            let path = json_path_from_value(&right)?;
            match operator {
                JsonOperator::HashArrow => match json_get_path(&json_val, &path) {
                    Some(val) => Ok(Value::Jsonb(val.to_string())),
                    None => Ok(Value::Null),
                },
                JsonOperator::HashLongArrow => match json_get_path(&json_val, &path) {
                    None => Ok(Value::Null),
                    Some(serde_json::Value::Null) => Ok(Value::Null),
                    Some(serde_json::Value::String(s)) => Ok(Value::Text(s.clone())),
                    Some(other) => Ok(Value::Text(other.to_string())),
                },
                JsonOperator::HashMinus => {
                    json_delete_path(&mut json_val, &path);
                    Ok(Value::Jsonb(json_val.to_string()))
                }
                _ => Err(SqlError::Unsupported(format!(
                    "Unsupported JSON operator: {:?}",
                    operator
                ))
                .into()),
            }
        }
        _ => {
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
                    JsonOperator::Arrow => {
                        // -> operator returns JSONB type
                        // Client drivers will parse the JSONB value and extract the actual value
                        Ok(Value::Jsonb(val.to_string()))
                    }
                    JsonOperator::LongArrow => match val {
                        serde_json::Value::Null => Ok(Value::Null),
                        serde_json::Value::Bool(b) => Ok(Value::Text(b.to_string())),
                        serde_json::Value::Number(n) => Ok(Value::Text(n.to_string())),
                        serde_json::Value::String(s) => Ok(Value::Text(s.clone())),
                        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                            Ok(Value::Text(val.to_string()))
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

fn parse_pg_text_array_literal(s: &str) -> Result<Vec<String>> {
    let trimmed = s.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        if inner.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(inner
            .split(',')
            .map(|p| p.trim().trim_matches('"').to_string())
            .collect());
    }
    Ok(vec![trimmed.to_string()])
}

fn json_path_from_value(path: &Value) -> Result<Vec<String>> {
    match path {
        Value::Array(arr) => Ok(arr
            .iter()
            .map(|v| match v {
                Value::Text(s) => s.clone(),
                other => other.to_string(),
            })
            .collect()),
        Value::Text(s) => parse_pg_text_array_literal(s),
        Value::Null => Ok(Vec::new()),
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

fn json_delete_path(current: &mut serde_json::Value, path: &[String]) -> bool {
    if path.is_empty() {
        return false;
    }
    if path.len() == 1 {
        let key = &path[0];
        match current {
            serde_json::Value::Object(obj) => obj.remove(key).is_some(),
            serde_json::Value::Array(arr) => {
                if let Ok(idx) = key.parse::<i64>() {
                    let idx = if idx < 0 { arr.len() as i64 + idx } else { idx };
                    if idx >= 0 && (idx as usize) < arr.len() {
                        arr.remove(idx as usize);
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    } else {
        let key = &path[0];
        match current {
            serde_json::Value::Object(obj) => match obj.get_mut(key) {
                Some(child) => json_delete_path(child, &path[1..]),
                None => false,
            },
            serde_json::Value::Array(arr) => {
                let Ok(idx) = key.parse::<i64>() else {
                    return false;
                };
                let idx = if idx < 0 { arr.len() as i64 + idx } else { idx };
                if idx >= 0 && (idx as usize) < arr.len() {
                    json_delete_path(&mut arr[idx as usize], &path[1..])
                } else {
                    false
                }
            }
            _ => false,
        }
    }
}
