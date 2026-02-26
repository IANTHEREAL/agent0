//! In-memory key encoding for SQL operators.
//!
//! Produces deterministic, injective byte keys for `Value` / `&[Value]` suitable
//! for use as keys in `HashMap` / `HashSet` during GROUP BY, DISTINCT, SET operations,
//! WINDOW PARTITION BY, and ANALYZE statistics collection.
//!
//! These keys are **ephemeral** — never persisted to storage. The encoding format
//! is internal to this module and may change between releases.
//!
//! Design: each value is prefixed by a 1-byte type discriminant, followed by a
//! fixed-width or length-prefixed payload. Canonicalization (NaN → canonical NaN,
//! -0.0 → 0.0, Decimal scale normalization) is applied before encoding so that
//! semantically equal values produce identical byte sequences.

use rust_decimal::Decimal;

use crate::model::Value;

// ── Canonicalization ─────────────────────────────────────────────────────

const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;

fn canonicalize_f64(value: f64) -> f64 {
    if value.is_nan() {
        f64::from_bits(CANONICAL_NAN_BITS)
    } else if value == 0.0 {
        0.0 // normalize -0.0
    } else {
        value
    }
}

fn canonicalize_decimal(value: &Decimal) -> Decimal {
    if value.is_zero() {
        Decimal::ZERO
    } else {
        value.normalize()
    }
}

/// Return the canonicalized form of a value (NaN → canonical NaN, -0.0 → 0.0,
/// Decimal scale normalization, recursive into arrays). Values that need no
/// canonicalization are cloned as-is.
pub(crate) fn canonicalize_value(value: &Value) -> Value {
    match value {
        Value::Float64(f) => Value::Float64(canonicalize_f64(*f)),
        Value::Numeric(d) => Value::Numeric(canonicalize_decimal(d)),
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_value).collect()),
        _ => value.clone(),
    }
}

// ── Type discriminants ───────────────────────────────────────────────────
// Must be unique per variant so the encoding is injective.

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_I32: u8 = 2;
const TAG_I64: u8 = 3;
const TAG_F64: u8 = 4;
const TAG_TEXT: u8 = 5;
const TAG_BYTES: u8 = 6;
const TAG_TIMESTAMP: u8 = 7;
const TAG_INTERVAL: u8 = 8;
const TAG_UUID: u8 = 9;
const TAG_ARRAY: u8 = 10;
const TAG_VECTOR: u8 = 11;
const TAG_JSON: u8 = 12;
const TAG_JSONB: u8 = 13;
const TAG_TIME: u8 = 14;
const TAG_DATE: u8 = 15;
const TAG_NUMERIC: u8 = 16;
const TAG_TSVECTOR: u8 = 17;
const TAG_TSQUERY: u8 = 18;

// ── Encoder ──────────────────────────────────────────────────────────────

