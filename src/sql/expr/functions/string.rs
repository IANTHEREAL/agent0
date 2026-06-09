use crate::model::Value;
use crate::sql::error::SqlError;
use crate::sql::expr::{simple_unicode_lower_char, simple_unicode_upper_char};
use crate::sql::quoting;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

/// Maximum output size in bytes for string-producing functions (matches PostgreSQL's MaxAllocSize).
const MAX_STRING_OUTPUT_BYTES: usize = 1_073_741_823; // 1GB - 1

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("UPPER", upper);
    map.insert("LOWER", lower);
    map.insert("LENGTH", length);
    map.insert("CHAR_LENGTH", length);
    map.insert("CHARACTER_LENGTH", length);
    map.insert("OCTET_LENGTH", octet_length);
    map.insert("BIT_LENGTH", bit_length);
    map.insert("CONCAT", concat);
    map.insert("CONCAT_WS", concat_ws);
    map.insert("LEFT", left);
    map.insert("RIGHT", right);
    map.insert("TRIM", trim);
    map.insert("BTRIM", btrim);
    map.insert("LTRIM", ltrim);
    map.insert("RTRIM", rtrim);
    map.insert("LPAD", lpad);
    map.insert("RPAD", rpad);
    map.insert("REPEAT", repeat);
    map.insert("REPLACE", replace);
    map.insert("REVERSE", reverse);
    map.insert("INITCAP", initcap);
    map.insert("ASCII", ascii);
    map.insert("CHR", chr);
    map.insert("STRPOS", strpos);
    map.insert("STARTS_WITH", starts_with);
    map.insert("SPLIT_PART", split_part);
    map.insert("TRANSLATE", translate);
    map.insert("QUOTE_IDENT", quote_ident);
    map.insert("QUOTE_LITERAL", quote_literal);
    map.insert("QUOTE_NULLABLE", quote_nullable);
    map.insert("SUBSTRING", substring);
    map.insert("SUBSTR", substring);
    map.insert("OVERLAY", overlay);
    map.insert("POSITION", position);
}

pub fn upper(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(
            s.chars().map(simple_unicode_upper_char).collect(),
        )),
        Some(v) => Ok(v),
        None => Ok(Value::Null),
    }
}

pub fn lower(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(
            s.chars().map(simple_unicode_lower_char).collect(),
        )),
        Some(v) => Ok(v),
        None => Ok(Value::Null),
    }
}

pub fn length(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Int32(s.chars().count() as i32)),
        Some(Value::Bytes(b)) => Ok(Value::Int32(b.len() as i32)),
        _ => Ok(Value::Null),
    }
}

pub fn octet_length(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Int32(s.len() as i32)),
        Some(Value::Bytes(b)) => Ok(Value::Int32(b.len() as i32)),
        _ => Ok(Value::Null),
    }
}

pub fn bit_length(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Int32((s.len() * 8) as i32)),
        Some(Value::Bytes(b)) => Ok(Value::Int32((b.len() * 8) as i32)),
        _ => Ok(Value::Null),
    }
}

pub fn concat(args: Vec<Value>) -> Result<Value> {
    let mut result = String::new();
    for val in args {
        match val {
            Value::Null => {}
            Value::Text(s) => result.push_str(&s),
            v => result.push_str(&v.to_string()),
        }
    }
    Ok(Value::Text(result))
}

pub fn concat_ws(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let sep = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => String::new(),
    };
    let parts: Vec<String> = iter
        .filter_map(|v| match v {
            Value::Null => None,
            Value::Text(s) => Some(s),
            v => Some(v.to_string()),
        })
        .collect();
    Ok(Value::Text(parts.join(&sep)))
}

pub fn left(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let n = match iter.next() {
        Some(Value::Int32(n)) => i64::from(n),
        Some(Value::Int64(n)) => n,
        _ => return Ok(Value::Null),
    };
    let chars: Vec<char> = s.chars().collect();
    let result = if n >= 0 {
        let keep = usize::try_from(n).unwrap_or(usize::MAX).min(chars.len());
        chars[..keep].iter().collect()
    } else {
        let trim = usize::try_from(n.unsigned_abs()).unwrap_or(usize::MAX);
        let keep = chars.len().saturating_sub(trim);
        chars[..keep].iter().collect()
    };
    Ok(Value::Text(result))
}

pub fn right(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let n = match iter.next() {
        Some(Value::Int32(n)) => i64::from(n),
        Some(Value::Int64(n)) => n,
        _ => return Ok(Value::Null),
    };
    let chars: Vec<char> = s.chars().collect();
    let start = if n >= 0 {
        let len = usize::try_from(n).unwrap_or(usize::MAX);
        chars.len().saturating_sub(len)
    } else {
        usize::try_from(n.unsigned_abs())
            .unwrap_or(usize::MAX)
            .min(chars.len())
    };
    Ok(Value::Text(chars[start..].iter().collect()))
}

pub fn trim(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let chars_to_trim = match iter.next() {
        None => None,
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Text(chars)) => Some(chars),
        Some(_) => return Ok(Value::Null),
    };

    match chars_to_trim {
        Some(chars) => {
            let char_set: std::collections::HashSet<char> = chars.chars().collect();
            Ok(Value::Text(
                s.trim_matches(|c| char_set.contains(&c)).to_string(),
            ))
        }
        None => Ok(Value::Text(s.trim_matches(' ').to_string())),
    }
}

