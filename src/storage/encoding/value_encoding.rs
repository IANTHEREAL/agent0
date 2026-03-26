//! Memcomparable encode/decode for all supported value types.
//!
//! The encoding preserves lexicographic sort order so that TiKV range scans
//! return rows in the correct SQL ORDER BY sequence.

use crate::model::{DataType, Value};
use anyhow::{Context, Result};
use memcomparable::Deserializer;
use rust_decimal::Decimal;

const NULL_TAG: u8 = 0x00;
const NOT_NULL_TAG: u8 = 0x01;
const DECIMAL_SIGN_NEG: u8 = 0x00;
const DECIMAL_SIGN_ZERO: u8 = 0x01;
const DECIMAL_SIGN_POS: u8 = 0x02;
// rust_decimal supports up to 28 significant base-10 digits.
const DECIMAL_MAX_DIGITS: usize = 28;

pub(super) fn encode_value_memcomparable(value: &Value, buf: &mut Vec<u8>) {
    match value {
        Value::Null => {
            buf.push(NULL_TAG);
        }
        Value::Boolean(v) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(v).unwrap());
        }
        Value::Int32(v) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(v).unwrap());
        }
        Value::Int64(v) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(v).unwrap());
        }
        Value::Float64(v) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(v).unwrap());
        }
        Value::Text(s) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(s).unwrap());
        }
        Value::Bytes(b) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(b).unwrap());
        }
        Value::Timestamp(ts) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(ts).unwrap());
        }
        Value::Interval(iv) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(&iv.months).unwrap());
            buf.extend(memcomparable::to_vec(&iv.millis).unwrap());
        }
        Value::Time(t) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(t).unwrap());
        }
        Value::Date(d) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(d).unwrap());
        }
        Value::Uuid(bytes) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(&bytes.to_vec()).unwrap());
        }
        Value::Array(arr) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(&(arr.len() as u32)).unwrap());
            for elem in arr {
                encode_value_memcomparable(elem, buf);
            }
        }
        Value::Vector(vec) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(&(vec.len() as u32)).unwrap());
            for f in vec {
                buf.extend(memcomparable::to_vec(f).unwrap());
            }
        }
        Value::Json(s) | Value::Jsonb(s) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(s).unwrap());
        }
        Value::Numeric(d) => {
            buf.push(NOT_NULL_TAG);
            // Memcomparable encoding for Decimal.
            //
            // We need a stable, order-preserving, *canonical* encoding so that:
            // - Lexicographic order of the encoded bytes matches numeric order.
            // - Equal numeric values encode to identical bytes (e.g. 1.0 == 1.00).
            //
            // Encoding format (32 bytes after NOT_NULL_TAG):
            //   [sign:1][exp:2][digits:28][digits_len:1]
            // - sign: 0x00 negative, 0x01 zero, 0x02 positive
            // - exp: i16 (digits_count - scale), stored as (exp ^ 0x8000) big-endian
            // - digits: absolute mantissa digits, left-aligned, right-padded with ASCII '0'
            // - digits_len: number of mantissa digits (1..=28), needed to decode values whose
            //   mantissa ends in 0 (e.g. 100)
            // For negative values, exp+digits bytes are bitwise inverted to reverse ordering.

            let mut normalized = *d;
            normalized.normalize_assign();
            let unpacked = normalized.unpack();
            let mantissa =
                unpacked.lo as u128 | (unpacked.mid as u128) << 32 | (unpacked.hi as u128) << 64;

            if mantissa == 0 {
                buf.push(DECIMAL_SIGN_ZERO);
                buf.extend_from_slice(&[0u8; 2 + DECIMAL_MAX_DIGITS + 1]);
                return;
            }

            let scale = unpacked.scale as i16;
            let mantissa_str = mantissa.to_string();
            let digits_len =
                u8::try_from(mantissa_str.len()).expect("Decimal mantissa digits fit in u8");
            let digits_count = i16::from(digits_len);
            let exp = digits_count - scale;

            let exp_u16 = (exp as u16) ^ 0x8000;
            let exp_bytes = exp_u16.to_be_bytes();

            let mut digits_buf = [b'0'; DECIMAL_MAX_DIGITS];
            digits_buf[..mantissa_str.len()].copy_from_slice(mantissa_str.as_bytes());

            if unpacked.negative {
                buf.push(DECIMAL_SIGN_NEG);
                buf.extend(exp_bytes.map(|b| !b));
                buf.extend(digits_buf.map(|b| !b));
                buf.push(!digits_len);
            } else {
                buf.push(DECIMAL_SIGN_POS);
                buf.extend(exp_bytes);
                buf.extend(digits_buf);
                buf.push(digits_len);
            }
        }
        Value::Tsvector(s) | Value::Tsquery(s) => {
            buf.push(NOT_NULL_TAG);
            buf.extend(memcomparable::to_vec(s).unwrap());
        }
    }
}

