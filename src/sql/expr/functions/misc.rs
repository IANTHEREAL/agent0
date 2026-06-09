use crate::model::Value;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

const MAX_FORMAT_OUTPUT_BYTES: usize = 1_073_741_823; // 1GB - 1

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("COALESCE", coalesce);
    map.insert("NULLIF", nullif);
    map.insert("GREATEST", greatest);
    map.insert("LEAST", least);
    map.insert("FORMAT", format_fn);
}

pub fn coalesce(args: Vec<Value>) -> Result<Value> {
    for val in args {
        if !matches!(val, Value::Null) {
            return Ok(val);
        }
    }
    Ok(Value::Null)
}

pub fn nullif(args: Vec<Value>) -> Result<Value> {
    if args.len() >= 2 && crate::sql::expr::compare_values(&args[0], &args[1])? == 0 {
        Ok(Value::Null)
    } else {
        Ok(args.into_iter().next().unwrap_or(Value::Null))
    }
}

pub fn greatest(args: Vec<Value>) -> Result<Value> {
    let mut max = Value::Null;
    for val in args {
        if matches!(max, Value::Null) || crate::sql::expr::compare_values(&val, &max)? > 0 {
            max = val;
        }
    }
    Ok(max)
}

pub fn least(args: Vec<Value>) -> Result<Value> {
    let mut min = Value::Null;
    for val in args {
        if matches!(min, Value::Null) || crate::sql::expr::compare_values(&val, &min)? < 0 {
            min = val;
        }
    }
    Ok(min)
}

fn format_arg_as_string(arg: &Value, ty: char) -> Result<String> {
    match ty {
        's' => Ok(match arg {
            Value::Null => String::new(),
            v => v.to_string(),
        }),
        'I' => match arg {
            Value::Null => Err(anyhow!(
                "null values cannot be formatted as an SQL identifier"
            )),
            Value::Text(s) => Ok(crate::sql::quoting::quote_ident(s)),
            v => Ok(crate::sql::quoting::quote_ident(&v.to_string())),
        },
        'L' => Ok(match arg {
            Value::Null => "NULL".to_string(),
            Value::Text(s) => crate::sql::quoting::quote_literal(s),
            v => crate::sql::quoting::quote_literal(&v.to_string()),
        }),
        other => Err(anyhow!(
            "unrecognized format() type specifier \"{}\"",
            other
        )),
    }
}

fn format_specifier_error(spec: char) -> anyhow::Error {
    anyhow!(
        "unrecognized format() type specifier \"{}\"\nHINT:  For a single \"%\" use \"%%\".",
        spec
    )
}

fn check_format_output_byte_size(byte_len: usize) -> Result<()> {
    if byte_len > MAX_FORMAT_OUTPUT_BYTES {
        anyhow::bail!("requested length too large");
    }
    Ok(())
}

