use super::scan::is_ident_char_or_dollar;
use pgwire::api::portal::Portal;
use pgwire::api::Type;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};

pub(in crate::protocol::handler) fn substitute_placeholders_outside_strings_and_dollar(
    query: &str,
    values: &[String],
) -> String {
    let bytes = query.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(query.len());
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<u8>> = None;
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(ref delim) = dollar_delim {
            let delim_len = delim.len();
            if i + delim_len <= bytes.len() && &bytes[i..i + delim_len] == delim.as_slice() {
                out.extend_from_slice(delim);
                i += delim_len;
                dollar_delim = None;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
            continue;
        }

        let b = bytes[i];

        if b == b'\'' && !in_double_quote {
            if in_single_quote && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                out.push(b'\'');
                out.push(b'\'');
                i += 2;
                continue;
            }
            in_single_quote = !in_single_quote;
            out.push(b);
            i += 1;
            continue;
        }

        if b == b'"' && !in_single_quote {
            if in_double_quote && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                out.push(b'"');
                out.push(b'"');
                i += 2;
                continue;
            }
            in_double_quote = !in_double_quote;
            out.push(b);
            i += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote {
            // SQL comments
            if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                out.push(b'-');
                out.push(b'-');
                i += 2;
                while i < bytes.len() {
                    out.push(bytes[i]);
                    let is_newline = bytes[i] == b'\n';
                    i += 1;
                    if is_newline {
                        break;
                    }
                }
                continue;
            }
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                out.push(b'/');
                out.push(b'*');
                i += 2;
                let mut depth = 1usize;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                        out.push(b'/');
                        out.push(b'*');
                        depth += 1;
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        out.push(b'*');
                        out.push(b'/');
                        depth -= 1;
                        i += 2;
                        continue;
                    }
                    out.push(bytes[i]);
                    i += 1;
                }
                continue;
            }
        }

        if !in_single_quote && !in_double_quote && b == b'$' {
            // Prepared-statement placeholder: $1, $2, ...
            let mut j = i + 1;
            let mut saw_digit = false;
            let mut num = 0usize;
            while j < bytes.len() && bytes[j].is_ascii_digit() && j - i <= 10 {
                saw_digit = true;
                num = num
                    .saturating_mul(10)
                    .saturating_add((bytes[j] - b'0') as usize);
                j += 1;
            }
            if saw_digit {
                let before_ok = i == 0 || !is_ident_char_or_dollar(bytes[i - 1]);
                let after_ok = j == bytes.len() || !is_ident_char_or_dollar(bytes[j]);
                if before_ok && after_ok {
                    if num >= 1 && num <= values.len() {
                        out.extend_from_slice(values[num - 1].as_bytes());
                    } else {
                        out.extend_from_slice(&bytes[i..j]);
                    }
                    i = j;
                    continue;
                }
            }

            // PostgreSQL dollar-quoted strings ($tag$ ... $tag$ or $$ ... $$)
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'$' {
                if !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'$' {
                let delim = bytes[i..=j].to_vec();
                out.extend_from_slice(&delim);
                dollar_delim = Some(delim);
                i = j + 1;
                continue;
            }
        }

        out.push(b);
        i += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| query.to_string())
}

pub(in crate::protocol::handler) fn dummy_sql_expr_for_param_type(param_type: &Type) -> String {
    match param_type {
        t if *t == Type::BOOL => "NULL::bool".to_string(),
        t if *t == Type::INT2 => "NULL::int2".to_string(),
        t if *t == Type::INT4 => "NULL::int4".to_string(),
        t if *t == Type::INT8 => "NULL::int8".to_string(),
        t if *t == Type::FLOAT4 => "NULL::float4".to_string(),
        t if *t == Type::FLOAT8 => "NULL::float8".to_string(),
        t if *t == Type::TEXT || *t == Type::VARCHAR => "NULL::text".to_string(),
        t if *t == Type::TIMESTAMP => "NULL::timestamp".to_string(),
        t if *t == Type::TIMESTAMPTZ => "NULL::timestamptz".to_string(),
        t if *t == Type::UUID => "NULL::uuid".to_string(),
        t if *t == Type::DATE => "NULL::date".to_string(),
        t if *t == Type::BYTEA => "NULL::bytea".to_string(),
        t if *t == Type::JSON => "NULL::json".to_string(),
        t if *t == Type::JSONB => "NULL::jsonb".to_string(),
        t if *t == Type::NUMERIC => "NULL::numeric".to_string(),
        _ => "NULL".to_string(),
    }
}

