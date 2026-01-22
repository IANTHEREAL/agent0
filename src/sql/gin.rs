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
    pub(crate) fn is_empty(&self) -> bool {
        self.key_values.is_empty() && self.key_exists.is_empty()
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> JsonValue {
        serde_json::from_str(s).unwrap()
    }

    fn to_sets(tokens: GinTokens) -> (std::collections::HashSet<u64>, std::collections::HashSet<u64>) {
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
}

