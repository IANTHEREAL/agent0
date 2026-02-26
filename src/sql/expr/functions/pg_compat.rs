use crate::model::Value;
use crate::sql::pg_types;
use crate::sql::quoting;
use anyhow::Result;
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("PG_TYPEOF", pg_typeof);
    map.insert("PG_COLUMN_SIZE", pg_column_size);
    map.insert("FORMAT_TYPE", format_type);
    map.insert("PG_IS_IN_RECOVERY", pg_is_in_recovery);
    map.insert("PG_TABLE_IS_VISIBLE", pg_table_is_visible);
    map.insert("PG_TYPE_IS_VISIBLE", pg_type_is_visible);
    map.insert("CLOCK_TIMESTAMP", clock_timestamp);
    // STATEMENT_TIMESTAMP and TRANSACTION_TIMESTAMP are handled as special cases
    // in eval_function (expr/mod.rs) because they need access to QueryContext.
    map.insert("TXID_CURRENT", txid_current);
    map.insert("PG_ENCODING_TO_CHAR", pg_encoding_to_char);
    map.insert("OBJ_DESCRIPTION", obj_description);
    map.insert("COL_DESCRIPTION", obj_description);
    map.insert("SHOBJ_DESCRIPTION", obj_description);
    map.insert("PG_GET_SERIAL_SEQUENCE", pg_get_serial_sequence);
    map.insert("PG_GET_EXPR", pg_get_expr);
    map.insert("HAS_SCHEMA_PRIVILEGE", has_privilege);
    map.insert("HAS_TABLE_PRIVILEGE", has_privilege);
    map.insert("HAS_DATABASE_PRIVILEGE", has_privilege);
    // Binary send functions (bytea serialization)
    map.insert("INT4SEND", int4send);
    map.insert("INT8SEND", int8send);
    map.insert("UUID_SEND", uuid_send);
    // Bit manipulation on bytea
    map.insert("SET_BIT", set_bit_bytea);
    map.insert("GET_BIT", get_bit_bytea);
    map.insert("HASHTEXT", hashtext);
}

fn pg_typeof_name(val: &Value) -> String {
    match val {
        Value::Null => "unknown".to_string(),
        Value::Boolean(_) => "boolean".to_string(),
        Value::Int32(_) => "integer".to_string(),
        Value::Int64(_) => "bigint".to_string(),
        Value::Float64(_) => "double precision".to_string(),
        Value::Numeric(_) => "numeric".to_string(),
        Value::Text(_) => "text".to_string(),
        Value::Bytes(_) => "bytea".to_string(),
        Value::Timestamp(_) => "timestamp with time zone".to_string(),
        Value::Date(_) => "date".to_string(),
        Value::Time(_) => "time".to_string(),
        Value::Interval(_) => "interval".to_string(),
        Value::Uuid(_) => "uuid".to_string(),
        Value::Json(_) => "json".to_string(),
        Value::Jsonb(_) => "jsonb".to_string(),
        Value::Array(arr) => {
            let elem_type = arr
                .iter()
                .find(|v| !matches!(v, Value::Null))
                .map(pg_typeof_name)
                .unwrap_or_else(|| "unknown".to_string());
            format!("{}[]", elem_type)
        }
        Value::Vector(_) => "vector".to_string(),
        Value::Tsvector(_) => "tsvector".to_string(),
        Value::Tsquery(_) => "tsquery".to_string(),
    }
}

pub fn pg_typeof(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    Ok(Value::Text(pg_typeof_name(&val)))
}

pub fn pg_column_size(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    let size = match &val {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Int32(_) => 4,
        Value::Int64(_) => 8,
        Value::Float64(_) => 8,
        Value::Numeric(_) => 16,
        Value::Text(s) => s.len() as i32 + 4,
        Value::Bytes(b) => b.len() as i32 + 4,
        Value::Timestamp(_) => 8,
        Value::Date(_) => 4,
        Value::Time(_) => 8,
        Value::Interval(_) => 16,
        Value::Uuid(_) => 16,
        Value::Json(s) | Value::Jsonb(s) => s.len() as i32 + 4,
        Value::Array(a) => a.len() as i32 * 8 + 4,
        Value::Vector(v) => v.len() as i32 * 4 + 4,
        Value::Tsvector(s) | Value::Tsquery(s) => s.len() as i32 + 4,
    };
    Ok(Value::Int32(size))
}

