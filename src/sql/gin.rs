//! JSONB GIN token extraction.
//!
//! pg-tikv uses a lightweight, GIN-like inverted index to accelerate JSONB `@>` queries.
//! This module extracts "tokens" (key-exists and key-value) from a JSON value and hashes
//! them into fixed-size identifiers that are stored in TiKV index keys.
//!
//! Important: tokenization must never produce false negatives for `@>` containment.
//! We allow false positives and rely on a final `jsonb::contains()` recheck.

use serde_json::{Number as JsonNumber, Value as JsonValue};

/// Maximum recursion depth when extracting GIN tokens.
///
/// This is a guardrail against pathological JSON that would otherwise create enormous
/// write amplification and deep recursion.
pub(crate) const MAX_GIN_DEPTH: usize = 32;

/// Extracted GIN tokens, split into two groups for better scan selectivity.
///
/// - `key_values` are generally more selective than `key_exists` and should be scanned first.
#[derive(Debug, Default, Clone)]
pub(crate) struct GinTokens {
    pub(crate) key_values: Vec<u64>,
    pub(crate) key_exists: Vec<u64>,
}

impl GinTokens {
    #[allow(dead_code)] // used in tests for GIN token verification
    pub(crate) fn iter_hashes(&self) -> impl Iterator<Item = u64> + '_ {
        self.key_values
            .iter()
            .copied()
            .chain(self.key_exists.iter().copied())
    }

    /// Returns all token hashes in a scan-friendly order (deduped).
    ///
    /// For intersection-based scans, scanning key-value tokens first typically reduces
    /// the candidate set size earlier.
    pub(crate) fn into_scan_hashes(mut self) -> Vec<u64> {
        self.key_values.sort_unstable();
        self.key_values.dedup();
        self.key_exists.sort_unstable();
        self.key_exists.dedup();
        self.key_values.extend(self.key_exists);
        self.key_values
    }
}

/// Extract hashed GIN tokens from a JSON value.
pub(crate) fn extract_gin_tokens(json: &JsonValue) -> GinTokens {
    let mut tokens = GinTokens::default();
    let mut path_buf = Vec::new();
    extract_gin_tokens_inner(json, &mut path_buf, 0, &mut tokens);
    tokens
}

fn extract_gin_tokens_inner(
    json: &JsonValue,
    path: &mut Vec<u8>,
    depth: usize,
    out: &mut GinTokens,
) {
    if depth >= MAX_GIN_DEPTH {
        return;
    }

    match json {
        JsonValue::Object(map) => {
            for (key, val) in map {
                let prev_len = path.len();
                if !path.is_empty() {
                    path.push(b'.');
                }
                path.extend_from_slice(key.as_bytes());

                // Key existence token.
                out.key_exists.push(hash_key_exists(path));

                match val {
                    JsonValue::Object(_) | JsonValue::Array(_) => {
                        extract_gin_tokens_inner(val, path, depth + 1, out);
                    }
                    _ => {
                        // Scalar value token.
                        out.key_values.push(hash_key_value(path, val));
                    }
                }

                path.truncate(prev_len);
            }
        }
        JsonValue::Array(arr) => {
            // PostgreSQL jsonb `@>` array containment is order-independent. We therefore
            // do NOT include element indices in token paths; doing so would create
            // false negatives.
            for elem in arr {
                match elem {
                    JsonValue::Object(_) | JsonValue::Array(_) => {
                        extract_gin_tokens_inner(elem, path, depth + 1, out);
                    }
                    _ => out.key_values.push(hash_key_value(path, elem)),
                }
            }
        }
        // Root scalar JSONB (rare but valid) — index the scalar itself.
        _ => out.key_values.push(hash_key_value(path, json)),
    }
}

// ----------------------------
// Hashing
// ----------------------------

// Deterministic, fast 64-bit FNV-1a hash.
const FNV1A_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV1A_PRIME: u64 = 0x100000001b3;

#[inline]
fn fnv1a_u64(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV1A_PRIME);
    }
    hash
}

#[inline]
fn hash_key_exists(path: &[u8]) -> u64 {
    let mut h = FNV1A_OFFSET_BASIS;
    h = fnv1a_u64(h, b"K");
    fnv1a_u64(h, path)
}

#[inline]
fn hash_key_value(path: &[u8], scalar: &JsonValue) -> u64 {
    let mut h = FNV1A_OFFSET_BASIS;
    h = fnv1a_u64(h, b"V");
    h = fnv1a_u64(h, path);
    h = fnv1a_u64(h, b"\0");
    hash_json_scalar(h, scalar)
}

