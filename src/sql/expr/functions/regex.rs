use crate::types::Value;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("REGEXP_REPLACE", regexp_replace);
    map.insert("REGEXP_MATCHES", regexp_matches);
    map.insert("REGEXP_SPLIT_TO_ARRAY", regexp_split_to_array);
}

pub fn regexp_replace(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let source = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let pattern = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Text(source)),
    };
    let replacement = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => String::new(),
        Some(v) => v.to_string(),
        None => String::new(),
    };
    let flags = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => String::new(),
    };
    let global = flags.contains('g');
    let case_insensitive = flags.contains('i');
    let regex_pattern = if case_insensitive {
        format!("(?i){}", pattern)
    } else {
        pattern
    };
    match regex::Regex::new(&regex_pattern) {
        Ok(re) => {
            let result = if global {
                re.replace_all(&source, replacement.as_str()).to_string()
            } else {
                re.replace(&source, replacement.as_str()).to_string()
            };
            Ok(Value::Text(result))
        }
        Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
    }
}

pub fn regexp_matches(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let source = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let pattern = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Array(vec![])),
    };
    let flags = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => String::new(),
    };
    let case_insensitive = flags.contains('i');
    let regex_pattern = if case_insensitive {
        format!("(?i){}", pattern)
    } else {
        pattern
    };
    match regex::Regex::new(&regex_pattern) {
        Ok(re) => {
            if let Some(caps) = re.captures(&source) {
                let matches: Vec<Value> = caps
                    .iter()
                    .skip(if caps.len() > 1 { 1 } else { 0 })
                    .map(|m| match m {
                        Some(m) => Value::Text(m.as_str().to_string()),
                        None => Value::Null,
                    })
                    .collect();
                if matches.is_empty() {
                    if let Some(m) = caps.get(0) {
                        Ok(Value::Array(vec![Value::Text(m.as_str().to_string())]))
                    } else {
                        Ok(Value::Null)
                    }
                } else {
                    Ok(Value::Array(matches))
                }
            } else {
                Ok(Value::Null)
            }
        }
        Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
    }
}

pub fn regexp_split_to_array(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let source = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let pattern = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Array(vec![Value::Text(source)])),
        Some(v) => v.to_string(),
        None => return Ok(Value::Array(vec![Value::Text(source)])),
    };
    let flags = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => String::new(),
    };
    let case_insensitive = flags.contains('i');
    let regex_pattern = if case_insensitive {
        format!("(?i){}", pattern)
    } else {
        pattern
    };
    match regex::Regex::new(&regex_pattern) {
        Ok(re) => {
            let parts: Vec<Value> = re
                .split(&source)
                .map(|s| Value::Text(s.to_string()))
                .collect();
            Ok(Value::Array(parts))
        }
        Err(e) => Err(anyhow!("Invalid regex pattern: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regexp_replace() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("hello world".into()),
                Value::Text("world".into()),
                Value::Text("rust".into()),
            ])
            .unwrap(),
            Value::Text("hello rust".into())
        );
    }

    #[test]
    fn test_regexp_replace_global() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("aaa".into()),
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Text("g".into()),
            ])
            .unwrap(),
            Value::Text("bbb".into())
        );
    }

    #[test]
    fn test_regexp_replace_case_insensitive() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("Hello World".into()),
                Value::Text("world".into()),
                Value::Text("rust".into()),
                Value::Text("i".into()),
            ])
            .unwrap(),
            Value::Text("Hello rust".into())
        );
    }

    #[test]
    fn test_regexp_matches() {
        let result = regexp_matches(vec![
            Value::Text("hello123world".into()),
            Value::Text(r"(\d+)".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Array(vec![Value::Text("123".into())]));
    }

    #[test]
    fn test_regexp_split_to_array() {
        assert_eq!(
            regexp_split_to_array(vec![Value::Text("a,b,c".into()), Value::Text(",".into()),])
                .unwrap(),
            Value::Array(vec![
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Text("c".into()),
            ])
        );
    }
}