pub fn btrim(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let chars_to_trim = match iter.next() {
        None => None,
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Text(chars)) => Some(chars),
        Some(_) => return Ok(Value::Null),
    };

    match chars_to_trim {
        Some(chars) => {
            let char_set: std::collections::HashSet<char> = chars.chars().collect();
            Ok(Value::Text(
                s.trim_matches(|c| char_set.contains(&c)).to_string(),
            ))
        }
        None => Ok(Value::Text(s.trim_matches(' ').to_string())),
    }
}

pub fn ltrim(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let chars_to_trim = match iter.next() {
        None => None,
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Text(chars)) => Some(chars),
        Some(_) => return Ok(Value::Null),
    };
    match chars_to_trim {
        Some(chars) => {
            let char_set: std::collections::HashSet<char> = chars.chars().collect();
            Ok(Value::Text(
                s.trim_start_matches(|c| char_set.contains(&c)).to_string(),
            ))
        }
        None => Ok(Value::Text(s.trim_start_matches(' ').to_string())),
    }
}

pub fn rtrim(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let chars_to_trim = match iter.next() {
        None => None,
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Text(chars)) => Some(chars),
        Some(_) => return Ok(Value::Null),
    };
    match chars_to_trim {
        Some(chars) => {
            let char_set: std::collections::HashSet<char> = chars.chars().collect();
            Ok(Value::Text(
                s.trim_end_matches(|c| char_set.contains(&c)).to_string(),
            ))
        }
        None => Ok(Value::Text(s.trim_end_matches(' ').to_string())),
    }
}

pub fn lpad(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let len = match iter.next() {
        Some(Value::Int32(n)) => n.max(0) as usize,
        Some(Value::Int64(n)) => usize::try_from(n.max(0)).unwrap_or(usize::MAX),
        _ => return Ok(Value::Null),
    };
    let fill = match iter.next() {
        None => " ".to_string(),
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let char_count = s.chars().count();
    if char_count >= len {
        let byte_len: usize = s.chars().take(len).map(|c| c.len_utf8()).sum();
        check_output_byte_size(byte_len)?;
        Ok(Value::Text(s.chars().take(len).collect()))
    } else {
        let fill_chars: Vec<char> = fill.chars().collect();
        if fill_chars.is_empty() {
            return Ok(Value::Text(s));
        }
        let needed = len - char_count;
        check_pad_output_size(s.len(), needed, &fill, &fill_chars)?;
        let mut result = String::new();
        for i in 0..needed {
            result.push(fill_chars[i % fill_chars.len()]);
        }
        result.push_str(&s);
        Ok(Value::Text(result))
    }
}

pub fn rpad(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let len = match iter.next() {
        Some(Value::Int32(n)) => n.max(0) as usize,
        Some(Value::Int64(n)) => usize::try_from(n.max(0)).unwrap_or(usize::MAX),
        _ => return Ok(Value::Null),
    };
    let fill = match iter.next() {
        None => " ".to_string(),
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let char_count = s.chars().count();
    if char_count >= len {
        let byte_len: usize = s.chars().take(len).map(|c| c.len_utf8()).sum();
        check_output_byte_size(byte_len)?;
        Ok(Value::Text(s.chars().take(len).collect()))
    } else {
        let fill_chars: Vec<char> = fill.chars().collect();
        if fill_chars.is_empty() {
            return Ok(Value::Text(s));
        }
        let needed = len - char_count;
        check_pad_output_size(s.len(), needed, &fill, &fill_chars)?;
        let mut result = s;
        for i in 0..needed {
            result.push(fill_chars[i % fill_chars.len()]);
        }
        Ok(Value::Text(result))
    }
}

pub fn repeat(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let n = match iter.next() {
        Some(Value::Int32(n)) => n.max(0) as usize,
        Some(Value::Int64(n)) => usize::try_from(n.max(0)).unwrap_or(usize::MAX),
        _ => return Ok(Value::Null),
    };
    let output_len = s.len().saturating_mul(n);
    if output_len > MAX_STRING_OUTPUT_BYTES {
        anyhow::bail!("requested length too large");
    }
    Ok(Value::Text(s.repeat(n)))
}

/// Check that a string output of `byte_len` bytes won't exceed MAX_STRING_OUTPUT_BYTES.
pub(crate) fn check_output_byte_size(byte_len: usize) -> Result<()> {
    if byte_len > MAX_STRING_OUTPUT_BYTES {
        anyhow::bail!("requested length too large");
    }
    Ok(())
}

/// Check that the byte size of a padded string won't exceed MAX_STRING_OUTPUT_BYTES.
/// `s_len` is the byte length of the original string.
/// `needed` is the number of fill characters to add.
/// `fill` is the fill string (its full byte length).
/// `fill_chars` is the fill string's characters (for partial-cycle byte computation).
fn check_pad_output_size(
    s_len: usize,
    needed: usize,
    fill: &str,
    fill_chars: &[char],
) -> Result<()> {
    let fill_char_count = fill_chars.len();
    if fill_char_count == 0 {
        return Ok(());
    }
    let full_cycles = needed / fill_char_count;
    let remaining = needed % fill_char_count;
    let partial_bytes: usize = fill_chars[..remaining].iter().map(|c| c.len_utf8()).sum();
    let fill_bytes = full_cycles
        .saturating_mul(fill.len())
        .saturating_add(partial_bytes);
    let total_bytes = fill_bytes.saturating_add(s_len);
    if total_bytes > MAX_STRING_OUTPUT_BYTES {
        anyhow::bail!("requested length too large");
    }
    Ok(())
}

pub fn replace(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let from = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let to = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    if from.is_empty() {
        return Ok(Value::Text(s));
    }
    Ok(Value::Text(s.replace(&from, &to)))
}

pub fn reverse(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(s.chars().rev().collect())),
        _ => Ok(Value::Null),
    }
}

