use crate::types::{DataType, Value};
use pgwire::api::results::{DataRowEncoder, FieldFormat};
use pgwire::api::Type;
use pgwire::error::{PgWireError, PgWireResult};

pub(in crate::protocol::handler) fn encode_value(
    encoder: &mut DataRowEncoder,
    value: &Value,
    col_type: Option<&DataType>,
    tz: crate::types::timestamp::TimeZoneSpec,
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
                // Treat as timestamp - reuse the timestamp encoding logic
                use chrono::{DateTime, Utc};
                const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;
                const MAX_REASONABLE_UNIX_MS: i64 = 10_000_000_000_000;

                let (seconds, micros) = if *i > MAX_REASONABLE_UNIX_MS {
                    let pg_micros = i;
                    let unix_secs = pg_micros.div_euclid(1_000_000) + PG_EPOCH_UNIX_SECS;
                    let micros = pg_micros.rem_euclid(1_000_000) as u32;
                    (unix_secs, micros)
                } else {
                    let secs = i.div_euclid(1000);
                    let millis = i.rem_euclid(1000) as u32;
                    (secs, millis * 1000)
                };

                let nanos = micros * 1000;
                if let Some(dt) = DateTime::<Utc>::from_timestamp(seconds, nanos) {
                    let is_timestamptz = matches!(col_type, Some(DataType::TimestampTz));
                    if is_timestamptz {
                        encoder.encode_field(&tz.format_timestamptz(dt, micros))
                    } else if micros == 0 {
                        encoder.encode_field(&dt.format("%Y-%m-%d %H:%M:%S").to_string())
                    } else {
                        encoder.encode_field(&dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
                    }
                } else {
                    encoder.encode_field(&"1970-01-01 00:00:00".to_string())
                }
            } else {
                encoder.encode_field(i)
            }
        }
        Value::Float64(f) => encoder.encode_field(f),
        Value::Text(s) => encoder.encode_field(s),
        Value::Bytes(b) => encoder.encode_field(&format!("\\x{}", hex::encode(b))),
        Value::Timestamp(ts) => {
            use chrono::{DateTime, Utc};

            // Detect timestamp format:
            // - Unix epoch milliseconds: typical values 1.0e12 to 2.5e12 (years 2001-2049)
            // - PostgreSQL epoch microseconds: typical values 0 to 1.6e15 (years 2000-2050)
            // If value is > 1e13 (year 2286 in Unix ms), assume it's PG epoch microseconds.
            // PostgreSQL epoch is 2000-01-01 00:00:00 UTC = 946684800 seconds since Unix epoch.
            const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;
            const MAX_REASONABLE_UNIX_MS: i64 = 10_000_000_000_000; // year ~2286

            // Only apply the legacy PG-epoch-micros heuristic for large *positive* values.
            // Unix-epoch millis can be large in magnitude for pre-epoch timestamps (e.g. year 0001).
            let (seconds, micros) = if *ts > MAX_REASONABLE_UNIX_MS {
                // Likely PostgreSQL epoch microseconds - convert to Unix seconds
                let pg_micros = ts;
                let unix_secs = pg_micros.div_euclid(1_000_000) + PG_EPOCH_UNIX_SECS;
                let micros = pg_micros.rem_euclid(1_000_000) as u32;
                (unix_secs, micros)
            } else {
                // Unix epoch milliseconds (our standard format)
                let secs = ts.div_euclid(1000);
                let millis = ts.rem_euclid(1000) as u32;
                (secs, millis * 1000)
            };

            let nanos = micros * 1000;
            if let Some(dt) = DateTime::<Utc>::from_timestamp(seconds, nanos) {
                let is_timestamptz = matches!(col_type, Some(DataType::TimestampTz));

                if is_timestamptz {
                    encoder.encode_field(&tz.format_timestamptz(dt, micros))
                } else if micros == 0 {
                    encoder.encode_field(&dt.format("%Y-%m-%d %H:%M:%S").to_string())
                } else {
                    encoder.encode_field(&dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
                }
            } else {
                // Fallback: encode as ISO string if all else fails
                encoder.encode_field(&format!("1970-01-01 00:00:00"))
            }
        }
        Value::Interval(iv) => encoder.encode_field(&iv.to_string()),
        Value::Uuid(bytes) => {
            let uuid = uuid::Uuid::from_bytes(*bytes);
            encoder.encode_field(&uuid.to_string())
        }
        Value::Array(elems) => {
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
                    Value::Float64(f) => Some(f.to_string()),
                    Value::Array(nested) => {
                        // Nested arrays: build PostgreSQL text literal {el1,el2,...}
                        // with NULL preserved for null elements.
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
        Value::Jsonb(s) => {
            fn write_jsonb_pg(out: &mut String, val: &serde_json::Value) {
                match val {
                    serde_json::Value::Null => out.push_str("null"),
                    serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                    serde_json::Value::Number(n) => out.push_str(&n.to_string()),
                    serde_json::Value::String(s) => {
                        // Delegate escaping to serde_json.
                        if let Ok(escaped) = serde_json::to_string(s) {
                            out.push_str(&escaped);
                        } else {
                            out.push_str("\"\"");
                        }
                    }
                    serde_json::Value::Array(arr) => {
                        out.push('[');
                        for (idx, item) in arr.iter().enumerate() {
                            if idx > 0 {
                                out.push_str(", ");
                            }
                            write_jsonb_pg(out, item);
                        }
                        out.push(']');
                    }
                    serde_json::Value::Object(obj) => {
                        use std::cmp::Ordering;
                        let mut items: Vec<(&String, &serde_json::Value)> = obj.iter().collect();
                        // PostgreSQL jsonb key ordering: length first, then binary (byte) order.
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

            match serde_json::from_str::<serde_json::Value>(s) {
                Ok(val) => {
                    let mut formatted = String::new();
                    write_jsonb_pg(&mut formatted, &val);
                    encoder.encode_field(&formatted)
                }
                Err(_) => encoder.encode_field(s),
            }
        }
        Value::Vector(vec) => encoder.encode_field(&crate::types::format_vector_pg_text(vec)),
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
                crate::types::date::format_date_days(*days).unwrap_or_else(|_| days.to_string());
            encoder.encode_field(&s)
        }
        Value::Numeric(d) => encoder.encode_field(&d.to_string()),
        Value::Tsvector(s) | Value::Tsquery(s) => encoder.encode_field(s),
    }
}