fn encode_value(buf: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => buf.push(TAG_NULL),
        Value::Boolean(b) => {
            buf.push(TAG_BOOL);
            buf.push(*b as u8);
        }
        Value::Int32(n) => {
            buf.push(TAG_I32);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Int64(n) => {
            buf.push(TAG_I64);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Float64(f) => {
            buf.push(TAG_F64);
            buf.extend_from_slice(&canonicalize_f64(*f).to_bits().to_le_bytes());
        }
        Value::Text(s) => {
            buf.push(TAG_TEXT);
            buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            buf.push(TAG_BYTES);
            buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Timestamp(ts) => {
            buf.push(TAG_TIMESTAMP);
            buf.extend_from_slice(&ts.to_le_bytes());
        }
        Value::Interval(iv) => {
            buf.push(TAG_INTERVAL);
            buf.extend_from_slice(&iv.months.to_le_bytes());
            buf.extend_from_slice(&iv.millis.to_le_bytes());
        }
        Value::Uuid(bytes) => {
            buf.push(TAG_UUID);
            buf.extend_from_slice(bytes);
        }
        Value::Array(arr) => {
            buf.push(TAG_ARRAY);
            buf.extend_from_slice(&(arr.len() as u64).to_le_bytes());
            for elem in arr {
                encode_value(buf, elem);
            }
        }
        Value::Vector(vec) => {
            buf.push(TAG_VECTOR);
            buf.extend_from_slice(&(vec.len() as u64).to_le_bytes());
            for f in vec {
                buf.extend_from_slice(&f.to_bits().to_le_bytes());
            }
        }
        Value::Json(s) => {
            buf.push(TAG_JSON);
            buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Jsonb(s) => {
            buf.push(TAG_JSONB);
            buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Time(t) => {
            buf.push(TAG_TIME);
            buf.extend_from_slice(&t.to_le_bytes());
        }
        Value::Date(d) => {
            buf.push(TAG_DATE);
            buf.extend_from_slice(&d.to_le_bytes());
        }
        Value::Numeric(d) => {
            buf.push(TAG_NUMERIC);
            let normalized = canonicalize_decimal(d);
            buf.extend_from_slice(&normalized.serialize());
        }
        Value::Tsvector(s) => {
            buf.push(TAG_TSVECTOR);
            buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Tsquery(s) => {
            buf.push(TAG_TSQUERY);
            buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
    }
}

/// Encode a single value into a deterministic byte key for in-memory operations.
///
/// Applies canonicalization (NaN, -0.0, Decimal normalization) so semantically
/// equal values produce identical byte sequences. These keys are ephemeral and
/// never persisted to storage.
pub(crate) fn encode_value_key(value: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_value(&mut buf, value);
    buf
}

/// Encode a slice of values into a deterministic composite byte key.
///
/// Used for GROUP BY keys, DISTINCT row keys, SET operation row keys, and
/// WINDOW PARTITION BY keys. Ephemeral — never persisted to storage.
pub(crate) fn encode_values_key(values: &[Value]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(values.len() * 16);
    buf.extend_from_slice(&(values.len() as u64).to_le_bytes());
    for value in values {
        encode_value(&mut buf, value);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn encode_values_key_canonicalizes_nan_payloads() {
        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(nan1.is_nan() && nan2.is_nan());

        let key1 = encode_values_key(&[Value::Float64(nan1)]);
        let key2 = encode_values_key(&[Value::Float64(nan2)]);
        assert_eq!(key1, key2);
    }

    #[test]
    fn encode_values_key_normalizes_negative_zero() {
        let key1 = encode_values_key(&[Value::Float64(-0.0)]);
        let key2 = encode_values_key(&[Value::Float64(0.0)]);
        assert_eq!(key1, key2);
    }

    #[test]
    fn encode_value_key_canonicalizes_nan_payloads() {
        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);

        let key1 = encode_value_key(&Value::Float64(nan1));
        let key2 = encode_value_key(&Value::Float64(nan2));
        assert_eq!(key1, key2);
    }

    #[test]
    fn encode_value_key_normalizes_negative_zero() {
        let key1 = encode_value_key(&Value::Float64(-0.0));
        let key2 = encode_value_key(&Value::Float64(0.0));
        assert_eq!(key1, key2);
    }

    #[test]
    fn encode_value_key_canonicalizes_arrays_with_floats() {
        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);

        let v1 = Value::Array(vec![Value::Float64(nan1), Value::Float64(-0.0)]);
        let v2 = Value::Array(vec![Value::Float64(nan2), Value::Float64(0.0)]);

        let key1 = encode_value_key(&v1);
        let key2 = encode_value_key(&v2);
        assert_eq!(key1, key2);
    }

    #[test]
    fn encode_values_key_canonicalizes_numeric_scales() {
        let d1 = Decimal::from_str("1.0").unwrap();
        let d2 = Decimal::from_str("1.00").unwrap();

        let key1 = encode_values_key(&[Value::Numeric(d1)]);
        let key2 = encode_values_key(&[Value::Numeric(d2)]);
        assert_eq!(key1, key2);
    }

    #[test]
    fn encode_value_key_canonicalizes_numeric_scales() {
        let d1 = Decimal::from_str("1.0").unwrap();
        let d2 = Decimal::from_str("1.00").unwrap();

        let key1 = encode_value_key(&Value::Numeric(d1));
        let key2 = encode_value_key(&Value::Numeric(d2));
        assert_eq!(key1, key2);
    }

    #[test]
    fn encode_value_key_canonicalizes_numeric_zeros() {
        let d1 = Decimal::from_str("0.00").unwrap();
        let d2 = Decimal::from_str("-0.0").unwrap();

        let key1 = encode_value_key(&Value::Numeric(d1));
        let key2 = encode_value_key(&Value::Numeric(d2));
        assert_eq!(key1, key2);
    }

    #[test]
    fn canonicalize_value_normalizes_neg_zero_and_nan() {
        let val = canonicalize_value(&Value::Float64(-0.0));
        assert_eq!(val, Value::Float64(0.0));
        assert!(!match val {
            Value::Float64(f) => f.is_sign_negative(),
            _ => unreachable!(),
        });

        let nan = f64::from_bits(0x7ff8_0000_0000_0001);
        let val = canonicalize_value(&Value::Float64(nan));
        match val {
            Value::Float64(f) => {
                assert!(f.is_nan());
                assert_eq!(f.to_bits(), CANONICAL_NAN_BITS);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn canonicalize_value_normalizes_decimal() {
        let d = Decimal::from_str("1.00").unwrap();
        let val = canonicalize_value(&Value::Numeric(d));
        assert_eq!(val, Value::Numeric(Decimal::from_str("1").unwrap()));
    }

    #[test]
    fn encode_value_key_distinct_types_produce_distinct_keys() {
        let k_i32 = encode_value_key(&Value::Int32(0));
        let k_i64 = encode_value_key(&Value::Int64(0));
        let k_null = encode_value_key(&Value::Null);
        let k_bool = encode_value_key(&Value::Boolean(false));

        assert_ne!(k_i32, k_i64);
        assert_ne!(k_i32, k_null);
        assert_ne!(k_i64, k_null);
        assert_ne!(k_bool, k_null);
        assert_ne!(k_bool, k_i32);
    }
}
