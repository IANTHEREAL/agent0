//! Parameter decoding for the extended query protocol.
//!
//! Converts raw wire bytes from Bind messages into `Value` objects,
//! dispatching on `StoredStatement.parameter_types` (wire `Type`) for
//! correct binary decode width (e.g. INT2 = 2 bytes, FLOAT4 = 4 bytes).

use crate::model::Value;
use pgwire::api::portal::Portal;
use pgwire::api::results::FieldFormat;
use pgwire::api::Type;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};

use super::super::prepared::PreparedStatement;

/// Decode all portal parameters from raw wire bytes into Values.
/// Uses `StoredStatement.parameter_types` (wire Type) for binary decode width.
pub(in crate::protocol::handler) fn decode_parameters(
    portal: &Portal<PreparedStatement>,
) -> PgWireResult<Vec<Option<Value>>> {
    let wire_types = &portal.statement.parameter_types;
    let mut values = Vec::with_capacity(portal.parameters.len());
    for i in 0..portal.parameters.len() {
        let pg_type = wire_types.get(i).cloned().unwrap_or(Type::TEXT);
        let format = portal.parameter_format.format_for(i);
        match &portal.parameters[i] {
            None => values.push(None), // SQL NULL
            Some(bytes) => {
                let val = if format == FieldFormat::Binary {
                    decode_binary(bytes, &pg_type, i)?
                } else {
                    decode_text(bytes, &pg_type, i)?
                };
                values.push(Some(val));
            }
        }
    }
    Ok(values)
}

fn invalid_param(index: usize, type_name: &str, message: String) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        "22P02".to_string(),
        format!(
            "invalid input syntax for parameter ${} ({}): {}",
            index + 1,
            type_name,
            message
        ),
    )))
}

/// PostgreSQL epoch: 2000-01-01 00:00:00 UTC, expressed as Unix seconds.
const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;
/// Days from 1970-01-01 to 2000-01-01.
const DAYS_FROM_1970_TO_2000: i32 = 10_957;

