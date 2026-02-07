//! JSON/JSONB helper operations.
//!
//! This module centralizes PostgreSQL-compatible semantics for JSONB operators that are
//! used by pg-tikv's expression evaluator.

use serde_json::{Number as JsonNumber, Value as JsonValue};

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
}
