use crate::model::Value;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

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
        if matches!(max, Value::Null) {
            max = val;
        } else if crate::sql::expr::compare_values(&val, &max)? > 0 {
            max = val;
        }
    }
    Ok(max)
}

pub fn least(args: Vec<Value>) -> Result<Value> {
    let mut min = Value::Null;
    for val in args {
        if matches!(min, Value::Null) {
            min = val;
        } else if crate::sql::expr::compare_values(&val, &min)? < 0 {
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
            let idx: usize = chars[pos_start..i]
                .iter()
                .collect::<String>()
                .parse()
                .map_err(|_| anyhow!("invalid format() argument index"))?;
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

        let width_start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        let width: Option<usize> = if i > width_start {
            Some(
                chars[width_start..i]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .map_err(|_| anyhow!("invalid format() width"))?,
            )
        } else {
            None
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

        let arg_index = match positional {
            Some(idx) => {
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
            let len = rendered.chars().count();
            if len < w {
                let pad = " ".repeat(w - len);
                if left_align {
                    rendered.push_str(&pad);
                } else {
                    rendered = format!("{pad}{rendered}");
                }
            }
        }

        out.push_str(&rendered);
    }

    Ok(Value::Text(out))
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
    }
}