pub fn initcap(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => {
            let mut result = String::with_capacity(s.len());
            let mut capitalize_next = true;
            for c in s.chars() {
                if c.is_whitespace() || !c.is_alphanumeric() {
                    capitalize_next = true;
                    result.push(c);
                } else if capitalize_next {
                    result.push(simple_unicode_upper_char(c));
                    capitalize_next = false;
                } else {
                    result.push(simple_unicode_lower_char(c));
                }
            }
            Ok(Value::Text(result))
        }
        _ => Ok(Value::Null),
    }
}

pub fn ascii(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(s
            .chars()
            .next()
            .map(|c| Value::Int32(c as i32))
            .unwrap_or(Value::Int32(0))),
        _ => Ok(Value::Null),
    }
}

pub fn chr(args: Vec<Value>) -> Result<Value> {
    fn chr_from_code(n: i64) -> Result<Value> {
        if n < 0 {
            return Err(SqlError::InvalidParameterValue {
                message: "character number must be positive".into(),
            }
            .into());
        }
        if n == 0 {
            return Err(SqlError::ValueTooLarge {
                message: "null character not permitted".into(),
            }
            .into());
        }

        let code = u32::try_from(n).map_err(|_| SqlError::ValueTooLarge {
            message: format!("requested character too large for encoding: {n}"),
        })?;
        if code > 0x10FFFF {
            return Err(SqlError::ValueTooLarge {
                message: format!("requested character too large for encoding: {n}"),
            }
            .into());
        }
        char::from_u32(code)
            .map(|c| Value::Text(c.to_string()))
            .ok_or_else(|| {
                SqlError::ValueTooLarge {
                    message: format!("requested character not valid for encoding: {n}"),
                }
                .into()
            })
    }

    match args.into_iter().next() {
        Some(Value::Int32(n)) => chr_from_code(i64::from(n)),
        Some(Value::Int64(n)) => chr_from_code(n),
        _ => Ok(Value::Null),
    }
}

pub fn strpos(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let haystack = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let needle = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    match haystack.find(&needle) {
        Some(pos) => {
            let char_pos = haystack[..pos].chars().count() + 1;
            Ok(Value::Int32(char_pos as i32))
        }
        None => Ok(Value::Int32(0)),
    }
}

pub fn starts_with(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let prefix = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    Ok(Value::Boolean(s.starts_with(&prefix)))
}

pub fn split_part(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let delimiter = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let field_num = match iter.next() {
        Some(Value::Int32(n)) => i64::from(n),
        Some(Value::Int64(n)) => n,
        _ => return Ok(Value::Null),
    };
    if field_num == 0 {
        return Err(SqlError::InvalidParameterValue {
            message: "field position must not be zero".into(),
        }
        .into());
    }
    if delimiter.is_empty() {
        return Ok(Value::Text(if field_num.abs() == 1 {
            s
        } else {
            String::new()
        }));
    }

    let parts: Vec<&str> = s.split(&delimiter).collect();
    let idx = if field_num > 0 {
        field_num - 1
    } else {
        parts.len() as i64 + field_num
    };
    if idx < 0 || idx >= parts.len() as i64 {
        return Ok(Value::Text(String::new()));
    }
    Ok(Value::Text(parts[idx as usize].to_string()))
}

pub fn translate(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let from = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let to = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let from_chars: Vec<char> = from.chars().collect();
    let to_chars: Vec<char> = to.chars().collect();
    let result: String = s
        .chars()
        .filter_map(|c| {
            if let Some(pos) = from_chars.iter().position(|&fc| fc == c) {
                to_chars.get(pos).copied()
            } else {
                Some(c)
            }
        })
        .collect();
    Ok(Value::Text(result))
}

pub fn quote_ident(args: Vec<Value>) -> Result<Value> {
    let val = match args.into_iter().next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => {
            let type_name = v
                .data_type()
                .map(|t| t.pg_display_name())
                .unwrap_or_else(|| "unknown".to_string());
            anyhow::bail!("function quote_ident({}) does not exist", type_name)
        }
        None => return Ok(Value::Null),
    };
    Ok(Value::Text(quoting::quote_ident(&val)))
}

pub fn quote_literal(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(quoting::quote_literal(&s))),
        Some(Value::Null) => Ok(Value::Null),
        Some(v) => Ok(Value::Text(quoting::quote_literal(&v.to_string()))),
        None => Ok(Value::Null),
    }
}

pub fn quote_nullable(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) => Ok(Value::Text("NULL".to_string())),
        Some(Value::Text(s)) => Ok(Value::Text(quoting::quote_literal(&s))),
        Some(v) => Ok(Value::Text(quoting::quote_literal(&v.to_string()))),
        None => Ok(Value::Text("NULL".to_string())),
    }
}

