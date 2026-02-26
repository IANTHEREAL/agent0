use crate::model::{DataType, Value};
use pgwire::api::results::{DataRowEncoder, FieldFormat};
use pgwire::api::Type;
use pgwire::error::{PgWireError, PgWireResult};

fn format_float8_pg_text(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }

    let abs = v.abs();
    if abs != 0.0 && (!(1e-6..1e15).contains(&abs)) {
        // PostgreSQL-style scientific notation for very small/large magnitudes.
        return format!("{:e}", v);
    }

    v.to_string()
}

pub(in crate::protocol::handler) fn encode_value(
    encoder: &mut DataRowEncoder,
    value: &Value,
    col_type: Option<&DataType>,
    tz: crate::model::timestamp::TimeZoneSpec,
    format: FieldFormat,
) -> PgWireResult<()> {
    if format == FieldFormat::Binary {
        return encode_value_binary(encoder, value, col_type, tz);
    }
    encode_value_text(encoder, value, col_type, tz)
}

/// Text-format encoding (original behavior).
fn encode_value_text(
    encoder: &mut DataRowEncoder,
    value: &Value,
    col_type: Option<&DataType>,
    tz: crate::model::timestamp::TimeZoneSpec,
) -> PgWireResult<()> {
    match value {
        Value::Null => encoder.encode_field(&None::<String>),
        Value::Boolean(b) => encoder.encode_field(b),
        Value::Int32(i) => encoder.encode_field(i),
        Value::Int64(i) => {
            // Check if this Int64 should be interpreted as a timestamp based on column type
            // This handles the case where timestamps were incorrectly stored as Int64
            if matches!(
                col_type,
                Some(DataType::Timestamp) | Some(DataType::TimestampTz)
            ) {
                encode_timestamp_text(encoder, *i, col_type, tz)
            } else {
                encoder.encode_field(i)
            }
        }
        Value::Float64(f) => encoder.encode_field(&format_float8_pg_text(*f)),
        Value::Text(s) => encoder.encode_field(s),
        Value::Bytes(b) => encoder.encode_field(&format!("\\x{}", hex::encode(b))),
        Value::Timestamp(ts) => encode_timestamp_text(encoder, *ts, col_type, tz),
        Value::Interval(iv) => encoder.encode_field(&iv.to_string()),
        Value::Uuid(bytes) => {
            let uuid = uuid::Uuid::from_bytes(*bytes);
            encoder.encode_field(&uuid.to_string())
        }
        Value::Array(elems) => {
            if matches!(col_type, Some(DataType::UserDefined(s)) if s.eq_ignore_ascii_case("record"))
            {
                fn format_record_field(v: &Value) -> String {
                    match v {
                        Value::Null => String::new(),
                        Value::Text(s) => {
                            let needs_quotes = s.is_empty()
                                || s.eq_ignore_ascii_case("null")
                                || s.chars().any(|ch| {
                                    matches!(ch, ',' | '(' | ')' | '"' | '\\') || ch.is_whitespace()
                                });
                            if needs_quotes {
                                let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
                                format!("\"{}\"", escaped)
                            } else {
                                s.clone()
                            }
                        }
                        Value::Array(nested) => {
                            let parts: Vec<String> = nested
                                .iter()
                                .map(|e| match e {
                                    Value::Null => "NULL".to_string(),
                                    Value::Text(t) => t.clone(),
                                    other => other.to_string(),
                                })
                                .collect();
                            format!("{{{}}}", parts.join(","))
                        }
                        other => other.to_string(),
                    }
                }

                let fields: Vec<String> = elems.iter().map(format_record_field).collect();
                let composite = format!("({})", fields.join(","));
                return encoder.encode_field(&composite);
            }
            // int2vector: encode as space-separated text "1 2 3"
            if matches!(col_type, Some(DataType::UserDefined(s)) if s == "int2vector") {
                let mut parts = Vec::with_capacity(elems.len());
                for v in elems {
                    match v {
                        Value::Int64(i) => parts.push(i.to_string()),
                        Value::Int32(i) => parts.push(i.to_string()),
                        other => {
                            return Err(PgWireError::ApiError(
                                format!("int2vector element must be integer, got {:?}", other)
                                    .into(),
                            ))
                        }
                    }
                }
                let text = parts.join(" ");
                return encoder.encode_field_with_type_and_format(
                    &text,
                    &Type::TEXT,
                    FieldFormat::Text,
                );
            }
            fn value_to_option_string(v: &Value) -> Option<String> {
                match v {
                    Value::Null => None,
                    Value::Text(t) => Some(t.clone()),
                    Value::Boolean(b) => Some(if *b { "t" } else { "f" }.to_string()),
                    Value::Int32(i) => Some(i.to_string()),
                    Value::Int64(i) => Some(i.to_string()),
                    Value::Float64(f) => Some(format_float8_pg_text(*f)),
                    Value::Array(nested) => {
                        let parts: Vec<String> = nested
                            .iter()
                            .map(|v| match value_to_option_string(v) {
                                Some(s) => s,
                                None => "NULL".to_string(),
                            })
                            .collect();
                        Some(format!("{{{}}}", parts.join(",")))
                    }
                    other => Some(other.to_string()),
                }
            }
            let option_elems: Vec<Option<String>> =
                elems.iter().map(value_to_option_string).collect();
            encoder.encode_field(&option_elems)
        }
        Value::Json(s) => encoder.encode_field(s),
        Value::Jsonb(s) => encoder.encode_field(&crate::sql::jsonb::format_jsonb_pg_str(s)),
        Value::Vector(vec) => encoder.encode_field(&crate::model::format_vector_pg_text(vec)),
        Value::Time(micros) => {
            let total_secs = micros / 1_000_000;
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            let frac = micros % 1_000_000;
            if frac > 0 {
                encoder.encode_field(&format!("{:02}:{:02}:{:02}.{:06}", hours, mins, secs, frac))
            } else {
                encoder.encode_field(&format!("{:02}:{:02}:{:02}", hours, mins, secs))
            }
        }
        Value::Date(days) => {
            let s =
                crate::model::date::format_date_days(*days).unwrap_or_else(|_| days.to_string());
            encoder.encode_field(&s)
        }
        Value::Numeric(d) => encoder.encode_field(&d.to_string()),
        Value::Tsvector(s) | Value::Tsquery(s) => encoder.encode_field(s),
    }
}