/// Binary decode: dispatches on pgwire Type for correct byte width.
fn decode_binary(bytes: &[u8], pg_type: &Type, index: usize) -> PgWireResult<Value> {
    let err = |msg: String| invalid_param(index, pg_type.name(), msg);

    match pg_type {
        t if *t == Type::BOOL => {
            if bytes.len() != 1 {
                return Err(err(format!("expected 1 byte, got {}", bytes.len())));
            }
            Ok(Value::Boolean(bytes[0] != 0))
        }
        t if *t == Type::INT2 => {
            let arr: [u8; 2] = bytes
                .try_into()
                .map_err(|_| err(format!("expected 2 bytes, got {}", bytes.len())))?;
            Ok(Value::Int32(i16::from_be_bytes(arr) as i32))
        }
        t if *t == Type::INT4 => {
            let arr: [u8; 4] = bytes
                .try_into()
                .map_err(|_| err(format!("expected 4 bytes, got {}", bytes.len())))?;
            Ok(Value::Int32(i32::from_be_bytes(arr)))
        }
        t if *t == Type::INT8 => {
            let arr: [u8; 8] = bytes
                .try_into()
                .map_err(|_| err(format!("expected 8 bytes, got {}", bytes.len())))?;
            Ok(Value::Int64(i64::from_be_bytes(arr)))
        }
        t if *t == Type::FLOAT4 => {
            let arr: [u8; 4] = bytes
                .try_into()
                .map_err(|_| err(format!("expected 4 bytes, got {}", bytes.len())))?;
            Ok(Value::Float64(f32::from_be_bytes(arr) as f64))
        }
        t if *t == Type::FLOAT8 => {
            let arr: [u8; 8] = bytes
                .try_into()
                .map_err(|_| err(format!("expected 8 bytes, got {}", bytes.len())))?;
            Ok(Value::Float64(f64::from_be_bytes(arr)))
        }
        t if *t == Type::TEXT || *t == Type::VARCHAR || *t == Type::BPCHAR || *t == Type::NAME => {
            let s = std::str::from_utf8(bytes).map_err(|e| err(e.to_string()))?;
            Ok(Value::Text(s.to_string()))
        }
        t if *t == Type::TIMESTAMP => {
            // PostgreSQL binary: i64 µs since 2000-01-01 00:00:00 UTC
            let arr: [u8; 8] = bytes
                .try_into()
                .map_err(|_| err(format!("expected 8 bytes, got {}", bytes.len())))?;
            let pg_us = i64::from_be_bytes(arr);
            // Convert to unix millis: (pg_us / 1000) + (PG_EPOCH_UNIX_SECS * 1000)
            let unix_ms = pg_us / 1000 + PG_EPOCH_UNIX_SECS * 1000;
            Ok(Value::Timestamp(unix_ms))
        }
        t if *t == Type::TIMESTAMPTZ => {
            // Same binary format as TIMESTAMP (µs since PG epoch)
            let arr: [u8; 8] = bytes
                .try_into()
                .map_err(|_| err(format!("expected 8 bytes, got {}", bytes.len())))?;
            let pg_us = i64::from_be_bytes(arr);
            let unix_ms = pg_us / 1000 + PG_EPOCH_UNIX_SECS * 1000;
            Ok(Value::Timestamp(unix_ms))
        }
        t if *t == Type::DATE => {
            // PostgreSQL binary: i32 days since 2000-01-01
            let arr: [u8; 4] = bytes
                .try_into()
                .map_err(|_| err(format!("expected 4 bytes, got {}", bytes.len())))?;
            let pg_days = i32::from_be_bytes(arr);
            // Convert to days since 1970-01-01
            let unix_days = pg_days + DAYS_FROM_1970_TO_2000;
            Ok(Value::Date(unix_days))
        }
        t if *t == Type::UUID => {
            let arr: [u8; 16] = bytes
                .try_into()
                .map_err(|_| err(format!("expected 16 bytes, got {}", bytes.len())))?;
            Ok(Value::Uuid(arr))
        }
        t if *t == Type::BYTEA => Ok(Value::Bytes(bytes.to_vec())),
        t if *t == Type::JSON => {
            let s = std::str::from_utf8(bytes).map_err(|e| err(e.to_string()))?;
            Ok(Value::Json(s.to_string()))
        }
        t if *t == Type::JSONB => {
            // PostgreSQL JSONB binary format: version byte (0x01) + JSON text
            if bytes.is_empty() {
                return Err(err("empty JSONB payload".to_string()));
            }
            let json_bytes = if bytes[0] == 1 {
                &bytes[1..]
            } else {
                return Err(err(format!(
                    "unsupported JSONB wire format version: {}",
                    bytes[0]
                )));
            };
            let s = std::str::from_utf8(json_bytes).map_err(|e| err(e.to_string()))?;
            let parsed: serde_json::Value =
                serde_json::from_str(s).map_err(|e| err(e.to_string()))?;
            Ok(Value::Jsonb(parsed.to_string()))
        }
        t if *t == Type::NUMERIC => {
            // PostgreSQL NUMERIC binary is complex (ndigits, weight, sign, dscale, digits).
            // Fall back to text decoding for now: attempt UTF-8 parse.
            if let Ok(s) = std::str::from_utf8(bytes) {
                return decode_text_value(s, s.trim(), &Type::NUMERIC, index);
            }
            Err(err("binary NUMERIC decoding not supported".to_string()))
        }
        t if *t == Type::INTERVAL => {
            // PostgreSQL interval binary: 8 bytes µs + 4 bytes days + 4 bytes months
            if bytes.len() != 16 {
                return Err(err(format!("expected 16 bytes, got {}", bytes.len())));
            }
            let us = i64::from_be_bytes(bytes[0..8].try_into().unwrap());
            let days = i32::from_be_bytes(bytes[8..12].try_into().unwrap());
            let months = i32::from_be_bytes(bytes[12..16].try_into().unwrap());
            let total_ms = us / 1000
                + (days as i64) * 24 * 60 * 60 * 1000
                + (months as i64) * 30 * 24 * 60 * 60 * 1000;
            Ok(Value::Interval(crate::model::IntervalValue::from_millis(
                total_ms,
            )))
        }
        t if *t == Type::TIME => {
            // PostgreSQL time binary: i64 µs since midnight
            let arr: [u8; 8] = bytes
                .try_into()
                .map_err(|_| err(format!("expected 8 bytes, got {}", bytes.len())))?;
            let us = i64::from_be_bytes(arr);
            // Value::Time stores µs since midnight
            Ok(Value::Time(us))
        }
        // Type::UNKNOWN (OID 705) - pgx/GORM sends binary unknown
        t if *t == Type::UNKNOWN => {
            // Prefer text decoding when possible
            if let Ok(s) = std::str::from_utf8(bytes) {
                let trimmed = s.trim();
                // Try integer first (common for LIMIT/OFFSET)
                if let Ok(v) = trimmed.parse::<i64>() {
                    return Ok(Value::Int64(v));
                }
                return Ok(Value::Text(s.to_string()));
            }
            // Non-UTF8: attempt fixed-width numeric decode
            match bytes.len() {
                8 => {
                    let arr: [u8; 8] = bytes.try_into().unwrap();
                    Ok(Value::Int64(i64::from_be_bytes(arr)))
                }
                4 => {
                    let arr: [u8; 4] = bytes.try_into().unwrap();
                    Ok(Value::Int32(i32::from_be_bytes(arr)))
                }
                2 => {
                    let arr: [u8; 2] = bytes.try_into().unwrap();
                    Ok(Value::Int32(i16::from_be_bytes(arr) as i32))
                }
                _ => Ok(Value::Bytes(bytes.to_vec())),
            }
        }
        _ => Err(PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_string(),
            "0A000".to_string(),
            format!(
                "unsupported binary parameter type {} for parameter ${}",
                pg_type.name(),
                index + 1
            ),
        )))),
    }
}

