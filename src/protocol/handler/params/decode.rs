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
        let pg_type = wire_types.get(i).cloned().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "08P01".to_string(),
                format!(
                    "no type resolved for parameter ${}: expected {} parameter types, got {}",
                    i + 1,
                    portal.parameters.len(),
                    wire_types.len(),
                ),
            )))
        })?;
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
    invalid_param_with_code(index, type_name, "22P02", message)
}

const INSUFFICIENT_DATA_LEFT_IN_MESSAGE: &str = "insufficient data left in message";

fn insufficient_data_param(_index: usize, _type_name: &str, _message: String) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        "08P01".to_string(),
        INSUFFICIENT_DATA_LEFT_IN_MESSAGE.to_string(),
    )))
}

fn invalid_param_with_code(
    index: usize,
    type_name: &str,
    code: &str,
    message: String,
) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        code.to_string(),
        format!(
            "invalid input syntax for parameter ${} ({}): {}",
            index + 1,
            type_name,
            message
        ),
    )))
}

fn invalid_param_character_not_in_repertoire(
    index: usize,
    type_name: &str,
    message: String,
) -> PgWireError {
    invalid_param_with_code(index, type_name, "22021", message)
}

fn format_hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("0x{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn invalid_utf8_param(
    index: usize,
    type_name: &str,
    bytes: &[u8],
    valid_up_to: usize,
    _error_len: Option<usize>,
) -> PgWireError {
    let lead = bytes[valid_up_to];
    let seq_len = match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    };
    let n = seq_len.min(bytes.len() - valid_up_to);
    let offending = format_hex_bytes(&bytes[valid_up_to..valid_up_to + n]);
    invalid_param_character_not_in_repertoire(
        index,
        type_name,
        format!("invalid byte sequence for encoding \"UTF8\": {offending}"),
    )
}

fn array_element_type(array_type: &Type) -> Option<Type> {
    match array_type {
        t if *t == Type::INT4_ARRAY => Some(Type::INT4),
        t if *t == Type::INT8_ARRAY => Some(Type::INT8),
        t if *t == Type::TEXT_ARRAY => Some(Type::TEXT),
        t if *t == Type::VARCHAR_ARRAY => Some(Type::VARCHAR),
        t if *t == Type::NAME_ARRAY => Some(Type::NAME),
        t if *t == Type::FLOAT8_ARRAY => Some(Type::FLOAT8),
        t if *t == Type::BOOL_ARRAY => Some(Type::BOOL),
        t if *t == Type::TIMESTAMP_ARRAY => Some(Type::TIMESTAMP),
        t if *t == Type::TIMESTAMPTZ_ARRAY => Some(Type::TIMESTAMPTZ),
        t if *t == Type::DATE_ARRAY => Some(Type::DATE),
        t if *t == Type::INTERVAL_ARRAY => Some(Type::INTERVAL),
        t if *t == Type::UUID_ARRAY => Some(Type::UUID),
        t if *t == Type::BYTEA_ARRAY => Some(Type::BYTEA),
        t if *t == Type::JSON_ARRAY => Some(Type::JSON),
        t if *t == Type::JSONB_ARRAY => Some(Type::JSONB),
        t if *t == Type::TIME_ARRAY => Some(Type::TIME),
        t if *t == Type::NUMERIC_ARRAY => Some(Type::NUMERIC),
        _ => None,
    }
}