/// SUBSTRING(string, start[, length]) or SUBSTRING(string, pattern)
///
/// PostgreSQL semantics:
/// - 2 args with text pattern: regex extraction (first capture group or full match)
/// - 2 args with integer start: extract from position to end (1-based)
/// - 3 args: extract from position for length characters
/// - Also handles bytea input.
pub fn substring(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let val = match iter.next() {
        Some(v) => v,
        None => return Ok(Value::Null),
    };
    let from_val = iter.next();
    let for_val = iter.next();

    match val {
        Value::Text(s) => {
            // Regex mode: SUBSTRING(string FROM pattern) — 2 args, second is text
            if let (Some(Value::Text(ref pattern)), None) = (&from_val, &for_val) {
                let re = regex::Regex::new(pattern)
                    .map_err(|e| super::regex::invalid_regular_expression_error(&e))?;
                if let Some(caps) = re.captures(&s) {
                    if caps.len() > 1 {
                        return Ok(caps
                            .get(1)
                            .map(|m| Value::Text(m.as_str().to_string()))
                            .unwrap_or(Value::Null));
                    }
                    return Ok(caps
                        .get(0)
                        .map(|m| Value::Text(m.as_str().to_string()))
                        .unwrap_or(Value::Null));
                }
                return Ok(Value::Null);
            }

            // Positional mode
            let start = match &from_val {
                Some(Value::Int32(n)) => i64::from(*n),
                Some(Value::Int64(n)) => *n,
                Some(Value::Null) => return Ok(Value::Null),
                Some(other) => {
                    let type_name = other
                        .data_type()
                        .map(|t| t.pg_display_name())
                        .unwrap_or_else(|| "unknown".to_string());
                    return Err(anyhow!(
                        "substring position must be integer, got {}",
                        type_name
                    ));
                }
                None => 0,
            };
            let len = match for_val {
                Some(Value::Int32(n)) if n < 0 => {
                    return Err(SqlError::SubstringError {
                        message: "negative substring length not allowed".into(),
                    }
                    .into());
                }
                Some(Value::Int64(n)) if n < 0 => {
                    return Err(SqlError::SubstringError {
                        message: "negative substring length not allowed".into(),
                    }
                    .into());
                }
                Some(Value::Int32(n)) => Some(i64::from(n)),
                Some(Value::Int64(n)) => Some(n),
                Some(Value::Null) => return Ok(Value::Null),
                Some(other) => {
                    let type_name = other
                        .data_type()
                        .map(|t| t.pg_display_name())
                        .unwrap_or_else(|| "unknown".to_string());
                    return Err(anyhow!(
                        "substring length must be integer, got {}",
                        type_name
                    ));
                }
                None => None,
            };
            Ok(Value::Text(substring_text_positional_pg(&s, start, len)))
        }
        Value::Bytes(bytes) => {
            let start = match &from_val {
                Some(Value::Int32(n)) => i64::from(*n),
                Some(Value::Int64(n)) => *n,
                Some(Value::Null) => return Ok(Value::Null),
                Some(other) => {
                    let type_name = other
                        .data_type()
                        .map(|t| t.pg_display_name())
                        .unwrap_or_else(|| "unknown".to_string());
                    return Err(anyhow!(
                        "substring position must be integer, got {}",
                        type_name
                    ));
                }
                None => 0,
            };
            let count = match for_val {
                Some(Value::Int32(n)) if n < 0 => {
                    return Err(SqlError::SubstringError {
                        message: "negative substring length not allowed".into(),
                    }
                    .into());
                }
                Some(Value::Int64(n)) if n < 0 => {
                    return Err(SqlError::SubstringError {
                        message: "negative substring length not allowed".into(),
                    }
                    .into());
                }
                Some(Value::Int32(n)) => Some(i64::from(n)),
                Some(Value::Int64(n)) => Some(n),
                Some(Value::Null) => return Ok(Value::Null),
                Some(other) => {
                    let type_name = other
                        .data_type()
                        .map(|t| t.pg_display_name())
                        .unwrap_or_else(|| "unknown".to_string());
                    return Err(anyhow!(
                        "substring length must be integer, got {}",
                        type_name
                    ));
                }
                None => None,
            };
            Ok(Value::Bytes(crate::sql::bytea::substring(
                bytes, start, count,
            )))
        }
        _ => Ok(Value::Null),
    }
}

/// OVERLAY(string PLACING replacement FROM start [FOR count])
///
/// Normalized by Analyzer to: OVERLAY(string, replacement, start[, count])
/// Default count = length of replacement string.
pub fn overlay(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let base = match iter.next() {
        Some(v) => v,
        None => return Ok(Value::Null),
    };
    let placing = match iter.next() {
        Some(v) => v,
        None => return Ok(Value::Null),
    };

    let start = match iter.next() {
        Some(Value::Int32(n)) => i64::from(n),
        Some(Value::Int64(n)) => n,
        Some(Value::Null) => return Ok(Value::Null),
        Some(other) => {
            let type_name = other
                .data_type()
                .map(|t| t.pg_display_name())
                .unwrap_or_else(|| "unknown".to_string());
            return Err(anyhow!("overlay start must be integer, got {}", type_name));
        }
        None => return Ok(Value::Null),
    };
    let count = match iter.next() {
        Some(Value::Int32(n)) => Some(i64::from(n)),
        Some(Value::Int64(n)) => Some(n),
        Some(Value::Null) => return Ok(Value::Null),
        None => None,
        Some(other) => {
            let type_name = other
                .data_type()
                .map(|t| t.pg_display_name())
                .unwrap_or_else(|| "unknown".to_string());
            return Err(anyhow!("overlay count must be integer, got {}", type_name));
        }
    };
    match (base, placing) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Text(s), Value::Text(placing)) => {
            Ok(Value::Text(overlay_text_pg(&s, &placing, start, count)?))
        }
        (Value::Bytes(base), Value::Bytes(placing)) => Ok(Value::Bytes(
            crate::sql::bytea::overlay(base, &placing, start, count)?,
        )),
        _ => Ok(Value::Null),
    }
}

