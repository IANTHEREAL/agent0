use super::*;

pub fn int4send(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    match val {
        Value::Null => Ok(Value::Null),
        Value::Int32(n) => Ok(Value::Bytes(n.to_be_bytes().to_vec())),
        Value::Int64(n) => Ok(Value::Bytes((n as i32).to_be_bytes().to_vec())),
        other => {
            let n: i32 = other
                .to_string()
                .parse()
                .map_err(|_| anyhow::anyhow!("function int4send(integer) does not exist"))?;
            Ok(Value::Bytes(n.to_be_bytes().to_vec()))
        }
    }
}

pub fn int8send(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    match val {
        Value::Null => Ok(Value::Null),
        Value::Int64(n) => Ok(Value::Bytes(n.to_be_bytes().to_vec())),
        Value::Int32(n) => Ok(Value::Bytes((n as i64).to_be_bytes().to_vec())),
        other => {
            let n: i64 = other
                .to_string()
                .parse()
                .map_err(|_| anyhow::anyhow!("function int8send(bigint) does not exist"))?;
            Ok(Value::Bytes(n.to_be_bytes().to_vec()))
        }
    }
}

pub fn uuid_send(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    match val {
        Value::Null => Ok(Value::Null),
        Value::Uuid(bytes) => Ok(Value::Bytes(bytes.to_vec())),
        Value::Text(s) => {
            let u: uuid::Uuid = s.parse().map_err(|_| {
                anyhow::anyhow!(
                    "{}",
                    crate::sql::error::SqlError::InvalidInputSyntax {
                        type_name: "uuid".into(),
                        value: s,
                    }
                )
            })?;
            Ok(Value::Bytes(u.as_bytes().to_vec()))
        }
        _ => anyhow::bail!("function uuid_send(uuid) does not exist"),
    }
}

pub fn set_bit_bytea(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let bytes = match iter.next() {
        Some(Value::Bytes(b)) => b,
        Some(Value::Null) | None => return Ok(Value::Null),
        _ => anyhow::bail!("function set_bit(bytea, integer, integer) does not exist"),
    };
    let bit_n = match iter.next() {
        Some(Value::Int32(n)) => n as i64,
        Some(Value::Int64(n)) => n,
        _ => anyhow::bail!("function set_bit(bytea, integer, integer) does not exist"),
    };
    let new_val = match iter.next() {
        Some(Value::Int32(n)) => n,
        Some(Value::Int64(n)) => n as i32,
        _ => anyhow::bail!("function set_bit(bytea, integer, integer) does not exist"),
    };

    if new_val != 0 && new_val != 1 {
        anyhow::bail!("new bit must be 0 or 1");
    }

    let total_bits = bytes.len() as i64 * 8;
    if bit_n < 0 || bit_n >= total_bits {
        anyhow::bail!("index {} out of valid range, 0..{}", bit_n, total_bits - 1);
    }

    let mut result = bytes;
    let byte_idx = (bit_n / 8) as usize;
    let bit_idx = (bit_n % 8) as u32;
    if new_val == 1 {
        result[byte_idx] |= 1 << bit_idx;
    } else {
        result[byte_idx] &= !(1 << bit_idx);
    }
    Ok(Value::Bytes(result))
}

pub fn get_bit_bytea(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let bytes = match iter.next() {
        Some(Value::Bytes(b)) => b,
        Some(Value::Null) | None => return Ok(Value::Null),
        _ => anyhow::bail!("function get_bit(bytea, integer) does not exist"),
    };
    let bit_n = match iter.next() {
        Some(Value::Int32(n)) => n as i64,
        Some(Value::Int64(n)) => n,
        _ => anyhow::bail!("function get_bit(bytea, integer) does not exist"),
    };

    let total_bits = bytes.len() as i64 * 8;
    if bit_n < 0 || bit_n >= total_bits {
        anyhow::bail!("index {} out of valid range, 0..{}", bit_n, total_bits - 1);
    }

    let byte_idx = (bit_n / 8) as usize;
    let bit_idx = (bit_n % 8) as u32;
    let bit_val = (bytes[byte_idx] >> bit_idx) & 1;
    Ok(Value::Int32(bit_val as i32))
}
