use crate::types::Value;
use anyhow::Result;
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("COALESCE", coalesce);
    map.insert("NULLIF", nullif);
    map.insert("GREATEST", greatest);
    map.insert("LEAST", least);
}

pub fn coalesce(args: Vec<Value>) -> Result<Value> {
    for val in args {
        if !matches!(val, Value::Null) {
            return Ok(val);
        }
    }
    Ok(Value::Null)
}

pub fn nullif(args: Vec<Value>) -> Result<Value> {
    if args.len() >= 2 && crate::sql::expr::compare_values(&args[0], &args[1]).unwrap_or(1) == 0 {
        Ok(Value::Null)
    } else {
        Ok(args.into_iter().next().unwrap_or(Value::Null))
    }
}

pub fn greatest(args: Vec<Value>) -> Result<Value> {
    let mut max = Value::Null;
    for val in args {
        if matches!(max, Value::Null) {
            max = val;
        } else if crate::sql::expr::compare_values(&val, &max).unwrap_or(0) > 0 {
            max = val;
        }
    }
    Ok(max)
}

pub fn least(args: Vec<Value>) -> Result<Value> {
    let mut min = Value::Null;
    for val in args {
        if matches!(min, Value::Null) {
            min = val;
        } else if crate::sql::expr::compare_values(&val, &min).unwrap_or(0) < 0 {
            min = val;
        }
    }
    Ok(min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coalesce() {
        assert_eq!(
            coalesce(vec![Value::Null, Value::Int32(1), Value::Int32(2)]).unwrap(),
            Value::Int32(1)
        );
        assert_eq!(
            coalesce(vec![Value::Null, Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            coalesce(vec![Value::Text("hello".into())]).unwrap(),
            Value::Text("hello".into())
        );
    }

    #[test]
    fn test_nullif() {
        assert_eq!(
            nullif(vec![Value::Int32(5), Value::Int32(5)]).unwrap(),
            Value::Null
        );
        assert_eq!(
            nullif(vec![Value::Int32(5), Value::Int32(3)]).unwrap(),
            Value::Int32(5)
        );
    }

    #[test]
    fn test_greatest() {
        assert_eq!(
            greatest(vec![Value::Int32(1), Value::Int32(5), Value::Int32(3)]).unwrap(),
            Value::Int32(5)
        );
    }

    #[test]
    fn test_least() {
        assert_eq!(
            least(vec![Value::Int32(1), Value::Int32(5), Value::Int32(3)]).unwrap(),
            Value::Int32(1)
        );
    }
}
