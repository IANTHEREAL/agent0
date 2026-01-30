use crate::types::Value;
use anyhow::Result;
use std::collections::HashMap;

use super::SqlFn;

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
    map.insert("SPLIT_PART", split_part);
    map.insert("TRANSLATE", translate);
    map.insert("MD5", md5);
    map.insert("QUOTE_IDENT", quote_ident);
    map.insert("QUOTE_LITERAL", quote_literal);
    map.insert("QUOTE_NULLABLE", quote_nullable);
}

pub fn upper(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(s.to_uppercase())),
        Some(v) => Ok(v),
        None => Ok(Value::Null),
    }
}

pub fn lower(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(s.to_lowercase())),
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
        Some(Value::Text(s)) => Ok(Value::Int32((s.as_bytes().len() * 8) as i32)),
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
        _ => String::new(),
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
        Some(Value::Int32(n)) => n.max(0) as usize,
        Some(Value::Int64(n)) => n.max(0) as usize,
        _ => return Ok(Value::Null),
    };
    Ok(Value::Text(s.chars().take(n).collect()))
}

pub fn right(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let n = match iter.next() {
        Some(Value::Int32(n)) => n.max(0) as usize,
        Some(Value::Int64(n)) => n.max(0) as usize,
        _ => return Ok(Value::Null),
    };
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    Ok(Value::Text(chars[start..].iter().collect()))
}

pub fn trim(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(s.trim().to_string())),
        Some(Value::Null) => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

pub fn btrim(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let chars_to_trim: Option<String> = iter.next().and_then(|v| match v {
        Value::Text(s) => Some(s),
        _ => None,
    });

    match chars_to_trim {
        Some(chars) => {
            let char_set: std::collections::HashSet<char> = chars.chars().collect();
            Ok(Value::Text(
                s.trim_matches(|c| char_set.contains(&c)).to_string(),
            ))
        }
        None => Ok(Value::Text(s.trim().to_string())),
    }
}

pub fn ltrim(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let chars_to_trim: Option<String> = iter.next().and_then(|v| match v {
        Value::Text(s) => Some(s),
        _ => None,
    });
    match chars_to_trim {
        Some(chars) => {
            let char_set: std::collections::HashSet<char> = chars.chars().collect();
            Ok(Value::Text(
                s.trim_start_matches(|c| char_set.contains(&c)).to_string(),
            ))
        }
        None => Ok(Value::Text(s.trim_start().to_string())),
    }
}

pub fn rtrim(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let chars_to_trim: Option<String> = iter.next().and_then(|v| match v {
        Value::Text(s) => Some(s),
        _ => None,
    });
    match chars_to_trim {
        Some(chars) => {
            let char_set: std::collections::HashSet<char> = chars.chars().collect();
            Ok(Value::Text(
                s.trim_end_matches(|c| char_set.contains(&c)).to_string(),
            ))
        }
        None => Ok(Value::Text(s.trim_end().to_string())),
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
        Some(Value::Int64(n)) => n.max(0) as usize,
        _ => return Ok(Value::Null),
    };
    let fill = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => " ".to_string(),
    };
    let char_count = s.chars().count();
    if char_count >= len {
        Ok(Value::Text(s.chars().take(len).collect()))
    } else {
        let fill_chars: Vec<char> = fill.chars().collect();
        if fill_chars.is_empty() {
            return Ok(Value::Text(s));
        }
        let mut result = String::new();
        let needed = len - char_count;
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
        Some(Value::Int64(n)) => n.max(0) as usize,
        _ => return Ok(Value::Null),
    };
    let fill = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => " ".to_string(),
    };
    let char_count = s.chars().count();
    if char_count >= len {
        Ok(Value::Text(s.chars().take(len).collect()))
    } else {
        let fill_chars: Vec<char> = fill.chars().collect();
        if fill_chars.is_empty() {
            return Ok(Value::Text(s));
        }
        let mut result = s;
        let needed = len - char_count;
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
        Some(Value::Int64(n)) => n.max(0) as usize,
        _ => return Ok(Value::Null),
    };
    Ok(Value::Text(s.repeat(n)))
}

