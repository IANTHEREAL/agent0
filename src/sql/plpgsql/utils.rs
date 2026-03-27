//! PL/pgSQL utility functions: RAISE formatting, variable substitution, type keyword checking,
//! identifier replacement, and exit signal management.

use crate::model::{DataType, Value};
use crate::sql::scanner::SqlCharScanner;

use super::PlpgsqlContext;
use crate::sql::quoting;

/// Parse a PL/pgSQL type string into a `DataType`.
pub(super) fn parse_plpgsql_type(type_str: &str) -> DataType {
    let t = type_str.trim().to_lowercase();
    match t.as_str() {
        "integer" | "int" | "int4" => DataType::Int32,
        "bigint" | "int8" => DataType::Int64,
        "smallint" | "int2" => DataType::Int32,
        "boolean" | "bool" => DataType::Boolean,
        "text" => DataType::Text,
        "varchar" | "character varying" => DataType::Varchar(0),
        "real" | "float4" | "double precision" | "float8" | "float" => DataType::Float64,
        "timestamp"
        | "timestamptz"
        | "timestamp with time zone"
        | "timestamp without time zone" => DataType::Timestamp,
        "date" => DataType::Date,
        "uuid" => DataType::Uuid,
        "json" | "jsonb" => DataType::Json,
        "bytea" => DataType::Bytes,
        _ if t.starts_with("varchar") || t.starts_with("character varying") => DataType::Varchar(0),
        _ if t.starts_with("numeric") || t.starts_with("decimal") => DataType::Float64,
        _ => DataType::Text,
    }
}

/// Parse a literal value string into a `Value`.
pub(super) fn parse_literal_value(s: &str, _data_type: &DataType) -> anyhow::Result<Value> {
    let s = s.trim();

    if s.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    if s.eq_ignore_ascii_case("true") {
        return Ok(Value::Boolean(true));
    }
    if s.eq_ignore_ascii_case("false") {
        return Ok(Value::Boolean(false));
    }
    if (s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')) {
        let inner = &s[1..s.len() - 1];
        let unescaped = inner.replace("''", "'").replace("\\\"", "\"");
        return Ok(Value::Text(unescaped));
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(Value::Int64(i));
    }
    if let Ok(f) = s.parse::<f64>() {
        return Ok(Value::Float64(f));
    }
    Ok(Value::Text(s.to_string()))
}

/// Check whether a string is a PL/pgSQL type keyword (not a parameter name).
pub(super) fn is_type_keyword(s: &str) -> bool {
    matches!(
        s.to_lowercase().as_str(),
        "integer"
            | "int"
            | "int4"
            | "int8"
            | "bigint"
            | "smallint"
            | "int2"
            | "boolean"
            | "bool"
            | "text"
            | "varchar"
            | "character"
            | "real"
            | "float4"
            | "float8"
            | "float"
            | "double"
            | "numeric"
            | "decimal"
            | "timestamp"
            | "timestamptz"
            | "date"
            | "uuid"
            | "json"
            | "jsonb"
            | "bytea"
            | "trigger"
            | "void"
    )
}

/// Format a RAISE message, stripping surrounding quotes if present.
pub(super) fn format_raise_message(_ctx: &PlpgsqlContext, msg: &str) -> String {
    let msg = msg.trim();
    if (msg.starts_with('\'') && msg.ends_with('\''))
        || (msg.starts_with('"') && msg.ends_with('"'))
    {
        msg[1..msg.len() - 1].to_string()
    } else {
        msg.to_string()
    }
}

/// Substitute PL/pgSQL variables in a SQL string with their current values.
pub(super) fn substitute_variables(ctx: &PlpgsqlContext, s: &str) -> String {
    let mut result = s.to_string();
    let mut vars: Vec<(&String, &Value)> = ctx.variables.iter().collect();
    vars.sort_by(|(a, _), (b, _)| b.len().cmp(&a.len()));
    for (name, value) in vars {
        let value_str = match value {
            Value::Null => "NULL".to_string(),
            Value::Text(t) => quoting::quote_literal(t),
            // Wrap negative numbers in parens to prevent "--5" becoming a SQL comment
            Value::Int32(i) if *i < 0 => format!("({})", i),
            Value::Int64(i) if *i < 0 => format!("({})", i),
            Value::Float64(f) if *f < 0.0 => format!("({})", f),
            Value::Numeric(d) if d.is_sign_negative() => format!("({})", d),
            v => v.to_string(),
        };
        result = replace_identifier(&result, name, &value_str);
    }
    result
}

/// Replace all occurrences of an identifier (respecting word boundaries and string literals)
/// with a replacement string.
pub(crate) fn replace_identifier(s: &str, name: &str, replacement: &str) -> String {
    if name.is_empty() {
        return s.to_string();
    }

    let bytes = s.as_bytes();
    let name_bytes = name.as_bytes();
    let mut i = 0;
    let mut in_string = false;

    let mut result = Vec::with_capacity(bytes.len());
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if in_string {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    result.push(bytes[i]);
                    result.push(bytes[i + 1]);
                    i += 2;
                    continue;
                }
                in_string = false;
            } else {
                in_string = true;
            }
            result.push(bytes[i]);
            i += 1;
            continue;
        }

        if in_string {
            result.push(bytes[i]);
            i += 1;
            continue;
        }

        if i + name_bytes.len() <= bytes.len()
            && bytes[i..i + name_bytes.len()].eq_ignore_ascii_case(name_bytes)
        {
            let before_ok = i == 0 || (!is_ident_char(bytes[i - 1]) && bytes[i - 1] != b'.');
            let after_ok = i + name_bytes.len() == bytes.len()
                || (!is_ident_char(bytes[i + name_bytes.len()])
                    && bytes[i + name_bytes.len()] != b'.');

            if before_ok && after_ok {
                result.extend_from_slice(replacement.as_bytes());
                i += name_bytes.len();
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }

    String::from_utf8(result).unwrap_or_else(|_| s.to_string())
}

