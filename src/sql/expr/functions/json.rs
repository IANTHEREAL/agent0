use crate::model::Value;
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, HashMap};

use super::SqlFn;

const MAX_JSONB_SET_PATH_DEPTH: usize = 64;
const MAX_JSON_RECURSION_DEPTH: usize = 64;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("JSONB_ARRAY_LENGTH", jsonb_array_length);
    map.insert("JSON_ARRAY_LENGTH", jsonb_array_length);
    map.insert("JSONB_TYPEOF", jsonb_typeof);
    map.insert("JSON_TYPEOF", jsonb_typeof);
    map.insert("JSONB_BUILD_OBJECT", jsonb_build_object);
    map.insert("JSON_BUILD_OBJECT", json_build_object);
    map.insert("JSONB_BUILD_ARRAY", jsonb_build_array);
    map.insert("JSON_BUILD_ARRAY", json_build_array);
    map.insert("JSONB_EXISTS", jsonb_exists);
    map.insert("JSONB_EXISTS_ANY", jsonb_exists_any);
    map.insert("JSONB_EXISTS_ALL", jsonb_exists_all);
    map.insert("JSONB_OBJECT_KEYS", jsonb_object_keys);
    map.insert("JSON_OBJECT_KEYS", json_object_keys);
    map.insert("JSONB_EXTRACT_PATH", jsonb_extract_path);
    map.insert("JSON_EXTRACT_PATH", jsonb_extract_path);
    map.insert("JSONB_EXTRACT_PATH_TEXT", jsonb_extract_path_text);
    map.insert("JSON_EXTRACT_PATH_TEXT", jsonb_extract_path_text);
    map.insert("JSONB_PRETTY", jsonb_pretty);
    map.insert("TO_JSON", to_json);
    map.insert("TO_JSONB", to_jsonb);
    map.insert("ROW_TO_JSON", row_to_json);
    map.insert("JSONB_SET", jsonb_set);
    map.insert("JSON_SET", jsonb_set);
    map.insert("JSONB_ARRAY_ELEMENTS", jsonb_array_elements);
    map.insert("JSON_ARRAY_ELEMENTS", json_array_elements);
    map.insert("JSONB_ARRAY_ELEMENTS_TEXT", jsonb_array_elements_text);
    map.insert("JSON_ARRAY_ELEMENTS_TEXT", json_array_elements_text);
    map.insert("JSONB_EACH", jsonb_each);
    map.insert("JSON_EACH", json_each);
    map.insert("JSONB_EACH_TEXT", jsonb_each_text);
    map.insert("JSON_EACH_TEXT", json_each_text);
}

fn json_recursion_depth_error() -> anyhow::Error {
    anyhow!("json value is too deep")
}

fn json_path_recursion_depth_error() -> anyhow::Error {
    anyhow!("json path is too deep")
}

pub(crate) fn value_to_json(val: &Value) -> Result<serde_json::Value> {
    value_to_json_inner(val, 0)
}

fn value_to_json_inner(val: &Value, depth: usize) -> Result<serde_json::Value> {
    Ok(match val {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Int32(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
        Value::Int64(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
        Value::Float64(n) => json_float64_value(*n),
        Value::Text(s) => serde_json::Value::String(s.clone()),
        Value::Json(s) | Value::Jsonb(s) => {
            serde_json::from_str(s).unwrap_or(serde_json::Value::String(s.clone()))
        }
        Value::Array(arr) => {
            if depth >= MAX_JSON_RECURSION_DEPTH {
                return Err(json_recursion_depth_error());
            }
            serde_json::Value::Array(
                arr.iter()
                    .map(|value| value_to_json_inner(value, depth + 1))
                    .collect::<Result<Vec<_>>>()?,
            )
        }
        Value::Timestamp(ts) => serde_json::Value::String(render_json_timestamp_millis(*ts)),
        Value::Uuid(bytes) => serde_json::Value::String(uuid::Uuid::from_bytes(*bytes).to_string()),
        Value::Bytes(b) => serde_json::Value::String(format!("\\x{}", hex::encode(b))),
        Value::Interval(iv) => serde_json::Value::String(iv.to_string()),
        Value::Vector(vec) => serde_json::Value::Array(
            vec.iter()
                .filter_map(|f| serde_json::Number::from_f64(*f))
                .map(serde_json::Value::Number)
                .collect(),
        ),
        Value::Time(_) => serde_json::Value::String(val.to_string()),
        Value::Date(days) => serde_json::Value::String(
            crate::model::date::format_date_days(*days).unwrap_or_else(|_| days.to_string()),
        ),
        Value::Numeric(d) => {
            let s = d.to_string();
            serde_json::Value::Number(serde_json::Number::from_string_unchecked(s))
        }
        Value::Tsvector(s) | Value::Tsquery(s) => serde_json::Value::String(s.clone()),
    })
}

pub(crate) fn render_json_timestamp_millis(ts_millis: i64) -> String {
    let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts_millis) else {
        return ts_millis.to_string();
    };
    let mut out = dt.format("%Y-%m-%dT%H:%M:%S").to_string();
    append_json_millis_fraction(&mut out, dt.timestamp_subsec_millis());
    out
}

pub(crate) fn render_json_timestamptz_millis(ts_millis: i64, timezone: &str) -> String {
    let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts_millis) else {
        return ts_millis.to_string();
    };
    let local = match crate::model::timestamp::TimeZoneSpec::parse(timezone) {
        crate::model::timestamp::TimeZoneSpec::Fixed(offset) => dt.with_timezone(&offset),
        crate::model::timestamp::TimeZoneSpec::Named(tz) => dt.with_timezone(&tz).fixed_offset(),
    };
    let mut out = local.format("%Y-%m-%dT%H:%M:%S").to_string();
    append_json_millis_fraction(&mut out, local.timestamp_subsec_millis());
    out.push_str(&format_json_offset_suffix(local.offset().local_minus_utc()));
    out
}

fn append_json_millis_fraction(out: &mut String, millis: u32) {
    if millis == 0 {
        return;
    }

    let mut fraction = format!("{millis:03}");
    while fraction.ends_with('0') {
        fraction.pop();
    }
    out.push('.');
    out.push_str(&fraction);
}

fn format_json_offset_suffix(offset_secs: i32) -> String {
    let sign = if offset_secs >= 0 { '+' } else { '-' };
    let abs = offset_secs.unsigned_abs();
    let hours = abs / 3600;
    let minutes = (abs % 3600) / 60;
    let seconds = abs % 60;

    if seconds == 0 {
        format!("{sign}{hours:02}:{minutes:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    }
}

fn json_float64_value(value: f64) -> serde_json::Value {
    if let Some(number) = serde_json::Number::from_f64(value) {
        return serde_json::Value::Number(number);
    }
    let rendered = if value.is_nan() {
        "NaN"
    } else if value.is_sign_positive() {
        "Infinity"
    } else {
        "-Infinity"
    };
    serde_json::Value::String(rendered.to_owned())
}

pub(crate) fn render_json_text_pg_from_value(val: &Value) -> Result<String> {
    render_json_text_pg_from_value_inner(val, 0)
}

fn render_json_text_pg_from_value_inner(val: &Value, depth: usize) -> Result<String> {
    match val {
        Value::Json(raw) => Ok(raw.clone()),
        Value::Jsonb(raw) => Ok(crate::sql::jsonb::format_jsonb_pg_str(raw)),
        Value::Array(values) => render_json_text_pg_array_inner(values, depth),
        Value::Numeric(value) => Ok(value.to_string()),
        other => Ok(value_to_json(other)?.to_string()),
    }
}

fn append_checked(out: &mut String, value: &str) -> Result<()> {
    super::string::check_output_byte_size(out.len().saturating_add(value.len()))?;
    out.push_str(value);
    Ok(())
}

fn push_char_checked(out: &mut String, value: char) -> Result<()> {
    super::string::check_output_byte_size(out.len().saturating_add(value.len_utf8()))?;
    out.push(value);
    Ok(())
}

fn render_json_text_pg_array_inner(values: &[Value], depth: usize) -> Result<String> {
    if depth >= MAX_JSON_RECURSION_DEPTH {
        return Err(json_recursion_depth_error());
    }

    let mut out = String::new();
    push_char_checked(&mut out, '[')?;
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            append_checked(&mut out, ", ")?;
        }
        let rendered = render_json_text_pg_from_value_inner(value, depth + 1)?;
        append_checked(&mut out, &rendered)?;
    }
    push_char_checked(&mut out, ']')?;
    Ok(out)
}

pub(crate) fn render_jsonb_text_pg_from_value(val: &Value) -> Result<String> {
    render_jsonb_text_pg_from_value_inner(val, 0)
}

fn render_jsonb_text_pg_from_value_inner(val: &Value, depth: usize) -> Result<String> {
    match val {
        Value::Json(raw) | Value::Jsonb(raw) => Ok(crate::sql::jsonb::format_jsonb_pg_str(raw)),
        Value::Array(values) => {
            if depth >= MAX_JSON_RECURSION_DEPTH {
                return Err(json_recursion_depth_error());
            }
            let rendered = values
                .iter()
                .map(|value| render_jsonb_text_pg_from_value_inner(value, depth + 1))
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("[{}]", rendered.join(",")))
        }
        Value::Numeric(value) => Ok(value.to_string()),
        other => Ok(value_to_json(other)?.to_string()),
    }
}