pub fn replace(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let from = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Text(s)),
    };
    let to = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => String::new(),
    };
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
                    result.extend(c.to_uppercase());
                    capitalize_next = false;
                } else {
                    result.extend(c.to_lowercase());
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
    match args.into_iter().next() {
        Some(Value::Int32(n)) => char::from_u32(n as u32)
            .map(|c| Value::Text(c.to_string()))
            .ok_or_else(|| anyhow::anyhow!("Invalid character code: {}", n)),
        Some(Value::Int64(n)) => char::from_u32(n as u32)
            .map(|c| Value::Text(c.to_string()))
            .ok_or_else(|| anyhow::anyhow!("Invalid character code: {}", n)),
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
        _ => return Ok(Value::Int32(0)),
    };
    match haystack.find(&needle) {
        Some(pos) => {
            let char_pos = haystack[..pos].chars().count() + 1;
            Ok(Value::Int32(char_pos as i32))
        }
        None => Ok(Value::Int32(0)),
    }
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
        Some(Value::Int32(n)) => n,
        Some(Value::Int64(n)) => n as i32,
        _ => return Ok(Value::Null),
    };
    if field_num <= 0 {
        return Ok(Value::Text(String::new()));
    }
    let parts: Vec<&str> = s.split(&delimiter).collect();
    let idx = (field_num - 1) as usize;
    Ok(Value::Text(parts.get(idx).unwrap_or(&"").to_string()))
}

pub fn translate(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let s = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Null),
    };
    let from = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => return Ok(Value::Text(s)),
    };
    let to = match iter.next() {
        Some(Value::Text(s)) => s,
        _ => String::new(),
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

pub fn md5(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) => Ok(Value::Null),
        Some(Value::Text(s)) => Ok(Value::Text(format!("{:x}", md5::compute(s.as_bytes())))),
        Some(Value::Bytes(b)) => Ok(Value::Text(format!("{:x}", md5::compute(&b)))),
        Some(v) => Ok(Value::Text(format!(
            "{:x}",
            md5::compute(v.to_string().as_bytes())
        ))),
        None => Ok(Value::Null),
    }
}

pub fn quote_ident(args: Vec<Value>) -> Result<Value> {
    let val = match args.into_iter().next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    Ok(Value::Text(quote_ident_impl(&val)))
}

pub fn quote_literal(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(quote_literal_impl(&s))),
        Some(Value::Null) => Ok(Value::Null),
        Some(v) => Ok(Value::Text(quote_literal_impl(&v.to_string()))),
        None => Ok(Value::Null),
    }
}

pub fn quote_nullable(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) => Ok(Value::Text("NULL".to_string())),
        Some(Value::Text(s)) => Ok(Value::Text(quote_literal_impl(&s))),
        Some(v) => Ok(Value::Text(quote_literal_impl(&v.to_string()))),
        None => Ok(Value::Text("NULL".to_string())),
    }
}

fn quote_literal_impl(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn quote_ident_impl(ident: &str) -> String {
    let needs_quote = ident.is_empty() || !is_simple_unquoted_ident(ident) || is_sql_keyword(ident);
    if needs_quote {
        format!("\"{}\"", ident.replace('"', "\"\""))
    } else {
        ident.to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_md5_null() {
        assert_eq!(md5(vec![Value::Null]).unwrap(), Value::Null);
    }

    #[test]
    fn test_quote_ident_non_text_arg() {
        assert_eq!(
            quote_ident(vec![Value::Int32(123)]).unwrap(),
            Value::Text("\"123\"".to_string())
        );
    }

    #[test]
    fn test_upper() {
        assert_eq!(
            upper(vec![Value::Text("hello".into())]).unwrap(),
            Value::Text("HELLO".into())
        );
    }

    #[test]
    fn test_lower() {
        assert_eq!(
            lower(vec![Value::Text("HELLO".into())]).unwrap(),
            Value::Text("hello".into())
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
    fn test_left_right() {
        assert_eq!(
            left(vec![Value::Text("hello".into()), Value::Int32(3)]).unwrap(),
            Value::Text("hel".into())
        );
        assert_eq!(
            right(vec![Value::Text("hello".into()), Value::Int32(3)]).unwrap(),
            Value::Text("llo".into())
        );
    }

    #[test]
    fn test_trim() {
        assert_eq!(
            trim(vec![Value::Text("  hello  ".into())]).unwrap(),
            Value::Text("hello".into())
        );
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
    }
}