/// Binary-format encoding for the PostgreSQL wire protocol.
/// Uses `encode_field_with_type_and_format` with explicit types to ensure
/// correct binary serialization via postgres-types `ToSql` trait.
fn encode_value_binary(
    encoder: &mut DataRowEncoder,
    value: &Value,
    col_type: Option<&DataType>,
    _tz: crate::model::timestamp::TimeZoneSpec,
) -> PgWireResult<()> {
    match value {
        Value::Null => encoder.encode_field_with_type_and_format(
            &None::<i32>,
            &Type::INT4,
            FieldFormat::Binary,
        ),
        Value::Boolean(b) => {
            encoder.encode_field_with_type_and_format(b, &Type::BOOL, FieldFormat::Binary)
        }
        Value::Int32(i) => {
            encoder.encode_field_with_type_and_format(i, &Type::INT4, FieldFormat::Binary)
        }
        Value::Int64(i) => {
            if matches!(
                col_type,
                Some(DataType::Timestamp) | Some(DataType::TimestampTz)
            ) {
                let dt = int64_to_datetime(*i);
                let is_timestamptz = matches!(col_type, Some(DataType::TimestampTz));
                if is_timestamptz {
                    encoder.encode_field_with_type_and_format(
                        &dt,
                        &Type::TIMESTAMPTZ,
                        FieldFormat::Binary,
                    )
                } else {
                    encoder.encode_field_with_type_and_format(
                        &dt.naive_utc(),
                        &Type::TIMESTAMP,
                        FieldFormat::Binary,
                    )
                }
            } else if matches!(col_type, Some(DataType::Int32)) {
                encoder.encode_field_with_type_and_format(
                    &(*i as i32),
                    &Type::INT4,
                    FieldFormat::Binary,
                )
            } else {
                encoder.encode_field_with_type_and_format(i, &Type::INT8, FieldFormat::Binary)
            }
        }
        Value::Float64(f) => {
            encoder.encode_field_with_type_and_format(f, &Type::FLOAT8, FieldFormat::Binary)
        }
        Value::Text(s) => {
            encoder.encode_field_with_type_and_format(s, &Type::TEXT, FieldFormat::Binary)
        }
        Value::Bytes(b) => {
            encoder.encode_field_with_type_and_format(b, &Type::BYTEA, FieldFormat::Binary)
        }
        Value::Timestamp(ts) => {
            let dt = int64_to_datetime(*ts);
            let is_timestamptz = matches!(col_type, Some(DataType::TimestampTz));
            if is_timestamptz {
                encoder.encode_field_with_type_and_format(
                    &dt,
                    &Type::TIMESTAMPTZ,
                    FieldFormat::Binary,
                )
            } else {
                encoder.encode_field_with_type_and_format(
                    &dt.naive_utc(),
                    &Type::TIMESTAMP,
                    FieldFormat::Binary,
                )
            }
        }
        Value::Uuid(bytes) => {
            // UUID binary = 16 raw bytes; encode manually since uuid::Uuid
            // doesn't implement postgres_types::ToSql without the feature.
            encoder.encode_field_with_type_and_format(
                &bytes.as_slice(),
                &Type::UUID,
                FieldFormat::Binary,
            )
        }
        Value::Json(s) => {
            // JSON binary in PostgreSQL = raw JSON text bytes (same as text)
            encoder.encode_field_with_type_and_format(s, &Type::JSON, FieldFormat::Binary)
        }
        Value::Jsonb(s) => {
            // JSONB binary = version byte (0x01) + canonical JSON text bytes
            use bytes::BufMut;
            let canonical = crate::sql::jsonb::format_jsonb_pg_str(s);
            let json_bytes = canonical.as_bytes();
            let mut buf = Vec::with_capacity(1 + json_bytes.len());
            buf.put_u8(1); // JSONB version byte
            buf.extend_from_slice(json_bytes);
            encoder.encode_field_with_type_and_format(
                &buf.as_slice(),
                &Type::BYTEA,
                FieldFormat::Binary,
            )
        }
        Value::Date(days) => {
            let date = crate::model::date::date_days_to_naive_date(*days)
                .map_err(|e| PgWireError::ApiError(e.into()))?;
            encoder.encode_field_with_type_and_format(&date, &Type::DATE, FieldFormat::Binary)
        }
        Value::Time(micros) => {
            use chrono::NaiveTime;
            let total_secs = (micros / 1_000_000) as u32;
            let frac_micros = (micros % 1_000_000) as u32;
            let h = total_secs / 3600;
            let m = (total_secs % 3600) / 60;
            let s = total_secs % 60;
            if let Some(time) = NaiveTime::from_hms_micro_opt(h, m, s, frac_micros) {
                encoder.encode_field_with_type_and_format(&time, &Type::TIME, FieldFormat::Binary)
            } else {
                encoder.encode_field_with_type_and_format(
                    &"00:00:00",
                    &Type::TEXT,
                    FieldFormat::Text,
                )
            }
        }
        Value::Interval(iv) => {
            // PostgreSQL binary interval: 8 bytes (microseconds) + 4 bytes (days) + 4 bytes (months)
            use bytes::BufMut;
            const MILLIS_PER_DAY: i64 = 86_400_000;
            let days_i64 = iv.millis / MILLIS_PER_DAY;
            let days = i32::try_from(days_i64)
                .map_err(|_| PgWireError::ApiError("interval day field out of range".into()))?;
            let remainder_millis = iv.millis % MILLIS_PER_DAY;
            let microseconds = remainder_millis * 1000;
            let mut buf = Vec::with_capacity(16);
            buf.put_i64(microseconds);
            buf.put_i32(days);
            buf.put_i32(iv.months);
            encoder.encode_field_with_type_and_format(
                &buf.as_slice(),
                &Type::BYTEA,
                FieldFormat::Binary,
            )
        }
        Value::Array(elems) => {
            // Arrays: encode as text representation (PostgreSQL array text format)
            // then send as TEXT to avoid complex binary array encoding.
            fn value_to_option_string(v: &Value) -> Option<String> {
                match v {
                    Value::Null => None,
                    Value::Text(t) => Some(t.clone()),
                    Value::Boolean(b) => Some(if *b { "t" } else { "f" }.to_string()),
                    Value::Int32(i) => Some(i.to_string()),
                    Value::Int64(i) => Some(i.to_string()),
                    Value::Float64(f) => Some(format_float8_pg_text(*f)),
                    Value::Array(nested) => {
                        let parts: Vec<String> = nested
                            .iter()
                            .map(|v| match value_to_option_string(v) {
                                Some(s) => s,
                                None => "NULL".to_string(),
                            })
                            .collect();
                        Some(format!("{{{}}}", parts.join(",")))
                    }
                    other => Some(other.to_string()),
                }
            }
            let option_elems: Vec<Option<String>> =
                elems.iter().map(value_to_option_string).collect();
            encoder.encode_field_with_type_and_format(
                &option_elems,
                &Type::TEXT_ARRAY,
                FieldFormat::Text,
            )
        }
        Value::Numeric(d) => {
            // rust_decimal::Decimal implements ToSql for Type::NUMERIC via db-postgres feature
            encoder.encode_field_with_type_and_format(d, &Type::NUMERIC, FieldFormat::Binary)
        }
        Value::Vector(vec) => {
            // pgvector currently maps to TEXT OID in db9; emit binary text bytes.
            encoder.encode_field_with_type_and_format(
                &crate::model::format_vector_pg_text(vec),
                &Type::TEXT,
                FieldFormat::Binary,
            )
        }
        Value::Tsvector(s) | Value::Tsquery(s) => {
            // Full-text types: encode as text (binary tsvector/tsquery is complex)
            encoder.encode_field_with_type_and_format(s, &Type::TEXT, FieldFormat::Text)
        }
    }
}