pub fn format_type(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let oid = match iter.next().unwrap_or(Value::Null) {
        Value::Int32(n) => n as i64,
        Value::Int64(n) => n,
        Value::Text(s) => s.trim().parse::<i64>().map_err(|_| {
            anyhow::anyhow!(
                "{}",
                crate::sql::error::SqlError::InvalidInputSyntax {
                    type_name: "oid".into(),
                    value: s.trim().to_string(),
                }
            )
        })?,
        Value::Null => return Ok(Value::Null),
        _ => 0,
    };
    let typmod = match iter.next() {
        Some(Value::Int32(n)) => n as i64,
        Some(Value::Int64(n)) => n,
        _ => -1,
    };
    // Handle types that need typmod for canonical name
    let formatted = match oid {
        pg_types::OID_VARCHAR => {
            if typmod > 0 {
                format!("character varying({})", typmod - 4)
            } else {
                "character varying".to_string()
            }
        }
        pg_types::OID_BPCHAR => {
            if typmod > 0 {
                format!("character({})", typmod - 4)
            } else {
                "character".to_string()
            }
        }
        pg_types::OID_NUMERIC => {
            if typmod > 0 {
                let precision = ((typmod - 4) >> 16) & 0xffff;
                let scale = (typmod - 4) & 0xffff;
                format!("numeric({},{})", precision, scale)
            } else {
                "numeric".to_string()
            }
        }
        _ => pg_types::format_type_name_for_oid(oid)
            .unwrap_or("text")
            .to_string(),
    };
    Ok(Value::Text(formatted))
}

pub fn pg_is_in_recovery(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Boolean(false))
}

pub fn pg_table_is_visible(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(_) => Ok(Value::Boolean(true)),
    }
}

/// Check if a type is visible in the current search_path.
///
/// In db9-server, all types within the keyspace are visible, so this always
/// returns true for non-NULL inputs (similar to pg_table_is_visible).
///
/// PostgreSQL signature: pg_type_is_visible(type_oid oid) → boolean
pub fn pg_type_is_visible(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(_) => Ok(Value::Boolean(true)),
    }
}

pub fn clock_timestamp(_args: Vec<Value>) -> Result<Value> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    Ok(Value::Timestamp(ts))
}

pub fn txid_current(_args: Vec<Value>) -> Result<Value> {
    use std::time::{SystemTime, UNIX_EPOCH};
    Ok(Value::Int64(
        std::process::id() as i64 * 1000000
            + SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros() as i64
                % 1000000,
    ))
}

pub fn pg_encoding_to_char(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    match iter.next() {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(_) => Ok(Value::Text("UTF8".to_string())),
    }
}

pub fn obj_description(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Null)
}

pub fn pg_get_serial_sequence(_args: Vec<Value>) -> Result<Value> {
    fn parse_qname_token(token: &str) -> Option<(Option<String>, String)> {
        fn push_part(parts: &mut Vec<String>, raw: &str, quoted: bool) -> Option<()> {
            let trimmed = if quoted { raw } else { raw.trim() };
            if trimmed.is_empty() {
                return None;
            }
            if quoted {
                parts.push(trimmed.to_string());
            } else {
                parts.push(trimmed.to_lowercase());
            }
            Some(())
        }

        let mut parts: Vec<String> = Vec::new();
        let mut buf = String::new();
        let mut in_quotes = false;
        let mut part_quoted = false;

        let mut chars = token.trim().chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '"' => {
                    if in_quotes {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            buf.push('"');
                        } else {
                            in_quotes = false;
                        }
                    } else {
                        in_quotes = true;
                        part_quoted = true;
                    }
                }
                '.' if !in_quotes => {
                    push_part(&mut parts, &buf, part_quoted)?;
                    buf.clear();
                    part_quoted = false;
                }
                _ => buf.push(ch),
            }
        }

        if in_quotes {
            return None;
        }
        push_part(&mut parts, &buf, part_quoted)?;

        if parts.len() >= 2 {
            Some((
                Some(parts[parts.len() - 2].clone()),
                parts[parts.len() - 1].clone(),
            ))
        } else {
            Some((None, parts[0].clone()))
        }
    }

    let mut iter = _args.into_iter();
    let table_name = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(v) => v.to_string(),
    };
    let col_name = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(v) => v.to_string(),
    };

    let (schema_opt, table) = match parse_qname_token(&table_name) {
        Some(parsed) => parsed,
        None => return Ok(Value::Null),
    };
    let (_, column) = match parse_qname_token(&col_name) {
        Some(parsed) => parsed,
        None => return Ok(Value::Null),
    };

    let schema = schema_opt.unwrap_or_else(|| "public".to_string());
    let seq_name = format!("{}_{}_seq", table, column);
    Ok(Value::Text(format!(
        "{}.{}",
        quoting::quote_ident(&schema),
        quoting::quote_ident(&seq_name)
    )))
}

