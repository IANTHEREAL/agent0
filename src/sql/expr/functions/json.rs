use crate::model::Value;
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("JSONB_ARRAY_LENGTH", jsonb_array_length);
    map.insert("JSON_ARRAY_LENGTH", jsonb_array_length);
    map.insert("JSONB_TYPEOF", jsonb_typeof);
    map.insert("JSON_TYPEOF", jsonb_typeof);
    map.insert("JSONB_BUILD_OBJECT", jsonb_build_object);
    map.insert("JSON_BUILD_OBJECT", json_build_object);
    map.insert("JSONB_BUILD_ARRAY", jsonb_build_array);
    map.insert("JSON_BUILD_ARRAY", jsonb_build_array);
    map.insert("JSONB_EXISTS", jsonb_exists);
    map.insert("JSONB_EXISTS_ANY", jsonb_exists_any);
    map.insert("JSONB_EXISTS_ALL", jsonb_exists_all);
    map.insert("JSONB_OBJECT_KEYS", jsonb_object_keys);
    map.insert("JSON_OBJECT_KEYS", jsonb_object_keys);
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
    map.insert("JSON_ARRAY_ELEMENTS_TEXT", jsonb_array_elements_text);
    map.insert("JSONB_EACH", jsonb_each);
    map.insert("JSON_EACH", json_each);
    map.insert("JSONB_EACH_TEXT", jsonb_each_text);
    map.insert("JSON_EACH_TEXT", json_each_text);
}

pub(crate) fn value_to_json(val: &Value) -> serde_json::Value {
    match val {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Int32(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
        Value::Int64(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
        Value::Float64(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Text(s) => serde_json::Value::String(s.clone()),
        Value::Json(s) | Value::Jsonb(s) => {
            serde_json::from_str(s).unwrap_or(serde_json::Value::String(s.clone()))
        }
        Value::Array(arr) => serde_json::Value::Array(arr.iter().map(value_to_json).collect()),
        Value::Timestamp(ts) => serde_json::Value::Number(serde_json::Number::from(*ts)),
        Value::Uuid(bytes) => serde_json::Value::String(uuid::Uuid::from_bytes(*bytes).to_string()),
        Value::Bytes(b) => serde_json::Value::String(format!("\\x{}", hex::encode(b))),
        Value::Interval(iv) => serde_json::Value::String(iv.to_string()),
        Value::Vector(vec) => serde_json::Value::Array(
            vec.iter()
                .filter_map(|f| serde_json::Number::from_f64(*f))
                .map(serde_json::Value::Number)
                .collect(),
        ),
        Value::Time(micros) => serde_json::Value::Number(serde_json::Number::from(*micros)),
        Value::Date(days) => serde_json::Value::String(
            crate::model::date::format_date_days(*days).unwrap_or_else(|_| days.to_string()),
        ),
        Value::Numeric(d) => {
            let s = d.to_string();
            if let Ok(i) = s.parse::<i64>() {
                return serde_json::Value::Number(serde_json::Number::from(i));
            }
            if let Ok(u) = s.parse::<u64>() {
                return serde_json::Value::Number(serde_json::Number::from(u));
            }
            if let Ok(f) = s.parse::<f64>() {
                if let Some(n) = serde_json::Number::from_f64(f) {
                    return serde_json::Value::Number(n);
                }
            }
            serde_json::Value::String(s)
        }
        Value::Tsvector(s) | Value::Tsquery(s) => serde_json::Value::String(s.clone()),
    }
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

    let mut obj = serde_json::Map::new();
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
        let json_val = value_to_json(&chunk[1]);
        obj.insert(key_str, json_val);
    }
    Ok(Value::Jsonb(serde_json::Value::Object(obj).to_string()))
}

/// PostgreSQL `json_build_object` uses `" : "` separator (space before and after colon),
/// which differs from `jsonb_build_object`'s compact `": "` format.
pub fn json_build_object(args: Vec<Value>) -> Result<Value> {
    // PostgreSQL requires even number of arguments
    if !args.len().is_multiple_of(2) {
        return Err(SqlError::InvalidParameterValue {
            message: "argument list must have even number of elements".into(),
        }
        .into());
    }

    let mut obj = serde_json::Map::new();
    let mut iter = args.into_iter();
    while let Some(key) = iter.next() {
        // PostgreSQL errors on NULL keys
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
        let json_val = value_to_json(&val);
        obj.insert(key_str, json_val);
    }
    // PostgreSQL json type uses " : " separator
    let pairs: Vec<String> = obj
        .iter()
        .map(|(k, v)| format!("{} : {}", serde_json::Value::String(k.clone()), v))
        .collect();
    Ok(Value::Json(format!("{{{}}}", pairs.join(", "))))
}

pub fn jsonb_build_array(args: Vec<Value>) -> Result<Value> {
    let arr: Vec<serde_json::Value> = args.into_iter().map(|v| value_to_json(&v)).collect();
    Ok(Value::Jsonb(serde_json::Value::Array(arr).to_string()))
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

pub fn jsonb_extract_path(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let json_str = match iter.next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("json_extract_path requires json/jsonb argument")),
    };
    let mut json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    for path_part in iter {
        let key = match path_part {
            Value::Text(s) => s,
            v => v.to_string(),
        };
        json_val = match json_val.get(&key) {
            Some(v) => v.clone(),
            None => return Ok(Value::Null),
        };
    }
    Ok(Value::Jsonb(json_val.to_string()))
}

pub fn jsonb_extract_path_text(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let json_str = match iter.next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => {
            return Err(anyhow!(
                "json_extract_path_text requires json/jsonb argument"
            ))
        }
    };
    let mut json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    for path_part in iter {
        let key = match path_part {
            Value::Text(s) => s,
            v => v.to_string(),
        };
        json_val = match json_val.get(&key) {
            Some(v) => v.clone(),
            None => return Ok(Value::Null),
        };
    }
    match json_val {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::String(s) => Ok(Value::Text(s)),
        other => Ok(Value::Text(other.to_string())),
    }
}