pub(crate) fn format_jsonb_pg(value: &serde_json::Value) -> Result<String> {
    let mut out = String::new();
    write_jsonb_pg(&mut out, value, 0)?;
    Ok(out)
}

fn write_jsonb_pg(out: &mut String, value: &serde_json::Value, depth: usize) -> Result<()> {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => out.push_str(&value.to_string()),
        serde_json::Value::String(value) => {
            if let Ok(escaped) = serde_json::to_string(value) {
                out.push_str(&escaped);
            } else {
                out.push_str("\"\"");
            }
        }
        serde_json::Value::Array(values) => {
            if depth >= MAX_JSON_RECURSION_DEPTH {
                return Err(json_recursion_depth_error());
            }
            out.push('[');
            for (index, item) in values.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_jsonb_pg(out, item, depth + 1)?;
            }
            out.push(']');
        }
        serde_json::Value::Object(values) => {
            if depth >= MAX_JSON_RECURSION_DEPTH {
                return Err(json_recursion_depth_error());
            }
            out.push('{');
            for (index, (key, value)) in crate::sql::jsonb::sorted_jsonb_object_items(values)
                .into_iter()
                .enumerate()
            {
                if index > 0 {
                    out.push_str(", ");
                }
                if let Ok(escaped_key) = serde_json::to_string(key) {
                    out.push_str(&escaped_key);
                } else {
                    out.push_str("\"\"");
                }
                out.push_str(": ");
                write_jsonb_pg(out, value, depth + 1)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

pub(crate) fn format_jsonb_pretty_pg(value: &serde_json::Value) -> Result<String> {
    let mut out = String::new();
    write_jsonb_pretty_pg(&mut out, value, 0)?;
    Ok(out)
}

fn write_jsonb_pretty_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("    ");
    }
}

fn write_jsonb_pretty_pg(out: &mut String, value: &serde_json::Value, depth: usize) -> Result<()> {
    match value {
        serde_json::Value::Array(values) => {
            if depth >= MAX_JSON_RECURSION_DEPTH {
                return Err(json_recursion_depth_error());
            }
            if values.is_empty() {
                out.push_str("[]");
                return Ok(());
            }

            out.push_str("[\n");
            for (index, item) in values.iter().enumerate() {
                write_jsonb_pretty_indent(out, depth + 1);
                write_jsonb_pretty_pg(out, item, depth + 1)?;
                if index + 1 < values.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_jsonb_pretty_indent(out, depth);
            out.push(']');
        }
        serde_json::Value::Object(values) => {
            if depth >= MAX_JSON_RECURSION_DEPTH {
                return Err(json_recursion_depth_error());
            }
            if values.is_empty() {
                out.push_str("{}");
                return Ok(());
            }

            out.push_str("{\n");
            let items = crate::sql::jsonb::sorted_jsonb_object_items(values);
            for (index, (key, value)) in items.iter().enumerate() {
                write_jsonb_pretty_indent(out, depth + 1);
                if let Ok(escaped_key) = serde_json::to_string(key) {
                    out.push_str(&escaped_key);
                } else {
                    out.push_str("\"\"");
                }
                out.push_str(": ");
                write_jsonb_pretty_pg(out, value, depth + 1)?;
                if index + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_jsonb_pretty_indent(out, depth);
            out.push('}');
        }
        _ => write_jsonb_pg(out, value, depth)?,
    }
    Ok(())
}

fn parse_pg_text_array_literal(value: &str) -> Result<Vec<Option<String>>> {
    let trimmed = value.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        if inner.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut escape_next = false;
        let mut quoted = false;

        for ch in inner.chars() {
            if escape_next {
                current.push(ch);
                escape_next = false;
                continue;
            }

            if in_quotes {
                match ch {
                    '\\' => escape_next = true,
                    '"' => in_quotes = false,
                    other => current.push(other),
                }
                continue;
            }

            match ch {
                ',' => {
                    out.push(parse_pg_text_array_element(&current, quoted));
                    current.clear();
                    quoted = false;
                }
                '"' if current.trim().is_empty() && !quoted => {
                    current.clear();
                    in_quotes = true;
                    quoted = true;
                }
                '\\' => escape_next = true,
                whitespace if quoted && whitespace.is_whitespace() => {}
                other => current.push(other),
            }
        }

        if escape_next || in_quotes {
            return Err(anyhow!("invalid text[] path literal"));
        }

        out.push(parse_pg_text_array_element(&current, quoted));
        return Ok(out);
    }
    Ok(vec![Some(trimmed.to_string())])
}

fn parse_pg_text_array_element(value: &str, quoted: bool) -> Option<String> {
    if quoted {
        Some(value.to_string())
    } else {
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("NULL") {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

pub(crate) fn extract_json_path_raw<'a>(json_str: &'a str, path: &[String]) -> Option<&'a str> {
    let mut current = json_str.trim();
    for part in path {
        current = extract_json_child_raw(current, part)?;
    }
    Some(current.trim())
}

pub(crate) fn json_text_value_from_raw(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed == "null" {
        return Value::Null;
    }
    if trimmed.starts_with('"') {
        return serde_json::from_str::<String>(trimmed)
            .map(Value::Text)
            .unwrap_or_else(|_| Value::Text(trimmed.to_string()));
    }
    Value::Text(trimmed.to_string())
}

pub(crate) fn jsonb_text_value_from_raw(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed == "null" {
        return Value::Null;
    }
    if trimmed.starts_with('"') {
        return serde_json::from_str::<String>(trimmed)
            .map(Value::Text)
            .unwrap_or_else(|_| Value::Text(trimmed.to_string()));
    }
    Value::Text(crate::sql::jsonb::format_jsonb_pg_str(trimmed))
}

pub(crate) fn jsonb_value_from_raw(raw: &str) -> Value {
    Value::Jsonb(crate::sql::jsonb::format_jsonb_pg_str(raw))
}

pub(crate) fn extract_json_object_key_raw<'a>(json_str: &'a str, key: &str) -> Option<&'a str> {
    extract_json_object_value_raw(json_str.trim(), key)
}

pub(crate) fn extract_json_array_index_raw(json_str: &str, index: i64) -> Option<&str> {
    extract_json_array_value_raw(json_str.trim(), index)
}

fn extract_json_child_raw<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    let trimmed = raw.trim();
    match trimmed.as_bytes().first().copied() {
        Some(b'{') => extract_json_object_value_raw(trimmed, key),
        Some(b'[') => extract_json_array_value_raw(trimmed, key.parse().ok()?),
        _ => None,
    }
}

fn extract_json_object_value_raw<'a>(raw: &'a str, target: &str) -> Option<&'a str> {
    let bytes = raw.as_bytes();
    let mut idx = skip_json_whitespace(raw, 0);
    if bytes.get(idx).copied()? != b'{' {
        return None;
    }
    idx += 1;
    let mut last_match: Option<&'a str> = None;

    loop {
        idx = skip_json_whitespace(raw, idx);
        match bytes.get(idx).copied()? {
            b'}' => return last_match,
            b'"' => {}
            _ => return None,
        }

        let key_end = parse_json_string_end(raw, idx)?;
        let key = serde_json::from_str::<String>(&raw[idx..key_end]).ok()?;
        idx = skip_json_whitespace(raw, key_end);
        if bytes.get(idx).copied()? != b':' {
            return None;
        }
        idx = skip_json_whitespace(raw, idx + 1);
        let value_start = idx;
        let value_end = json_value_end(raw, value_start)?;
        if key == target {
            last_match = Some(raw[value_start..value_end].trim());
        }
        idx = skip_json_whitespace(raw, value_end);
        match bytes.get(idx).copied()? {
            b',' => idx += 1,
            b'}' => return last_match,
            _ => return None,
        }
    }
}

fn extract_json_array_value_raw(raw: &str, index: i64) -> Option<&str> {
    let bytes = raw.as_bytes();
    let mut idx = skip_json_whitespace(raw, 0);
    if bytes.get(idx).copied()? != b'[' {
        return None;
    }
    idx += 1;

    let mut items = Vec::new();
    loop {
        idx = skip_json_whitespace(raw, idx);
        match bytes.get(idx).copied()? {
            b']' => break,
            _ => {
                let start = idx;
                let end = json_value_end(raw, start)?;
                items.push(raw[start..end].trim());
                idx = skip_json_whitespace(raw, end);
                match bytes.get(idx).copied()? {
                    b',' => idx += 1,
                    b']' => break,
                    _ => return None,
                }
            }
        }
    }

    let normalized = if index < 0 {
        items.len() as i64 + index
    } else {
        index
    };
    if normalized < 0 {
        return None;
    }
    items.get(normalized as usize).copied()
}

fn raw_array_index(index: i64, len: usize) -> Option<usize> {
    let normalized = if index < 0 { len as i64 + index } else { index };
    if normalized < 0 {
        return None;
    }
    let normalized = usize::try_from(normalized).ok()?;
    (normalized < len).then_some(normalized)
}

fn render_jsonb_raw_object(values: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::from("{");
    for (index, (key, value)) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(key)?);
        out.push(':');
        out.push_str(value);
    }
    out.push('}');
    Ok(out)
}

