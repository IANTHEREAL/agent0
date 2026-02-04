use crate::types::Value;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("JSONB_ARRAY_LENGTH", jsonb_array_length);
    map.insert("JSON_ARRAY_LENGTH", jsonb_array_length);
    map.insert("JSONB_TYPEOF", jsonb_typeof);
    map.insert("JSON_TYPEOF", jsonb_typeof);
    map.insert("JSONB_BUILD_OBJECT", jsonb_build_object);
    map.insert("JSON_BUILD_OBJECT", jsonb_build_object);
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
    map.insert("JSONB_SET", jsonb_set);
    map.insert("JSON_SET", jsonb_set);
    map.insert("JSONB_ARRAY_ELEMENTS", jsonb_array_elements);
    map.insert("JSON_ARRAY_ELEMENTS", jsonb_array_elements);
    map.insert("JSONB_ARRAY_ELEMENTS_TEXT", jsonb_array_elements_text);
    map.insert("JSON_ARRAY_ELEMENTS_TEXT", jsonb_array_elements_text);
    map.insert("JSONB_EACH", jsonb_each);
    map.insert("JSON_EACH", jsonb_each);
    map.insert("JSONB_EACH_TEXT", jsonb_each_text);
    map.insert("JSON_EACH_TEXT", jsonb_each_text);
}

fn value_to_json(val: &Value) -> serde_json::Value {
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
            crate::types::date::format_date_days(*days).unwrap_or_else(|_| days.to_string()),
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
    let mut obj = serde_json::Map::new();
    let mut iter = args.into_iter();
    while let Some(key) = iter.next() {
        let key_str = match key {
            Value::Text(s) => s,
            Value::Null => "null".to_string(),
            v => v.to_string(),
        };
        let val = iter.next().unwrap_or(Value::Null);
        let json_val = value_to_json(&val);
        obj.insert(key_str, json_val);
    }
    Ok(Value::Jsonb(serde_json::Value::Object(obj).to_string()))
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

pub fn jsonb_set(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let json_str = match iter.next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("jsonb_set requires json/jsonb as first argument")),
    };
    let path = match iter.next() {
        Some(Value::Array(arr)) => arr,
        Some(Value::Text(s)) => {
            let trimmed = s.trim().trim_start_matches('{').trim_end_matches('}');
            trimmed
                .split(',')
                .map(|p| Value::Text(p.trim().to_string()))
                .collect()
        }
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
    ) -> bool {
        if path.is_empty() {
            *val = new_val;
            return true;
        }
        let key = match &path[0] {
            Value::Text(s) => s.clone(),
            v => v.to_string(),
        };
        match val {
            serde_json::Value::Object(obj) => {
                if path.len() == 1 {
                    if create_missing || obj.contains_key(&key) {
                        obj.insert(key, new_val);
                        return true;
                    }
                } else if let Some(child) = obj.get_mut(&key) {
                    return set_at_path(child, &path[1..], new_val, create_missing);
                } else if create_missing {
                    let mut child = serde_json::Value::Object(serde_json::Map::new());
                    if set_at_path(&mut child, &path[1..], new_val, create_missing) {
                        obj.insert(key, child);
                        return true;
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                if let Ok(idx) = key.parse::<usize>() {
                    if path.len() == 1 {
                        if idx < arr.len() {
                            arr[idx] = new_val;
                            return true;
                        } else if create_missing {
                            while arr.len() <= idx {
                                arr.push(serde_json::Value::Null);
                            }
                            arr[idx] = new_val;
                            return true;
                        }
                    } else if idx < arr.len() {
                        return set_at_path(&mut arr[idx], &path[1..], new_val, create_missing);
                    }
                }
            }
            _ => {}
        }
        false
    }

    set_at_path(&mut json_val, &path, new_value, create_missing);
    Ok(Value::Jsonb(json_val.to_string()))
}

pub fn jsonb_array_elements(args: Vec<Value>) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("jsonb_array_elements requires json/jsonb argument")),
    };
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    match json_val {
        serde_json::Value::Array(arr) => {
            let elements: Vec<Value> = arr
                .into_iter()
                .map(|v| Value::Jsonb(v.to_string()))
                .collect();
            Ok(Value::Array(elements))
        }
        _ => Err(anyhow!("cannot extract elements from a non-array")),
    }
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

pub fn jsonb_each(args: Vec<Value>) -> Result<Value> {
    jsonb_each_impl(args, false)
}

pub fn jsonb_each_text(args: Vec<Value>) -> Result<Value> {
    jsonb_each_impl(args, true)
}

fn jsonb_each_impl(args: Vec<Value>, is_text: bool) -> Result<Value> {
    let json_str = match args.into_iter().next() {
        Some(Value::Text(s)) | Some(Value::Json(s)) | Some(Value::Jsonb(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Err(anyhow!("jsonb_each requires json/jsonb argument")),
    };
    let json_val: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| anyhow!("Invalid JSON: {}", e))?;
    match json_val {
        serde_json::Value::Object(obj) => {
            let pairs: Vec<Value> = obj
                .into_iter()
                .map(|(k, v)| {
                    let val_str = if is_text {
                        match v {
                            serde_json::Value::String(s) => s,
                            serde_json::Value::Null => "".to_string(),
                            other => other.to_string(),
                        }
                    } else {
                        v.to_string()
                    };
                    Value::Text(format!("({},{})", k, val_str))
                })
                .collect();
            Ok(Value::Array(pairs))
        }
        _ => Err(anyhow!("cannot call jsonb_each on a non-object")),
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
}
