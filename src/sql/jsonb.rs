//! JSON/JSONB helper operations.
//!
//! This module centralizes PostgreSQL-compatible semantics for JSONB operators that are
//! used by db9-server's expression evaluator.

use std::{collections::BTreeMap, str::FromStr};

use anyhow::{anyhow, Result};
use bigdecimal::BigDecimal;
use serde_json::{value::RawValue, Value as JsonValue};

const MAX_JSONB_RECURSION_DEPTH: usize = 64;

// ── PostgreSQL JSONB canonical text formatting ──────────────────────────────

/// Parse raw JSON string and format as PostgreSQL JSONB text.
/// Returns original string on parse failure (defensive).
pub(crate) fn format_jsonb_pg_str(raw: &str) -> String {
    try_format_jsonb_pg_raw_str(raw).unwrap_or_else(|_| raw.to_string())
}

fn try_format_jsonb_pg_raw_str(raw: &str) -> std::result::Result<String, ()> {
    let mut out = String::new();
    write_jsonb_pg_raw(&mut out, raw.trim())?;
    Ok(out)
}

fn write_jsonb_pg_raw(out: &mut String, raw: &str) -> std::result::Result<(), ()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(());
    }

    match trimmed.as_bytes()[0] {
        b'n' if trimmed == "null" => out.push_str("null"),
        b't' if trimmed == "true" => out.push_str("true"),
        b'f' if trimmed == "false" => out.push_str("false"),
        b'"' => {
            let value = serde_json::from_str::<String>(trimmed).map_err(|_| ())?;
            out.push_str(&serde_json::to_string(&value).map_err(|_| ())?);
        }
        b'-' | b'0'..=b'9' => {
            let _: serde_json::Number = serde_json::from_str(trimmed).map_err(|_| ())?;
            let value = BigDecimal::from_str(trimmed).map_err(|_| ())?;
            out.push_str(&value.to_string());
        }
        b'[' => {
            let values = serde_json::from_str::<Vec<Box<RawValue>>>(trimmed).map_err(|_| ())?;
            out.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_jsonb_pg_raw(out, value.get())?;
            }
            out.push(']');
        }
        b'{' => {
            let values =
                serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(trimmed).map_err(|_| ())?;
            let mut items: Vec<(&String, &Box<RawValue>)> = values.iter().collect();
            items.sort_by(|(left_key, _), (right_key, _)| {
                match left_key.len().cmp(&right_key.len()) {
                    std::cmp::Ordering::Equal => left_key.cmp(right_key),
                    other => other,
                }
            });
            out.push('{');
            for (index, (key, value)) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&serde_json::to_string(key).map_err(|_| ())?);
                out.push_str(": ");
                write_jsonb_pg_raw(out, value.get())?;
            }
            out.push('}');
        }
        _ => return Err(()),
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn format_jsonb_pretty_pg(val: &JsonValue) -> String {
    let mut out = String::new();
    write_jsonb_pretty_pg(&mut out, val, 0);
    out
}

#[cfg(test)]
pub(crate) fn format_jsonb_pretty_pg_str(raw: &str) -> String {
    match serde_json::from_str::<JsonValue>(raw) {
        Ok(val) => format_jsonb_pretty_pg(&val),
        Err(_) => raw.to_string(),
    }
}

pub(crate) fn sorted_jsonb_object_items(
    obj: &serde_json::Map<String, JsonValue>,
) -> Vec<(&String, &JsonValue)> {
    use std::cmp::Ordering;

    let mut items: Vec<(&String, &JsonValue)> = obj.iter().collect();
    items.sort_by(|(k1, _), (k2, _)| match k1.len().cmp(&k2.len()) {
        Ordering::Equal => k1.cmp(k2),
        other => other,
    });
    items
}

