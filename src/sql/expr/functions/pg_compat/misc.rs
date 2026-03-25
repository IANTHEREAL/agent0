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