fn substring_text_positional_pg(text: &str, start: i64, len: Option<i64>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start_idx = if start <= 1 {
        0
    } else {
        usize::try_from(start.saturating_sub(1)).unwrap_or(usize::MAX)
    };

    match len {
        Some(len) if len <= 0 => String::new(),
        Some(len) if start <= 0 => {
            let adjusted = len.saturating_add(start).saturating_sub(1);
            if adjusted <= 0 {
                String::new()
            } else {
                chars
                    .iter()
                    .take(usize::try_from(adjusted).unwrap_or(usize::MAX))
                    .collect()
            }
        }
        Some(len) => chars
            .iter()
            .skip(start_idx)
            .take(usize::try_from(len).unwrap_or(usize::MAX))
            .collect(),
        None => chars.iter().skip(start_idx).collect(),
    }
}

fn overlay_text_pg(base: &str, placing: &str, start: i64, count: Option<i64>) -> Result<String> {
    // PostgreSQL reports "negative substring length" for non-positive start
    // positions and negative counts here instead of clamping them.
    if start <= 0 || count.is_some_and(|value| value < 0) {
        return Err(SqlError::SubstringError {
            message: "negative substring length not allowed".into(),
        }
        .into());
    }

    let replacement_len = i64::try_from(placing.chars().count()).unwrap_or(i64::MAX);
    let count = count.unwrap_or(replacement_len);
    let suffix_start = overlay_suffix_start(start, count)?;
    let prefix = substring_text_positional_pg(base, 1, Some(start.saturating_sub(1)));
    let suffix = substring_text_positional_pg(base, suffix_start, None);
    let mut result = String::with_capacity(prefix.len() + placing.len() + suffix.len());
    result.push_str(&prefix);
    result.push_str(placing);
    result.push_str(&suffix);
    Ok(result)
}

fn overlay_suffix_start(start: i64, count: i64) -> Result<i64> {
    let start = i32::try_from(start).map_err(|_| SqlError::NumericValueOutOfRange {
        message: "integer out of range".into(),
    })?;
    let count = i32::try_from(count).map_err(|_| SqlError::NumericValueOutOfRange {
        message: "integer out of range".into(),
    })?;
    let suffix_start = start
        .checked_add(count)
        .ok_or(SqlError::NumericValueOutOfRange {
            message: "integer out of range".into(),
        })?;
    Ok(i64::from(suffix_start))
}