pub fn format_fn(args: Vec<Value>) -> Result<Value> {
    if args.is_empty() {
        return Ok(Value::Null);
    }

    let fmt = match &args[0] {
        Value::Text(s) => s.clone(),
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let fmt_args = &args[1..];
    let mut next_arg_index = 0usize;
    let mut out = String::with_capacity(fmt.len() + fmt_args.len() * 8);

    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            return Err(anyhow!("unterminated format() type specifier"));
        }
        if chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }

        let mut positional: Option<usize> = None;
        let pos_start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '$' && i > pos_start {
            let idx = chars[pos_start..i].iter().try_fold(0usize, |acc, &c| {
                acc.checked_mul(10)
                    .and_then(|value| value.checked_add(c.to_digit(10).unwrap() as usize))
                    .ok_or_else(|| anyhow!("invalid format() argument index"))
            })?;
            if idx == 0 {
                return Err(anyhow!("format() argument index starts at 1"));
            }
            positional = Some(idx - 1);
            i += 1;
        } else {
            i = pos_start;
        }

        let mut left_align = false;
        if i < chars.len() && chars[i] == '-' {
            left_align = true;
            i += 1;
        }

        let mut dynamic_width = false;
        let mut dynamic_width_position: Option<usize> = None;
        let width: Option<usize> = if i < chars.len() && chars[i] == '*' {
            dynamic_width = true;
            i += 1;
            let width_pos_start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i < chars.len() && chars[i] == '$' && i > width_pos_start {
                let idx: usize = chars[width_pos_start..i]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .map_err(|_| anyhow!("invalid format() width"))?;
                if idx == 0 {
                    return Err(anyhow!("format() argument index starts at 1"));
                }
                dynamic_width_position = Some(idx - 1);
                i += 1;
            }
            None
        } else {
            let width_start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i > width_start {
                Some(
                    chars[width_start..i]
                        .iter()
                        .collect::<String>()
                        .parse()
                        .map_err(|_| anyhow!("invalid format() width"))?,
                )
            } else {
                None
            }
        };

        if i < chars.len() && chars[i] == '.' {
            return Err(format_specifier_error('.'));
        }
        if i >= chars.len() {
            return Err(anyhow!("unterminated format() type specifier"));
        }

        let ty = chars[i];
        i += 1;
        if !matches!(ty, 's' | 'I' | 'L') {
            return Err(format_specifier_error(ty));
        }

        let mut width = width;
        if dynamic_width {
            let width_index = match dynamic_width_position {
                Some(idx) => {
                    next_arg_index = next_arg_index.max(idx.saturating_add(1));
                    idx
                }
                None => {
                    let idx = next_arg_index;
                    next_arg_index += 1;
                    idx
                }
            };
            let width_arg = fmt_args
                .get(width_index)
                .ok_or_else(|| anyhow!("too few arguments for format()"))?;
            let mut width_value = format_width_arg(width_arg)?;
            if width_value < 0 {
                left_align = true;
                width_value = width_value
                    .checked_neg()
                    .ok_or_else(|| anyhow!("invalid format() width"))?;
            }
            width =
                Some(usize::try_from(width_value).map_err(|_| anyhow!("invalid format() width"))?);
        }

        let arg_index = match positional {
            Some(idx) => {
                // PostgreSQL resets the implicit cursor after the value
                // argument, even if a positional width argument was consumed.
                next_arg_index = idx.saturating_add(1);
                idx
            }
            None => {
                let idx = next_arg_index;
                next_arg_index += 1;
                idx
            }
        };
        let arg = fmt_args
            .get(arg_index)
            .ok_or_else(|| anyhow!("too few arguments for format()"))?;
        let mut rendered = format_arg_as_string(arg, ty)?;

        if let Some(w) = width {
            let len = rendered.chars().take(w).count();
            if len < w {
                let pad_len = w - len;
                let padded_len = rendered.len().saturating_add(pad_len);
                check_format_output_byte_size(out.len().saturating_add(padded_len))?;
                let pad = " ".repeat(pad_len);
                if left_align {
                    rendered.push_str(&pad);
                } else {
                    let mut padded = String::with_capacity(padded_len);
                    padded.push_str(&pad);
                    padded.push_str(&rendered);
                    rendered = padded;
                }
            } else {
                check_format_output_byte_size(out.len().saturating_add(rendered.len()))?;
            }
        } else {
            check_format_output_byte_size(out.len().saturating_add(rendered.len()))?;
        }

        out.push_str(&rendered);
    }

    Ok(Value::Text(out))
}

