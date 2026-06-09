use super::*;
use crate::sql::pg_types;

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

pub(crate) fn pg_typeof_name_for_datatype(dt: &DataType) -> String {
    match dt {
        DataType::Boolean => "boolean".to_string(),
        DataType::Int32 => "integer".to_string(),
        DataType::Int64 => "bigint".to_string(),
        DataType::Oid => "oid".to_string(),
        DataType::Float64 => "double precision".to_string(),
        DataType::Numeric { .. } => "numeric".to_string(),
        DataType::Text => "text".to_string(),
        DataType::Bytes => "bytea".to_string(),
        DataType::Timestamp => "timestamp without time zone".to_string(),
        DataType::TimestampTz => "timestamp with time zone".to_string(),
        DataType::Date => "date".to_string(),
        DataType::Time => "time without time zone".to_string(),
        DataType::Interval => "interval".to_string(),
        DataType::Uuid => "uuid".to_string(),
        DataType::Json => "json".to_string(),
        DataType::Jsonb => "jsonb".to_string(),
        DataType::Array(inner) => format!("{}[]", pg_typeof_name_for_datatype(inner)),
        DataType::Vector(_) => "vector".to_string(),
        DataType::Tsvector => "tsvector".to_string(),
        DataType::Tsquery => "tsquery".to_string(),
        DataType::Name => "name".to_string(),
        DataType::Varchar(_) => "character varying".to_string(),
        DataType::UserDefined(name) if name.eq_ignore_ascii_case("regclass") => {
            "regclass".to_string()
        }
        DataType::UserDefined(name) if name.eq_ignore_ascii_case("pg_catalog.regclass") => {
            "regclass".to_string()
        }
        DataType::UserDefined(name) if name.eq_ignore_ascii_case("regtype") => {
            "regtype".to_string()
        }
        DataType::UserDefined(name) if name.eq_ignore_ascii_case("pg_catalog.regtype") => {
            "regtype".to_string()
        }
        DataType::Unknown => "unknown".to_string(),
        DataType::UserDefined(name) => name
            .strip_prefix("pg_catalog.")
            .unwrap_or(name.as_str())
            .to_string(),
    }
}

pub fn pg_typeof(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    Ok(Value::Text(pg_typeof_name(&val)))
}

pub fn pg_column_size(args: Vec<Value>) -> Result<Value> {
    use crate::sql::types::sizing;

    let val = args.into_iter().next().unwrap_or(Value::Null);
    // PG's pg_column_size() is strict: returns NULL for NULL input.
    if matches!(val, Value::Null) {
        return Ok(Value::Null);
    }
    let size = match &val {
        Value::Null => unreachable!(), // handled above
        Value::Boolean(_) => 1,
        Value::Int32(_) => 4,
        Value::Int64(_) | Value::Float64(_) => 8,
        Value::Numeric(d) => sizing::numeric_datum_width(d),
        Value::Date(_) => 4,
        Value::Time(_) | Value::Timestamp(_) => 8,
        Value::Interval { .. } => 16,
        Value::Uuid(_) => 16,
        Value::Text(s) => s.len() + 4,
        Value::Json(s) => s.len() + 4,
        Value::Jsonb(s) => sizing::jsonb_datum_width(s),
        Value::Bytes(b) => b.len() + 4,
        Value::Tsvector(s) => sizing::tsvector_datum_width(s),
        Value::Tsquery(s) => sizing::tsquery_datum_width(s),
        Value::Vector(v) => v.len() * 4 + 4,
        Value::Array(a) => sizing::array_datum_width(a),
    };
    Ok(Value::Int32(size as i32))
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

pub fn pg_get_expr(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(s)),
        Some(Value::Null) | None => Ok(Value::Null),
        Some(v) => Ok(Value::Text(v.to_string())),
    }
}

pub fn pg_get_statisticsobjdef_columns(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) | None => Ok(Value::Null),
        _ => Ok(Value::Text(String::new())),
    }
}

pub fn has_privilege(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Boolean(true))
}

pub fn pg_relation_is_publishable(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(_) => Ok(Value::Boolean(false)),
    }
}

pub fn pg_partition_ancestors(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Null)
}

fn pg_hash_mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(4);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(6);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(8);
    *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(16);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(19);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(4);
    *b = b.wrapping_add(*a);
}

fn pg_hash_final(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(14));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(11));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(25));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(16));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(4));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(14));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(24));
}

fn pg_hash_bytes(bytes: &[u8]) -> u32 {
    let len = bytes.len() as u32;
    let mut a = 0x9e37_79b9_u32.wrapping_add(len).wrapping_add(3_923_095);
    let mut b = a;
    let mut c = a;
    let mut offset = 0usize;
    let mut remaining = bytes.len();

    while remaining >= 12 {
        a = a.wrapping_add(u32::from_le_bytes(
            bytes[offset..offset + 4].try_into().unwrap(),
        ));
        b = b.wrapping_add(u32::from_le_bytes(
            bytes[offset + 4..offset + 8].try_into().unwrap(),
        ));
        c = c.wrapping_add(u32::from_le_bytes(
            bytes[offset + 8..offset + 12].try_into().unwrap(),
        ));
        pg_hash_mix(&mut a, &mut b, &mut c);
        offset += 12;
        remaining -= 12;
    }

    let tail = &bytes[offset..];
    if remaining == 11 {
        c = c.wrapping_add((tail[10] as u32) << 24);
    }
    if remaining >= 10 {
        c = c.wrapping_add((tail[9] as u32) << 16);
    }
    if remaining >= 9 {
        c = c.wrapping_add((tail[8] as u32) << 8);
    }
    if remaining >= 8 {
        b = b.wrapping_add((tail[7] as u32) << 24);
    }
    if remaining >= 7 {
        b = b.wrapping_add((tail[6] as u32) << 16);
    }
    if remaining >= 6 {
        b = b.wrapping_add((tail[5] as u32) << 8);
    }
    if remaining >= 5 {
        b = b.wrapping_add(tail[4] as u32);
    }
    if remaining >= 4 {
        a = a.wrapping_add((tail[3] as u32) << 24);
    }
    if remaining >= 3 {
        a = a.wrapping_add((tail[2] as u32) << 16);
    }
    if remaining >= 2 {
        a = a.wrapping_add((tail[1] as u32) << 8);
    }
    if remaining >= 1 {
        a = a.wrapping_add(tail[0] as u32);
    }

    pg_hash_final(&mut a, &mut b, &mut c);
    c
}

pub fn hashtext(args: Vec<Value>) -> Result<Value> {
    let text = match args.into_iter().next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(v) => v.to_string(),
    };
    Ok(Value::Int32(pg_hash_bytes(text.as_bytes()) as i32))
}

#[cfg(test)]
mod hashtext_tests {
    use super::*;

    #[test]
    fn hashtext_matches_pg_for_hello() {
        assert_eq!(
            hashtext(vec![Value::Text("hello".to_string())]).unwrap(),
            Value::Int32(-1_870_292_951)
        );
    }
}
