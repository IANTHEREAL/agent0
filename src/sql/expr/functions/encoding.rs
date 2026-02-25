use crate::model::Value;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("ENCODE", encode);
    map.insert("DECODE", decode);
    map.insert("MD5", md5);
}

pub fn encode(args: Vec<Value>) -> Result<Value> {
    use base64::Engine;

    if args.len() != 2 {
        return Err(anyhow!("encode requires exactly 2 arguments"));
    }

    let mut iter = args.into_iter();
    let data = match iter.next().unwrap_or(Value::Null) {
        Value::Bytes(b) => b,
        Value::Text(s) => s.into_bytes(),
        Value::Null => return Ok(Value::Null),
        v => v.to_string().into_bytes(),
    };
    let fmt = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let fmt = fmt.trim();

    if fmt.eq_ignore_ascii_case("base64") {
        return Ok(Value::Text(
            base64::engine::general_purpose::STANDARD.encode(&data),
        ));
    }
    if fmt.eq_ignore_ascii_case("hex") {
        return Ok(Value::Text(hex::encode(&data)));
    }
    if fmt.eq_ignore_ascii_case("escape") {
        return Ok(Value::Text(crate::sql::bytea::encode_escape(&data)));
    }

    Err(anyhow!("unrecognized encoding: {}", fmt))
}

pub fn decode(args: Vec<Value>) -> Result<Value> {
    use base64::Engine;

    if args.len() != 2 {
        return Err(anyhow!("decode requires exactly 2 arguments"));
    }

    let mut iter = args.into_iter();
    let data = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let fmt = match iter.next().unwrap_or(Value::Null) {
        Value::Text(s) => s,
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let fmt = fmt.trim();

    if fmt.eq_ignore_ascii_case("base64") {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.as_bytes())
            .map_err(|e| anyhow!("invalid base64 data: {}", e))?;
        return Ok(Value::Bytes(bytes));
    }
    if fmt.eq_ignore_ascii_case("hex") {
        let s = data.trim();
        let s = s.strip_prefix("\\x").unwrap_or(s);
        let bytes = hex::decode(s).map_err(|e| anyhow!("invalid hex data: {}", e))?;
        return Ok(Value::Bytes(bytes));
    }
    if fmt.eq_ignore_ascii_case("escape") {
        return Ok(Value::Bytes(crate::sql::bytea::decode_escape(&data)?));
    }

    Err(anyhow!("unrecognized encoding: {}", fmt))
}

pub fn md5(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(format!("{:x}", md5::compute(s.as_bytes())))),
        Some(Value::Bytes(b)) => Ok(Value::Text(format!("{:x}", md5::compute(&b)))),
        Some(Value::Null) => Ok(Value::Null),
        None => Ok(Value::Null),
        Some(v) => Ok(Value::Text(format!(
            "{:x}",
            md5::compute(v.to_string().as_bytes())
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_base64() {
        let result = encode(vec![
            Value::Text("hello".into()),
            Value::Text("base64".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Text("aGVsbG8=".into()));
    }

    #[test]
    fn test_encode_hex() {
        let result = encode(vec![
            Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            Value::Text("hex".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Text("deadbeef".into()));
    }

    #[test]
    fn test_decode_base64() {
        let result = decode(vec![
            Value::Text("aGVsbG8=".into()),
            Value::Text("base64".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Bytes(b"hello".to_vec()));
    }

    #[test]
    fn test_decode_hex() {
        let result = decode(vec![
            Value::Text("deadbeef".into()),
            Value::Text("hex".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    #[test]
    fn test_md5() {
        let result = md5(vec![Value::Text("hello".into())]).unwrap();
        assert_eq!(
            result,
            Value::Text("5d41402abc4b2a76b9719d911017c592".into())
        );
    }
}