fn hash_json_scalar(mut h: u64, scalar: &JsonValue) -> u64 {
    match scalar {
        JsonValue::Null => fnv1a_u64(h, b"n"),
        JsonValue::Bool(b) => fnv1a_u64(h, if *b { b"t" } else { b"f" }),
        JsonValue::String(s) => {
            h = fnv1a_u64(h, b"s");
            fnv1a_u64(h, s.as_bytes())
        }
        JsonValue::Number(n) => hash_json_number(h, n),
        // Non-scalars should not reach here; fall back to type tag only.
        JsonValue::Array(_) => fnv1a_u64(h, b"a"),
        JsonValue::Object(_) => fnv1a_u64(h, b"o"),
    }
}

fn hash_json_number(mut h: u64, n: &JsonNumber) -> u64 {
    // Canonicalize numbers so that values considered equal by our jsonb numeric
    // comparison (`jsonb::contains` -> number_eq) hash to the same bytes, notably:
    //   1 == 1.0
    if let Some(i) = n.as_i64() {
        h = fnv1a_u64(h, b"i");
        return fnv1a_u64(h, &i.to_be_bytes());
    }
    if let Some(u) = n.as_u64() {
        h = fnv1a_u64(h, b"u");
        return fnv1a_u64(h, &u.to_be_bytes());
    }

    let Some(f) = n.as_f64() else {
        return fnv1a_u64(h, b"?");
    };

    if f.is_finite() && f.fract() == 0.0 {
        // Integral float: hash as integer when representable.
        if f >= (i64::MIN as f64) && f <= (i64::MAX as f64) {
            h = fnv1a_u64(h, b"i");
            return fnv1a_u64(h, &(f as i64).to_be_bytes());
        }
        if f >= 0.0 && f <= (u64::MAX as f64) {
            h = fnv1a_u64(h, b"u");
            return fnv1a_u64(h, &(f as u64).to_be_bytes());
        }
    }

    h = fnv1a_u64(h, b"f");
    fnv1a_u64(h, &f.to_bits().to_be_bytes())
}

// ----------------------------
// ARRAY GIN Token Extraction
// ----------------------------

use crate::types::Value;