pub fn jsonb_pretty(args: Vec<Value>) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("jsonb_pretty requires json/jsonb argument")),
    };
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    let pretty = serde_json::to_string_pretty(&json_val)
        .map_err(|e| anyhow!("Failed to format JSON: {}", e))?;
    Ok(Value::Text(pretty))
}

pub fn to_json(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    let json_val = value_to_json(&val);
    Ok(Value::Json(json_val.to_string()))
}

pub fn to_jsonb(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    let json_val = value_to_json(&val);
    Ok(Value::Jsonb(json_val.to_string()))
}

pub fn row_to_json(args: Vec<Value>) -> Result<Value> {
    let Some(val) = args.into_iter().next() else {
        return Ok(Value::Null);
    };
    match val {
        Value::Null => Ok(Value::Null),
        Value::Array(arr) => {
            let mut obj = serde_json::Map::new();
            for (i, v) in arr.iter().enumerate() {
                obj.insert(format!("f{}", i + 1), value_to_json(v));
            }
            Ok(Value::Json(serde_json::Value::Object(obj).to_string()))
        }
        other => Ok(Value::Json(value_to_json(&other).to_string())),
    }
}

fn parse_jsonb_set_text_path(path: &str) -> Result<Vec<Value>> {
    let mut path = path.trim();
    if path.len() >= 2 && path.starts_with('\'') && path.ends_with('\'') {
        path = &path[1..path.len() - 1];
    }
    if !path.starts_with('{') || !path.ends_with('}') {
        return Err(anyhow!("invalid text[] path literal"));
    }

    let inner = &path[1..path.len() - 1];
    if inner.is_empty() {
        return Ok(Vec::new());
    }

    let mut elements = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escape_next = false;

    for ch in inner.chars() {
        if escape_next {
            current.push(ch);
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => {
                current.push(ch);
                escape_next = true;
            }
            '"' => {
                current.push(ch);
                in_quotes = !in_quotes;
            }
            ',' if !in_quotes => {
                elements.push(current);
                current = String::new();
            }
            _ => current.push(ch),
        }
    }
    if in_quotes {
        return Err(anyhow!("invalid text[] path literal"));
    }
    elements.push(current);

    let mut path_elems = Vec::with_capacity(elements.len());
    for element in elements {
        let element = element.trim();
        if element.len() >= 2 && element.starts_with('"') && element.ends_with('"') {
            path_elems.push(Value::Text(
                element[1..element.len() - 1].replace("\\\"", "\""),
            ));
        } else if element.eq_ignore_ascii_case("NULL") {
            path_elems.push(Value::Null);
        } else {
            path_elems.push(Value::Text(element.to_string()));
        }
    }
    Ok(path_elems)
}