pub(in crate::protocol::handler) fn substitute_parameters(
    query: &str,
    portal: &Portal<String>,
) -> PgWireResult<String> {
    let mut values: Vec<String> = Vec::with_capacity(portal.parameter_len());

    fn quote_sql_string_literal(value: &str) -> String {
        format!("'{}'", value.replace("'", "''"))
    }

    for i in 0..portal.parameter_len() {
        let param_type = portal
            .statement
            .parameter_types
            .get(i)
            .cloned()
            .unwrap_or(Type::UNKNOWN);

        let param = portal
            .parameters
            .get(i)
            .ok_or_else(|| PgWireError::ParameterIndexOutOfBound(i))?;

        let Some(param_bytes) = param.as_ref() else {
            values.push("NULL".to_string());
            continue;
        };

        let invalid_param = |message: String| -> PgWireError {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "22P02".to_string(),
                format!(
                    "invalid input syntax for parameter ${} ({}): {}",
                    i + 1,
                    param_type.name(),
                    message
                ),
            )))
        };

        let value_str = if portal.parameter_format.is_binary(i) {
            match &param_type {
                t if *t == Type::BOOL => match portal.parameter::<bool>(i, &param_type)? {
                    Some(v) => {
                        if v {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        }
                    }
                    None => "NULL".to_string(),
                },
                t if *t == Type::INT2 => match portal.parameter::<i16>(i, &param_type)? {
                    Some(v) => v.to_string(),
                    None => "NULL".to_string(),
                },
                t if *t == Type::INT4 => match portal.parameter::<i32>(i, &param_type)? {
                    Some(v) => v.to_string(),
                    None => "NULL".to_string(),
                },
                t if *t == Type::INT8 => match portal.parameter::<i64>(i, &param_type)? {
                    Some(v) => v.to_string(),
                    None => "NULL".to_string(),
                },
                t if *t == Type::FLOAT4 => match portal.parameter::<f32>(i, &param_type)? {
                    Some(v) => v.to_string(),
                    None => "NULL".to_string(),
                },
                t if *t == Type::FLOAT8 => match portal.parameter::<f64>(i, &param_type)? {
                    Some(v) => v.to_string(),
                    None => "NULL".to_string(),
                },
                t if *t == Type::TIMESTAMPTZ => {
                    use chrono::{DateTime, Utc};
                    match portal.parameter::<DateTime<Utc>>(i, &param_type)? {
                        Some(ts) => format!("'{}'", ts.format("%Y-%m-%d %H:%M:%S%.6f%:z")),
                        None => "NULL".to_string(),
                    }
                }
                t if *t == Type::TIMESTAMP => {
                    use chrono::NaiveDateTime;
                    match portal.parameter::<NaiveDateTime>(i, &param_type)? {
                        Some(ts) => format!("'{}'", ts.format("%Y-%m-%d %H:%M:%S%.6f")),
                        None => "NULL".to_string(),
                    }
                }
                t if *t == Type::UUID => {
                    let uuid = uuid::Uuid::from_slice(param_bytes.as_ref())
                        .map_err(|e| invalid_param(e.to_string()))?;
                    format!("{}::uuid", quote_sql_string_literal(&uuid.to_string()))
                }
                t if *t == Type::BYTEA => {
                    let hex = hex::encode(param_bytes.as_ref());
                    let repr = format!("\\x{}", hex);
                    format!("{}::bytea", quote_sql_string_literal(&repr))
                }
                t if *t == Type::TEXT => {
                    let s = std::str::from_utf8(param_bytes.as_ref())
                        .map_err(|e| invalid_param(e.to_string()))?;
                    quote_sql_string_literal(s)
                }
                t if *t == Type::JSON => {
                    let s = std::str::from_utf8(param_bytes.as_ref())
                        .map_err(|e| invalid_param(e.to_string()))?;
                    quote_sql_string_literal(s)
                }
                // Type::UNKNOWN (OID 705) - pgx/GORM sends binary unknown when type is not inferred.
                // Prefer fixed-width numeric decoding when payload contains NUL/control bytes
                // (common for binary integers). Otherwise, treat as text.
                t if *t == Type::UNKNOWN => {
                    let bytes = param_bytes.as_ref();
                    let has_control_bytes = bytes
                        .iter()
                        .any(|b| *b == 0 || (*b < 0x20 && !matches!(*b, b'\t' | b'\n' | b'\r')));

                    if has_control_bytes {
                        match bytes.len() {
                            8 => {
                                let arr: [u8; 8] = bytes.try_into().unwrap();
                                i64::from_be_bytes(arr).to_string()
                            }
                            4 => {
                                let arr: [u8; 4] = bytes.try_into().unwrap();
                                i32::from_be_bytes(arr).to_string()
                            }
                            2 => {
                                let arr: [u8; 2] = bytes.try_into().unwrap();
                                i16::from_be_bytes(arr).to_string()
                            }
                            1 => (bytes[0] as i8).to_string(),
                            _ => {
                                let hex = hex::encode(bytes);
                                format!("'\\x{}'::bytea", hex)
                            }
                        }
                    } else if let Ok(s) = std::str::from_utf8(bytes) {
                        // Common case: drivers send "binary" for unknown but the payload is ASCII.
                        // Try to parse as integer first (LIMIT/OFFSET), otherwise quote as text.
                        if let Ok(v) = s.trim().parse::<i64>() {
                            v.to_string()
                        } else {
                            quote_sql_string_literal(s)
                        }
                    } else {
                        // Non-UTF8 and no obvious control bytes: best-effort numeric decode by size.
                        match bytes.len() {
                            8 => {
                                let arr: [u8; 8] = bytes.try_into().unwrap();
                                i64::from_be_bytes(arr).to_string()
                            }
                            4 => {
                                let arr: [u8; 4] = bytes.try_into().unwrap();
                                i32::from_be_bytes(arr).to_string()
                            }
                            _ => {
                                let hex = hex::encode(bytes);
                                format!("'\\x{}'::bytea", hex)
                            }
                        }
                    }
                }
                _ => {
                    return Err(invalid_param(format!(
                        "unsupported binary parameter type {}",
                        param_type.name()
                    )));
                }
            }
        } else {
            let raw = std::str::from_utf8(param_bytes.as_ref())
                .map_err(|e| invalid_param(e.to_string()))?;

            let trimmed = raw.trim();

            match &param_type {
                t if *t == Type::BOOL => {
                    let lower = trimmed.to_ascii_lowercase();
                    match lower.as_str() {
                        "t" | "true" | "1" => "true".to_string(),
                        "f" | "false" | "0" => "false".to_string(),
                        _ => return Err(invalid_param(format!("\"{}\"", raw))),
                    }
                }
                t if *t == Type::INT2 => trimmed
                    .parse::<i16>()
                    .map(|v| v.to_string())
                    .map_err(|e| invalid_param(e.to_string()))?,
                t if *t == Type::INT4 => trimmed
                    .parse::<i32>()
                    .map(|v| v.to_string())
                    .map_err(|e| invalid_param(e.to_string()))?,
                t if *t == Type::INT8 => trimmed
                    .parse::<i64>()
                    .map(|v| v.to_string())
                    .map_err(|e| invalid_param(e.to_string()))?,
                t if *t == Type::FLOAT4 => {
                    let v = trimmed
                        .parse::<f32>()
                        .map_err(|e| invalid_param(e.to_string()))?;
                    if !v.is_finite() {
                        return Err(invalid_param(format!(
                            "non-finite FLOAT4 is not supported: \"{}\"",
                            raw
                        )));
                    }
                    v.to_string()
                }
                t if *t == Type::FLOAT8 => {
                    let v = trimmed
                        .parse::<f64>()
                        .map_err(|e| invalid_param(e.to_string()))?;
                    if !v.is_finite() {
                        return Err(invalid_param(format!(
                            "non-finite FLOAT8 is not supported: \"{}\"",
                            raw
                        )));
                    }
                    v.to_string()
                }
                t if *t == Type::UUID => format!("{}::uuid", quote_sql_string_literal(raw)),
                t if *t == Type::BYTEA => format!("{}::bytea", quote_sql_string_literal(raw)),
                _ => quote_sql_string_literal(raw),
            }
        };

        values.push(value_str);
    }

    Ok(substitute_placeholders_outside_strings_and_dollar(
        query, &values,
    ))
}