fn render_jsonb_raw_array(values: &[String]) -> String {
    format!("[{}]", values.join(","))
}

fn delete_json_path_raw_inner(raw: &str, path: &[String], depth: usize) -> Result<Option<String>> {
    if path.is_empty() {
        return Ok(None);
    }
    if depth >= MAX_JSONB_SET_PATH_DEPTH {
        return Err(json_path_recursion_depth_error());
    }

    let trimmed = raw.trim();
    match trimmed.as_bytes().first().copied() {
        Some(b'{') => {
            let parsed = serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(trimmed)?;
            let mut values: BTreeMap<String, String> = parsed
                .into_iter()
                .map(|(key, value)| (key, value.get().trim().to_string()))
                .collect();
            let key = &path[0];
            if path.len() == 1 {
                if values.remove(key).is_some() {
                    return render_jsonb_raw_object(&values).map(Some);
                }
                return Ok(None);
            }
            let Some(child) = values.get(key).cloned() else {
                return Ok(None);
            };
            let Some(new_child) = delete_json_path_raw_inner(&child, &path[1..], depth + 1)? else {
                return Ok(None);
            };
            values.insert(key.clone(), new_child);
            render_jsonb_raw_object(&values).map(Some)
        }
        Some(b'[') => {
            let parsed = serde_json::from_str::<Vec<Box<RawValue>>>(trimmed)?;
            let mut values: Vec<String> = parsed
                .into_iter()
                .map(|value| value.get().trim().to_string())
                .collect();
            let Ok(index) = path[0].parse::<i64>() else {
                return Ok(None);
            };
            let Some(index) = raw_array_index(index, values.len()) else {
                return Ok(None);
            };
            if path.len() == 1 {
                values.remove(index);
                return Ok(Some(render_jsonb_raw_array(&values)));
            }
            let Some(new_child) =
                delete_json_path_raw_inner(&values[index], &path[1..], depth + 1)?
            else {
                return Ok(None);
            };
            values[index] = new_child;
            Ok(Some(render_jsonb_raw_array(&values)))
        }
        _ => Ok(None),
    }
}

pub(crate) fn delete_json_path_raw(json_str: &str, path: &[String]) -> Result<String> {
    match delete_json_path_raw_inner(json_str, path, 0)? {
        Some(updated) => Ok(crate::sql::jsonb::format_jsonb_pg_str(&updated)),
        None => Ok(crate::sql::jsonb::format_jsonb_pg_str(json_str)),
    }
}

fn validate_jsonb_raw(json_str: &str) -> Result<()> {
    serde_json::from_str::<Box<RawValue>>(json_str)
        .map(|_| ())
        .map_err(|e| anyhow!("Invalid JSON: {}", e))
}

fn parse_json_string_end(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    if bytes.get(start).copied()? != b'"' {
        return None;
    }

    let mut idx = start + 1;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\\' => idx += 2,
            b'"' => return Some(idx + 1),
            _ => idx += 1,
        }
    }
    None
}

fn json_value_end(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let idx = skip_json_whitespace(raw, start);
    match bytes.get(idx).copied()? {
        b'"' => parse_json_string_end(raw, idx),
        b'{' => json_matching_delim_end(raw, idx, b'{', b'}'),
        b'[' => json_matching_delim_end(raw, idx, b'[', b']'),
        b't' if raw[idx..].starts_with("true") => Some(idx + 4),
        b'f' if raw[idx..].starts_with("false") => Some(idx + 5),
        b'n' if raw[idx..].starts_with("null") => Some(idx + 4),
        b'-' | b'0'..=b'9' => {
            let mut end = idx + 1;
            while end < bytes.len() {
                match bytes[end] {
                    b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t' => break,
                    _ => end += 1,
                }
            }
            Some(end)
        }
        _ => None,
    }
}

fn json_matching_delim_end(raw: &str, start: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut depth = 0usize;
    let mut idx = start;
    let mut in_string = false;
    let mut escape = false;

    while idx < bytes.len() {
        let byte = bytes[idx];
        if in_string {
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                in_string = false;
            }
            idx += 1;
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b if b == open => depth += 1,
            b if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx + 1);
                }
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

fn skip_json_whitespace(raw: &str, mut idx: usize) -> usize {
    let bytes = raw.as_bytes();
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    idx
}

pub fn jsonb_array_length(args: Vec<Value>) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("jsonb_array_length requires json/jsonb argument")),
    };
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    match json_val {
        serde_json::Value::Array(arr) => Ok(Value::Int32(arr.len() as i32)),
        _ => Err(anyhow!("cannot get array length of a non-array")),
    }
}

pub fn jsonb_typeof(args: Vec<Value>) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("jsonb_typeof requires json/jsonb argument")),
    };
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    let type_name = match json_val {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    };
    Ok(Value::Text(type_name.to_string()))
}

pub fn jsonb_build_object(args: Vec<Value>) -> Result<Value> {
    // PostgreSQL requires even number of arguments
    if !args.len().is_multiple_of(2) {
        return Err(SqlError::InvalidParameterValue {
            message: "argument list must have even number of elements".into(),
        }
        .into());
    }

    let mut object = serde_json::Map::new();
    for (i, chunk) in args.chunks(2).enumerate() {
        // PostgreSQL errors on NULL keys, reporting the 1-based argument position
        if matches!(chunk[0], Value::Null) {
            return Err(SqlError::InvalidParameterValue {
                message: format!("argument {}: key must not be null", i * 2 + 1),
            }
            .into());
        }

        let key_str = match &chunk[0] {
            Value::Text(s) => s.clone(),
            v => v.to_string(),
        };
        object.insert(key_str, value_to_json(&chunk[1])?);
    }
    Ok(Value::Jsonb(format_jsonb_pg(&serde_json::Value::Object(
        object,
    ))?))
}

/// PostgreSQL `json_build_object` preserves insertion order and renders JSON values
/// using JSON text semantics instead of JSONB canonicalization.
pub fn json_build_object(args: Vec<Value>) -> Result<Value> {
    if !args.len().is_multiple_of(2) {
        return Err(SqlError::InvalidParameterValue {
            message: "argument list must have even number of elements".into(),
        }
        .into());
    }

    let mut rendered_pairs = Vec::with_capacity(args.len() / 2);
    let mut iter = args.into_iter();
    while let Some(key) = iter.next() {
        if matches!(key, Value::Null) {
            return Err(SqlError::NullValueNotAllowed {
                message: "null value not allowed for object key".into(),
            }
            .into());
        }

        let key_str = match key {
            Value::Text(s) => s,
            v => v.to_string(),
        };
        let val = iter.next().expect("Even argument count guaranteed above");
        let rendered_key = serde_json::to_string(&key_str).unwrap_or_else(|_| "\"\"".into());
        rendered_pairs.push(format!(
            "{} : {}",
            rendered_key,
            render_json_text_pg_from_value(&val)?
        ));
    }
    Ok(Value::Json(format!("{{{}}}", rendered_pairs.join(", "))))
}

pub fn jsonb_build_array(args: Vec<Value>) -> Result<Value> {
    let values = args.iter().map(value_to_json).collect::<Result<Vec<_>>>()?;
    Ok(Value::Jsonb(format_jsonb_pg(&serde_json::Value::Array(
        values,
    ))?))
}

pub fn json_build_array(args: Vec<Value>) -> Result<Value> {
    Ok(Value::Json(render_json_text_pg_array_inner(&args, 0)?))
}

pub fn jsonb_exists(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("jsonb_exists requires exactly 2 arguments"));
    }
    let mut iter = args.into_iter();
    let json_str = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let key = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };

    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    Ok(Value::Boolean(crate::sql::jsonb::exists(&json_val, &key)))
}

pub fn jsonb_exists_any(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("jsonb_exists_any requires exactly 2 arguments"));
    }
    let mut iter = args.into_iter();
    let json_str = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let keys_val = iter.next().unwrap_or(Value::Null);

    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

    let exists_any = match keys_val {
        Value::Array(keys) => {
            if keys
                .iter()
                .all(|k| matches!(k, Value::Null | Value::Text(_)))
            {
                crate::sql::jsonb::exists_any(
                    &json_val,
                    keys.iter().filter_map(|k| match k {
                        Value::Text(s) => Some(s.as_str()),
                        _ => None,
                    }),
                )
            } else {
                keys.iter().any(|k| {
                    if let Value::Null = k {
                        return false;
                    }
                    match k {
                        Value::Text(s) => crate::sql::jsonb::exists(&json_val, s),
                        other => crate::sql::jsonb::exists(&json_val, &other.to_string()),
                    }
                })
            }
        }
        Value::Null => return Ok(Value::Null),
        other => crate::sql::jsonb::exists(&json_val, &other.to_string()),
    };

    Ok(Value::Boolean(exists_any))
}