pub fn jsonb_set(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let json_str = match iter.next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("jsonb_set requires json/jsonb as first argument")),
    };
    let path = match iter.next() {
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
        Some(Value::Null) => serde_json::Value::Null,
        Some(v) => serde_json::Value::String(v.to_string()),
        None => return Err(anyhow!("jsonb_set requires new value as third argument")),
    };
    let create_missing = match iter.next() {
        Some(Value::Boolean(b)) => b,
        _ => true,
    };

    let mut json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;

    fn set_at_path(
        val: &mut serde_json::Value,
        path: &[Value],
        new_val: serde_json::Value,
        create_missing: bool,
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
                    return set_at_path(child, &path[1..], new_val, create_missing);
                }
                // Intermediate step missing: PostgreSQL returns original value unchanged
                // (create_missing only applies to the final step)
            }
            serde_json::Value::Array(arr) => {
                let raw_idx = key
                    .parse::<isize>()
                    .map_err(|_| SqlError::InvalidInputSyntax {
                        type_name: "integer".into(),
                        value: key.clone(),
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
                        return set_at_path(&mut arr[i], &path[1..], new_val, create_missing);
                    }
                    // Array index out of bounds for intermediate step: return unchanged
                }
            }
            _ => {}
        }
        Ok(false)
    }

    // Validate path elements: NULL is not allowed
    for (i, v) in path.iter().enumerate() {
        if matches!(v, Value::Null) {
            return Err(SqlError::NullValueNotAllowed {
                message: format!("path element at position {} is null", i + 1),
            }
            .into());
        }
    }

    // PG 17 empty-path semantics:
    //   - scalar target → error
    //   - object/array target → return unchanged (no-op)
    if path.is_empty() {
        if matches!(
            json_val,
            serde_json::Value::Null
                | serde_json::Value::Bool(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::String(_)
        ) {
            return Err(SqlError::InvalidParameterValue {
                message: "cannot set path in scalar".into(),
            }
            .into());
        }
        return Ok(Value::Jsonb(json_str));
    }

    // Store original value in case path cannot be set
    let original_json = json_val.clone();
    if set_at_path(&mut json_val, &path, new_value, create_missing)? {
        Ok(Value::Jsonb(json_val.to_string()))
    } else {
        // Path couldn't be set (intermediate steps missing), return original
        Ok(Value::Jsonb(original_json.to_string()))
    }
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
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("{} requires json/jsonb argument", func_name)),
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
        _ => Err(anyhow!("cannot extract elements from a non-array")),
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
                    elements.push(chars[start..i].iter().collect());
                }
                break;
            }
            continue;
        }
        if c == ',' && depth == 1 {
            // Element separator at top level of array
            if i > start {
                elements.push(chars[start..i].iter().collect());
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
        _ => Err(anyhow!("cannot extract elements from a non-array")),
    }
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
    fn test_jsonb_build_array() {
        let result =
            jsonb_build_array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]).unwrap();
        assert_eq!(result, Value::Jsonb("[1,2,3]".into()));
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
        let result = jsonb_pretty(vec![Value::Jsonb("{\"a\":1}".into())]).unwrap();
        if let Value::Text(s) = result {
            assert!(s.contains('\n'));
            assert!(s.contains("\"a\""));
        } else {
            panic!("Expected Text value");
        }
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
    }

    #[test]
    fn test_jsonb_set_text_path_null_rejected() {
        let err = jsonb_set(vec![
            Value::Jsonb("{\"a\":1}".into()),
            Value::Text("{NULL}".into()),
            Value::Jsonb("42".into()),
        ])
        .unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("expected SqlError for NULL path element");
        assert!(matches!(sql_err, SqlError::NullValueNotAllowed { .. }));
        assert_eq!(sql_err.sqlstate(), "22004");
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
    fn test_jsonb_set_array_path_sql_null_rejected() {
        let err = jsonb_set(vec![
            Value::Jsonb("{\"a\":1}".into()),
            Value::Array(vec![Value::Null]),
            Value::Jsonb("42".into()),
        ])
        .unwrap_err();
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("expected SqlError for NULL path element");
        assert!(matches!(sql_err, SqlError::NullValueNotAllowed { .. }));
        assert_eq!(sql_err.sqlstate(), "22004");
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
        assert!(matches!(sql_err, SqlError::InvalidInputSyntax { .. }));
        assert_eq!(sql_err.sqlstate(), "22P02");
    }

    #[test]
    fn test_jsonb_set_array_path_negative_index() {
        let result = jsonb_set(vec![
            Value::Jsonb("[1,2,3]".into()),
            Value::Array(vec![Value::Text("-1".into())]),
            Value::Jsonb("42".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Jsonb("[1,2,42]".into()));
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
        assert_eq!(result, Value::Jsonb("[1,2,42]".into()));
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