#[cfg(test)]
fn write_jsonb_pg(out: &mut String, val: &JsonValue) {
    match val {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JsonValue::Number(n) => out.push_str(&n.to_string()),
        JsonValue::String(s) => {
            if let Ok(escaped) = serde_json::to_string(s) {
                out.push_str(&escaped);
            } else {
                out.push_str("\"\"");
            }
        }
        JsonValue::Array(arr) => {
            out.push('[');
            for (idx, item) in arr.iter().enumerate() {
                if idx > 0 {
                    out.push_str(", ");
                }
                write_jsonb_pg(out, item);
            }
            out.push(']');
        }
        JsonValue::Object(obj) => {
            out.push('{');
            for (idx, (k, v)) in sorted_jsonb_object_items(obj).into_iter().enumerate() {
                if idx > 0 {
                    out.push_str(", ");
                }
                if let Ok(key) = serde_json::to_string(k) {
                    out.push_str(&key);
                } else {
                    out.push_str("\"\"");
                }
                out.push_str(": ");
                write_jsonb_pg(out, v);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
pub(crate) fn write_jsonb_pretty_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("    ");
    }
}

#[cfg(test)]
pub(crate) fn write_jsonb_pretty_pg(out: &mut String, val: &JsonValue, depth: usize) {
    match val {
        JsonValue::Array(arr) => {
            if arr.is_empty() {
                out.push_str("[]");
                return;
            }

            out.push_str("[\n");
            for (idx, item) in arr.iter().enumerate() {
                write_jsonb_pretty_indent(out, depth + 1);
                write_jsonb_pretty_pg(out, item, depth + 1);
                if idx + 1 < arr.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_jsonb_pretty_indent(out, depth);
            out.push(']');
        }
        JsonValue::Object(obj) => {
            if obj.is_empty() {
                out.push_str("{}");
                return;
            }

            out.push_str("{\n");
            let items = sorted_jsonb_object_items(obj);
            for (idx, (key, value)) in items.iter().enumerate() {
                write_jsonb_pretty_indent(out, depth + 1);
                if let Ok(escaped_key) = serde_json::to_string(key) {
                    out.push_str(&escaped_key);
                } else {
                    out.push_str("\"\"");
                }
                out.push_str(": ");
                write_jsonb_pretty_pg(out, value, depth + 1);
                if idx + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_jsonb_pretty_indent(out, depth);
            out.push('}');
        }
        _ => write_jsonb_pg(out, val),
    }
}

pub(crate) fn contains_str(container: &str, containee: &str) -> Result<bool> {
    let container = parse_jsonb_raw_value(container)?;
    let containee = parse_jsonb_raw_value(containee)?;
    raw_jsonb_contains(&container, &containee, 0)
}

/// Returns `true` if `json ? key` is true (JSONB key existence semantics).
///
/// - For objects: key exists at top-level.
/// - For arrays: a string element equal to `key` exists at top-level.
pub(crate) fn exists(json: &JsonValue, key: &str) -> bool {
    match json {
        JsonValue::Object(obj) => obj.contains_key(key),
        JsonValue::Array(arr) => arr
            .iter()
            .any(|v| matches!(v, JsonValue::String(s) if s == key)),
        _ => false,
    }
}

/// Returns `true` if any `key` exists (`?|` semantics).
pub(crate) fn exists_any<'a, I>(json: &JsonValue, keys: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    match json {
        JsonValue::Object(obj) => keys.into_iter().any(|k| obj.contains_key(k)),
        JsonValue::Array(arr) => keys.into_iter().any(|k| {
            arr.iter()
                .any(|v| matches!(v, JsonValue::String(s) if s == k))
        }),
        _ => false,
    }
}

/// Returns `true` if all `keys` exist (`?&` semantics).
pub(crate) fn exists_all<'a, I>(json: &JsonValue, keys: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    match json {
        JsonValue::Object(obj) => keys.into_iter().all(|k| obj.contains_key(k)),
        JsonValue::Array(arr) => keys.into_iter().all(|k| {
            arr.iter()
                .any(|v| matches!(v, JsonValue::String(s) if s == k))
        }),
        _ => false,
    }
}

