use crate::types::Value;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("ARRAY_LENGTH", array_length);
    map.insert("ARRAY_UPPER", array_upper);
    map.insert("ARRAY_LOWER", array_lower);
    map.insert("CARDINALITY", cardinality);
    map.insert("ARRAY_POSITION", array_position);
    map.insert("ARRAY_CAT", array_cat);
    map.insert("ARRAY_APPEND", array_append);
    map.insert("ARRAY_PREPEND", array_prepend);
    map.insert("ARRAY_REMOVE", array_remove);
    map.insert("ARRAY_TO_STRING", array_to_string);
    map.insert("STRING_TO_ARRAY", string_to_array);
    map.insert("UNNEST", unnest);
}

pub fn array_length(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let dim = match iter.next() {
        Some(Value::Int32(d)) => d,
        Some(Value::Int64(d)) => d as i32,
        _ => 1,
    };
    if dim == 1 {
        Ok(Value::Int32(arr.len() as i32))
    } else {
        Ok(Value::Null)
    }
}

pub fn array_upper(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let dim = match iter.next() {
        Some(Value::Int32(d)) => d,
        Some(Value::Int64(d)) => d as i32,
        _ => 1,
    };
    if dim == 1 && !arr.is_empty() {
        Ok(Value::Int32(arr.len() as i32))
    } else {
        Ok(Value::Null)
    }
}

pub fn array_lower(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let dim = match iter.next() {
        Some(Value::Int32(d)) => d,
        Some(Value::Int64(d)) => d as i32,
        _ => 1,
    };
    if dim == 1 && !arr.is_empty() {
        Ok(Value::Int32(1))
    } else {
        Ok(Value::Null)
    }
}

pub fn cardinality(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Array(a)) => Ok(Value::Int32(a.len() as i32)),
        Some(Value::Null) => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

pub fn array_position(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let elem = match iter.next() {
        Some(v) => v,
        None => return Ok(Value::Null),
    };
    for (i, v) in arr.iter().enumerate() {
        if crate::sql::expr::compare_values(v, &elem)? == 0 {
            return Ok(Value::Int32((i + 1) as i32));
        }
    }
    Ok(Value::Null)
}

pub fn array_cat(args: Vec<Value>) -> Result<Value> {
    let mut result = Vec::new();
    for arg in args {
        match arg {
            Value::Array(a) => result.extend(a),
            Value::Null => {}
            v => result.push(v),
        }
    }
    Ok(Value::Array(result))
}

pub fn array_append(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let mut arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => Vec::new(),
        _ => return Err(anyhow!("ARRAY_APPEND requires array as first argument")),
    };
    if let Some(elem) = iter.next() {
        arr.push(elem);
    }
    Ok(Value::Array(arr))
}

pub fn array_prepend(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let elem = iter.next().unwrap_or(Value::Null);
    let mut arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => Vec::new(),
        _ => return Err(anyhow!("ARRAY_PREPEND requires array as second argument")),
    };
    arr.insert(0, elem);
    Ok(Value::Array(arr))
}

pub fn array_remove(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let elem = match iter.next() {
        Some(v) => v,
        None => return Ok(Value::Array(arr)),
    };
    let mut result = Vec::new();
    for v in arr {
        if crate::sql::expr::compare_values(&v, &elem)? != 0 {
            result.push(v);
        }
    }
    Ok(Value::Array(result))
}

pub fn array_to_string(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let arr = match iter.next() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let delimiter = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(v) => v.to_string(),
        None => ",".to_string(),
    };
    let null_str = iter.next().and_then(|v| match v {
        Value::Text(s) => Some(s),
        Value::Null => None,
        v => Some(v.to_string()),
    });
    let parts: Vec<String> = arr
        .into_iter()
        .filter_map(|v| match v {
            Value::Null => null_str.clone(),
            v => Some(v.to_string()),
        })
        .collect();
    Ok(Value::Text(parts.join(&delimiter)))
}

pub fn string_to_array(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let text = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let delimiter = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => {
            return Ok(Value::Array(
                text.chars().map(|c| Value::Text(c.to_string())).collect(),
            ))
        }
        Some(v) => v.to_string(),
        None => return Ok(Value::Array(vec![Value::Text(text)])),
    };
    let null_str = iter.next().and_then(|v| match v {
        Value::Text(s) => Some(s),
        Value::Null => None,
        v => Some(v.to_string()),
    });
    let parts: Vec<Value> = if delimiter.is_empty() {
        text.chars().map(|c| Value::Text(c.to_string())).collect()
    } else {
        text.split(&delimiter)
            .map(|s| {
                if null_str.as_ref().map_or(false, |ns| s == ns) {
                    Value::Null
                } else {
                    Value::Text(s.to_string())
                }
            })
            .collect()
    };
    Ok(Value::Array(parts))
}

pub fn unnest(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Array(arr)) => Ok(Value::Array(arr)),
        Some(Value::Null) | None => Ok(Value::Null),
        Some(v) => Ok(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_array_length() {
        assert_eq!(
            array_length(vec![
                Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]),
                Value::Int32(1)
            ])
            .unwrap(),
            Value::Int32(3)
        );
    }

    #[test]
    fn test_cardinality() {
        assert_eq!(
            cardinality(vec![Value::Array(vec![
                Value::Int32(1),
                Value::Int32(2),
                Value::Int32(3),
                Value::Int32(4)
            ])])
            .unwrap(),
            Value::Int32(4)
        );
    }

    #[test]
    fn test_array_append() {
        assert_eq!(
            array_append(vec![
                Value::Array(vec![Value::Int32(1), Value::Int32(2)]),
                Value::Int32(3)
            ])
            .unwrap(),
            Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)])
        );
    }

    #[test]
    fn test_array_prepend() {
        assert_eq!(
            array_prepend(vec![
                Value::Int32(0),
                Value::Array(vec![Value::Int32(1), Value::Int32(2)])
            ])
            .unwrap(),
            Value::Array(vec![Value::Int32(0), Value::Int32(1), Value::Int32(2)])
        );
    }

    #[test]
    fn test_array_to_string() {
        assert_eq!(
            array_to_string(vec![
                Value::Array(vec![
                    Value::Text("a".into()),
                    Value::Text("b".into()),
                    Value::Text("c".into())
                ]),
                Value::Text(",".into())
            ])
            .unwrap(),
            Value::Text("a,b,c".into())
        );
    }

    #[test]
    fn test_string_to_array() {
        assert_eq!(
            string_to_array(vec![Value::Text("a,b,c".into()), Value::Text(",".into())]).unwrap(),
            Value::Array(vec![
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Text("c".into())
            ])
        );
    }
}
