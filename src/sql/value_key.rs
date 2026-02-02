use anyhow::{Context, Result};
use rust_decimal::Decimal;

use crate::types::Value;

// Canonical quiet NaN payload to ensure all NaNs key/hash the same.
const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;

fn f64_needs_key_canonicalization(value: f64) -> bool {
    if value.is_nan() {
        value.to_bits() != CANONICAL_NAN_BITS
    } else {
        value == 0.0 && value.is_sign_negative()
    }
}

fn canonicalize_f64_for_key(value: f64) -> f64 {
    if value.is_nan() {
        f64::from_bits(CANONICAL_NAN_BITS)
    } else if value == 0.0 {
        // Normalize -0.0 to 0.0.
        0.0
    } else {
        value
    }
}

fn decimal_needs_key_canonicalization(value: &Decimal) -> bool {
    if value.is_zero() {
        // Normalize any signed zero and any non-zero scale zero (e.g., 0.00) to Decimal::ZERO.
        let unpacked = value.unpack();
        unpacked.negative || unpacked.scale != 0
    } else {
        // Normalize numerically-equal values that only differ by trailing zeros in the mantissa
        // (e.g., 1.0 vs 1.00) so keying for DISTINCT/GROUP BY/SET ops/partitioning treats them
        // as equal.
        value.normalize().scale() != value.scale()
    }
}

fn canonicalize_decimal_for_key(value: &Decimal) -> Decimal {
    if value.is_zero() {
        Decimal::ZERO
    } else {
        value.normalize()
    }
}

fn value_needs_key_canonicalization(value: &Value) -> bool {
    match value {
        Value::Float64(f) => f64_needs_key_canonicalization(*f),
        Value::Numeric(d) => decimal_needs_key_canonicalization(d),
        Value::Array(values) => values.iter().any(value_needs_key_canonicalization),
        _ => false,
    }
}

fn canonicalize_value_for_key(value: &Value) -> Value {
    match value {
        Value::Float64(f) => Value::Float64(canonicalize_f64_for_key(*f)),
        Value::Numeric(d) => Value::Numeric(canonicalize_decimal_for_key(d)),
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_value_for_key).collect()),
        _ => value.clone(),
    }
}

pub(crate) fn serialize_value_for_key(value: &Value) -> Result<Vec<u8>> {
    if value_needs_key_canonicalization(value) {
        bincode::serialize(&canonicalize_value_for_key(value))
            .context("Failed to serialize canonicalized key value")
    } else {
        bincode::serialize(value).context("Failed to serialize key value")
    }
}

pub(crate) fn serialize_values_for_key(values: &[Value]) -> Result<Vec<u8>> {
    if values.iter().any(value_needs_key_canonicalization) {
        let normalized: Vec<Value> = values.iter().map(canonicalize_value_for_key).collect();
        bincode::serialize(&normalized).context("Failed to serialize canonicalized key values")
    } else {
        bincode::serialize(values).context("Failed to serialize key values")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn serialize_values_for_key_canonicalizes_nan_payloads() {
        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(nan1.is_nan() && nan2.is_nan());

        let key1 = serialize_values_for_key(&[Value::Float64(nan1)]).unwrap();
        let key2 = serialize_values_for_key(&[Value::Float64(nan2)]).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn serialize_values_for_key_normalizes_negative_zero() {
        let key1 = serialize_values_for_key(&[Value::Float64(-0.0)]).unwrap();
        let key2 = serialize_values_for_key(&[Value::Float64(0.0)]).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn serialize_value_for_key_canonicalizes_nan_payloads() {
        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);

        let key1 = serialize_value_for_key(&Value::Float64(nan1)).unwrap();
        let key2 = serialize_value_for_key(&Value::Float64(nan2)).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn serialize_value_for_key_normalizes_negative_zero() {
        let key1 = serialize_value_for_key(&Value::Float64(-0.0)).unwrap();
        let key2 = serialize_value_for_key(&Value::Float64(0.0)).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn serialize_value_for_key_canonicalizes_arrays_with_floats() {
        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);

        let v1 = Value::Array(vec![Value::Float64(nan1), Value::Float64(-0.0)]);
        let v2 = Value::Array(vec![Value::Float64(nan2), Value::Float64(0.0)]);

        let key1 = serialize_value_for_key(&v1).unwrap();
        let key2 = serialize_value_for_key(&v2).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn serialize_values_for_key_canonicalizes_numeric_scales() {
        let d1 = Decimal::from_str("1.0").unwrap();
        let d2 = Decimal::from_str("1.00").unwrap();

        let key1 = serialize_values_for_key(&[Value::Numeric(d1)]).unwrap();
        let key2 = serialize_values_for_key(&[Value::Numeric(d2)]).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn serialize_value_for_key_canonicalizes_numeric_scales() {
        let d1 = Decimal::from_str("1.0").unwrap();
        let d2 = Decimal::from_str("1.00").unwrap();

        let key1 = serialize_value_for_key(&Value::Numeric(d1)).unwrap();
        let key2 = serialize_value_for_key(&Value::Numeric(d2)).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn serialize_value_for_key_canonicalizes_numeric_zeros() {
        let d1 = Decimal::from_str("0.00").unwrap();
        let d2 = Decimal::from_str("-0.0").unwrap();

        let key1 = serialize_value_for_key(&Value::Numeric(d1)).unwrap();
        let key2 = serialize_value_for_key(&Value::Numeric(d2)).unwrap();
        assert_eq!(key1, key2);
    }
}