pub fn jsonb_exists_all(args: Vec<Value>) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("jsonb_exists_all requires exactly 2 arguments"));
    }
    let mut iter = args.into_iter();
    let json_str = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let keys_val = iter.next().unwrap_or(Value::Null);

    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

    let exists_all = match keys_val {
        Value::Array(keys) => {
            if keys
                .iter()
                .all(|k| matches!(k, Value::Null | Value::Text(_)))
            {
                crate::sql::jsonb::exists_all(
                    &json_val,
                    keys.iter().filter_map(|k| match k {
                        Value::Text(s) => Some(s.as_str()),
                        _ => None,
                    }),
                )
            } else {
                keys.iter().all(|k| {
                    if let Value::Null = k {
                        return true;
                    }
                    match k {
                        Value::Text(s) => crate::sql::jsonb::exists(&json_val, s),
                        other => crate::sql::jsonb::exists(&json_val, &other.to_string()),
                    }
                })
            }
        }
        Value::Null => return Ok(Value::Null),
        other => crate::sql::jsonb::exists(&json_val, &other.to_string()),
    };

    Ok(Value::Boolean(exists_all))
}

pub fn jsonb_object_keys(args: Vec<Value>) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("jsonb_object_keys requires json/jsonb argument")),
    };
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    match json_val {
        serde_json::Value::Object(obj) => {
            let keys: Vec<Value> = obj.keys().map(|k| Value::Text(k.clone())).collect();
            Ok(Value::Array(keys))
        }
        _ => Err(anyhow!("cannot call jsonb_object_keys on a non-object")),
    }
}

/// json_object_keys for JSON type — preserves original key order by parsing raw string.
/// PostgreSQL only defines json_object_keys(json); jsonb input must use jsonb_object_keys(jsonb).
pub fn json_object_keys(args: Vec<Value>) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) => s,
        Some(Value::Jsonb(_)) => {
            return Err(SqlError::FunctionNotFound("json_object_keys(jsonb)".into()).into())
        }
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("json_object_keys requires json argument")),
    };
    // Validate it's an object — PostgreSQL distinguishes array vs. scalar errors
    let json_val: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| anyhow!("invalid input syntax for type json: {}", e))?;
    match &json_val {
        serde_json::Value::Object(_) => {}
        serde_json::Value::Array(_) => {
            return Err(anyhow!("cannot call json_object_keys on an array"));
        }
        _ => {
            return Err(anyhow!("cannot call json_object_keys on a scalar"));
        }
    }
    let keys = extract_json_object_keys_raw(&json_str);
    Ok(Value::Array(keys.into_iter().map(Value::Text).collect()))
}

/// Extract object keys from a raw JSON object string, preserving original order.
fn extract_json_object_keys_raw(json_str: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let chars: Vec<char> = json_str.trim().chars().collect();
    if chars.is_empty() || chars[0] != '{' {
        return keys;
    }

    let mut depth = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut expect_key = true; // After '{' or ',' at depth 1, next string is a key

    for (i, &c) in chars.iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' && in_string {
            escape = true;
            continue;
        }
        if c == '"' {
            if !in_string {
                in_string = true;
                if depth == 1 && expect_key {
                    // Start of a key — find the closing quote
                    let key_start = i + 1;
                    let mut key_end = key_start;
                    let mut esc = false;
                    for (j, &kc) in chars[key_start..].iter().enumerate() {
                        if esc {
                            esc = false;
                            continue;
                        }
                        if kc == '\\' {
                            esc = true;
                            continue;
                        }
                        if kc == '"' {
                            key_end = key_start + j;
                            break;
                        }
                    }
                    let raw_key: String = chars[key_start..key_end].iter().collect();
                    // Unescape JSON string escapes
                    if let Ok(serde_json::Value::String(unescaped)) =
                        serde_json::from_str(&format!("\"{}\"", raw_key))
                    {
                        keys.push(unescaped);
                    } else {
                        keys.push(raw_key);
                    }
                    expect_key = false;
                }
            } else {
                in_string = false;
            }
            continue;
        }
        if in_string {
            continue;
        }
        if c == '{' || c == '[' {
            depth += 1;
            continue;
        }
        if c == '}' || c == ']' {
            depth -= 1;
            continue;
        }
        if c == ',' && depth == 1 {
            expect_key = true;
        }
    }

    keys
}

pub fn jsonb_extract_path(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let (json_str, preserve_json_text) = match iter.next() {
        Some(Value::Jsonb(s)) => (s, false),
        Some(Value::Text(s)) | Some(Value::Json(s)) => (s, true),
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("json_extract_path requires json/jsonb argument")),
    };
    let path: Vec<String> = iter
        .map(|path_part| match path_part {
            Value::Text(s) => s,
            v => v.to_string(),
        })
        .collect();

    if preserve_json_text {
        return Ok(match extract_json_path_raw(&json_str, &path) {
            Some(raw) => Value::Json(raw.to_string()),
            None => Value::Null,
        });
    }

    validate_jsonb_raw(&json_str)?;
    Ok(match extract_json_path_raw(&json_str, &path) {
        Some(raw) => jsonb_value_from_raw(raw),
        None => Value::Null,
    })
}

pub fn jsonb_extract_path_text(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let (json_str, preserve_json_text) = match iter.next() {
        Some(Value::Jsonb(s)) => (s, false),
        Some(Value::Text(s)) | Some(Value::Json(s)) => (s, true),
        Some(Value::Null) => return Ok(Value::Null),
        _ => {
            return Err(anyhow!(
                "json_extract_path_text requires json/jsonb argument"
            ))
        }
    };
    let path: Vec<String> = iter
        .map(|path_part| match path_part {
            Value::Text(s) => s,
            v => v.to_string(),
        })
        .collect();

    if preserve_json_text {
        return Ok(match extract_json_path_raw(&json_str, &path) {
            Some(raw) => json_text_value_from_raw(raw),
            None => Value::Null,
        });
    }

    validate_jsonb_raw(&json_str)?;
    Ok(match extract_json_path_raw(&json_str, &path) {
        Some(raw) => jsonb_text_value_from_raw(raw),
        None => Value::Null,
    })
}

pub fn jsonb_pretty(args: Vec<Value>) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("jsonb_pretty requires json/jsonb argument")),
    };
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    Ok(Value::Text(format_jsonb_pretty_pg(&json_val)?))
}

pub fn to_json(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    Ok(Value::Json(render_json_text_pg_from_value(&val)?))
}

pub fn to_jsonb(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    let rendered = render_jsonb_text_pg_from_value(&val)?;
    Ok(Value::Jsonb(crate::sql::jsonb::format_jsonb_pg_str(
        &rendered,
    )))
}

pub fn row_to_json(args: Vec<Value>) -> Result<Value> {
    let Some(val) = args.into_iter().next() else {
        return Ok(Value::Null);
    };
    match val {
        Value::Null => Ok(Value::Null),
        Value::Array(arr) => {
            let mut rendered_fields = Vec::with_capacity(arr.len());
            for (i, v) in arr.iter().enumerate() {
                rendered_fields.push(format!(
                    "{}:{}",
                    serde_json::Value::String(format!("f{}", i + 1)),
                    render_json_text_pg_from_value(v)?
                ));
            }
            Ok(Value::Json(format!("{{{}}}", rendered_fields.join(","))))
        }
        other => Ok(Value::Json(render_json_text_pg_from_value(&other)?)),
    }
}

fn parse_jsonb_set_text_path(path: &str) -> Result<Vec<Value>> {
    let path = path.trim();
    if !path.starts_with('{') || !path.ends_with('}') {
        return Err(anyhow!("invalid text[] path literal"));
    }

    parse_pg_text_array_literal(path).map(|parts| {
        parts
            .into_iter()
            .map(|part| part.map_or(Value::Null, Value::Text))
            .collect()
    })
}