fn is_ident_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum BlockKind {
    Begin,
    Case,
}

/// Scan the outer PL/pgSQL `BEGIN .. END;` block while ignoring strings,
/// comments, and dollar-quoted SQL text.
fn plpgsql_outer_block_scan(body: &str) -> Option<(usize, usize, bool)> {
    let bytes = body.as_bytes();
    let mut skip_until = 0usize;
    let mut stack: Vec<BlockKind> = Vec::new();
    let mut block_start: Option<usize> = None;

    for ctx in SqlCharScanner::new(body) {
        if !ctx.is_code() || ctx.pos < skip_until {
            continue;
        }

        let i = ctx.pos;
        let b = bytes[i];
        if !(b.is_ascii_alphabetic() || b == b'_') {
            continue;
        }

        let start = i;
        let mut end = i + 1;
        while end < bytes.len() {
            let b = bytes[end];
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
                end += 1;
            } else {
                break;
            }
        }
        skip_until = end;

        let token = &body[start..end];
        if token.eq_ignore_ascii_case("BEGIN") {
            if stack.is_empty() {
                block_start = Some(end);
            }
            stack.push(BlockKind::Begin);
            continue;
        }

        if token.eq_ignore_ascii_case("CASE") {
            if !stack.is_empty() {
                stack.push(BlockKind::Case);
            }
            continue;
        }

        if !token.eq_ignore_ascii_case("END") || stack.is_empty() {
            continue;
        }

        let mut j = end;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }

        let mut next_token_end = j;
        let mut next_token: Option<&str> = None;
        if next_token_end < bytes.len()
            && (bytes[next_token_end].is_ascii_alphabetic() || bytes[next_token_end] == b'_')
        {
            let next_start = next_token_end;
            next_token_end += 1;
            while next_token_end < bytes.len() {
                let b = bytes[next_token_end];
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
                    next_token_end += 1;
                } else {
                    break;
                }
            }
            next_token = Some(&body[next_start..next_token_end]);
        }

        if let Some(next) = next_token {
            if next.eq_ignore_ascii_case("IF") || next.eq_ignore_ascii_case("LOOP") {
                continue;
            }
            if next.eq_ignore_ascii_case("CASE") {
                if matches!(stack.last(), Some(BlockKind::Case)) {
                    stack.pop();
                }
                skip_until = next_token_end;
                continue;
            }
        }

        if matches!(stack.last(), Some(BlockKind::Case)) {
            stack.pop();
            continue;
        }

        if matches!(stack.last(), Some(BlockKind::Begin)) {
            if j < bytes.len() && bytes[j] == b';' {
                stack.pop();
                if stack.is_empty() {
                    return Some((block_start.unwrap_or(start), start, true));
                }
                continue;
            }

            if next_token.is_some() {
                let mut k = next_token_end;
                while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b';' {
                    stack.pop();
                    if stack.is_empty() {
                        return Some((block_start.unwrap_or(start), start, true));
                    }
                }
            }
        }
    }

    block_start.map(|start| (start, body.len(), false))
}

/// Find the outer PL/pgSQL `BEGIN .. END;` block while allowing an
/// unterminated body to extend to EOF. Trigger extraction uses this relaxed
/// behavior.
pub(crate) fn plpgsql_outer_block_range(body: &str) -> Option<(usize, usize)> {
    plpgsql_outer_block_scan(body).map(|(start, end, _closed)| (start, end))
}

/// Find the outer PL/pgSQL `BEGIN .. END;` block and require an explicit outer
/// `END;` terminator. Body validation/parsing uses this strict behavior.
pub(crate) fn plpgsql_outer_block_range_strict(body: &str) -> Option<(usize, usize)> {
    match plpgsql_outer_block_scan(body) {
        Some((start, end, true)) => Some((start, end)),
        _ => None,
    }
}

pub(super) const EXIT_SIGNAL_VAR: &str = "__db9_plpgsql_exit_signal__";

pub(super) fn set_exit_signal(ctx: &mut PlpgsqlContext) {
    ctx.set_var(EXIT_SIGNAL_VAR, Value::Boolean(true));
}

pub(super) fn has_exit_signal(ctx: &PlpgsqlContext) -> bool {
    ctx.variables
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case(EXIT_SIGNAL_VAR) && matches!(v, Value::Boolean(true)))
}

pub(super) fn consume_exit_signal(ctx: &mut PlpgsqlContext) -> bool {
    let mut found_key: Option<String> = None;
    let mut signaled = false;
    for (k, v) in &ctx.variables {
        if k.eq_ignore_ascii_case(EXIT_SIGNAL_VAR) {
            found_key = Some(k.clone());
            signaled = matches!(v, Value::Boolean(true));
            break;
        }
    }
    if let Some(key) = found_key {
        ctx.variables.remove(&key);
    }
    signaled
}