pub fn pg_get_expr(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(s)),
        Some(Value::Null) | None => Ok(Value::Null),
        Some(v) => Ok(Value::Text(v.to_string())),
    }
}

pub fn has_privilege(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Boolean(true))
}

/// int4send(integer) → bytea — 4-byte big-endian encoding
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

/// int8send(bigint) → bytea — 8-byte big-endian encoding
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

/// uuid_send(uuid) → bytea — 16-byte binary representation
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

/// set_bit(bytea, n, newvalue) → bytea
///
/// PostgreSQL bytea bit indexing: bit `n` maps to byte `n / 8`, and within
/// that byte the bit position is `n % 8` (LSB-first: bit 0 = rightmost).
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

/// get_bit(bytea, n) → integer
///
/// PostgreSQL bytea bit indexing: bit `n` maps to byte `n / 8`, bit position
/// `n % 8` within that byte (LSB-first: bit 0 = rightmost).
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

pub fn hashtext(args: Vec<Value>) -> Result<Value> {
    let text = match args.into_iter().next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(v) => v.to_string(),
    };
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    let h = hasher.finish();
    Ok(Value::Int32(h as i32))
}

#[cfg(test)]
mod tests {
    use crate::sql::expr::functions::string::{quote_ident, quote_literal, quote_nullable};

    use super::*;

    #[test]
    fn test_pg_typeof() {
        assert_eq!(
            pg_typeof(vec![Value::Int32(42)]).unwrap(),
            Value::Text("integer".into())
        );
        assert_eq!(
            pg_typeof(vec![Value::Text("hello".into())]).unwrap(),
            Value::Text("text".into())
        );
        assert_eq!(
            pg_typeof(vec![Value::Null]).unwrap(),
            Value::Text("unknown".into())
        );
    }

    #[test]
    fn test_quote_ident() {
        assert_eq!(
            quote_ident(vec![Value::Text("simple".into())]).unwrap(),
            Value::Text("simple".into())
        );
        assert_eq!(
            quote_ident(vec![Value::Text("SELECT".into())]).unwrap(),
            Value::Text("\"SELECT\"".into())
        );
        assert_eq!(
            quote_ident(vec![Value::Text("has space".into())]).unwrap(),
            Value::Text("\"has space\"".into())
        );
    }

    #[test]
    fn test_quote_literal() {
        assert_eq!(
            quote_literal(vec![Value::Text("hello".into())]).unwrap(),
            Value::Text("'hello'".into())
        );
        assert_eq!(
            quote_literal(vec![Value::Text("it's".into())]).unwrap(),
            Value::Text("'it''s'".into())
        );
    }

    #[test]
    fn test_quote_nullable() {
        assert_eq!(
            quote_nullable(vec![Value::Null]).unwrap(),
            Value::Text("NULL".into())
        );
        assert_eq!(
            quote_nullable(vec![Value::Text("hello".into())]).unwrap(),
            Value::Text("'hello'".into())
        );
    }

    #[test]
    fn test_pg_column_size() {
        assert_eq!(
            pg_column_size(vec![Value::Int32(42)]).unwrap(),
            Value::Int32(4)
        );
        assert_eq!(
            pg_column_size(vec![Value::Int64(42)]).unwrap(),
            Value::Int32(8)
        );
    }