fn number_text_eq(a: &str, b: &str) -> bool {
    match (BigDecimal::from_str(a), BigDecimal::from_str(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

enum RawJsonbValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<RawJsonbValue>),
    Object(BTreeMap<String, RawJsonbValue>),
}

fn jsonb_recursion_depth_error() -> anyhow::Error {
    anyhow!("json value is too deep")
}

fn parse_jsonb_raw_value(raw: &str) -> Result<RawJsonbValue> {
    parse_jsonb_raw_value_inner(raw, 0)
}

fn parse_jsonb_raw_value_inner(raw: &str, depth: usize) -> Result<RawJsonbValue> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("Invalid JSONB: empty input"));
    }

    match trimmed.as_bytes()[0] {
        b'n' if trimmed == "null" => Ok(RawJsonbValue::Null),
        b't' if trimmed == "true" => Ok(RawJsonbValue::Bool(true)),
        b'f' if trimmed == "false" => Ok(RawJsonbValue::Bool(false)),
        b'"' => Ok(RawJsonbValue::String(serde_json::from_str(trimmed)?)),
        b'[' => {
            if depth >= MAX_JSONB_RECURSION_DEPTH {
                return Err(jsonb_recursion_depth_error());
            }
            let values: Vec<Box<RawValue>> = serde_json::from_str(trimmed)?;
            values
                .into_iter()
                .map(|value| parse_jsonb_raw_value_inner(value.get(), depth + 1))
                .collect::<Result<Vec<_>>>()
                .map(RawJsonbValue::Array)
        }
        b'{' => {
            if depth >= MAX_JSONB_RECURSION_DEPTH {
                return Err(jsonb_recursion_depth_error());
            }
            let values: BTreeMap<String, Box<RawValue>> = serde_json::from_str(trimmed)?;
            values
                .into_iter()
                .map(|(key, value)| Ok((key, parse_jsonb_raw_value_inner(value.get(), depth + 1)?)))
                .collect::<Result<BTreeMap<_, _>>>()
                .map(RawJsonbValue::Object)
        }
        b'-' | b'0'..=b'9' => {
            let _: serde_json::Number = serde_json::from_str(trimmed)?;
            Ok(RawJsonbValue::Number(trimmed.to_string()))
        }
        _ => Err(anyhow!("Invalid JSONB: {trimmed}")),
    }
}