fn decode_binary_array(bytes: &[u8], pg_type: &Type, index: usize) -> PgWireResult<Value> {
    let err = |msg: String| invalid_param(index, pg_type.name(), msg);
    let insufficient_data = |msg: String| insufficient_data_param(index, pg_type.name(), msg);
    let mut pos = 0usize;

    let read_i32 = |bytes: &[u8], pos: &mut usize| -> PgWireResult<i32> {
        if bytes.len().saturating_sub(*pos) < 4 {
            return Err(insufficient_data(
                "truncated binary array header".to_string(),
            ));
        }
        let v = i32::from_be_bytes(
            bytes[*pos..*pos + 4]
                .try_into()
                .map_err(|_| insufficient_data("truncated binary array header".to_string()))?,
        );
        *pos += 4;
        Ok(v)
    };

    let ndim = read_i32(bytes, &mut pos)?;
    let _has_nulls = read_i32(bytes, &mut pos)?;
    let _elem_oid = read_i32(bytes, &mut pos)?;

    if ndim < 0 {
        return Err(err(format!("invalid binary array ndim {}", ndim)));
    }
    if ndim == 0 {
        return Ok(Value::Array(vec![]));
    }
    if ndim != 1 {
        return Err(err(format!(
            "binary arrays with ndim={} are not supported yet",
            ndim
        )));
    }

    let len = read_i32(bytes, &mut pos)?;
    let _lbound = read_i32(bytes, &mut pos)?;
    if len < 0 {
        return Err(err(format!("invalid binary array length {}", len)));
    }

    let elem_type = array_element_type(pg_type)
        .ok_or_else(|| err(format!("unsupported binary array type {}", pg_type.name())))?;
    let mut values = Vec::with_capacity(len as usize);

    for _ in 0..len {
        let elem_len = read_i32(bytes, &mut pos)?;
        if elem_len < -1 {
            return Err(err(format!(
                "invalid binary array element length {}",
                elem_len
            )));
        }
        if elem_len == -1 {
            values.push(Value::Null);
            continue;
        }
        let elem_len = elem_len as usize;
        if bytes.len().saturating_sub(pos) < elem_len {
            return Err(insufficient_data(format!(
                "truncated binary array element: need {} bytes, have {}",
                elem_len,
                bytes.len().saturating_sub(pos)
            )));
        }
        let elem_bytes = &bytes[pos..pos + elem_len];
        pos += elem_len;
        let elem = decode_binary(elem_bytes, &elem_type, index)?;
        values.push(elem);
    }

    if pos != bytes.len() {
        return Err(err(format!(
            "unexpected trailing bytes in binary array payload: {}",
            bytes.len() - pos
        )));
    }

    Ok(Value::Array(values))
}

/// PostgreSQL epoch: 2000-01-01 00:00:00 UTC, expressed as Unix seconds.
const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;
/// Days from 1970-01-01 to 2000-01-01.
const DAYS_FROM_1970_TO_2000: i32 = 10_957;
const PG_NUMERIC_POS: i16 = 0x0000;
const PG_NUMERIC_NEG: i16 = 0x4000;
const PG_NUMERIC_NAN: i16 = 0xC000u16 as i16;
const MAX_RUNTIME_NUMERIC_SCALE: u32 = rust_decimal::Decimal::MAX_SCALE;

