//! JSON/JSONB helper operations.
//!
//! This module centralizes PostgreSQL-compatible semantics for JSONB operators that are
//! used by pg-tikv's expression evaluator.

use serde_json::{Number as JsonNumber, Value as JsonValue};

// ── PostgreSQL JSONB canonical text formatting ──────────────────────────────

/// Format a `serde_json::Value` as PostgreSQL JSONB text output.
///
/// PostgreSQL JSONB output contract:
/// - Object keys sorted by length first, then lexicographically within equal lengths
/// - Separators: ": " (colon-space) and ", " (comma-space)
/// - Standard JSON string escaping
pub(crate) fn format_jsonb_pg(val: &JsonValue) -> String {
    let mut out = String::new();
    write_jsonb_pg(&mut out, val);
    out
}

/// Parse raw JSON string and format as PostgreSQL JSONB text.
/// Returns original string on parse failure (defensive).
pub(crate) fn format_jsonb_pg_str(raw: &str) -> String {
    match serde_json::from_str::<JsonValue>(raw) {
        Ok(val) => format_jsonb_pg(&val),
        Err(_) => raw.to_string(),
    }
}

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
            use std::cmp::Ordering;
            let mut items: Vec<(&String, &JsonValue)> = obj.iter().collect();
            items.sort_by(|(k1, _), (k2, _)| match k1.len().cmp(&k2.len()) {
                Ordering::Equal => k1.cmp(k2),
                other => other,
            });

            out.push('{');
            for (idx, (k, v)) in items.into_iter().enumerate() {
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

/// Returns `true` if `container` JSONB contains `containee` JSONB (`@>` semantics).
///
/// Semantics:
/// - Objects: every key/value in `containee` must exist in `container` (recursively).
/// - Arrays: every element in `containee` must be contained in some element in `container`
///   (order-independent; recursive for nested objects/arrays).
/// - Scalars: equality, with PostgreSQL-like numeric equality (e.g. `1` equals `1.0`).
pub(crate) fn contains(container: &JsonValue, containee: &JsonValue) -> bool {
    match (container, containee) {
        (JsonValue::Object(container_obj), JsonValue::Object(containee_obj)) => containee_obj
            .iter()
            .all(|(key, containee_val)| match container_obj.get(key) {
                Some(container_val) => contains(container_val, containee_val),
                None => false,
            }),
        (JsonValue::Array(container_arr), JsonValue::Array(containee_arr)) => {
            containee_arr.iter().all(|containee_elem| {
                container_arr
                    .iter()
                    .any(|container_elem| contains(container_elem, containee_elem))
            })
        }
        _ => scalar_eq(container, containee),
    }
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

fn scalar_eq(a: &JsonValue, b: &JsonValue) -> bool {
    match (a, b) {
        (JsonValue::Number(a), JsonValue::Number(b)) => number_eq(a, b),
        _ => a == b,
    }
}

fn number_eq(a: &JsonNumber, b: &JsonNumber) -> bool {
    // serde_json::Number uses distinct integer/float variants, while PostgreSQL jsonb
    // compares numerically. We keep exact integer equality and only fall back to f64
    // when at least one side is a float.

    if let (Some(ai), Some(bi)) = (a.as_i64(), b.as_i64()) {
        return ai == bi;
    }
    if let (Some(au), Some(bu)) = (a.as_u64(), b.as_u64()) {
        return au == bu;
    }
    if let (Some(ai), Some(bu)) = (a.as_i64(), b.as_u64()) {
        return ai >= 0 && (ai as u64) == bu;
    }
    if let (Some(au), Some(bi)) = (a.as_u64(), b.as_i64()) {
        return bi >= 0 && au == (bi as u64);
    }

    let (Some(af), Some(bf)) = (a.as_f64(), b.as_f64()) else {
        return false;
    };

    // If one side is an integer and the other is a float, treat them equal only
    // when the float is an exact integer value.
    if let Some(ai) = a.as_i64() {
        return float_equals_i64(bf, ai);
    }
    if let Some(bi) = b.as_i64() {
        return float_equals_i64(af, bi);
    }
    if let Some(au) = a.as_u64() {
        return float_equals_u64(bf, au);
    }
    if let Some(bu) = b.as_u64() {
        return float_equals_u64(af, bu);
    }

    af == bf
}

fn float_equals_i64(f: f64, i: i64) -> bool {
    if !f.is_finite() || f.fract() != 0.0 {
        return false;
    }
    if f < (i64::MIN as f64) || f > (i64::MAX as f64) {
        return false;
    }
    (f as i64) == i
}

fn float_equals_u64(f: f64, u: u64) -> bool {
    if !f.is_finite() || f.fract() != 0.0 {
        return false;
    }
    if f < 0.0 || f > (u64::MAX as f64) {
        return false;
    }
    (f as u64) == u
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> JsonValue {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn contains_objects_nested() {
        assert!(contains(&j(r#"{"a":1,"b":2}"#), &j(r#"{"a":1}"#)));
        assert!(!contains(&j(r#"{"a":1}"#), &j(r#"{"a":1,"b":2}"#)));
        assert!(contains(
            &j(r#"{"a":{"b":1,"c":2}}"#),
            &j(r#"{"a":{"b":1}}"#)
        ));
        assert!(contains(&j(r#"{"a":{"b":1}}"#), &j(r#"{"a":{}}"#)));
    }

    #[test]
    fn contains_arrays_recursive() {
        assert!(contains(&j(r#"[1,2,3]"#), &j(r#"[2,1]"#)));
        assert!(!contains(&j(r#"[1,2]"#), &j(r#"[1,2,3]"#)));

        // Recursive containment for array elements (PostgreSQL jsonb semantics).
        assert!(contains(&j(r#"[{"a":1,"b":2}]"#), &j(r#"[{"a":1}]"#)));
    }

    #[test]
    fn contains_numeric_equality() {
        assert!(contains(&j(r#"{"a":1}"#), &j(r#"{"a":1.0}"#)));
        assert!(contains(&j(r#"{"a":1.0}"#), &j(r#"{"a":1}"#)));
        assert!(!contains(&j(r#"{"a":1.1}"#), &j(r#"{"a":1}"#)));
    }

    #[test]
    fn exists_semantics() {
        assert!(exists(&j(r#"{"a":1}"#), "a"));
        assert!(!exists(&j(r#"{"a":1}"#), "b"));

        assert!(exists(&j(r#"["a","b"]"#), "b"));
        assert!(!exists(&j(r#"["a","b"]"#), "c"));
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