pub fn jsonb_set(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let json_str = match iter.next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("jsonb_set requires json/jsonb as first argument")),
    };
    let path = match iter.next() {
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Array(arr)) => arr,
        Some(Value::Text(s)) => parse_jsonb_set_text_path(&s)
            .map_err(|_| anyhow!("jsonb_set requires array path as second argument"))?,
        _ => return Err(anyhow!("jsonb_set requires array path as second argument")),
    };
    let new_value = match iter.next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => {
            serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
        }
        Some(Value::Int32(n)) => serde_json::Value::Number(n.into()),
        Some(Value::Int64(n)) => serde_json::Value::Number(n.into()),
        Some(Value::Boolean(b)) => serde_json::Value::Bool(b),
        Some(Value::Numeric(d)) => {
            let s = d.to_string();
            serde_json::Value::Number(serde_json::Number::from_string_unchecked(s))
        }
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => serde_json::Value::String(v.to_string()),
        None => return Err(anyhow!("jsonb_set requires new value as third argument")),
    };
    let create_missing = match iter.next() {
        Some(Value::Boolean(b)) => b,
        Some(Value::Null) => return Ok(Value::Null),
        _ => true,
    };
    if path.len() > MAX_JSONB_SET_PATH_DEPTH {
        return Err(SqlError::InvalidParameterValue {
            message: format!("jsonb_set path exceeds maximum depth of {MAX_JSONB_SET_PATH_DEPTH}"),
        }
        .into());
    }

    let mut json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

    fn set_at_path(
        val: &mut serde_json::Value,
        path: &[Value],
        new_val: serde_json::Value,
        create_missing: bool,
        path_offset: usize,
    ) -> std::result::Result<bool, SqlError> {
        if path.is_empty() {
            // Should not be reached: caller handles empty path before calling set_at_path.
            return Ok(false);
        }
        let key = match &path[0] {
            Value::Text(s) => s.clone(),
            v => v.to_string(),
        };
        match val {
            serde_json::Value::Object(obj) => {
                if path.len() == 1 {
                    // Final step: can create if create_missing=true
                    if create_missing || obj.contains_key(&key) {
                        obj.insert(key, new_val);
                        return Ok(true);
                    }
                } else if let Some(child) = obj.get_mut(&key) {
                    // Intermediate step exists: continue down the path
                    return set_at_path(
                        child,
                        &path[1..],
                        new_val,
                        create_missing,
                        path_offset + 1,
                    );
                }
                // Intermediate step missing: PostgreSQL returns original value unchanged
                // (create_missing only applies to the final step)
            }
            serde_json::Value::Array(arr) => {
                let raw_idx =
                    key.parse::<isize>()
                        .map_err(|_| SqlError::InvalidParameterValue {
                            message: format!(
                                "path element at position {} is not an integer: \"{}\"",
                                path_offset + 1,
                                key
                            ),
                        })?;
                let idx = if raw_idx >= 0 {
                    Some(raw_idx as usize)
                } else {
                    let abs = raw_idx.unsigned_abs();
                    if abs <= arr.len() {
                        Some(arr.len() - abs)
                    } else {
                        None
                    }
                };
                if path.len() == 1 {
                    if let Some(i) = idx {
                        if i < arr.len() {
                            arr[i] = new_val;
                            return Ok(true);
                        }
                    }
                    if create_missing {
                        // PostgreSQL prepends for out-of-range negative indexes,
                        // appends for out-of-range positive indexes.
                        if raw_idx < 0 && idx.is_none() {
                            arr.insert(0, new_val);
                        } else {
                            arr.push(new_val);
                        }
                        return Ok(true);
                    }
                } else if let Some(i) = idx {
                    if i < arr.len() {
                        // Intermediate step exists: continue down the path
                        return set_at_path(
                            &mut arr[i],
                            &path[1..],
                            new_val,
                            create_missing,
                            path_offset + 1,
                        );
                    }
                    // Array index out of bounds for intermediate step: return unchanged
                }
            }
            _ => {}
        }
        Ok(false)
    }

    for v in &path {
        if matches!(v, Value::Null) {
            return Ok(Value::Null);
        }
    }

    if path.is_empty() {
        return match &json_val {
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                Ok(Value::Jsonb(format_jsonb_pg(&json_val)?))
            }
            _ => Err(anyhow!("cannot set path in scalar")),
        };
    }

    set_at_path(&mut json_val, &path, new_value, create_missing, 0)?;
    Ok(Value::Jsonb(format_jsonb_pg(&json_val)?))
}

pub fn jsonb_array_elements(args: Vec<Value>) -> Result<Value> {
    jsonb_array_elements_impl(args, true)
}

pub fn json_array_elements(args: Vec<Value>) -> Result<Value> {
    jsonb_array_elements_impl(args, false)
}

fn jsonb_array_elements_impl(args: Vec<Value>, is_jsonb: bool) -> Result<Value> {
    let func_name = if is_jsonb {
        "jsonb_array_elements"
    } else {
        "json_array_elements"
    };
    let json_str = match args.into_iter().next() {
        Some(Value::Jsonb(s)) => {
            if !is_jsonb {
                // PostgreSQL only defines json_array_elements(json); jsonb input must use
                // jsonb_array_elements(jsonb).
                return Err(SqlError::FunctionNotFound("json_array_elements(jsonb)".into()).into());
            }
            s
        }
        Some(Value::Text(s)) | Some(Value::Json(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("{} requires json argument", func_name)),
    };

    // Parse the array to get element count and structure
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

    match json_val {
        serde_json::Value::Array(arr) => {
            // For JSON (non-JSONB), preserve original key order by extracting raw substrings.
            // For JSONB, canonicalize by re-serializing.
            let elements: Vec<Value> = if !is_jsonb {
                // Extract raw JSON strings for each element to preserve key order
                extract_json_array_elements_raw(&json_str)
                    .into_iter()
                    .map(Value::Json)
                    .collect()
            } else {
                arr.into_iter()
                    .map(|v| Value::Jsonb(v.to_string()))
                    .collect()
            };
            Ok(Value::Array(elements))
        }
        _ => Err(anyhow!("cannot call {} on a non-array", func_name)),
    }
}

/// Extract raw JSON element strings from a JSON array string, preserving original formatting.
fn extract_json_array_elements_raw(json_str: &str) -> Vec<String> {
    let mut elements = Vec::new();
    let chars: Vec<char> = json_str.trim().chars().collect();
    if chars.is_empty() || chars[0] != '[' {
        return elements;
    }

    let mut depth = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut start = 1; // Skip opening '['

    for (i, &c) in chars.iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' && in_string {
            escape = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if c == '[' || c == '{' {
            depth += 1;
            continue;
        }
        if c == ']' || c == '}' {
            depth -= 1;
            if depth == 0 && c == ']' {
                // End of array
                if i > start {
                    let elem: String = chars[start..i].iter().collect::<String>();
                    let trimmed = elem.trim();
                    if !trimmed.is_empty() {
                        elements.push(trimmed.to_string());
                    }
                }
                break;
            }
            continue;
        }
        if c == ',' && depth == 1 {
            // Element separator at top level of array
            let elem: String = chars[start..i].iter().collect::<String>();
            let trimmed = elem.trim();
            if !trimmed.is_empty() {
                elements.push(trimmed.to_string());
            }
            start = i + 1;
        }
    }

    elements
}

pub fn jsonb_array_elements_text(args: Vec<Value>) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => {
            return Err(anyhow!(
                "jsonb_array_elements_text requires json/jsonb argument"
            ))
        }
    };
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    match json_val {
        serde_json::Value::Array(arr) => {
            let elements: Vec<Value> = arr
                .into_iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => Value::Text(s),
                    serde_json::Value::Null => Value::Null,
                    other => Value::Text(other.to_string()),
                })
                .collect();
            Ok(Value::Array(elements))
        }
        _ => Err(anyhow!(
            "cannot call jsonb_array_elements_text on a non-array"
        )),
    }
}

/// json_array_elements_text for JSON type — preserves original key order in object elements.
/// PostgreSQL only defines json_array_elements_text(json); jsonb input must use
/// jsonb_array_elements_text(jsonb).
pub fn json_array_elements_text(args: Vec<Value>) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) => s,
        Some(Value::Jsonb(_)) => {
            return Err(SqlError::FunctionNotFound("json_array_elements_text(jsonb)".into()).into())
        }
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("json_array_elements_text requires json argument")),
    };
    // Validate it's an array
    let json_val: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| anyhow!("invalid input syntax for type json: {}", e))?;
    if !json_val.is_array() {
        return Err(anyhow!(
            "cannot call json_array_elements_text on a non-array"
        ));
    }
    // Use raw extraction to preserve key order in object elements
    let raw_elements = extract_json_array_elements_raw(&json_str);
    let elements: Vec<Value> = raw_elements
        .into_iter()
        .map(|raw| {
            let trimmed = raw.trim();
            if trimmed == "null" {
                Value::Null
            } else if trimmed.starts_with('"') {
                // JSON string — parse to get unescaped value
                if let Ok(serde_json::Value::String(s)) = serde_json::from_str(trimmed) {
                    Value::Text(s)
                } else {
                    Value::Text(trimmed.to_string())
                }
            } else {
                // Numbers, booleans, objects, arrays — return raw text (preserves key order)
                Value::Text(trimmed.to_string())
            }
        })
        .collect();
    Ok(Value::Array(elements))
}

