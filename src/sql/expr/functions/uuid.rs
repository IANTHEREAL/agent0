use crate::types::Value;
use anyhow::Result;
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("GEN_RANDOM_UUID", gen_random_uuid);
    map.insert("UUID_GENERATE_V4", gen_random_uuid);
    map.insert("UUIDV7", uuidv7);
}

pub fn gen_random_uuid(_args: Vec<Value>) -> Result<Value> {
    let uuid = uuid::Uuid::new_v4();
    Ok(Value::Uuid(*uuid.as_bytes()))
}

pub fn uuidv7(_args: Vec<Value>) -> Result<Value> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let mut bytes = [0u8; 16];
    let random_uuid = uuid::Uuid::new_v4();
    bytes.copy_from_slice(random_uuid.as_bytes());

    let ts_bytes = timestamp_ms.to_be_bytes();
    bytes[0..6].copy_from_slice(&ts_bytes[2..8]);

    bytes[6] = (bytes[6] & 0x0F) | 0x70;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;

    Ok(Value::Uuid(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gen_random_uuid() {
        let result = gen_random_uuid(vec![]).unwrap();
        if let Value::Uuid(bytes) = result {
            assert_eq!(bytes.len(), 16);
            assert_eq!(bytes[6] >> 4, 4);
        } else {
            panic!("Expected Uuid value");
        }
    }

    #[test]
    fn test_uuidv7() {
        let result = uuidv7(vec![]).unwrap();
        if let Value::Uuid(bytes) = result {
            assert_eq!(bytes.len(), 16);
            assert_eq!(bytes[6] >> 4, 7);
            assert_eq!(bytes[8] >> 6, 2);
        } else {
            panic!("Expected Uuid value");
        }
    }
}