fn raw_jsonb_eq(a: &RawJsonbValue, b: &RawJsonbValue, depth: usize) -> Result<bool> {
    match (a, b) {
        (RawJsonbValue::Null, RawJsonbValue::Null) => Ok(true),
        (RawJsonbValue::Bool(a), RawJsonbValue::Bool(b)) => Ok(a == b),
        (RawJsonbValue::Number(a), RawJsonbValue::Number(b)) => Ok(number_text_eq(a, b)),
        (RawJsonbValue::String(a), RawJsonbValue::String(b)) => Ok(a == b),
        (RawJsonbValue::Array(a), RawJsonbValue::Array(b)) => {
            if depth >= MAX_JSONB_RECURSION_DEPTH {
                return Err(jsonb_recursion_depth_error());
            }
            if a.len() != b.len() {
                return Ok(false);
            }
            for (left, right) in a.iter().zip(b.iter()) {
                if !raw_jsonb_eq(left, right, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (RawJsonbValue::Object(a), RawJsonbValue::Object(b)) => {
            if depth >= MAX_JSONB_RECURSION_DEPTH {
                return Err(jsonb_recursion_depth_error());
            }
            if a.len() != b.len() {
                return Ok(false);
            }
            for (key, left) in a.iter() {
                let Some(right) = b.get(key) else {
                    return Ok(false);
                };
                if !raw_jsonb_eq(left, right, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn raw_jsonb_contains(
    container: &RawJsonbValue,
    containee: &RawJsonbValue,
    depth: usize,
) -> Result<bool> {
    match (container, containee) {
        (RawJsonbValue::Object(container), RawJsonbValue::Object(containee)) => {
            if depth >= MAX_JSONB_RECURSION_DEPTH {
                return Err(jsonb_recursion_depth_error());
            }
            for (key, containee_val) in containee.iter() {
                let Some(container_val) = container.get(key) else {
                    return Ok(false);
                };
                if !raw_jsonb_contains(container_val, containee_val, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (RawJsonbValue::Array(container), RawJsonbValue::Array(containee)) => {
            if depth >= MAX_JSONB_RECURSION_DEPTH {
                return Err(jsonb_recursion_depth_error());
            }
            for containee_elem in containee {
                if !jsonb_array_member_contains(container, containee_elem, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (RawJsonbValue::Array(container), containee) => {
            jsonb_array_member_contains(container, containee, depth)
        }
        _ => raw_jsonb_eq(container, containee, depth),
    }
}

fn jsonb_array_member_contains(
    container: &[RawJsonbValue],
    containee: &RawJsonbValue,
    depth: usize,
) -> Result<bool> {
    if depth >= MAX_JSONB_RECURSION_DEPTH {
        return Err(jsonb_recursion_depth_error());
    }
    match containee {
        RawJsonbValue::Array(_) => {
            for container_elem in container {
                if matches!(container_elem, RawJsonbValue::Array(_))
                    && raw_jsonb_contains(container_elem, containee, depth + 1)?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        RawJsonbValue::Object(_) => {
            for container_elem in container {
                if matches!(container_elem, RawJsonbValue::Object(_))
                    && raw_jsonb_contains(container_elem, containee, depth + 1)?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => {
            for container_elem in container {
                if raw_jsonb_eq(container_elem, containee, depth + 1)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> JsonValue {
        serde_json::from_str(s).unwrap()
    }

    fn deeply_nested_json(depth: usize) -> String {
        let mut value = JsonValue::Number(serde_json::Number::from(1));
        for _ in 0..depth {
            value = JsonValue::Array(vec![value]);
        }
        value.to_string()
    }

    #[test]
    fn contains_objects_nested() {
        assert!(contains_str(r#"{"a":1,"b":2}"#, r#"{"a":1}"#).unwrap());
        assert!(!contains_str(r#"{"a":1}"#, r#"{"a":1,"b":2}"#).unwrap());
        assert!(contains_str(r#"{"a":{"b":1,"c":2}}"#, r#"{"a":{"b":1}}"#).unwrap());
        assert!(contains_str(r#"{"a":{"b":1}}"#, r#"{"a":{}}"#).unwrap());
    }

    #[test]
    fn contains_arrays_recursive() {
        assert!(contains_str(r#"[1,2,3]"#, r#"[2,1]"#).unwrap());
        assert!(!contains_str(r#"[1,2]"#, r#"[1,2,3]"#).unwrap());

        // Recursive containment for array elements (PostgreSQL jsonb semantics).
        assert!(contains_str(r#"[{"a":1,"b":2}]"#, r#"[{"a":1}]"#).unwrap());
        assert!(contains_str(r#"[{"a":1,"b":2}]"#, r#"{"a":1}"#).unwrap());
        assert!(!contains_str(r#"[[1,2]]"#, r#"[1,2]"#).unwrap());
        assert!(contains_str(r#"[[1,2]]"#, r#"[[1]]"#).unwrap());
    }

    #[test]
    fn contains_rejects_excessive_recursion_depth() {
        let deep = deeply_nested_json(MAX_JSONB_RECURSION_DEPTH + 1);
        let err = contains_str(&deep, &deep).unwrap_err();
        assert_eq!(err.to_string(), "json value is too deep");
    }

    #[test]
    fn contains_numeric_equality() {
        assert!(contains_str(r#"{"a":1}"#, r#"{"a":1.0}"#).unwrap());
        assert!(contains_str(r#"{"a":1.0}"#, r#"{"a":1}"#).unwrap());
        assert!(!contains_str(r#"{"a":1.1}"#, r#"{"a":1}"#).unwrap());
        assert!(!contains_str("18446744073709551616", "18446744073709551615").unwrap());
        assert!(contains_str(r#"{"a":[1.0,{"b":2}]}"#, r#"{"a":[1,{"b":2.0}]}"#).unwrap());
        assert!(!contains_str("[18446744073709551616]", "18446744073709551617").unwrap());
        assert!(!contains_str(
            "9007199254740992.0000000000000000001",
            "9007199254740992.0000000000000000002"
        )
        .unwrap());
    }

    #[test]
    fn exists_semantics() {
        assert!(exists(&j(r#"{"a":1}"#), "a"));
        assert!(!exists(&j(r#"{"a":1}"#), "b"));

        assert!(exists(&j(r#"["a","b"]"#), "b"));
        assert!(!exists(&j(r#"["a","b"]"#), "c"));
    }

    #[test]
    fn format_jsonb_pretty_pg_uses_pg_indentation() {
        assert_eq!(
            format_jsonb_pretty_pg_str(r#"{"b":2,"a":{"y":2,"x":1}}"#),
            "{\n    \"a\": {\n        \"x\": 1,\n        \"y\": 2\n    },\n    \"b\": 2\n}"
        );
    }

    #[test]
    fn exists_any_all_semantics() {
        let obj = j(r#"{"a":1,"b":2}"#);
        assert!(exists_any(&obj, ["a", "c"].into_iter()));
        assert!(exists_all(&obj, ["a", "b"].into_iter()));
        assert!(!exists_all(&obj, ["a", "c"].into_iter()));
    }

    // ── format_jsonb_pg tests ───────────────────────────────────────────────

    #[test]
    fn format_length_first_sort() {
        // "size" (4) before "color" (5)
        assert_eq!(
            format_jsonb_pg_str(r#"{"color":"w","size":"M"}"#),
            r#"{"size": "M", "color": "w"}"#
        );
    }

    #[test]
    fn format_equal_length_lexical() {
        assert_eq!(
            format_jsonb_pg_str(r#"{"bb":1,"aa":2}"#),
            r#"{"aa": 2, "bb": 1}"#
        );
    }

    #[test]
    fn format_spaced_separators() {
        assert_eq!(format_jsonb_pg_str(r#"{"a":1}"#), r#"{"a": 1}"#);
    }

    #[test]
    fn format_array() {
        assert_eq!(format_jsonb_pg_str(r#"[1,2,3]"#), r#"[1, 2, 3]"#);
    }

    #[test]
    fn format_nested_objects() {
        assert_eq!(
            format_jsonb_pg_str(r#"{"b":{"d":1,"c":2},"a":3}"#),
            r#"{"a": 3, "b": {"c": 2, "d": 1}}"#
        );
    }

    #[test]
    fn format_scalars() {
        assert_eq!(format_jsonb_pg_str("null"), "null");
        assert_eq!(format_jsonb_pg_str("true"), "true");
        assert_eq!(format_jsonb_pg_str("false"), "false");
        assert_eq!(format_jsonb_pg_str("42"), "42");
        assert_eq!(format_jsonb_pg_str(r#""hello""#), r#""hello""#);
    }

    #[test]
    fn format_preserves_raw_numbers_and_collapses_duplicate_keys() {
        assert_eq!(
            format_jsonb_pg_str(r#"{"n":9007199254740993.123456789}"#),
            r#"{"n": 9007199254740993.123456789}"#
        );
        assert_eq!(
            format_jsonb_pg_str(r#"{"b":1,"a":2,"b":3}"#),
            r#"{"a": 2, "b": 3}"#
        );
    }

    #[test]
    fn format_normalizes_exponent_numbers() {
        assert_eq!(format_jsonb_pg_str("1e2"), "100");
        assert_eq!(format_jsonb_pg_str("1e-2"), "0.01");
        assert_eq!(format_jsonb_pg_str("-1.2300e+2"), "-123.00");
        assert_eq!(format_jsonb_pg_str(r#"{"b":1e2}"#), r#"{"b": 100}"#);
    }

    #[test]
    fn format_string_escaping() {
        assert_eq!(format_jsonb_pg_str(r#"{"k":"a\"b"}"#), r#"{"k": "a\"b"}"#);
    }

    #[test]
    fn format_idempotent() {
        let input = r#"{"color":"white","size":"M"}"#;
        let once = format_jsonb_pg_str(input);
        let twice = format_jsonb_pg_str(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn format_invalid_json_passthrough() {
        assert_eq!(format_jsonb_pg_str("not json"), "not json");
    }
}