pub fn decode_value_memcomparable(data: &[u8], data_type: &DataType) -> Result<(Value, usize)> {
    if data.is_empty() {
        anyhow::bail!("Empty data for memcomparable decode");
    }

    if data[0] == NULL_TAG {
        return Ok((Value::Null, 1));
    }

    let payload = &data[1..];
    let mut deserializer = Deserializer::new(payload);

    let (value, consumed) = match data_type {
        DataType::Boolean => {
            let v: bool = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Boolean(v), deserializer.position())
        }
        DataType::Int32 => {
            let v: i32 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Int32(v), deserializer.position())
        }
        DataType::Int64 => {
            let v: i64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Int64(v), deserializer.position())
        }
        DataType::Float64 => {
            let v: f64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Float64(v), deserializer.position())
        }
        DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::UserDefined(_) => {
            let v: String = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Text(v), deserializer.position())
        }
        DataType::Bytes => {
            let v: Vec<u8> = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Bytes(v), deserializer.position())
        }
        DataType::Timestamp | DataType::TimestampTz => {
            let v: i64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Timestamp(v), deserializer.position())
        }
        DataType::Interval => {
            let months: i32 = serde::Deserialize::deserialize(&mut deserializer)?;
            let millis: i64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (
                Value::Interval(crate::model::IntervalValue::new(months, millis)),
                deserializer.position(),
            )
        }
        DataType::Time => {
            let v: i64 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Time(v), deserializer.position())
        }
        DataType::Date => {
            let v: i32 = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Date(v), deserializer.position())
        }
        DataType::Uuid => {
            let v: Vec<u8> = serde::Deserialize::deserialize(&mut deserializer)?;
            if v.len() < 16 {
                anyhow::bail!("UUID decode: expected 16 bytes, got {}", v.len());
            }
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&v[..16]);
            (Value::Uuid(bytes), deserializer.position())
        }
        DataType::Json => {
            let v: String = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Json(v), deserializer.position())
        }
        DataType::Jsonb => {
            let v: String = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Jsonb(v), deserializer.position())
        }
        DataType::Array(_) | DataType::Vector(_) => {
            anyhow::bail!("Array/Vector decoding not supported in index keys");
        }
        DataType::Tsvector => {
            let v: String = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Tsvector(v), deserializer.position())
        }
        DataType::Tsquery => {
            let v: String = serde::Deserialize::deserialize(&mut deserializer)?;
            (Value::Tsquery(v), deserializer.position())
        }
        DataType::Numeric { .. } => {
            // Decode memcomparable Numeric:
            //   [sign:1][exp:2][digits:28][digits_len:1]
            let expected = 1 + 2 + DECIMAL_MAX_DIGITS + 1;
            if payload.len() < expected {
                anyhow::bail!(
                    "Numeric decode: expected {} bytes, got {}",
                    expected,
                    payload.len()
                );
            }

            let sign_byte = payload[0];
            if sign_byte == DECIMAL_SIGN_ZERO {
                return Ok((Value::Numeric(Decimal::ZERO), 1 + expected));
            }

            if sign_byte != DECIMAL_SIGN_NEG && sign_byte != DECIMAL_SIGN_POS {
                anyhow::bail!("Numeric decode: invalid sign byte: {}", sign_byte);
            }

            let is_negative = sign_byte == DECIMAL_SIGN_NEG;
            let exp_bytes: [u8; 2] = payload[1..3]
                .try_into()
                .map_err(|_| anyhow::anyhow!("Numeric decode: exponent slice not 2 bytes"))?;
            let digits_bytes: &[u8] = &payload[3..3 + DECIMAL_MAX_DIGITS];
            let digits_len_byte = payload[3 + DECIMAL_MAX_DIGITS];

            let exp_bytes = if is_negative {
                exp_bytes.map(|b| !b)
            } else {
                exp_bytes
            };
            let digits_bytes: Vec<u8> = if is_negative {
                digits_bytes.iter().map(|b| !b).collect()
            } else {
                digits_bytes.to_vec()
            };
            let digits_len_byte = if is_negative {
                !digits_len_byte
            } else {
                digits_len_byte
            };

            let exp_u16 = u16::from_be_bytes(exp_bytes);
            let exp = ((exp_u16 ^ 0x8000) as i16) as i32;

            let digits_len = usize::from(digits_len_byte);
            if !(1..=DECIMAL_MAX_DIGITS).contains(&digits_len) {
                anyhow::bail!("Numeric decode: invalid digits_len: {}", digits_len);
            }
            let digits_str = std::str::from_utf8(&digits_bytes[..digits_len])
                .context("Numeric decode: mantissa digits not utf8")?;
            let mantissa = digits_str
                .parse::<u128>()
                .context("Numeric decode: invalid mantissa digits")?;

            let scale_i32 = (digits_len as i32)
                .checked_sub(exp)
                .ok_or_else(|| anyhow::anyhow!("Numeric decode: invalid scale computation"))?;
            if !(0..=28).contains(&scale_i32) {
                anyhow::bail!("Numeric decode: scale out of range: {}", scale_i32);
            }
            if (mantissa >> 96) != 0 {
                anyhow::bail!("Numeric decode: mantissa out of range");
            }

            let lo = mantissa as u32;
            let mid = (mantissa >> 32) as u32;
            let hi = (mantissa >> 64) as u32;

            let d = Decimal::from_parts(lo, mid, hi, is_negative, scale_i32 as u32);
            (Value::Numeric(d), expected)
        }
        DataType::Unknown => {
            unreachable!("DataType::Unknown must be resolved before reaching storage decoding")
        }
    };

    Ok((value, 1 + consumed))
}

/// Decode PK values from a non-unique index key suffix.
/// `pk_bytes` should be the portion after the separator byte (0x01).
/// `pk_types` describes the data types of each PK column.
pub fn decode_pk_from_index_suffix(pk_bytes: &[u8], pk_types: &[DataType]) -> Result<Vec<Value>> {
    let mut values = Vec::with_capacity(pk_types.len());
    let mut offset = 0;
    for data_type in pk_types {
        let (value, consumed) = decode_value_memcomparable(&pk_bytes[offset..], data_type)?;
        values.push(value);
        offset += consumed;
    }
    Ok(values)
}