/// Text decode: UTF-8 string → Value, dispatched by type.
fn decode_text(bytes: &[u8], pg_type: &Type, index: usize) -> PgWireResult<Value> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid_param(index, pg_type.name(), "invalid UTF-8".to_string()))?;
    let trimmed = text.trim();
    decode_text_value(text, trimmed, pg_type, index)
}

/// Parse a text value string into a Value based on the pgwire type.
fn decode_text_value(
    raw: &str,
    trimmed: &str,
    pg_type: &Type,
    index: usize,
) -> PgWireResult<Value> {
    let err = |msg: String| invalid_param(index, pg_type.name(), msg);

    match pg_type {
        t if *t == Type::BOOL => {
            let lower = trimmed.to_ascii_lowercase();
            match lower.as_str() {
                "t" | "true" | "1" | "yes" | "on" => Ok(Value::Boolean(true)),
                "f" | "false" | "0" | "no" | "off" => Ok(Value::Boolean(false)),
                _ => Err(err(format!("\"{}\"", trimmed))),
            }
        }
        t if *t == Type::INT2 => trimmed
            .parse::<i16>()
            .map(|v| Value::Int32(v as i32))
            .map_err(|e| err(e.to_string())),
        t if *t == Type::INT4 => trimmed
            .parse::<i32>()
            .map(Value::Int32)
            .map_err(|e| err(e.to_string())),
        t if *t == Type::INT8 => trimmed
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|e| err(e.to_string())),
        t if *t == Type::FLOAT4 => {
            let v = trimmed.parse::<f32>().map_err(|e| err(e.to_string()))?;
            Ok(Value::Float64(v as f64))
        }
        t if *t == Type::FLOAT8 => {
            let v = trimmed.parse::<f64>().map_err(|e| err(e.to_string()))?;
            Ok(Value::Float64(v))
        }
        t if *t == Type::TEXT || *t == Type::VARCHAR || *t == Type::BPCHAR || *t == Type::NAME => {
            Ok(Value::Text(raw.to_string()))
        }
        t if *t == Type::TIMESTAMP || *t == Type::TIMESTAMPTZ => {
            crate::sql::expr::parse_timestamp_string(trimmed)
                .map_err(|_| err(format!("\"{}\"", trimmed)))
        }
        t if *t == Type::DATE => crate::model::date::parse_date_days(trimmed)
            .map(Value::Date)
            .map_err(|_| err(format!("\"{}\"", trimmed))),
        t if *t == Type::UUID => uuid::Uuid::parse_str(trimmed)
            .map(|u| Value::Uuid(*u.as_bytes()))
            .map_err(|e| err(e.to_string())),
        t if *t == Type::BYTEA => {
            if trimmed.starts_with("\\x") {
                hex::decode(&trimmed[2..])
                    .map(Value::Bytes)
                    .map_err(|e| err(e.to_string()))
            } else {
                Ok(Value::Bytes(trimmed.as_bytes().to_vec()))
            }
        }
        t if *t == Type::JSON => Ok(Value::Json(trimmed.to_string())),
        t if *t == Type::JSONB => {
            // Normalize via serde_json roundtrip for canonical form
            let parsed: serde_json::Value =
                serde_json::from_str(trimmed).map_err(|e| err(e.to_string()))?;
            Ok(Value::Jsonb(parsed.to_string()))
        }
        t if *t == Type::INTERVAL => crate::sql::expr::parse_interval_string(trimmed)
            .map_err(|_| err(format!("\"{}\"", trimmed))),
        t if *t == Type::TIME => crate::sql::value_coercion::parse_time_string(trimmed)
            .map(Value::Time)
            .ok_or_else(|| err(format!("\"{}\"", trimmed))),
        t if *t == Type::NUMERIC => {
            use rust_decimal::Decimal;
            use std::str::FromStr;
            Decimal::from_str(trimmed)
                .map(Value::Numeric)
                .map_err(|e| err(e.to_string()))
        }
        // Array types
        t if *t == Type::INT4_ARRAY
            || *t == Type::INT8_ARRAY
            || *t == Type::TEXT_ARRAY
            || *t == Type::FLOAT8_ARRAY
            || *t == Type::BOOL_ARRAY =>
        {
            crate::sql::value_coercion::parse_pg_array(trimmed)
                .map(Value::Array)
                .map_err(|_| err(format!("\"{}\"", trimmed)))
        }
        // Default: treat as text
        _ => Ok(Value::Text(raw.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_text_preserves_whitespace_for_text_types() {
        let v = decode_text(b"  hello  ", &Type::TEXT, 0).unwrap();
        assert_eq!(v, Value::Text("  hello  ".to_string()));
    }

    #[test]
    fn decode_text_numeric_still_accepts_surrounding_whitespace() {
        let v = decode_text(b"  42  ", &Type::INT4, 0).unwrap();
        assert_eq!(v, Value::Int32(42));
    }
}
