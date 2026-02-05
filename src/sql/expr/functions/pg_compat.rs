use crate::types::Value;
use anyhow::Result;
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("PG_TYPEOF", pg_typeof);
    map.insert("QUOTE_IDENT", quote_ident);
    map.insert("QUOTE_LITERAL", quote_literal);
    map.insert("QUOTE_NULLABLE", quote_nullable);
    map.insert("PG_COLUMN_SIZE", pg_column_size);
    map.insert("FORMAT_TYPE", format_type);
    map.insert("PG_IS_IN_RECOVERY", pg_is_in_recovery);
    map.insert("PG_TABLE_IS_VISIBLE", pg_table_is_visible);
    map.insert("PG_TYPE_IS_VISIBLE", pg_type_is_visible);
    map.insert("CLOCK_TIMESTAMP", clock_timestamp);
    map.insert("STATEMENT_TIMESTAMP", clock_timestamp);
    map.insert("TRANSACTION_TIMESTAMP", clock_timestamp);
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

fn is_simple_unquoted_ident(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    for ch in chars {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '$') {
            return false;
        }
    }
    true
}

fn is_sql_keyword(ident: &str) -> bool {
    let upper = ident.to_ascii_uppercase();
    sqlparser::keywords::ALL_KEYWORDS
        .binary_search(&upper.as_str())
        .is_ok()
}

pub fn quote_ident(args: Vec<Value>) -> Result<Value> {
    let val = match args.into_iter().next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let needs_quote = val.is_empty() || !is_simple_unquoted_ident(&val) || is_sql_keyword(&val);
    let result = if needs_quote {
        format!("\"{}\"", val.replace('"', "\"\""))
    } else {
        val
    };
    Ok(Value::Text(result))
}

pub fn quote_literal(args: Vec<Value>) -> Result<Value> {
    let val = match args.into_iter().next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    Ok(Value::Text(format!("'{}'", val.replace('\'', "''"))))
}

pub fn quote_nullable(args: Vec<Value>) -> Result<Value> {
    let val = match args.into_iter().next() {
        Some(Value::Null) => return Ok(Value::Text("NULL".to_string())),
        Some(Value::Text(s)) => s,
        Some(v) => v.to_string(),
        None => return Ok(Value::Text("NULL".to_string())),
    };
    Ok(Value::Text(format!("'{}'", val.replace('\'', "''"))))
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
        Value::Text(s) => s.trim().parse::<i64>().unwrap_or(0),
        Value::Null => return Ok(Value::Null),
        _ => 0,
    };
    let type_name = match oid {
        16 => "bool",
        20 => "int8",
        23 => "int4",
        701 => "float8",
        25 => "text",
        17 => "bytea",
        1114 => "timestamp",
        1184 => "timestamptz",
        2950 => "uuid",
        114 => "json",
        3802 => "jsonb",
        16385 => "vector",
        _ => "text",
    };
    Ok(Value::Text(type_name.to_string()))
}

pub fn pg_is_in_recovery(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Boolean(false))
}

pub fn pg_table_is_visible(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Boolean(true))
}

/// Check if a type is visible in the current search_path.
///
/// In pg-tikv, all types within the keyspace are visible, so this always
/// returns true (similar to pg_table_is_visible).
///
/// PostgreSQL signature: pg_type_is_visible(type_oid oid) → boolean
pub fn pg_type_is_visible(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Boolean(true))
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

    fn quote_ident_str(ident: &str) -> String {
        let needs_quote =
            ident.is_empty() || !is_simple_unquoted_ident(ident) || is_sql_keyword(ident);
        if needs_quote {
            format!("\"{}\"", ident.replace('"', "\"\""))
        } else {
            ident.to_string()
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
        quote_ident_str(&schema),
        quote_ident_str(&seq_name)
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

#[cfg(test)]
mod tests {
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
    }

    #[test]
    fn test_pg_type_is_visible() {
        assert_eq!(
            pg_type_is_visible(vec![Value::Int32(12345)]).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            pg_type_is_visible(vec![Value::Null]).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(pg_type_is_visible(vec![]).unwrap(), Value::Boolean(true));
    }

    #[test]
    fn test_pg_table_is_visible() {
        assert_eq!(
            pg_table_is_visible(vec![Value::Int32(12345)]).unwrap(),
            Value::Boolean(true)
        );
    }
}
