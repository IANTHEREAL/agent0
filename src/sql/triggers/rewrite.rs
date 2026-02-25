use crate::model::{Row, TableSchema, Value};

use crate::sql::quoting;

pub(crate) fn substitute_row_references(
    expr: &str,
    schema: &TableSchema,
    new_values: &[Value],
    old_row: Option<&Row>,
) -> String {
    let bytes = expr.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;

    while i < bytes.len() {
        // Line comment.
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            let start = i;
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.extend_from_slice(&bytes[start..i]);
            continue;
        }

        // Block comment (supports nesting).
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            let mut depth = 1usize;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                    continue;
                }
                if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    depth = depth.saturating_sub(1);
                    i += 2;
                    continue;
                }
                i += 1;
            }
            out.extend_from_slice(&bytes[start..i]);
            continue;
        }

        // Single-quoted string literal.
        if bytes[i] == b'\'' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    // Escaped quote: ''.
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.extend_from_slice(&bytes[start..i]);
            continue;
        }

        // Double-quoted identifier.
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.extend_from_slice(&bytes[start..i]);
            continue;
        }

        // Dollar-quoted strings ($tag$...$tag$ or $$...$$).
        if bytes[i] == b'$' {
            // Skip parameter placeholders like $1.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                out.extend_from_slice(&bytes[i..j]);
                i = j;
                continue;
            }

            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'$' {
                if bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' {
                    j += 1;
                    continue;
                }
                break;
            }
            if j < bytes.len() && bytes[j] == b'$' {
                let start = i;
                let delim = &bytes[i..=j];
                let delim_len = delim.len();
                i = j + 1;
                while i + delim_len <= bytes.len() {
                    if &bytes[i..i + delim_len] == delim {
                        i += delim_len;
                        break;
                    }
                    i += 1;
                }
                out.extend_from_slice(&bytes[start..i]);
                continue;
            }

            out.push(bytes[i]);
            i += 1;
            continue;
        }

        // Identifier token.
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }

            let ident = &bytes[start..i];
            let is_new = ident.eq_ignore_ascii_case(b"NEW");
            let is_old = ident.eq_ignore_ascii_case(b"OLD");
            if (is_new || is_old) && i < bytes.len() && bytes[i] == b'.' {
                let col_start = i + 1;
                if col_start < bytes.len() && is_ident_start(bytes[col_start]) {
                    let mut col_end = col_start + 1;
                    while col_end < bytes.len() && is_ident_continue(bytes[col_end]) {
                        col_end += 1;
                    }

                    if let Ok(col_name) = std::str::from_utf8(&bytes[col_start..col_end]) {
                        if let Some(idx) = schema
                            .columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(col_name))
                        {
                            if is_new {
                                let value = new_values
                                    .get(idx)
                                    .map(value_to_sql_literal)
                                    .unwrap_or_else(|| "NULL".to_string());
                                out.extend_from_slice(value.as_bytes());
                                i = col_end;
                                continue;
                            }

                            if let Some(old) = old_row {
                                let value = old
                                    .values
                                    .get(idx)
                                    .map(value_to_sql_literal)
                                    .unwrap_or_else(|| "NULL".to_string());
                                out.extend_from_slice(value.as_bytes());
                                i = col_end;
                                continue;
                            }
                        }
                    }
                }
            }

            out.extend_from_slice(ident);
            continue;
        }

        out.push(bytes[i]);
        i += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| expr.to_string())
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

pub(crate) fn value_to_sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Int32(n) => n.to_string(),
        Value::Int64(n) => n.to_string(),
        Value::Float64(f) => f.to_string(),
        Value::Text(s) => quoting::quote_literal(s),
        Value::Timestamp(ts) => {
            let secs = ts / 1000;
            let millis = ts % 1000;
            let datetime = chrono::DateTime::from_timestamp(secs, (millis * 1_000_000) as u32)
                .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
            format!("'{}'", datetime.format("%Y-%m-%d %H:%M:%S%.3f"))
        }
        Value::Date(days) => match crate::model::date::format_date_days(*days) {
            Ok(s) => format!("'{}'", s),
            Err(_) => format!("'{}'", days),
        },
        Value::Uuid(bytes) => {
            let u = uuid::Uuid::from_bytes(*bytes);
            format!("'{}'", u)
        }
        Value::Bytes(b) => format!("'\\x{}'", hex::encode(b)),
        Value::Json(s) | Value::Jsonb(s) => quoting::quote_literal(s),
        Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(value_to_sql_literal).collect();
            format!("ARRAY[{}]", items.join(","))
        }
        Value::Vector(v) => {
            let items: Vec<String> = v.iter().map(|f| f.to_string()).collect();
            format!("[{}]", items.join(","))
        }
        Value::Interval(iv) => format!("INTERVAL '{}'", iv),
        Value::Time(micros) => {
            let total_secs = micros / 1_000_000;
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            format!("'{:02}:{:02}:{:02}'", hours, mins, secs)
        }
        Value::Numeric(d) => d.to_string(),
        Value::Tsvector(s) | Value::Tsquery(s) => quoting::quote_literal(s),
    }
}