fn format_width_arg(arg: &Value) -> Result<i64> {
    match arg {
        Value::Int32(value) => Ok(i64::from(*value)),
        Value::Int64(value) => Ok(*value),
        _ => Err(anyhow!("format() width argument must be integer")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coalesce() {
        assert_eq!(
            coalesce(vec![Value::Null, Value::Int32(1), Value::Int32(2)]).unwrap(),
            Value::Int32(1)
        );
        assert_eq!(
            coalesce(vec![Value::Null, Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            coalesce(vec![Value::Text("hello".into())]).unwrap(),
            Value::Text("hello".into())
        );
    }

    #[test]
    fn test_nullif() {
        assert_eq!(
            nullif(vec![Value::Int32(5), Value::Int32(5)]).unwrap(),
            Value::Null
        );
        assert_eq!(
            nullif(vec![Value::Int32(5), Value::Int32(3)]).unwrap(),
            Value::Int32(5)
        );
    }

    #[test]
    fn test_greatest() {
        assert_eq!(
            greatest(vec![Value::Int32(1), Value::Int32(5), Value::Int32(3)]).unwrap(),
            Value::Int32(5)
        );
    }

    #[test]
    fn test_least() {
        assert_eq!(
            least(vec![Value::Int32(1), Value::Int32(5), Value::Int32(3)]).unwrap(),
            Value::Int32(1)
        );
    }

    #[test]
    fn test_format_basic_specifiers() {
        assert_eq!(
            format_fn(vec![
                Value::Text("%s %I %L".into()),
                Value::Text("hi".into()),
                Value::Text("col name".into()),
                Value::Text("it's".into())
            ])
            .unwrap(),
            Value::Text("hi \"col name\" 'it''s'".into())
        );
    }

    #[test]
    fn test_format_positional_and_width() {
        assert_eq!(
            format_fn(vec![
                Value::Text("%2$s %1$s %10s %-5s".into()),
                Value::Text("A".into()),
                Value::Text("B".into()),
                Value::Text("xy".into()),
                Value::Text("z".into())
            ])
            .unwrap(),
            Value::Text("B A          B xy   ".into())
        );
        assert_eq!(
            format_fn(vec![
                Value::Text("|%*s|%-*s|".into()),
                Value::Int32(3),
                Value::Text("x".into()),
                Value::Int64(4),
                Value::Text("y".into()),
            ])
            .unwrap(),
            Value::Text("|  x|y   |".into())
        );
        assert_eq!(
            format_fn(vec![
                Value::Text("|%1$*2$s|".into()),
                Value::Text("x".into()),
                Value::Int32(3),
            ])
            .unwrap(),
            Value::Text("|  x|".into())
        );
        assert_eq!(
            format_fn(vec![
                Value::Text("%1$s|%s".into()),
                Value::Text("x".into()),
                Value::Text("y".into()),
                Value::Text("z".into()),
            ])
            .unwrap(),
            Value::Text("x|y".into())
        );
        assert_eq!(
            format_fn(vec![
                Value::Text("%2$s|%s".into()),
                Value::Text("x".into()),
                Value::Text("y".into()),
                Value::Text("z".into()),
            ])
            .unwrap(),
            Value::Text("y|z".into())
        );
        assert_eq!(
            format_fn(vec![
                Value::Text("|%1$*2$s|%s|".into()),
                Value::Text("x".into()),
                Value::Int32(3),
                Value::Text("y".into()),
            ])
            .unwrap(),
            Value::Text("|  x|3|".into())
        );
        assert_eq!(
            format_fn(vec![Value::Text("%L".into()), Value::Text("a\\b".into())]).unwrap(),
            Value::Text(r"E'a\\b'".into())
        );
        assert_eq!(
            format_fn(vec![
                Value::Text("%s|%L".into()),
                Value::Float64(f64::INFINITY),
                Value::Float64(f64::NEG_INFINITY),
            ])
            .unwrap(),
            Value::Text("Infinity|'-Infinity'".into())
        );
        let err = format_fn(vec![
            Value::Text("%1073741824s".into()),
            Value::Text("x".into()),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("requested length too large"));
        let err = format_fn(vec![
            Value::Text("%*s".into()),
            Value::Int64(1_073_741_824),
            Value::Text("x".into()),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("requested length too large"));
    }

    #[test]
    fn test_format_renders_bytea_using_pg_text() {
        assert_eq!(
            format_fn(vec![
                Value::Text("%s/%L".into()),
                Value::Bytes(vec![0xde, 0xad]),
                Value::Bytes(vec![0xbe, 0xef]),
            ])
            .unwrap(),
            Value::Text(r"\xdead/E'\\xbeef'".into())
        );
    }
}