    #[test]
    fn test_format_type() {
        assert_eq!(
            format_type(vec![Value::Int32(23)]).unwrap(),
            Value::Text("int4".into())
        );
        assert_eq!(
            format_type(vec![Value::Int32(25)]).unwrap(),
            Value::Text("text".into())
        );
        assert_eq!(
            format_type(vec![Value::Int32(1700)]).unwrap(),
            Value::Text("numeric".into())
        );
        assert_eq!(
            format_type(vec![Value::Int32(1083)]).unwrap(),
            Value::Text("time without time zone".into())
        );
        assert_eq!(
            format_type(vec![Value::Int32(1186)]).unwrap(),
            Value::Text("interval".into())
        );
        // 2-arg form: VARCHAR with typmod
        assert_eq!(
            format_type(vec![Value::Int32(1043), Value::Int32(7)]).unwrap(),
            Value::Text("character varying(3)".into())
        );
        // 2-arg form: VARCHAR without typmod
        assert_eq!(
            format_type(vec![Value::Int32(1043), Value::Int32(-1)]).unwrap(),
            Value::Text("character varying".into())
        );
    }

    #[test]
    fn test_pg_type_is_visible() {
        assert_eq!(
            pg_type_is_visible(vec![Value::Int32(12345)]).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(pg_type_is_visible(vec![Value::Null]).unwrap(), Value::Null);
        assert_eq!(pg_type_is_visible(vec![]).unwrap(), Value::Null);
    }

    #[test]
    fn test_pg_table_is_visible() {
        assert_eq!(
            pg_table_is_visible(vec![Value::Int32(12345)]).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(pg_table_is_visible(vec![Value::Null]).unwrap(), Value::Null);
        assert_eq!(pg_table_is_visible(vec![]).unwrap(), Value::Null);
    }

    #[test]
    fn test_int4send() {
        // int4send(16909060) → \x01020304 (big-endian)
        assert_eq!(
            int4send(vec![Value::Int32(16909060)]).unwrap(),
            Value::Bytes(vec![0x01, 0x02, 0x03, 0x04])
        );
        assert_eq!(int4send(vec![Value::Null]).unwrap(), Value::Null);
    }

    #[test]
    fn test_int8send() {
        // int8send(72623859790382856) → \x0102030405060708 (big-endian)
        assert_eq!(
            int8send(vec![Value::Int64(72623859790382856)]).unwrap(),
            Value::Bytes(vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08])
        );
        assert_eq!(int8send(vec![Value::Null]).unwrap(), Value::Null);
    }

    #[test]
    fn test_set_bit_bytea() {
        // PG 17.7: set_bit('\x00'::bytea, 0, 1) → \x01 (bit 0 = LSB)
        assert_eq!(
            set_bit_bytea(vec![
                Value::Bytes(vec![0x00]),
                Value::Int32(0),
                Value::Int32(1)
            ])
            .unwrap(),
            Value::Bytes(vec![0x01])
        );
        // PG 17.7: set_bit('\x00'::bytea, 7, 1) → \x80 (bit 7 = MSB)
        assert_eq!(
            set_bit_bytea(vec![
                Value::Bytes(vec![0x00]),
                Value::Int32(7),
                Value::Int32(1)
            ])
            .unwrap(),
            Value::Bytes(vec![0x80])
        );
    }

    #[test]
    fn test_get_bit_bytea() {
        // PG 17.7: get_bit('\x80'::bytea, 0) → 0 (bit 0 = LSB)
        assert_eq!(
            get_bit_bytea(vec![Value::Bytes(vec![0x80]), Value::Int32(0)]).unwrap(),
            Value::Int32(0)
        );
        // PG 17.7: get_bit('\x80'::bytea, 7) → 1 (bit 7 = MSB)
        assert_eq!(
            get_bit_bytea(vec![Value::Bytes(vec![0x80]), Value::Int32(7)]).unwrap(),
            Value::Int32(1)
        );
    }

    #[test]
    fn test_hashtext() {
        let h1 = hashtext(vec![Value::Text("hello".into())]).unwrap();
        let h2 = hashtext(vec![Value::Text("hello".into())]).unwrap();
        assert_eq!(h1, h2);

        let h3 = hashtext(vec![Value::Text("world".into())]).unwrap();
        assert_ne!(h1, h3);

        assert_eq!(hashtext(vec![Value::Null]).unwrap(), Value::Null);
    }
}