/// Quote a field for PostgreSQL composite (record) output.
/// Rules follow PG's `record_out`: if the value contains `"`, `\`, `,`, `(`, `)`,
/// whitespace, or is empty, wrap in double-quotes and double any internal `"` or `\`.
fn composite_quote(s: &str) -> String {
    let needs_quoting = s.is_empty()
        || s.contains(|c: char| {
            c == '"' || c == '\\' || c == ',' || c == '(' || c == ')' || c.is_whitespace()
        });
    if !needs_quoting {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push('"');
    out
}

pub fn jsonb_each(args: Vec<Value>) -> Result<Value> {
    jsonb_each_impl(args, false, true, "jsonb_each")
}

pub fn jsonb_each_text(args: Vec<Value>) -> Result<Value> {
    jsonb_each_impl(args, true, true, "jsonb_each_text")
}

pub fn json_each(args: Vec<Value>) -> Result<Value> {
    jsonb_each_impl(args, false, false, "json_each")
}

pub fn json_each_text(args: Vec<Value>) -> Result<Value> {
    jsonb_each_impl(args, true, false, "json_each_text")
}

fn jsonb_each_impl(
    args: Vec<Value>,
    is_text: bool,
    is_jsonb: bool,
    func_name: &str,
) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("{} requires json/jsonb argument", func_name)),
    };
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    match json_val {
        serde_json::Value::Object(obj) => {
            let pairs: Vec<Value> = obj
                .into_iter()
                .map(|(k, v)| {
                    if is_text {
                        let val_part = match v {
                            serde_json::Value::Null => String::new(),
                            serde_json::Value::String(s) => composite_quote(&s),
                            other => composite_quote(&other.to_string()),
                        };
                        Value::Text(format!("({},{})", composite_quote(&k), val_part))
                    } else {
                        let val_str = v.to_string();
                        Value::Text(format!(
                            "({},{})",
                            composite_quote(&k),
                            composite_quote(&val_str)
                        ))
                    }
                })
                .collect();
            Ok(Value::Array(pairs))
        }
        _ if is_jsonb => Err(anyhow!("cannot call {} on a non-object", func_name)),
        serde_json::Value::Array(_) => Err(anyhow!("cannot deconstruct an array as an object")),
        _ => Err(anyhow!("cannot deconstruct a scalar")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn deeply_nested_array(depth: usize) -> Value {
        let mut value = Value::Int32(1);
        for _ in 0..depth {
            value = Value::Array(vec![value]);
        }
        value
    }

    fn deeply_nested_json(depth: usize) -> String {
        let mut value = serde_json::Value::Number(serde_json::Number::from(1));
        for _ in 0..depth {
            value = serde_json::Value::Array(vec![value]);
        }
        value.to_string()
    }

    #[test]
    fn test_jsonb_array_length() {
        assert_eq!(
            jsonb_array_length(vec![Value::Jsonb("[1,2,3]".into())]).unwrap(),
            Value::Int32(3)
        );
    }

    #[test]
    fn test_jsonb_typeof() {
        assert_eq!(
            jsonb_typeof(vec![Value::Jsonb("\"hello\"".into())]).unwrap(),
            Value::Text("string".into())
        );
        assert_eq!(
            jsonb_typeof(vec![Value::Jsonb("[1,2]".into())]).unwrap(),
            Value::Text("array".into())
        );
        assert_eq!(
            jsonb_typeof(vec![Value::Jsonb("{\"a\":1}".into())]).unwrap(),
            Value::Text("object".into())
        );
    }

    #[test]
    fn test_jsonb_build_object() {
        let result = jsonb_build_object(vec![
            Value::Text("a".into()),
            Value::Int32(1),
            Value::Text("b".into()),
            Value::Text("hello".into()),
        ])
        .unwrap();
        if let Value::Jsonb(s) = result {
            let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
            assert_eq!(parsed["a"], 1);
            assert_eq!(parsed["b"], "hello");
        } else {
            panic!("Expected Jsonb value");
        }
        assert_eq!(
            jsonb_build_object(vec![
                Value::Text("b".into()),
                Value::Int32(1),
                Value::Text("a".into()),
                Value::Int32(2),
                Value::Text("b".into()),
                Value::Int32(3),
            ])
            .unwrap(),
            Value::Jsonb(r#"{"a": 2, "b": 3}"#.into())
        );
    }

    #[test]
    fn test_jsonb_build_object_null_key_uses_22023() {
        let err = jsonb_build_object(vec![Value::Null, Value::Text("v".into())]).unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("expected SqlError for null key");
        assert!(matches!(sql_err, SqlError::InvalidParameterValue { .. }));
        assert_eq!(sql_err.sqlstate(), "22023");
    }

    #[test]
    fn test_json_build_object_preserves_argument_order() {
        assert_eq!(
            json_build_object(vec![
                Value::Text("b".into()),
                Value::Int32(2),
                Value::Text("a".into()),
                Value::Int32(1),
            ])
            .unwrap(),
            Value::Json("{\"b\" : 2, \"a\" : 1}".into())
        );
    }

    #[test]
    fn test_json_extract_path_preserves_raw_json_order() {
        assert_eq!(
            jsonb_extract_path(vec![
                Value::Json(r#"{"b":2,"a":{"y":2,"x":1}}"#.into()),
                Value::Text("a".into()),
            ])
            .unwrap(),
            Value::Json(r#"{"y":2,"x":1}"#.into())
        );
    }

    #[test]
    fn test_json_extract_path_text_preserves_raw_json_order() {
        assert_eq!(
            jsonb_extract_path_text(vec![
                Value::Json(r#"{"b":2,"a":{"y":2,"x":1}}"#.into()),
                Value::Text("a".into()),
            ])
            .unwrap(),
            Value::Text(r#"{"y":2,"x":1}"#.into())
        );
    }

    #[test]
    fn test_json_extract_path_uses_last_duplicate_key_for_text_json() {
        assert_eq!(
            jsonb_extract_path(vec![
                Value::Json(r#"{"a":1,"a":2}"#.into()),
                Value::Text("a".into()),
            ])
            .unwrap(),
            Value::Json("2".into())
        );
    }

    #[test]
    fn test_json_extract_path_text_uses_last_duplicate_key_for_text_json() {
        assert_eq!(
            jsonb_extract_path_text(vec![
                Value::Json(r#"{"a":1,"a":2}"#.into()),
                Value::Text("a".into()),
            ])
            .unwrap(),
            Value::Text("2".into())
        );
    }

    #[test]
    fn test_json_extract_path_supports_array_index() {
        assert_eq!(
            jsonb_extract_path(vec![
                Value::Jsonb(r#"{"a":[10,20,30]}"#.into()),
                Value::Text("a".into()),
                Value::Text("1".into()),
            ])
            .unwrap(),
            Value::Jsonb("20".into())
        );
    }

    #[test]
    fn test_json_extract_path_text_supports_negative_array_index() {
        assert_eq!(
            jsonb_extract_path_text(vec![
                Value::Jsonb(r#"{"a":[10,20,30]}"#.into()),
                Value::Text("a".into()),
                Value::Text("-1".into()),
            ])
            .unwrap(),
            Value::Text("30".into())
        );
    }

    #[test]
    fn test_jsonb_extract_path_text_uses_pg_jsonb_format() {
        assert_eq!(
            jsonb_extract_path_text(vec![
                Value::Jsonb(r#"{"b":2,"a":{"y":2,"x":1}}"#.into()),
                Value::Text("a".into()),
            ])
            .unwrap(),
            Value::Text(r#"{"x": 1, "y": 2}"#.into())
        );
    }

    #[test]
    fn test_jsonb_extract_path_preserves_high_precision_numbers() {
        assert_eq!(
            jsonb_extract_path(vec![
                Value::Jsonb(r#"{"n":9007199254740993.123456789}"#.into()),
                Value::Text("n".into()),
            ])
            .unwrap(),
            Value::Jsonb("9007199254740993.123456789".into())
        );
        assert_eq!(
            jsonb_extract_path_text(vec![
                Value::Jsonb(r#"{"n":9007199254740993.123456789}"#.into()),
                Value::Text("n".into()),
            ])
            .unwrap(),
            Value::Text("9007199254740993.123456789".into())
        );
    }

    #[test]
    fn test_jsonb_build_array() {
        let result =
            jsonb_build_array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]).unwrap();
        assert_eq!(result, Value::Jsonb("[1, 2, 3]".into()));
    }

    #[test]
    fn test_json_build_array_uses_json_semantics() {
        assert_eq!(
            json_build_array(vec![
                Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
                Value::Text("x".into()),
            ])
            .unwrap(),
            Value::Json(r#"[[1, 2], "x"]"#.into())
        );
    }

    #[test]
    fn test_jsonb_exists() {
        assert_eq!(
            jsonb_exists(vec![
                Value::Jsonb("{\"a\":1,\"b\":2}".into()),
                Value::Text("a".into())
            ])
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            jsonb_exists(vec![
                Value::Jsonb("{\"a\":1}".into()),
                Value::Text("c".into())
            ])
            .unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn test_jsonb_extract_path() {
        let result = jsonb_extract_path(vec![
            Value::Jsonb("{\"a\":{\"b\":1}}".into()),
            Value::Text("a".into()),
            Value::Text("b".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Jsonb("1".into()));
    }

    #[test]
    fn test_jsonb_pretty() {
        assert_eq!(
            jsonb_pretty(vec![Value::Jsonb(r#"{"b":2,"a":{"y":2,"x":1}}"#.into())]).unwrap(),
            Value::Text(
                "{\n    \"a\": {\n        \"x\": 1,\n        \"y\": 2\n    },\n    \"b\": 2\n}"
                    .into()
            )
        );
    }

    #[test]
    fn test_to_json() {
        assert_eq!(
            to_json(vec![Value::Int32(42)]).unwrap(),
            Value::Json("42".into())
        );
        assert_eq!(
            to_json(vec![Value::Text("hello".into())]).unwrap(),
            Value::Json("\"hello\"".into())
        );
        let high_precision_numeric =
            Value::Numeric(Decimal::from_str_exact("9007199254740993.123456789").unwrap());
        assert_eq!(
            to_json(vec![high_precision_numeric.clone()]).unwrap(),
            Value::Json("9007199254740993.123456789".into())
        );
        assert_eq!(
            value_to_json(&high_precision_numeric).unwrap().to_string(),
            "9007199254740993.123456789"
        );
        assert_eq!(
            to_jsonb(vec![high_precision_numeric.clone()]).unwrap(),
            Value::Jsonb("9007199254740993.123456789".into())
        );
        assert_eq!(
            to_json(vec![Value::Timestamp(1_700_000_000_000)]).unwrap(),
            Value::Json("\"2023-11-14T22:13:20\"".into())
        );
        assert_eq!(
            to_json(vec![Value::Timestamp(1_700_000_000_123)]).unwrap(),
            Value::Json("\"2023-11-14T22:13:20.123\"".into())
        );
        assert_eq!(
            to_jsonb(vec![Value::Time(3_723_004_005)]).unwrap(),
            Value::Jsonb("\"01:02:03.004005\"".into())
        );
        assert_eq!(
            to_json(vec![Value::Array(vec![Value::Int32(1), Value::Int32(2)])]).unwrap(),
            Value::Json("[1, 2]".into())
        );
        assert_eq!(
            to_jsonb(vec![Value::Array(vec![Value::Int32(1), Value::Int32(2)])]).unwrap(),
            Value::Jsonb("[1, 2]".into())
        );
        assert_eq!(
            to_json(vec![Value::Json(r#"{"b":1,"aa":2}"#.into())]).unwrap(),
            Value::Json(r#"{"b":1,"aa":2}"#.into())
        );
        assert_eq!(
            to_json(vec![Value::Jsonb(r#"{"b":1,"aa":2}"#.into())]).unwrap(),
            Value::Json(r#"{"b": 1, "aa": 2}"#.into())
        );
        assert_eq!(
            to_json(vec![Value::Float64(f64::NAN)]).unwrap(),
            Value::Json("\"NaN\"".into())
        );
        assert_eq!(
            to_jsonb(vec![Value::Float64(f64::INFINITY)]).unwrap(),
            Value::Jsonb("\"Infinity\"".into())
        );
        assert_eq!(
            to_json(vec![Value::Float64(f64::NEG_INFINITY)]).unwrap(),
            Value::Json("\"-Infinity\"".into())
        );
        assert_eq!(
            json_build_object(vec![
                Value::Text("n".into()),
                high_precision_numeric.clone()
            ])
            .unwrap(),
            Value::Json(r#"{"n" : 9007199254740993.123456789}"#.into())
        );
        assert_eq!(
            jsonb_build_object(vec![
                Value::Text("n".into()),
                high_precision_numeric.clone()
            ])
            .unwrap(),
            Value::Jsonb(r#"{"n": 9007199254740993.123456789}"#.into())
        );
        assert_eq!(
            jsonb_build_array(vec![high_precision_numeric.clone()]).unwrap(),
            Value::Jsonb("[9007199254740993.123456789]".into())
        );
        assert_eq!(
            row_to_json(vec![Value::Array(vec![high_precision_numeric])]).unwrap(),
            Value::Json(r#"{"f1":9007199254740993.123456789}"#.into())
        );
        assert_eq!(
            row_to_json(vec![Value::Array(vec![Value::Timestamp(
                1_700_000_000_000
            )])])
            .unwrap(),
            Value::Json(r#"{"f1":"2023-11-14T22:13:20"}"#.into())
        );
        assert_eq!(
            row_to_json(vec![Value::Array(vec![
                Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
                Value::Json(r#"{"x":1,"y":[1,2]}"#.into()),
                Value::Jsonb(r#"{"b":1,"aa":2}"#.into()),
            ])])
            .unwrap(),
            Value::Json(r#"{"f1":[1, 2],"f2":{"x":1,"y":[1,2]},"f3":{"b": 1, "aa": 2}}"#.into())
        );
    }

    #[test]
    fn test_json_helpers_reject_excessive_recursion_depth() {
        let deep_array = deeply_nested_array(MAX_JSON_RECURSION_DEPTH + 1);

        let err = to_json(vec![deep_array.clone()]).unwrap_err();
        assert_eq!(err.to_string(), "json value is too deep");

        let err = to_jsonb(vec![deep_array.clone()]).unwrap_err();
        assert_eq!(err.to_string(), "json value is too deep");

        let err = jsonb_build_array(vec![deep_array.clone()]).unwrap_err();
        assert_eq!(err.to_string(), "json value is too deep");

        let err = jsonb_build_object(vec![Value::Text("x".into()), deep_array]).unwrap_err();
        assert_eq!(err.to_string(), "json value is too deep");

        let deep_json = deeply_nested_json(MAX_JSON_RECURSION_DEPTH + 1);
        let err = jsonb_pretty(vec![Value::Jsonb(deep_json.clone())]).unwrap_err();
        assert_eq!(err.to_string(), "json value is too deep");

        let parsed = serde_json::from_str::<serde_json::Value>(&deep_json).unwrap();
        let err = format_jsonb_pg(&parsed).unwrap_err();
        assert_eq!(err.to_string(), "json value is too deep");
    }

    #[test]
    fn test_jsonb_set_text_path_null_returns_null() {
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb("{\"a\":1}".into()),
                Value::Text("{NULL}".into()),
                Value::Jsonb("42".into()),
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_jsonb_set_sql_null_path_returns_null() {
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb("{\"a\":1}".into()),
                Value::Null,
                Value::Jsonb("42".into()),
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_jsonb_set_sql_null_new_value_returns_null() {
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb("{\"a\":1}".into()),
                Value::Text("{a}".into()),
                Value::Null,
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_jsonb_set_sql_null_create_missing_returns_null() {
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb("{\"a\":1}".into()),
                Value::Text("{a}".into()),
                Value::Jsonb("42".into()),
                Value::Null,
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_jsonb_set_empty_path_returns_original_value() {
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb("{\"a\":1}".into()),
                Value::Array(vec![]),
                Value::Jsonb("2".into()),
            ])
            .unwrap(),
            Value::Jsonb("{\"a\": 1}".into())
        );
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb("[1,2,3]".into()),
                Value::Array(vec![]),
                Value::Jsonb("2".into()),
            ])
            .unwrap(),
            Value::Jsonb("[1, 2, 3]".into())
        );
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb("1".into()),
                Value::Array(vec![]),
                Value::Jsonb("2".into()),
            ])
            .unwrap_err()
            .to_string(),
            "cannot set path in scalar"
        );
    }

    #[test]
    fn test_jsonb_set_array_path_text_null_is_valid_key() {
        let result = jsonb_set(vec![
            Value::Jsonb("{\"a\":1}".into()),
            Value::Array(vec![Value::Text("NULL".into())]),
            Value::Jsonb("42".into()),
        ])
        .unwrap();
        let Value::Jsonb(s) = result else {
            panic!("expected jsonb result");
        };
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["NULL"], 42);
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn test_jsonb_set_array_path_sql_null_returns_null() {
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb("{\"a\":1}".into()),
                Value::Array(vec![Value::Null]),
                Value::Jsonb("42".into()),
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_jsonb_set_text_path_quoted_null_is_valid_key() {
        let result = jsonb_set(vec![
            Value::Jsonb("{\"a\":1}".into()),
            Value::Text("{\"NULL\"}".into()),
            Value::Jsonb("42".into()),
        ])
        .unwrap();
        let Value::Jsonb(s) = result else {
            panic!("expected jsonb result");
        };
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["NULL"], 42);
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn test_jsonb_set_text_path_preserves_numeric_like_key() {
        let result = jsonb_set(vec![
            Value::Jsonb("{\"01\":0}".into()),
            Value::Text("{01}".into()),
            Value::Jsonb("42".into()),
        ])
        .unwrap();
        let Value::Jsonb(s) = result else {
            panic!("expected jsonb result");
        };
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["01"], 42);
    }

    #[test]
    fn test_jsonb_set_text_path_preserves_boolean_like_key_case() {
        let result = jsonb_set(vec![
            Value::Jsonb("{\"TRUE\":0}".into()),
            Value::Text("{TRUE}".into()),
            Value::Jsonb("42".into()),
        ])
        .unwrap();
        let Value::Jsonb(s) = result else {
            panic!("expected jsonb result");
        };
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["TRUE"], 42);
    }

    #[test]
    fn test_jsonb_set_text_path_unescapes_text_array_literals() {
        let result = jsonb_set(vec![
            Value::Jsonb("{\"a,b\":1}".into()),
            Value::Text(r#"{a\,b}"#.into()),
            Value::Jsonb("42".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Jsonb(r#"{"a,b": 42}"#.into()));

        let result = jsonb_set(vec![
            Value::Jsonb(r#"{"a\"b":1}"#.into()),
            Value::Text(r#"{"a\"b"}"#.into()),
            Value::Jsonb("42".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Jsonb(r#"{"a\"b": 42}"#.into()));
    }

    #[test]
    fn test_jsonb_set_text_path_rejects_literal_quotes() {
        let err = jsonb_set(vec![
            Value::Jsonb("{\"a\":1}".into()),
            Value::Text("'{a}'".into()),
            Value::Jsonb("42".into()),
        ])
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("requires array path as second argument"));
    }

    #[test]
    fn test_jsonb_set_canonicalizes_jsonb_output() {
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb(r#"{"aa":1,"b":2}"#.into()),
                Value::Array(vec![Value::Text("b".into())]),
                Value::Jsonb("9".into()),
            ])
            .unwrap(),
            Value::Jsonb(r#"{"b": 9, "aa": 1}"#.into())
        );
    }

    #[test]
    fn test_jsonb_set_preserves_high_precision_numbers() {
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb(r#"{"n":0}"#.into()),
                Value::Array(vec![Value::Text("n".into())]),
                Value::Jsonb("9007199254740993.123456789".into()),
            ])
            .unwrap(),
            Value::Jsonb(r#"{"n": 9007199254740993.123456789}"#.into())
        );
        assert_eq!(
            jsonb_set(vec![
                Value::Jsonb(r#"{"n":0}"#.into()),
                Value::Array(vec![Value::Text("n".into())]),
                Value::Numeric(Decimal::from_str_exact("9007199254740993.123456789").unwrap()),
            ])
            .unwrap(),
            Value::Jsonb(r#"{"n": 9007199254740993.123456789}"#.into())
        );
    }

    #[test]
    fn test_jsonb_set_array_path_non_integer_rejected() {
        let err = jsonb_set(vec![
            Value::Jsonb("[1,2,3]".into()),
            Value::Array(vec![Value::Text("x".into())]),
            Value::Jsonb("42".into()),
        ])
        .unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("expected SqlError for non-integer array index");
        assert!(matches!(sql_err, SqlError::InvalidParameterValue { .. }));
        assert_eq!(
            err.to_string(),
            r#"path element at position 1 is not an integer: "x""#
        );
    }

    #[test]
    fn test_jsonb_set_array_path_negative_index() {
        let result = jsonb_set(vec![
            Value::Jsonb("[1,2,3]".into()),
            Value::Array(vec![Value::Text("-1".into())]),
            Value::Jsonb("42".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Jsonb("[1, 2, 42]".into()));
    }

    #[test]
    fn test_jsonb_set_array_path_out_of_range_appends_without_padding() {
        let result = jsonb_set(vec![
            Value::Jsonb("[1,2]".into()),
            Value::Array(vec![Value::Text("5".into())]),
            Value::Jsonb("42".into()),
            Value::Boolean(true),
        ])
        .unwrap();
        assert_eq!(result, Value::Jsonb("[1, 2, 42]".into()));
    }

    #[test]
    fn test_jsonb_set_rejects_excessive_path_depth() {
        let path = (0..=MAX_JSONB_SET_PATH_DEPTH)
            .map(|idx| Value::Text(format!("k{idx}")))
            .collect();
        let err = jsonb_set(vec![
            Value::Jsonb(r#"{"k0":{}}"#.into()),
            Value::Array(path),
            Value::Jsonb("42".into()),
        ])
        .unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("expected SqlError for excessive path depth");
        assert!(matches!(sql_err, SqlError::InvalidParameterValue { .. }));
        assert_eq!(sql_err.sqlstate(), "22023");
        assert_eq!(
            err.to_string(),
            format!("jsonb_set path exceeds maximum depth of {MAX_JSONB_SET_PATH_DEPTH}")
        );
    }

    #[test]
    fn jsonb_each_text_null_value_produces_null_repr() {
        // JSON null → composite text `(a,)` (NULL: nothing after comma)
        let result = jsonb_each_text(vec![Value::Jsonb(r#"{"a":null}"#.into())]).unwrap();
        assert_eq!(result, Value::Array(vec![Value::Text("(a,)".into())]));
    }

    #[test]
    fn jsonb_each_text_empty_string_quoted() {
        // Empty string → composite text `(a,"")` (distinct from NULL)
        let result = jsonb_each_text(vec![Value::Jsonb(r#"{"a":""}"#.into())]).unwrap();
        assert_eq!(result, Value::Array(vec![Value::Text("(a,\"\")".into())]));
    }

    #[test]
    fn jsonb_each_text_null_vs_empty_string_distinct() {
        // NULL and empty string must be distinguishable in scalar output
        let result = jsonb_each_text(vec![Value::Jsonb(r#"{"e":"","n":null}"#.into())]).unwrap();
        if let Value::Array(pairs) = result {
            assert_eq!(pairs.len(), 2);
            // jsonb sorts keys alphabetically
            assert_eq!(pairs[0], Value::Text("(e,\"\")".into()));
            assert_eq!(pairs[1], Value::Text("(n,)".into()));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn jsonb_each_text_composite_quoting_matches_pg() {
        // PG 17.7: SELECT jsonb_each_text('{"a":"", "b":"\"\"", "c":"hello\"world"}'::jsonb);
        //   (a,"")
        //   (b,"""""")
        //   (c,"hello""world")
        let result = jsonb_each_text(vec![Value::Jsonb(
            r#"{"a":"", "b":"\"\"", "c":"hello\"world"}"#.into(),
        )])
        .unwrap();
        if let Value::Array(pairs) = result {
            assert_eq!(pairs.len(), 3);
            assert_eq!(pairs[0], Value::Text("(a,\"\")".into()));
            // b = two literal double-quotes → doubled inside composite quotes: """"""
            assert_eq!(pairs[1], Value::Text("(b,\"\"\"\"\"\")".into()));
            // c = hello"world → "hello""world"
            assert_eq!(pairs[2], Value::Text("(c,\"hello\"\"world\")".into()));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn jsonb_each_text_special_chars_quoting() {
        // Values with spaces, commas, parens need quoting; backslash is doubled
        let result = jsonb_each_text(vec![Value::Jsonb(
            r#"{"a":"has space","b":"has,comma","c":"has(paren)","d":"has\\backslash"}"#.into(),
        )])
        .unwrap();
        if let Value::Array(pairs) = result {
            assert_eq!(pairs[0], Value::Text("(a,\"has space\")".into()));
            assert_eq!(pairs[1], Value::Text("(b,\"has,comma\")".into()));
            assert_eq!(pairs[2], Value::Text("(c,\"has(paren)\")".into()));
            assert_eq!(pairs[3], Value::Text("(d,\"has\\\\backslash\")".into()));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn jsonb_each_text_key_quoting() {
        // Keys with special characters need composite quoting too
        let result = jsonb_each_text(vec![Value::Jsonb(
            r#"{"has space":"val","has\"quote":"val"}"#.into(),
        )])
        .unwrap();
        if let Value::Array(pairs) = result {
            assert_eq!(pairs[0], Value::Text("(\"has space\",val)".into()));
            assert_eq!(pairs[1], Value::Text("(\"has\"\"quote\",val)".into()));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn test_json_each_array_error_pg_parity() {
        let err = json_each(vec![Value::Json("[1]".into())]).unwrap_err();
        assert_eq!(err.to_string(), "cannot deconstruct an array as an object");
    }

    #[test]
    fn test_json_each_scalar_error_pg_parity() {
        let err = json_each(vec![Value::Json("1".into())]).unwrap_err();
        assert_eq!(err.to_string(), "cannot deconstruct a scalar");
    }

    #[test]
    fn test_json_each_text_array_error_pg_parity() {
        let err = json_each_text(vec![Value::Json("[1]".into())]).unwrap_err();
        assert_eq!(err.to_string(), "cannot deconstruct an array as an object");
    }

    #[test]
    fn test_json_each_text_scalar_error_pg_parity() {
        let err = json_each_text(vec![Value::Json("1".into())]).unwrap_err();
        assert_eq!(err.to_string(), "cannot deconstruct a scalar");
    }

    #[test]
    fn test_jsonb_each_array_error_pg_parity() {
        let err = jsonb_each(vec![Value::Jsonb("[1]".into())]).unwrap_err();
        assert_eq!(err.to_string(), "cannot call jsonb_each on a non-object");
    }

    #[test]
    fn test_jsonb_each_scalar_error_pg_parity() {
        let err = jsonb_each(vec![Value::Jsonb("1".into())]).unwrap_err();
        assert_eq!(err.to_string(), "cannot call jsonb_each on a non-object");
    }

    #[test]
    fn test_jsonb_each_text_array_error_pg_parity() {
        let err = jsonb_each_text(vec![Value::Jsonb("[1]".into())]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot call jsonb_each_text on a non-object"
        );
    }

    #[test]
    fn test_jsonb_each_text_scalar_error_pg_parity() {
        let err = jsonb_each_text(vec![Value::Jsonb("1".into())]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot call jsonb_each_text on a non-object"
        );
    }

    #[test]
    fn test_jsonb_each_non_object_error_uses_correct_name() {
        let err = jsonb_each(vec![Value::Jsonb("[1,2,3]".into())])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "cannot call jsonb_each on a non-object");
    }

    #[test]
    fn test_jsonb_each_text_non_object_error_uses_correct_name() {
        let err = jsonb_each_text(vec![Value::Jsonb("[1,2,3]".into())])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "cannot call jsonb_each_text on a non-object");
    }

    #[test]
    fn test_json_each_non_object_error_matches_pg() {
        let err = json_each(vec![Value::Json("[1,2,3]".into())])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "cannot deconstruct an array as an object");
    }

    #[test]
    fn test_json_each_text_non_object_error_matches_pg() {
        let err = json_each_text(vec![Value::Json("[1,2,3]".into())])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "cannot deconstruct an array as an object");
    }

    #[test]
    fn test_jsonb_each_scalar_error_matches_pg() {
        let err = jsonb_each(vec![Value::Jsonb("1".into())])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "cannot call jsonb_each on a non-object");
    }

    #[test]
    fn test_jsonb_each_text_scalar_error_matches_pg() {
        let err = jsonb_each_text(vec![Value::Jsonb("1".into())])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "cannot call jsonb_each_text on a non-object");
    }

    #[test]
    fn test_json_each_scalar_error_matches_pg() {
        let err = json_each(vec![Value::Json("1".into())])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "cannot deconstruct a scalar");
    }

    #[test]
    fn test_json_each_text_scalar_error_matches_pg() {
        let err = json_each_text(vec![Value::Json("1".into())])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "cannot deconstruct a scalar");
    }
}