/// POSITION(substring IN string)
///
/// Normalized by Analyzer to: STRPOS(string, substring) — but the Analyzer
/// actually maps POSITION to STRPOS already, so this is just an alias.
/// Registered for completeness if anything calls it directly.
pub fn position(args: Vec<Value>) -> Result<Value> {
    strpos(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_ident_non_text_arg() {
        let err = quote_ident(vec![Value::Int32(123)]).unwrap_err();
        assert!(
            err.to_string().contains("quote_ident(integer)"),
            "expected integer type in error, got: {}",
            err
        );
    }

    #[test]
    fn test_quote_ident_rejects_boolean() {
        let err = quote_ident(vec![Value::Boolean(true)]).unwrap_err();
        assert!(
            err.to_string().contains("quote_ident(boolean)"),
            "expected boolean type in error, got: {}",
            err
        );
    }

    #[test]
    fn test_quote_ident_rejects_numeric() {
        let err = quote_ident(vec![Value::Float64(1.5)]).unwrap_err();
        assert!(
            err.to_string().contains("quote_ident(double precision)"),
            "expected double precision type in error, got: {}",
            err
        );
    }

    #[test]
    fn test_quote_ident_null_returns_null() {
        assert_eq!(quote_ident(vec![Value::Null]).unwrap(), Value::Null);
    }

    #[test]
    fn test_chr_zero_rejected() {
        let err = chr(vec![Value::Int32(0)]).unwrap_err();
        assert!(err.to_string().contains("null character not permitted"));
        let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
        assert_eq!(sql_err.sqlstate(), "54000");
    }

    #[test]
    fn test_chr_error_sqlstates_match_pg() {
        let err = chr(vec![Value::Int32(-1)]).unwrap_err();
        assert_eq!(err.to_string(), "character number must be positive");
        let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
        assert_eq!(sql_err.sqlstate(), "22023");

        let err = chr(vec![Value::Int32(55_296)]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "requested character not valid for encoding: 55296"
        );
        let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
        assert_eq!(sql_err.sqlstate(), "54000");

        let err = chr(vec![Value::Int32(1_114_112)]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "requested character too large for encoding: 1114112"
        );
        let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
        assert_eq!(sql_err.sqlstate(), "54000");
    }

    #[test]
    fn test_quote_literal_formats_bytea_using_pg_text() {
        assert_eq!(
            quote_literal(vec![Value::Bytes(vec![0x00, 0x01, 0xff])]).unwrap(),
            Value::Text(r"E'\\x0001ff'".into())
        );
        assert_eq!(
            quote_literal(vec![Value::Text("a\\b".into())]).unwrap(),
            Value::Text(r"E'a\\b'".into())
        );
        assert_eq!(
            quote_nullable(vec![Value::Text("a\\b".into())]).unwrap(),
            Value::Text(r"E'a\\b'".into())
        );
    }

    #[test]
    fn test_string_functions_render_float_special_values_as_pg_text() {
        assert_eq!(
            concat(vec![
                Value::Float64(f64::INFINITY),
                Value::Text("|".into()),
                Value::Float64(f64::NEG_INFINITY),
                Value::Text("|".into()),
                Value::Float64(f64::NAN),
            ])
            .unwrap(),
            Value::Text("Infinity|-Infinity|NaN".into())
        );
        assert_eq!(
            quote_literal(vec![Value::Float64(f64::INFINITY)]).unwrap(),
            Value::Text("'Infinity'".into())
        );
    }

    #[test]
    fn test_upper() {
        assert_eq!(
            upper(vec![Value::Text("hello".into())]).unwrap(),
            Value::Text("HELLO".into())
        );
        assert_eq!(
            upper(vec![Value::Text("\u{00DF}".into())]).unwrap(),
            Value::Text("\u{00DF}".into())
        );
    }

    #[test]
    fn test_lower() {
        assert_eq!(
            lower(vec![Value::Text("HELLO".into())]).unwrap(),
            Value::Text("hello".into())
        );
        assert_eq!(
            lower(vec![Value::Text("\u{0130}".into())]).unwrap(),
            Value::Text("i".into())
        );
    }

    #[test]
    fn test_initcap_unicode_simple_mapping() {
        assert_eq!(
            initcap(vec![Value::Text(
                "\u{0130}\u{00DF} \u{0149} \u{FB03} \u{03C3}\u{03A3}".into()
            )])
            .unwrap(),
            Value::Text("\u{0130}\u{00DF} \u{0149} \u{FB03} \u{03A3}\u{03C3}".into())
        );
    }

    #[test]
    fn test_length() {
        assert_eq!(
            length(vec![Value::Text("hello".into())]).unwrap(),
            Value::Int32(5)
        );
    }

    #[test]
    fn test_concat() {
        assert_eq!(
            concat(vec![
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Text("c".into())
            ])
            .unwrap(),
            Value::Text("abc".into())
        );
    }

    #[test]
    fn test_concat_ws_stringifies_non_text_separator() {
        assert_eq!(
            concat_ws(vec![
                Value::Int32(7),
                Value::Text("alpha".into()),
                Value::Null,
                Value::Bytes(vec![0xde, 0xad]),
            ])
            .unwrap(),
            Value::Text("alpha7\\xdead".into())
        );
    }

    #[test]
    fn test_left_right() {
        assert_eq!(
            left(vec![Value::Text("hello".into()), Value::Int32(3)]).unwrap(),
            Value::Text("hel".into())
        );
        assert_eq!(
            right(vec![Value::Text("hello".into()), Value::Int32(3)]).unwrap(),
            Value::Text("llo".into())
        );
        assert_eq!(
            left(vec![Value::Text("AbC".into()), Value::Int32(-1)]).unwrap(),
            Value::Text("Ab".into())
        );
        assert_eq!(
            right(vec![Value::Text("AbC".into()), Value::Int32(-1)]).unwrap(),
            Value::Text("bC".into())
        );
    }

    #[test]
    fn test_trim() {
        assert_eq!(
            trim(vec![Value::Text("  hello  ".into())]).unwrap(),
            Value::Text("hello".into())
        );
        assert_eq!(
            trim(vec![Value::Text("\thello\t".into())]).unwrap(),
            Value::Text("\thello\t".into())
        );
        assert_eq!(
            btrim(vec![Value::Text("\tabc\t".into())]).unwrap(),
            Value::Text("\tabc\t".into())
        );
        assert_eq!(
            ltrim(vec![Value::Text("\tabc".into())]).unwrap(),
            Value::Text("\tabc".into())
        );
        assert_eq!(
            rtrim(vec![Value::Text("abc\t".into())]).unwrap(),
            Value::Text("abc\t".into())
        );
        assert_eq!(
            btrim(vec![Value::Text(" abc ".into()), Value::Null]).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_substring_pg_start_and_length_edge_cases() {
        assert_eq!(
            substring(vec![
                Value::Text("abc".into()),
                Value::Int32(0),
                Value::Int32(3)
            ])
            .unwrap(),
            Value::Text("ab".into())
        );
        assert_eq!(
            substring(vec![
                Value::Text("abc".into()),
                Value::Int32(-1),
                Value::Int32(4)
            ])
            .unwrap(),
            Value::Text("ab".into())
        );
        let err = substring(vec![
            Value::Text("abc".into()),
            Value::Int32(2),
            Value::Int32(-1),
        ])
        .unwrap_err();
        assert_eq!(err.to_string(), "negative substring length not allowed");
        let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
        assert!(matches!(sql_err, SqlError::SubstringError { .. }));
        assert_eq!(sql_err.sqlstate(), "22011");

        let err = substring(vec![Value::Text("abc".into()), Value::Boolean(true)]).unwrap_err();
        assert!(err
            .to_string()
            .contains("substring position must be integer"));
    }

    #[test]
    fn test_substring_regex_invalid_pattern_preserves_sqlstate() {
        let err = substring(vec![Value::Text("abc".into()), Value::Text("([".into())]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid regular expression: brackets [] not balanced"
        );
        let sql_err = err
            .downcast_ref::<SqlError>()
            .expect("substring regex error should preserve SQLSTATE");
        assert_eq!(sql_err.sqlstate(), "2201B");
    }

    #[test]
    fn test_overlay_pg_start_and_length_edge_cases() {
        let err = overlay(vec![
            Value::Text("abcdef".into()),
            Value::Text("Z".into()),
            Value::Int32(2),
            Value::Int32(-1),
        ])
        .unwrap_err();
        assert_eq!(err.to_string(), "negative substring length not allowed");
        let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
        assert!(matches!(sql_err, SqlError::SubstringError { .. }));
        assert_eq!(sql_err.sqlstate(), "22011");

        let err = overlay(vec![
            Value::Text("abcdef".into()),
            Value::Text("Z".into()),
            Value::Int32(0),
        ])
        .unwrap_err();
        assert_eq!(err.to_string(), "negative substring length not allowed");
        let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
        assert!(matches!(sql_err, SqlError::SubstringError { .. }));
        assert_eq!(sql_err.sqlstate(), "22011");

        let overflow_err = overlay(vec![
            Value::Text("abcdef".into()),
            Value::Text("Z".into()),
            Value::Int32(i32::MAX),
            Value::Int32(1),
        ])
        .unwrap_err();
        assert_eq!(overflow_err.to_string(), "integer out of range");
        let sql_err = overflow_err.downcast_ref::<SqlError>().expect("sql error");
        assert!(matches!(sql_err, SqlError::NumericValueOutOfRange { .. }));
        assert_eq!(sql_err.sqlstate(), "22003");

        let start_type_err = overlay(vec![
            Value::Text("abcdef".into()),
            Value::Text("Z".into()),
            Value::Boolean(true),
        ])
        .unwrap_err();
        assert!(start_type_err
            .to_string()
            .contains("overlay start must be integer"));

        let count_type_err = overlay(vec![
            Value::Text("abcdef".into()),
            Value::Text("Z".into()),
            Value::Int32(2),
            Value::Boolean(true),
        ])
        .unwrap_err();
        assert!(count_type_err
            .to_string()
            .contains("overlay count must be integer"));
    }

    #[test]
    fn test_repeat() {
        assert_eq!(
            repeat(vec![Value::Text("ab".into()), Value::Int32(3)]).unwrap(),
            Value::Text("ababab".into())
        );
    }

    #[test]
    fn test_reverse() {
        assert_eq!(
            reverse(vec![Value::Text("hello".into())]).unwrap(),
            Value::Text("olleh".into())
        );
    }

    #[test]
    fn test_replace() {
        assert_eq!(
            replace(vec![
                Value::Text("hello".into()),
                Value::Text("l".into()),
                Value::Text("L".into())
            ])
            .unwrap(),
            Value::Text("heLLo".into())
        );
        assert_eq!(
            replace(vec![
                Value::Text("abc".into()),
                Value::Text(String::new()),
                Value::Text("x".into()),
            ])
            .unwrap(),
            Value::Text("abc".into())
        );
    }

    #[test]
    fn test_pushed_string_functions_propagate_nullable_args() {
        assert_eq!(
            lpad(vec![Value::Text("xy".into()), Value::Int32(4), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            rpad(vec![Value::Text("xy".into()), Value::Int32(4), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            replace(vec![
                Value::Text("alpha beta".into()),
                Value::Null,
                Value::Text("BETA".into()),
            ])
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            replace(vec![
                Value::Text("alpha beta".into()),
                Value::Text("beta".into()),
                Value::Null,
            ])
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            strpos(vec![Value::Text("alpha beta".into()), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            translate(vec![
                Value::Text("alpha beta".into()),
                Value::Null,
                Value::Text("AB".into()),
            ])
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            translate(vec![
                Value::Text("alpha beta".into()),
                Value::Text("ab".into()),
                Value::Null,
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_starts_with() {
        assert_eq!(
            starts_with(vec![
                Value::Text("alphabet".into()),
                Value::Text("alpha".into()),
            ])
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            starts_with(vec![
                Value::Text("alphabet".into()),
                Value::Text("beta".into()),
            ])
            .unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            starts_with(vec![Value::Text("abc".into()), Value::Text(String::new())]).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            starts_with(vec![Value::Null, Value::Text("a".into())]).unwrap(),
            Value::Null
        );
        assert_eq!(
            starts_with(vec![Value::Text("abc".into()), Value::Null]).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_split_part_matches_pg_negative_index_semantics() {
        assert_eq!(
            split_part(vec![
                Value::Text("a,b,c".into()),
                Value::Text(",".into()),
                Value::Int32(-1),
            ])
            .unwrap(),
            Value::Text("c".into())
        );
        assert_eq!(
            split_part(vec![
                Value::Text("a,b,c".into()),
                Value::Text(",".into()),
                Value::Int32(-2),
            ])
            .unwrap(),
            Value::Text("b".into())
        );
        assert_eq!(
            split_part(vec![
                Value::Text("a,b,c".into()),
                Value::Text(",".into()),
                Value::Int32(-4),
            ])
            .unwrap(),
            Value::Text(String::new())
        );
    }

    #[test]
    fn test_split_part_zero_rejects_with_pg_error() {
        let err = split_part(vec![
            Value::Text("a,b,c".into()),
            Value::Text(",".into()),
            Value::Int32(0),
        ])
        .unwrap_err();
        let sql_err = err.downcast_ref::<SqlError>().expect("sql error");
        assert!(matches!(sql_err, SqlError::InvalidParameterValue { .. }));
        assert_eq!(err.to_string(), "field position must not be zero");
    }

    #[test]
    fn test_split_part_empty_delimiter_matches_pg() {
        assert_eq!(
            split_part(vec![
                Value::Text("abc".into()),
                Value::Text(String::new()),
                Value::Int32(1),
            ])
            .unwrap(),
            Value::Text("abc".into())
        );
        assert_eq!(
            split_part(vec![
                Value::Text("abc".into()),
                Value::Text(String::new()),
                Value::Int32(2),
            ])
            .unwrap(),
            Value::Text(String::new())
        );
        assert_eq!(
            split_part(vec![
                Value::Text("abc".into()),
                Value::Text(String::new()),
                Value::Int32(-1),
            ])
            .unwrap(),
            Value::Text("abc".into())
        );
    }

    #[test]
    fn test_string_registry_includes_starts_with() {
        assert!(crate::sql::expr::functions::get_registry().contains_key("STARTS_WITH"));
    }

    #[test]
    fn test_repeat_too_large() {
        let result = repeat(vec![Value::Text("x".into()), Value::Int32(2_000_000_000)]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requested length too large"));
    }

    #[test]
    fn test_lpad_too_large() {
        let result = lpad(vec![
            Value::Text("x".into()),
            Value::Int32(2_000_000_000),
            Value::Text("y".into()),
        ]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requested length too large"));
    }

    #[test]
    fn test_rpad_too_large() {
        let result = rpad(vec![
            Value::Text("x".into()),
            Value::Int32(2_000_000_000),
            Value::Text("y".into()),
        ]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requested length too large"));
    }

    #[test]
    fn test_lpad_multibyte_too_large() {
        // Each emoji is 4 bytes. 300M chars * 4 bytes = 1.2GB > 1GB limit.
        let result = lpad(vec![
            Value::Text("x".into()),
            Value::Int32(300_000_000),
            Value::Text("\u{1F600}".into()), // 😀 = 4 bytes
        ]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requested length too large"));
    }

    #[test]
    fn test_rpad_multibyte_too_large() {
        let result = rpad(vec![
            Value::Text("x".into()),
            Value::Int32(300_000_000),
            Value::Text("\u{1F600}".into()), // 😀 = 4 bytes
        ]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requested length too large"));
    }

    #[test]
    fn test_repeat_boundary_exact() {
        // Verify the guard allows output of exactly MAX_STRING_OUTPUT_BYTES.
        // We test with check_pad_output_size directly to avoid a real 1GB allocation.
        // repeat("x", MAX) → output = MAX bytes → should pass.
        let max = super::MAX_STRING_OUTPUT_BYTES;
        assert!(super::check_pad_output_size(0, max, "x", &['x']).is_ok());
    }

    #[test]
    fn test_repeat_boundary_one_over() {
        // MAX + 1 bytes should fail.
        let max = super::MAX_STRING_OUTPUT_BYTES;
        let result = super::check_pad_output_size(0, max + 1, "x", &['x']);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requested length too large"));
    }

    #[test]
    fn test_repeat_boundary_via_function() {
        // Also verify via the repeat() function itself: 2-byte string * n
        // where 2*n == MAX+1 → should fail (one byte over).
        let max = super::MAX_STRING_OUTPUT_BYTES;
        let n = (max / 2) + 1; // 2 * n = max + 1 (since max is odd, max/2 rounds down)
        let result = repeat(vec![Value::Text("ab".into()), Value::Int32(n as i32)]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requested length too large"));
    }

    #[test]
    fn test_truncation_pre_check_rejects_oversized() {
        // Verify check_output_byte_size rejects when byte size > MAX.
        let max = super::MAX_STRING_OUTPUT_BYTES;
        // Exactly at limit → ok.
        assert!(super::check_output_byte_size(max).is_ok());
        // One byte over → error.
        let result = super::check_output_byte_size(max + 1);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requested length too large"));
    }

    #[test]
    fn test_truncation_pre_check_boundary() {
        // Test check_pad_output_size boundary for the truncation scenario:
        // s already has the bytes, no fill needed.
        let max = super::MAX_STRING_OUTPUT_BYTES;
        // Equivalent to a string of MAX 1-byte chars truncated to MAX → exactly MAX bytes.
        assert!(super::check_pad_output_size(max, 0, "x", &['x']).is_ok());
        // MAX + 1 bytes → should fail.
        assert!(super::check_pad_output_size(max + 1, 0, "x", &['x']).is_err());
    }
}