fn decode_binary_numeric(bytes: &[u8], pg_type: &Type, index: usize) -> PgWireResult<Value> {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let err = |msg: String| invalid_param(index, pg_type.name(), msg);
    let insufficient_data = |msg: String| insufficient_data_param(index, pg_type.name(), msg);

    if bytes.len() < 8 {
        return Err(insufficient_data(format!(
            "truncated NUMERIC binary payload: expected at least 8 bytes, got {}",
            bytes.len()
        )));
    }

    let ndigits = i16::from_be_bytes(
        bytes[0..2]
            .try_into()
            .map_err(|_| insufficient_data("truncated NUMERIC header".to_string()))?,
    );
    let weight = i16::from_be_bytes(
        bytes[2..4]
            .try_into()
            .map_err(|_| insufficient_data("truncated NUMERIC header".to_string()))?,
    );
    let sign = i16::from_be_bytes(
        bytes[4..6]
            .try_into()
            .map_err(|_| insufficient_data("truncated NUMERIC header".to_string()))?,
    );
    let dscale = i16::from_be_bytes(
        bytes[6..8]
            .try_into()
            .map_err(|_| insufficient_data("truncated NUMERIC header".to_string()))?,
    );

    if ndigits < 0 {
        return Err(err(format!("invalid NUMERIC ndigits {}", ndigits)));
    }
    if dscale < 0 {
        return Err(err(format!("invalid NUMERIC dscale {}", dscale)));
    }
    if sign == PG_NUMERIC_NAN {
        return Err(err("NUMERIC NaN is not supported".to_string()));
    }
    if sign != PG_NUMERIC_POS && sign != PG_NUMERIC_NEG {
        return Err(err(format!("invalid NUMERIC sign 0x{sign:04x}")));
    }

    let ndigits = ndigits as usize;
    let expected = 8usize + ndigits * 2usize;
    if bytes.len() < expected {
        return Err(insufficient_data(format!(
            "truncated NUMERIC binary payload: expected {}, got {}",
            expected,
            bytes.len()
        )));
    }
    if bytes.len() > expected {
        return Err(err(format!(
            "invalid NUMERIC binary length: expected {}, got {}",
            expected,
            bytes.len()
        )));
    }

    let mut digits = Vec::with_capacity(ndigits);
    let mut pos = 8usize;
    for _ in 0..ndigits {
        let d = i16::from_be_bytes(
            bytes[pos..pos + 2]
                .try_into()
                .map_err(|_| insufficient_data("truncated NUMERIC digit".to_string()))?,
        );
        pos += 2;
        if !(0..=9999).contains(&d) {
            return Err(err(format!("invalid NUMERIC base-10000 digit {}", d)));
        }
        digits.push(d as u16);
    }

    let dscale = dscale as usize;
    let integer_group_count = (weight as i32) + 1;

    let mut integer_groups: Vec<u16> = Vec::new();
    let mut fractional_groups: Vec<u16> = Vec::new();

    if integer_group_count <= 0 {
        // Number is in (0, 1): all transmitted groups are fractional, but we may need
        // to prepend extra zero groups based on weight.
        fractional_groups.resize((-integer_group_count) as usize, 0);
        fractional_groups.extend(digits.iter().copied());
    } else {
        let int_groups = integer_group_count as usize;
        if ndigits >= int_groups {
            integer_groups.extend(digits[..int_groups].iter().copied());
            fractional_groups.extend(digits[int_groups..].iter().copied());
        } else {
            integer_groups.extend(digits.iter().copied());
            integer_groups.resize(int_groups, 0);
        }
    }

    let int_non_zero = integer_groups.iter().position(|d| *d != 0);
    let integer_part = if let Some(first) = int_non_zero {
        let mut s = String::new();
        s.push_str(&integer_groups[first].to_string());
        for d in &integer_groups[first + 1..] {
            s.push_str(&format!("{d:04}"));
        }
        s
    } else {
        "0".to_string()
    };

    let mut fractional_part = String::new();
    for d in &fractional_groups {
        fractional_part.push_str(&format!("{d:04}"));
    }
    if fractional_part.len() < dscale {
        fractional_part.push_str(&"0".repeat(dscale - fractional_part.len()));
    } else {
        fractional_part.truncate(dscale);
    }

    let mut numeric_text = String::new();
    if sign == PG_NUMERIC_NEG && (integer_part != "0" || fractional_part.chars().any(|c| c != '0'))
    {
        numeric_text.push('-');
    }
    numeric_text.push_str(&integer_part);
    if dscale > 0 {
        numeric_text.push('.');
        numeric_text.push_str(&fractional_part);
    }

    let mut d = Decimal::from_str(&numeric_text).map_err(|e| err(e.to_string()))?;
    if d.scale() > MAX_RUNTIME_NUMERIC_SCALE {
        d.rescale(MAX_RUNTIME_NUMERIC_SCALE);
    }
    Ok(Value::Numeric(d))
}

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
        t if *t == Type::NUMERIC => decode_binary_numeric(bytes, t, index),
        t if *t == Type::INTERVAL => {
            // PostgreSQL interval binary: 8 bytes µs + 4 bytes days + 4 bytes months
            if bytes.len() != 16 {
                let msg = format!("expected 16 bytes, got {}", bytes.len());
                if bytes.len() < 16 {
                    return Err(insufficient_data_param(index, pg_type.name(), msg));
                }
                return Err(err(msg));
            }
            let us = i64::from_be_bytes(
                bytes[0..8]
                    .try_into()
                    .map_err(|_| err(format!("expected 16 bytes, got {}", bytes.len())))?,
            );
            let days = i32::from_be_bytes(
                bytes[8..12]
                    .try_into()
                    .map_err(|_| err(format!("expected 16 bytes, got {}", bytes.len())))?,
            );
            let months = i32::from_be_bytes(
                bytes[12..16]
                    .try_into()
                    .map_err(|_| err(format!("expected 16 bytes, got {}", bytes.len())))?,
            );
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
        t if array_element_type(t).is_some() => decode_binary_array(bytes, t, index),
        // Type::UNKNOWN (OID 705) - pgx/GORM sends binary unknown
        t if *t == Type::UNKNOWN => {
            let nul_pos = bytes.iter().position(|b| *b == 0);
            let utf8_err = std::str::from_utf8(bytes).err();
            let utf8_pos = utf8_err.as_ref().map(|e| e.valid_up_to());

            match (nul_pos, utf8_pos) {
                (Some(n), Some(u)) if n <= u => {
                    Err(invalid_utf8_param(index, "unknown", bytes, n, Some(1)))
                }
                (Some(n), None) => Err(invalid_utf8_param(index, "unknown", bytes, n, Some(1))),
                (None, Some(u)) | (Some(_), Some(u)) => {
                    let error_len = utf8_err.as_ref().and_then(|e| e.error_len());
                    Err(invalid_utf8_param(index, "unknown", bytes, u, error_len))
                }
                (None, None) => {
                    let s = std::str::from_utf8(bytes).map_err(|e| {
                        invalid_utf8_param(index, "unknown", bytes, e.valid_up_to(), e.error_len())
                    })?;
                    Ok(Value::Text(s.to_string()))
                }
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
            if let Some(hex_str) = trimmed.strip_prefix("\\x") {
                hex::decode(hex_str)
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
            || *t == Type::VARCHAR_ARRAY
            || *t == Type::NAME_ARRAY
            || *t == Type::FLOAT8_ARRAY
            || *t == Type::BOOL_ARRAY
            || *t == Type::TIMESTAMP_ARRAY
            || *t == Type::TIMESTAMPTZ_ARRAY
            || *t == Type::DATE_ARRAY
            || *t == Type::INTERVAL_ARRAY
            || *t == Type::UUID_ARRAY
            || *t == Type::BYTEA_ARRAY
            || *t == Type::JSON_ARRAY
            || *t == Type::JSONB_ARRAY
            || *t == Type::TIME_ARRAY
            || *t == Type::NUMERIC_ARRAY =>
        {
            let elements = crate::sql::value_coercion::parse_pg_array(trimmed)
                .map_err(|_| err(format!("\"{}\"", trimmed)))?;
            // Re-decode each element through the scalar decode path for the
            // element type so that e.g. UUID strings become Value::Uuid, not
            // Value::Text. Skip for TEXT/VARCHAR/NAME arrays (already correct).
            if let Some(elem_type) = array_element_type(pg_type) {
                if elem_type == Type::TEXT || elem_type == Type::VARCHAR || elem_type == Type::NAME
                {
                    return Ok(Value::Array(elements));
                }
                let mut typed = Vec::with_capacity(elements.len());
                for v in elements {
                    if v == Value::Null {
                        typed.push(Value::Null);
                    } else {
                        let s = match &v {
                            Value::Text(s) => s.clone(),
                            other => other.to_string(),
                        };
                        typed.push(decode_text_value(&s, s.trim(), &elem_type, index)?);
                    }
                }
                Ok(Value::Array(typed))
            } else {
                Ok(Value::Array(elements))
            }
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

    #[test]
    fn decode_text_varchar_array_decodes_as_array() {
        let v = decode_text(b"{alice,bob}", &Type::VARCHAR_ARRAY, 0).unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                Value::Text("alice".to_string()),
                Value::Text("bob".to_string()),
            ])
        );
    }

    #[test]
    fn decode_text_name_array_decodes_as_array() {
        let v = decode_text(b"{public,tenant}", &Type::NAME_ARRAY, 0).unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                Value::Text("public".to_string()),
                Value::Text("tenant".to_string()),
            ])
        );
    }

    #[test]
    fn decode_binary_text_array_decodes_values() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1i32.to_be_bytes()); // ndim
        bytes.extend_from_slice(&0i32.to_be_bytes()); // hasnull
        bytes.extend_from_slice(&(Type::TEXT.oid() as i32).to_be_bytes()); // elem oid
        bytes.extend_from_slice(&2i32.to_be_bytes()); // length
        bytes.extend_from_slice(&1i32.to_be_bytes()); // lbound

        bytes.extend_from_slice(&5i32.to_be_bytes());
        bytes.extend_from_slice(b"alice");
        bytes.extend_from_slice(&3i32.to_be_bytes());
        bytes.extend_from_slice(b"bob");

        let v = decode_binary(&bytes, &Type::TEXT_ARRAY, 0).unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                Value::Text("alice".to_string()),
                Value::Text("bob".to_string()),
            ])
        );
    }

    #[test]
    fn decode_binary_text_array_allows_null_elements() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1i32.to_be_bytes()); // ndim
        bytes.extend_from_slice(&1i32.to_be_bytes()); // hasnull
        bytes.extend_from_slice(&(Type::TEXT.oid() as i32).to_be_bytes()); // elem oid
        bytes.extend_from_slice(&2i32.to_be_bytes()); // length
        bytes.extend_from_slice(&1i32.to_be_bytes()); // lbound

        bytes.extend_from_slice(&5i32.to_be_bytes());
        bytes.extend_from_slice(b"alice");
        bytes.extend_from_slice(&(-1i32).to_be_bytes()); // NULL

        let v = decode_binary(&bytes, &Type::TEXT_ARRAY, 0).unwrap();
        assert_eq!(
            v,
            Value::Array(vec![Value::Text("alice".to_string()), Value::Null])
        );
    }

    #[test]
    fn decode_binary_array_rejects_truncated_payload_with_08p01() {
        // Deliberately truncated: not enough bytes for full binary array header.
        let bytes = vec![0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00];
        let err = decode_binary(&bytes, &Type::TEXT_ARRAY, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "08P01");
                assert_eq!(info.message, INSUFFICIENT_DATA_LEFT_IN_MESSAGE);
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_numeric_rejects_truncated_payload_with_08p01() {
        // Deliberately truncated: NUMERIC binary payload requires at least 8 bytes.
        let bytes = vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00];
        let err = decode_binary(&bytes, &Type::NUMERIC, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "08P01");
                assert_eq!(info.message, INSUFFICIENT_DATA_LEFT_IN_MESSAGE);
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_numeric_rejects_truncated_digit_payload_with_08p01() {
        // Valid NUMERIC header (ndigits=2) but only one 2-byte digit is present.
        let bytes = build_numeric_bin(2, 0, PG_NUMERIC_POS, 0, &[12]);
        let err = decode_binary(&bytes, &Type::NUMERIC, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "08P01");
                assert_eq!(info.message, INSUFFICIENT_DATA_LEFT_IN_MESSAGE);
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_interval_rejects_truncated_payload_with_08p01() {
        // Deliberately truncated: INTERVAL binary payload must be exactly 16 bytes.
        let bytes = vec![0u8; 15];
        let err = decode_binary(&bytes, &Type::INTERVAL, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "08P01");
                assert_eq!(info.message, INSUFFICIENT_DATA_LEFT_IN_MESSAGE);
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    fn build_numeric_bin(
        ndigits: i16,
        weight: i16,
        sign: i16,
        dscale: i16,
        digits: &[i16],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&ndigits.to_be_bytes());
        out.extend_from_slice(&weight.to_be_bytes());
        out.extend_from_slice(&sign.to_be_bytes());
        out.extend_from_slice(&dscale.to_be_bytes());
        for d in digits {
            out.extend_from_slice(&d.to_be_bytes());
        }
        out
    }

    #[test]
    fn decode_binary_numeric_simple_decimal() {
        // 12.34 => ndigits=2, weight=0, digits=[12,3400], dscale=2
        let bytes = build_numeric_bin(2, 0, PG_NUMERIC_POS, 2, &[12, 3400]);
        let v = decode_binary(&bytes, &Type::NUMERIC, 0).unwrap();
        match v {
            Value::Numeric(d) => assert_eq!(d.to_string(), "12.34"),
            other => panic!("expected numeric, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_numeric_negative_fraction() {
        // -0.125 => ndigits=2, weight=-1, digits=[1250,0], dscale=3
        let bytes = build_numeric_bin(2, -1, PG_NUMERIC_NEG, 3, &[1250, 0]);
        let v = decode_binary(&bytes, &Type::NUMERIC, 0).unwrap();
        match v {
            Value::Numeric(d) => assert_eq!(d.to_string(), "-0.125"),
            other => panic!("expected numeric, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_unknown_utf8_digits_decodes_as_text() {
        let v = decode_binary(b"16", &Type::UNKNOWN, 0).unwrap();
        assert_eq!(v, Value::Text("16".to_string()));
    }

    #[test]
    fn decode_binary_unknown_utf8_text_decodes_as_text() {
        let v = decode_binary(b"hello", &Type::UNKNOWN, 0).unwrap();
        assert_eq!(v, Value::Text("hello".to_string()));
    }

    #[test]
    fn decode_binary_unknown_invalid_utf8_reports_three_bytes_for_e228a1() {
        let err = decode_binary(b"\xE2\x28\xA1", &Type::UNKNOWN, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22021");
                assert_eq!(
                    info.message,
                    "invalid input syntax for parameter $1 (unknown): invalid byte sequence for encoding \"UTF8\": 0xe2 0x28 0xa1"
                );
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_unknown_invalid_utf8_reports_two_bytes_for_c27f() {
        let err = decode_binary(b"\xC2\x7F", &Type::UNKNOWN, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22021");
                assert_eq!(
                    info.message,
                    "invalid input syntax for parameter $1 (unknown): invalid byte sequence for encoding \"UTF8\": 0xc2 0x7f"
                );
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_unknown_invalid_utf8_reports_single_byte_for_fffe() {
        let err = decode_binary(b"\xFF\xFE", &Type::UNKNOWN, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22021");
                assert_eq!(
                    info.message,
                    "invalid input syntax for parameter $1 (unknown): invalid byte sequence for encoding \"UTF8\": 0xff"
                );
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_unknown_invalid_utf8_reports_all_remaining_for_f09f() {
        let err = decode_binary(b"\xF0\x9F", &Type::UNKNOWN, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22021");
                assert_eq!(
                    info.message,
                    "invalid input syntax for parameter $1 (unknown): invalid byte sequence for encoding \"UTF8\": 0xf0 0x9f"
                );
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_unknown_invalid_utf8_reports_single_error_byte_after_valid_prefix() {
        let err = decode_binary(b"a\xFF\xFEb", &Type::UNKNOWN, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22021");
                assert_eq!(
                    info.message,
                    "invalid input syntax for parameter $1 (unknown): invalid byte sequence for encoding \"UTF8\": 0xff"
                );
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_unknown_reports_nul_error_before_utf8_error() {
        let err = decode_binary(b"\x00\x85", &Type::UNKNOWN, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22021");
                assert_eq!(
                    info.message,
                    "invalid input syntax for parameter $1 (unknown): invalid byte sequence for encoding \"UTF8\": 0x00"
                );
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_unknown_reports_utf8_error_before_nul_error() {
        let err = decode_binary(b"\xFF\x00", &Type::UNKNOWN, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22021");
                assert_eq!(
                    info.message,
                    "invalid input syntax for parameter $1 (unknown): invalid byte sequence for encoding \"UTF8\": 0xff"
                );
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_unknown_rejects_single_nul_byte() {
        let err = decode_binary(b"\x00", &Type::UNKNOWN, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22021");
                assert_eq!(
                    info.message,
                    "invalid input syntax for parameter $1 (unknown): invalid byte sequence for encoding \"UTF8\": 0x00"
                );
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary_unknown_rejects_nul_bytes() {
        let err = decode_binary(b"hi\x00there", &Type::UNKNOWN, 0).unwrap_err();
        match err {
            PgWireError::UserError(info) => {
                assert_eq!(info.code, "22021");
                assert_eq!(
                    info.message,
                    "invalid input syntax for parameter $1 (unknown): invalid byte sequence for encoding \"UTF8\": 0x00"
                );
            }
            other => panic!("expected user error, got {other:?}"),
        }
    }
}