/// Extract hashed GIN tokens from an ARRAY value.
///
/// For ARRAY containment (`@>`), we hash each non-null element to a token.
/// The containment check is order-independent, so we just hash element values.
pub(crate) fn extract_array_gin_tokens(arr: &[Value]) -> Vec<u64> {
    let mut tokens = Vec::with_capacity(arr.len());
    for elem in arr {
        if !matches!(elem, Value::Null) {
            tokens.push(hash_array_element(elem));
        }
    }
    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

fn hash_array_element(val: &Value) -> u64 {
    let mut h = FNV1A_OFFSET_BASIS;
    h = fnv1a_u64(h, b"A");
    match val {
        Value::Null => fnv1a_u64(h, b"n"),
        Value::Boolean(b) => fnv1a_u64(h, if *b { b"t" } else { b"f" }),
        Value::Int32(i) => {
            h = fnv1a_u64(h, b"i4");
            fnv1a_u64(h, &i.to_be_bytes())
        }
        Value::Int64(i) => {
            h = fnv1a_u64(h, b"i8");
            fnv1a_u64(h, &i.to_be_bytes())
        }
        Value::Float64(f) => {
            h = fnv1a_u64(h, b"f8");
            fnv1a_u64(h, &f.to_bits().to_be_bytes())
        }
        Value::Text(s) => {
            h = fnv1a_u64(h, b"s");
            fnv1a_u64(h, s.as_bytes())
        }
        Value::Uuid(bytes) => {
            h = fnv1a_u64(h, b"u");
            fnv1a_u64(h, bytes)
        }
        Value::Timestamp(ts) => {
            h = fnv1a_u64(h, b"ts");
            fnv1a_u64(h, &ts.to_be_bytes())
        }
        Value::Date(d) => {
            h = fnv1a_u64(h, b"d");
            fnv1a_u64(h, &d.to_be_bytes())
        }
        Value::Interval(iv) => {
            h = fnv1a_u64(h, b"iv");
            fnv1a_u64(h, iv.to_string().as_bytes())
        }
        Value::Bytes(b) => {
            h = fnv1a_u64(h, b"b");
            fnv1a_u64(h, b)
        }
        Value::Array(nested) => {
            h = fnv1a_u64(h, b"arr");
            for elem in nested {
                let child_hash = hash_array_element(elem);
                h = fnv1a_u64(h, &child_hash.to_be_bytes());
            }
            h
        }
        _ => fnv1a_u64(h, b"?"),
    }
}

// ----------------------------
// TSVECTOR GIN Token Extraction
// ----------------------------

/// Extract hashed GIN tokens from a tsvector string.
///
/// For FTS match (`@@`), we extract each lexeme (word) from the tsvector.
/// Format: 'word':posWeight e.g., "'hello':1A 'world':2B"
pub(crate) fn extract_tsvector_gin_tokens(tsvector: &str) -> Vec<u64> {
    let mut tokens = Vec::new();
    for part in tsvector.split_whitespace() {
        if let Some(word) = part.split(':').next() {
            let word = word.trim_matches('\'');
            if !word.is_empty() {
                tokens.push(hash_tsvector_lexeme(word));
            }
        }
    }
    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

/// Extract GIN tokens from a tsquery for index lookup.
///
/// Parses query terms from tsquery format: "'hello' & 'world'" => ["hello", "world"]
pub(crate) fn extract_tsquery_gin_tokens(tsquery: &str) -> Vec<u64> {
    let mut tokens = Vec::new();
    for term in tsquery.split(|c: char| matches!(c, '&' | '|' | '!' | '(' | ')')) {
        let word = term.trim().trim_matches('\'').to_lowercase();
        if !word.is_empty() {
            tokens.push(hash_tsvector_lexeme(&word));
        }
    }
    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

fn hash_tsvector_lexeme(word: &str) -> u64 {
    let mut h = FNV1A_OFFSET_BASIS;
    h = fnv1a_u64(h, b"T");
    fnv1a_u64(h, word.to_lowercase().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> JsonValue {
        serde_json::from_str(s).unwrap()
    }

    fn to_sets(
        tokens: GinTokens,
    ) -> (
        std::collections::HashSet<u64>,
        std::collections::HashSet<u64>,
    ) {
        (
            tokens.key_values.into_iter().collect(),
            tokens.key_exists.into_iter().collect(),
        )
    }

    #[test]
    fn tokens_scalar_numeric_canonicalizes_1_and_1_dot_0() {
        let (kv1, _) = to_sets(extract_gin_tokens(&j(r#"{"a":1}"#)));
        let (kv2, _) = to_sets(extract_gin_tokens(&j(r#"{"a":1.0}"#)));

        // Both must include the same key-value token for path "a".
        assert_eq!(kv1.len(), 1);
        assert_eq!(kv1, kv2);
    }

    #[test]
    fn tokens_objects_and_arrays_avoid_false_negatives_for_contains_examples() {
        // Mirrors jsonb::contains semantics for arrays (order-independent).
        let container = j(r#"{"a":[1,2,3], "b":{"c":"x"}}"#);
        let containee = j(r#"{"a":[2,1], "b":{"c":"x"}}"#);

        let container_tokens = extract_gin_tokens(&container);
        let query_tokens = extract_gin_tokens(&containee);

        let container_all: std::collections::HashSet<u64> =
            container_tokens.iter_hashes().collect();
        for t in query_tokens.iter_hashes() {
            assert!(container_all.contains(&t));
        }
    }

    #[test]
    fn tokens_dedup_and_order_for_scan_prefers_key_values() {
        let json = j(r#"{"a":1,"b":2}"#);
        let tokens = extract_gin_tokens(&json);
        let scan = tokens.clone().into_scan_hashes();
        assert_eq!(scan.len(), 4);

        // Key-values come first (2), followed by key-exists (2), after sorting+dedup.
        assert_eq!(tokens.key_values.len(), 2);
        assert_eq!(tokens.key_exists.len(), 2);
        let key_values_sorted: Vec<u64> = {
            let mut v = tokens.key_values.clone();
            v.sort_unstable();
            v
        };
        assert_eq!(&scan[..2], key_values_sorted.as_slice());
    }

    #[test]
    fn array_tokens_basic() {
        let arr = vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)];
        let tokens = extract_array_gin_tokens(&arr);
        assert_eq!(tokens.len(), 3);
    }

    #[test]
    fn array_tokens_deduped() {
        let arr = vec![Value::Int32(1), Value::Int32(1), Value::Int32(2)];
        let tokens = extract_array_gin_tokens(&arr);
        assert_eq!(tokens.len(), 2);
    }

    #[test]
    fn array_tokens_null_ignored() {
        let arr = vec![Value::Int32(1), Value::Null, Value::Int32(2)];
        let tokens = extract_array_gin_tokens(&arr);
        assert_eq!(tokens.len(), 2);
    }

    #[test]
    fn array_containment_tokens_subset() {
        let container = vec![
            Value::Text("rust".to_string()),
            Value::Text("tikv".to_string()),
            Value::Text("postgres".to_string()),
        ];
        let contained = vec![
            Value::Text("rust".to_string()),
            Value::Text("tikv".to_string()),
        ];
        let container_tokens: std::collections::HashSet<u64> =
            extract_array_gin_tokens(&container).into_iter().collect();
        let contained_tokens = extract_array_gin_tokens(&contained);
        for t in contained_tokens {
            assert!(container_tokens.contains(&t));
        }
    }
}