fn encode_timestamp_text(
    encoder: &mut DataRowEncoder,
    ts: i64,
    col_type: Option<&DataType>,
    tz: crate::model::timestamp::TimeZoneSpec,
) -> PgWireResult<()> {
    let dt = int64_to_datetime(ts);
    let micros = dt.timestamp_subsec_micros();
    if matches!(col_type, Some(DataType::TimestampTz)) {
        encoder.encode_field(&tz.format_timestamptz(dt, micros))
    } else if micros == 0 {
        encoder.encode_field(&dt.format("%Y-%m-%d %H:%M:%S").to_string())
    } else {
        encoder.encode_field(&dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
    }
}

/// Convert an internal timestamp value (Unix ms or PG epoch micros) to DateTime<Utc>.
fn int64_to_datetime(ts: i64) -> chrono::DateTime<chrono::Utc> {
    use chrono::{DateTime, Utc};
    const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;
    const MAX_REASONABLE_UNIX_MS: i64 = 10_000_000_000_000;

    let (seconds, micros) = if ts > MAX_REASONABLE_UNIX_MS {
        let unix_secs = ts.div_euclid(1_000_000) + PG_EPOCH_UNIX_SECS;
        let micros = ts.rem_euclid(1_000_000) as u32;
        (unix_secs, micros)
    } else {
        let secs = ts.div_euclid(1000);
        let millis = ts.rem_euclid(1000) as u32;
        (secs, millis * 1000)
    };

    let nanos = micros * 1000;
    DateTime::<Utc>::from_timestamp(seconds, nanos)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap())
}
