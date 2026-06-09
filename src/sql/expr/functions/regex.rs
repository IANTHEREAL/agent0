use crate::model::Value;
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("REGEXP_REPLACE", regexp_replace);
    map.insert("REGEXP_MATCH", regexp_match);
    map.insert("REGEXP_MATCHES", regexp_matches);
    map.insert("REGEXP_SPLIT_TO_ARRAY", regexp_split_to_array);
}

fn summarize_regex_error(err: &regex::Error) -> String {
    let text = err.to_string();
    let last_line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text.as_str())
        .trim();
    match last_line.strip_prefix("error: ").unwrap_or(last_line) {
        "unclosed group" => "parentheses () not balanced".to_owned(),
        "unclosed character class" => "brackets [] not balanced".to_owned(),
        other => other.to_owned(),
    }
}

fn summarize_fancy_regex_error(err: &fancy_regex::Error) -> String {
    match err {
        fancy_regex::Error::ParseError(_, parse_error) => match parse_error {
            fancy_regex::ParseError::UnclosedOpenParen => "parentheses () not balanced".to_owned(),
            fancy_regex::ParseError::InvalidClass => "brackets [] not balanced".to_owned(),
            _ => err.to_string(),
        },
        other => other.to_string(),
    }
}

pub(crate) fn invalid_regular_expression_error(err: &regex::Error) -> anyhow::Error {
    SqlError::InvalidRegularExpression {
        message: format!("invalid regular expression: {}", summarize_regex_error(err)),
    }
    .into()
}

pub(crate) fn invalid_fancy_regular_expression_error(err: &fancy_regex::Error) -> anyhow::Error {
    SqlError::InvalidRegularExpression {
        message: format!(
            "invalid regular expression: {}",
            summarize_fancy_regex_error(err)
        ),
    }
    .into()
}

fn pg_regex_invalid_escape_sequence_error() -> anyhow::Error {
    SqlError::InvalidRegularExpression {
        message: "invalid regular expression: invalid escape \\ sequence".to_owned(),
    }
    .into()
}

fn pg_regex_invalid_embedded_option_error() -> anyhow::Error {
    SqlError::InvalidRegularExpression {
        message: "invalid regular expression: invalid embedded option".to_owned(),
    }
    .into()
}

fn invalid_regex_option_error(flag: char) -> anyhow::Error {
    SqlError::InvalidParameterValue {
        message: format!(
            "invalid regular expression option: \"{}\"",
            flag.escape_default()
        ),
    }
    .into()
}

fn unsupported_global_regex_option_error(function_name: &str) -> anyhow::Error {
    anyhow!("{function_name}() does not support the \"global\" option")
}

fn apply_pg_newline_mode_flag(
    flag: char,
    multiline: &mut bool,
    dot_matches_new_line: &mut bool,
) -> bool {
    match flag {
        'm' | 'n' => {
            *multiline = true;
            *dot_matches_new_line = false;
            true
        }
        'p' => {
            *multiline = false;
            *dot_matches_new_line = false;
            true
        }
        'w' => {
            *multiline = true;
            *dot_matches_new_line = true;
            true
        }
        's' => {
            *multiline = false;
            *dot_matches_new_line = true;
            true
        }
        _ => false,
    }
}

fn build_pg_regex_inline_flags(
    case_insensitive: bool,
    multiline: bool,
    dot_matches_new_line: bool,
    ignore_whitespace: bool,
) -> String {
    let mut inline_flags = String::new();
    if case_insensitive {
        inline_flags.push('i');
    }
    if multiline {
        inline_flags.push('m');
    }
    if dot_matches_new_line {
        inline_flags.push('s');
    }
    if ignore_whitespace {
        inline_flags.push('x');
    }
    inline_flags
}

fn pg_regex_has_unsupported_rust_escape(pattern: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let mut idx = 0usize;

    while idx < chars.len() {
        if chars[idx] != '\\' {
            idx += 1;
            continue;
        }

        let slash_start = idx;
        while idx < chars.len() && chars[idx] == '\\' {
            idx += 1;
        }
        let slash_count = idx - slash_start;
        if slash_count.is_multiple_of(2) || idx >= chars.len() {
            continue;
        }

        match chars[idx] {
            'p' | 'P' => return true,
            'x' if matches!(chars.get(idx + 1), Some('{')) => return true,
            _ => {}
        }
        idx += 1;
    }

    false
}

fn validate_pg_regex_embedded_options(pattern: &str) -> Result<()> {
    const PG_INLINE_OPTION_FLAGS: &str = "bceimnpqstwx";

    let chars: Vec<char> = pattern.chars().collect();
    let mut idx = 0usize;
    let mut escaped = false;
    let mut in_class = false;
    let mut saw_prefix_options = false;

    while idx < chars.len() {
        let ch = chars[idx];
        if escaped {
            escaped = false;
            idx += 1;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            idx += 1;
            continue;
        }
        if in_class {
            if ch == ']' {
                in_class = false;
            }
            idx += 1;
            continue;
        }
        if ch == '[' {
            in_class = true;
            idx += 1;
            continue;
        }
        if ch != '(' || !matches!(chars.get(idx + 1), Some('?')) {
            idx += 1;
            continue;
        }

        match chars.get(idx + 2).copied() {
            Some(':') | Some('=') | Some('!') => {}
            Some('<') if matches!(chars.get(idx + 3), Some('=') | Some('!')) => {}
            Some('<') | Some('P') => return Err(pg_regex_invalid_embedded_option_error()),
            Some(flag) if flag.is_ascii_alphabetic() || flag == '-' => {
                let mut end = idx + 2;
                while end < chars.len() && chars[end] != ')' && chars[end] != ':' {
                    if !PG_INLINE_OPTION_FLAGS.contains(chars[end]) {
                        return Err(pg_regex_invalid_embedded_option_error());
                    }
                    end += 1;
                }
                if matches!(chars.get(end), Some(':')) {
                    return Err(pg_regex_invalid_embedded_option_error());
                }
                if matches!(chars.get(end), Some(')')) {
                    if idx != 0 || saw_prefix_options {
                        return Err(pg_regex_invalid_embedded_option_error());
                    }
                    saw_prefix_options = true;
                }
            }
            _ => {}
        }
        idx += 1;
    }

    Ok(())
}

pub(crate) fn translate_pg_regex_escapes(pattern: &str) -> Result<String> {
    if pg_regex_has_unsupported_rust_escape(pattern) {
        return Err(pg_regex_invalid_escape_sequence_error());
    }
    validate_pg_regex_embedded_options(pattern)?;

    let mut translated = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            translated.push(ch);
            continue;
        }

        let mut slash_count = 1usize;
        while matches!(chars.peek(), Some('\\')) {
            chars.next();
            slash_count += 1;
        }

        match chars.peek().copied() {
            Some('b') if slash_count % 2 == 1 => {
                for _ in 0..slash_count - 1 {
                    translated.push('\\');
                }
                chars.next();
                translated.push('\u{0008}');
            }
            Some('y') if slash_count % 2 == 1 => {
                for _ in 0..slash_count - 1 {
                    translated.push('\\');
                }
                chars.next();
                translated.push_str(r"\b");
            }
            Some('Y') if slash_count % 2 == 1 => {
                for _ in 0..slash_count - 1 {
                    translated.push('\\');
                }
                chars.next();
                translated.push_str(r"\B");
            }
            Some('m') if slash_count % 2 == 1 => {
                for _ in 0..slash_count - 1 {
                    translated.push('\\');
                }
                chars.next();
                translated.push_str(r"\b(?=\w)");
            }
            Some('M') if slash_count % 2 == 1 => {
                for _ in 0..slash_count - 1 {
                    translated.push('\\');
                }
                chars.next();
                translated.push_str(r"(?<=\w)\b");
            }
            _ => {
                for _ in 0..slash_count {
                    translated.push('\\');
                }
            }
        }
    }

    Ok(translated)
}

fn parse_regexp_pattern_flags(
    flags: &str,
    function_name: &str,
    reject_global: bool,
) -> Result<RegexpReplaceFlags> {
    // PostgreSQL defaults to the non-newline-sensitive "s" mode, so dot
    // matches newline unless the caller explicitly asks for n/p behavior.
    let mut parsed = RegexpReplaceFlags {
        dot_matches_new_line: true,
        ..RegexpReplaceFlags::default()
    };

    for flag in flags.chars() {
        match flag {
            'c' => parsed.case_insensitive = false,
            'g' if reject_global => {
                return Err(unsupported_global_regex_option_error(function_name));
            }
            'g' => parsed.global = true,
            'i' => parsed.case_insensitive = true,
            'q' => parsed.literal_pattern = true,
            't' => {}
            'x' => parsed.ignore_whitespace = true,
            flag if apply_pg_newline_mode_flag(
                flag,
                &mut parsed.multiline,
                &mut parsed.dot_matches_new_line,
            ) => {}
            _ => return Err(invalid_regex_option_error(flag)),
        }
    }

    Ok(parsed)
}

fn build_pg_regex_pattern(pattern: &str, flags: RegexpReplaceFlags) -> Result<String> {
    let pattern = if flags.literal_pattern {
        regex::escape(pattern)
    } else {
        translate_pg_regex_escapes(pattern)?
    };
    let inline_flags = build_pg_regex_inline_flags(
        flags.case_insensitive,
        flags.multiline,
        flags.dot_matches_new_line,
        flags.ignore_whitespace,
    );
    Ok(if inline_flags.is_empty() {
        pattern
    } else {
        format!("(?{inline_flags}){pattern}")
    })
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RegexpReplaceFlags {
    global: bool,
    case_insensitive: bool,
    multiline: bool,
    dot_matches_new_line: bool,
    ignore_whitespace: bool,
    literal_pattern: bool,
}

fn parse_regexp_replace_flags(flags: &str) -> Result<RegexpReplaceFlags> {
    parse_regexp_pattern_flags(flags, "regexp_replace", false)
}

fn append_checked(out: &mut String, value: &str) -> Result<()> {
    super::string::check_output_byte_size(out.len().saturating_add(value.len()))?;
    out.push_str(value);
    Ok(())
}

fn push_char_checked(out: &mut String, value: char) -> Result<()> {
    super::string::check_output_byte_size(out.len().saturating_add(value.len_utf8()))?;
    out.push(value);
    Ok(())
}

fn expand_pg_regexp_replacement(
    out: &mut String,
    replacement: &str,
    captures: &fancy_regex::Captures<'_>,
) -> Result<()> {
    let mut chars = replacement.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some('&') => {
                    if let Some(matched) = captures.get(0) {
                        append_checked(out, matched.as_str())?;
                    }
                }
                Some('0') => {
                    append_checked(out, "\\0")?;
                }
                Some(digit @ '1'..='9') => {
                    if let Some(group) = captures.get(digit.to_digit(10).unwrap() as usize) {
                        append_checked(out, group.as_str())?;
                    }
                }
                Some('\\') => append_checked(out, "\\")?,
                Some(other) => push_char_checked(out, other)?,
                None => append_checked(out, "\\")?,
            },
            other => push_char_checked(out, other)?,
        }
    }

    Ok(())
}

fn apply_pg_regexp_replace(
    regex: &fancy_regex::Regex,
    source: &str,
    replacement: &str,
    global: bool,
) -> Result<String> {
    if !global {
        if let Some(captures) = regex
            .captures(source)
            .map_err(|err| invalid_fancy_regular_expression_error(&err))?
        {
            if let Some(matched) = captures.get(0) {
                let mut out = String::new();
                append_checked(&mut out, &source[..matched.start()])?;
                expand_pg_regexp_replacement(&mut out, replacement, &captures)?;
                append_checked(&mut out, &source[matched.end()..])?;
                return Ok(out);
            }
        }
        super::string::check_output_byte_size(source.len())?;
        return Ok(source.to_string());
    }

    let mut out = String::new();
    let mut last_end = 0;
    for captures in regex.captures_iter(source) {
        let captures = captures.map_err(|err| invalid_fancy_regular_expression_error(&err))?;
        let Some(matched) = captures.get(0) else {
            continue;
        };
        append_checked(&mut out, &source[last_end..matched.start()])?;
        expand_pg_regexp_replacement(&mut out, replacement, &captures)?;
        last_end = matched.end();
    }
    append_checked(&mut out, &source[last_end..])?;
    Ok(out)
}

pub fn regexp_replace(args: Vec<Value>) -> Result<Value> {
    if !(3..=4).contains(&args.len()) {
        return Err(anyhow!(
            "regexp_replace requires 3 or 4 arguments, got {}",
            args.len()
        ));
    }

    let mut iter = args.into_iter();
    let source = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let pattern = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Text(source)),
    };
    let replacement = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let flags = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        _ => String::new(),
    };
    let parsed_flags = parse_regexp_replace_flags(&flags)?;
    let regex_pattern = build_pg_regex_pattern(&pattern, parsed_flags)?;
    let re = fancy_regex::Regex::new(&regex_pattern)
        .map_err(|e| invalid_fancy_regular_expression_error(&e))?;
    Ok(Value::Text(apply_pg_regexp_replace(
        &re,
        &source,
        &replacement,
        parsed_flags.global,
    )?))
}

pub fn regexp_match(args: Vec<Value>) -> Result<Value> {
    regexp_match_array(args, "regexp_match")
}

pub fn regexp_matches(args: Vec<Value>) -> Result<Value> {
    regexp_match_array(args, "regexp_matches")
}

fn regexp_match_array(args: Vec<Value>, function_name: &str) -> Result<Value> {
    let mut iter = args.into_iter();
    let source = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let pattern = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Array(vec![])),
    };
    let flags = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => String::new(),
    };
    let parsed_flags = parse_regexp_pattern_flags(&flags, function_name, true)?;
    let regex_pattern = build_pg_regex_pattern(&pattern, parsed_flags)?;
    match fancy_regex::Regex::new(&regex_pattern) {
        Ok(re) => {
            if let Some(caps) = re
                .captures(&source)
                .map_err(|err| invalid_fancy_regular_expression_error(&err))?
            {
                let matches: Vec<Value> = caps
                    .iter()
                    .skip(if caps.len() > 1 { 1 } else { 0 })
                    .map(|m| match m {
                        Some(m) => Value::Text(m.as_str().to_string()),
                        None => Value::Null,
                    })
                    .collect();
                if matches.is_empty() {
                    if let Some(m) = caps.get(0) {
                        Ok(Value::Array(vec![Value::Text(m.as_str().to_string())]))
                    } else {
                        Ok(Value::Null)
                    }
                } else {
                    Ok(Value::Array(matches))
                }
            } else {
                Ok(Value::Null)
            }
        }
        Err(e) => Err(invalid_fancy_regular_expression_error(&e)),
    }
}

pub fn regexp_split_to_array(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let source = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let pattern = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => return Ok(Value::Null),
    };
    let flags = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v.to_string(),
        None => String::new(),
    };
    let parsed_flags = parse_regexp_pattern_flags(&flags, "regexp_split_to_array", true)?;
    let regex_pattern = build_pg_regex_pattern(&pattern, parsed_flags)?;
    match fancy_regex::Regex::new(&regex_pattern) {
        Ok(re) => {
            let mut parts: Vec<String> = re
                .split(&source)
                .map(|part| {
                    part.map(|s| s.to_string())
                        .map_err(|err| invalid_fancy_regular_expression_error(&err))
                })
                .collect::<Result<Vec<_>>>()?;

            // PostgreSQL suppresses boundary empties from zero-width matches at the
            // start/end of the input while preserving interior zero-width splits.
            if matches!(
                re.find(&source)
                    .map_err(|err| invalid_fancy_regular_expression_error(&err))?,
                Some(m) if m.start() == 0 && m.end() == 0
            ) && matches!(parts.first(), Some(s) if s.is_empty())
            {
                parts.remove(0);
            }
            let last_match = re
                .find_iter(&source)
                .last()
                .transpose()
                .map_err(|err| invalid_fancy_regular_expression_error(&err))?;
            if matches!(last_match, Some(m) if m.start() == source.len() && m.end() == source.len())
                && matches!(parts.last(), Some(s) if s.is_empty())
            {
                parts.pop();
            }

            Ok(Value::Array(parts.into_iter().map(Value::Text).collect()))
        }
        Err(e) => Err(invalid_fancy_regular_expression_error(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regexp_replace() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("hello world".into()),
                Value::Text("world".into()),
                Value::Text("rust".into()),
            ])
            .unwrap(),
            Value::Text("hello rust".into())
        );
    }

    #[test]
    fn test_regexp_replace_global() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("aaa".into()),
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Text("g".into()),
            ])
            .unwrap(),
            Value::Text("bbb".into())
        );
    }

    #[test]
    fn test_regexp_replace_case_insensitive() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("Hello World".into()),
                Value::Text("world".into()),
                Value::Text("rust".into()),
                Value::Text("i".into()),
            ])
            .unwrap(),
            Value::Text("Hello rust".into())
        );
    }

    #[test]
    fn test_regexp_replace_matches_pg_null_short_circuit_order() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("abc".into()),
                Value::Text("([".into()),
                Value::Null,
            ])
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            regexp_replace(vec![
                Value::Text("abc".into()),
                Value::Text("([".into()),
                Value::Text("x".into()),
                Value::Null,
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_regexp_replace_validates_flags_and_patterns_like_pg() {
        let invalid_flag_err = regexp_replace(vec![
            Value::Text("abc".into()),
            Value::Text("a".into()),
            Value::Text("x".into()),
            Value::Text("z".into()),
        ])
        .unwrap_err();
        assert_eq!(
            invalid_flag_err.to_string(),
            "invalid regular expression option: \"z\""
        );
        let invalid_flag_sql_err = invalid_flag_err
            .downcast_ref::<SqlError>()
            .expect("regexp_replace invalid flag should preserve SQLSTATE");
        assert_eq!(invalid_flag_sql_err.sqlstate(), "22023");
        let invalid_pattern_err = regexp_replace(vec![
            Value::Text("abc".into()),
            Value::Text("([".into()),
            Value::Text("x".into()),
        ])
        .unwrap_err();
        assert_eq!(
            invalid_pattern_err.to_string(),
            "invalid regular expression: brackets [] not balanced"
        );
        let invalid_pattern_sql_err = invalid_pattern_err
            .downcast_ref::<SqlError>()
            .expect("regexp_replace invalid pattern should preserve SQLSTATE");
        assert_eq!(invalid_pattern_sql_err.sqlstate(), "2201B");
    }

    #[test]
    fn test_regexp_replace_supports_extended_flag() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("Ab".into()),
                Value::Text("a b".into()),
                Value::Text("X".into()),
                Value::Text("ix".into()),
            ])
            .unwrap(),
            Value::Text("X".into())
        );
    }

    #[test]
    fn test_regexp_replace_supports_multiline_flag() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("a\nb".into()),
                Value::Text("^b$".into()),
                Value::Text("X".into()),
                Value::Text("m".into()),
            ])
            .unwrap(),
            Value::Text("a\nX".into())
        );
    }

    #[test]
    fn test_regexp_replace_defaults_to_pg_newline_mode() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("a\nb".into()),
                Value::Text("a.b".into()),
                Value::Text("X".into()),
            ])
            .unwrap(),
            Value::Text("X".into())
        );
    }

    #[test]
    fn test_regexp_replace_supports_partial_newline_sensitive_flag() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("a\nb".into()),
                Value::Text(".".into()),
                Value::Text("X".into()),
                Value::Text("gp".into()),
            ])
            .unwrap(),
            Value::Text("X\nX".into())
        );
        assert_eq!(
            regexp_replace(vec![
                Value::Text("a\nb".into()),
                Value::Text("^b$".into()),
                Value::Text("X".into()),
                Value::Text("gp".into()),
            ])
            .unwrap(),
            Value::Text("a\nb".into())
        );
    }

    #[test]
    fn test_regexp_replace_supports_inverse_partial_newline_sensitive_flag() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("a\nb".into()),
                Value::Text(".".into()),
                Value::Text("X".into()),
                Value::Text("gw".into()),
            ])
            .unwrap(),
            Value::Text("XXX".into())
        );
        assert_eq!(
            regexp_replace(vec![
                Value::Text("a\nb".into()),
                Value::Text("^b$".into()),
                Value::Text("X".into()),
                Value::Text("gw".into()),
            ])
            .unwrap(),
            Value::Text("a\nX".into())
        );
    }

    #[test]
    fn test_regexp_replace_newline_flags_use_last_wins_semantics() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("a\nb".into()),
                Value::Text("a.b".into()),
                Value::Text("X".into()),
                Value::Text("sn".into()),
            ])
            .unwrap(),
            Value::Text("a\nb".into())
        );
        assert_eq!(
            regexp_replace(vec![
                Value::Text("a\nb".into()),
                Value::Text("a.b".into()),
                Value::Text("X".into()),
                Value::Text("ns".into()),
            ])
            .unwrap(),
            Value::Text("X".into())
        );
    }

    #[test]
    fn test_regexp_replace_supports_quote_literal_flag() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("a.c".into()),
                Value::Text("a.c".into()),
                Value::Text("X".into()),
                Value::Text("q".into()),
            ])
            .unwrap(),
            Value::Text("X".into())
        );
    }

    #[test]
    fn test_regexp_replace_accepts_tight_syntax_flag() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("(a)".into()),
                Value::Text("(".into()),
                Value::Text("X".into()),
                Value::Text("t".into()),
            ])
            .unwrap_err()
            .to_string(),
            "invalid regular expression: parentheses () not balanced"
        );
    }

    #[test]
    fn test_regexp_replace_uses_postgres_backref_syntax() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("foobarbaz".into()),
                Value::Text("b(..)".into()),
                Value::Text("X\\1Y".into()),
                Value::Text("g".into()),
            ])
            .unwrap(),
            Value::Text("fooXarYXazY".into())
        );
    }

    #[test]
    fn test_regexp_replace_treats_dollar_as_literal() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("bar".into()),
                Value::Text("(bar)".into()),
                Value::Text("$1".into()),
            ])
            .unwrap(),
            Value::Text("$1".into())
        );
    }

    #[test]
    fn test_regexp_replace_supports_pg_backrefs_and_lookaround() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("aa".into()),
                Value::Text(r"(.)\1".into()),
                Value::Text("X".into()),
            ])
            .unwrap(),
            Value::Text("X".into())
        );
        assert_eq!(
            regexp_replace(vec![
                Value::Text("ab".into()),
                Value::Text("a(?=b)".into()),
                Value::Text("X".into()),
            ])
            .unwrap(),
            Value::Text("Xb".into())
        );
    }

    #[test]
    fn test_regexp_rejects_pg_invalid_rust_regex_syntax() {
        for pattern in [r"(?i:a)", r"(?U)a+", r"\p{L}", r"\x{41}", r"(?P<x>a)"] {
            let err = regexp_match(vec![Value::Text("A".into()), Value::Text(pattern.into())])
                .unwrap_err();
            assert_eq!(
                err.downcast_ref::<crate::sql::error::SqlError>()
                    .expect("sql error")
                    .sqlstate(),
                "2201B"
            );
        }
    }

    #[test]
    fn test_regexp_replace_supports_entire_match_escape() {
        assert_eq!(
            regexp_replace(vec![
                Value::Text("bar".into()),
                Value::Text("bar".into()),
                Value::Text("<\\&>".into()),
            ])
            .unwrap(),
            Value::Text("<bar>".into())
        );
        assert_eq!(
            regexp_replace(vec![
                Value::Text("abc".into()),
                Value::Text("(a)(b)".into()),
                Value::Text("\\10".into()),
            ])
            .unwrap(),
            Value::Text("a0c".into())
        );
        assert_eq!(
            regexp_replace(vec![
                Value::Text("abc".into()),
                Value::Text("(.)".into()),
                Value::Text("\\0".into()),
            ])
            .unwrap(),
            Value::Text("\\0bc".into())
        );
    }

    #[test]
    fn test_regexp_matches() {
        let result = regexp_matches(vec![
            Value::Text("hello123world".into()),
            Value::Text(r"(\d+)".into()),
        ])
        .unwrap();
        assert_eq!(result, Value::Array(vec![Value::Text("123".into())]));
    }

    #[test]
    fn test_regexp_match_invalid_pattern_preserves_sqlstate() {
        let err =
            regexp_match(vec![Value::Text("abc".into()), Value::Text("[".into())]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid regular expression: brackets [] not balanced"
        );
        assert_eq!(
            err.downcast_ref::<SqlError>()
                .expect("sql error")
                .sqlstate(),
            "2201B"
        );
    }

    #[test]
    fn test_regexp_match_singular_and_scalar_matches_reject_global() {
        assert_eq!(
            regexp_match(vec![
                Value::Text("hello123world".into()),
                Value::Text(r"(\d+)".into()),
            ])
            .unwrap(),
            Value::Array(vec![Value::Text("123".into())])
        );
        assert_eq!(
            regexp_match(vec![
                Value::Text("a1 a2".into()),
                Value::Text(r"(a\d)".into()),
                Value::Text("g".into()),
            ])
            .unwrap_err()
            .to_string(),
            "regexp_match() does not support the \"global\" option"
        );
        assert_eq!(
            regexp_matches(vec![
                Value::Text("a1 a2".into()),
                Value::Text(r"(a\d)".into()),
                Value::Text("g".into()),
            ])
            .unwrap_err()
            .to_string(),
            "regexp_matches() does not support the \"global\" option"
        );
    }

    #[test]
    fn test_regexp_match_treats_pg_b_escape_as_backspace() {
        assert_eq!(
            regexp_match(vec![
                Value::Text("foo".into()),
                Value::Text(r"\bfoo\b".into()),
            ])
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            regexp_match(vec![
                Value::Text("\u{0008}foo\u{0008}".into()),
                Value::Text(r"\bfoo\b".into()),
            ])
            .unwrap(),
            Value::Array(vec![Value::Text("\u{0008}foo\u{0008}".into())])
        );
    }

    #[test]
    fn test_regexp_match_supports_pg_word_boundary_escapes() {
        assert_eq!(
            regexp_match(vec![
                Value::Text("foo".into()),
                Value::Text(r"\mfoo\M".into()),
            ])
            .unwrap(),
            Value::Array(vec![Value::Text("foo".into())])
        );
        assert_eq!(
            regexp_match(vec![
                Value::Text("foo!".into()),
                Value::Text(r"foo\y".into()),
            ])
            .unwrap(),
            Value::Array(vec![Value::Text("foo".into())])
        );
        assert_eq!(
            regexp_match(vec![
                Value::Text("foo!".into()),
                Value::Text(r"foo\Y".into()),
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_regexp_matches_supports_case_quote_and_tight_flags() {
        assert_eq!(
            regexp_matches(vec![
                Value::Text("A".into()),
                Value::Text("a".into()),
                Value::Text("c".into()),
            ])
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            regexp_matches(vec![
                Value::Text("a.b".into()),
                Value::Text(".".into()),
                Value::Text("q".into()),
            ])
            .unwrap(),
            Value::Array(vec![Value::Text(".".into())])
        );
        assert_eq!(
            regexp_matches(vec![
                Value::Text("(a)".into()),
                Value::Text("(".into()),
                Value::Text("t".into()),
            ])
            .unwrap_err()
            .to_string(),
            "invalid regular expression: parentheses () not balanced"
        );
    }

    #[test]
    fn test_regexp_split_to_array() {
        assert_eq!(
            regexp_split_to_array(vec![Value::Text("a,b,c".into()), Value::Text(",".into()),])
                .unwrap(),
            Value::Array(vec![
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Text("c".into()),
            ])
        );
    }

    #[test]
    fn test_regexp_split_to_array_null_pattern_returns_null() {
        assert_eq!(
            regexp_split_to_array(vec![Value::Text("a,b".into()), Value::Null]).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_regexp_split_to_array_null_flags_returns_null() {
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("a,b".into()),
                Value::Text("([".into()),
                Value::Null,
            ])
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_regexp_split_to_array_invalid_and_global_flags_error() {
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("a,b".into()),
                Value::Text(",".into()),
                Value::Text("z".into()),
            ])
            .unwrap_err()
            .to_string(),
            "invalid regular expression option: \"z\""
        );
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("ab".into()),
                Value::Text("a b".into()),
                Value::Text("g".into()),
            ])
            .unwrap_err()
            .to_string(),
            "regexp_split_to_array() does not support the \"global\" option"
        );
    }

    #[test]
    fn test_regexp_split_to_array_invalid_pattern_preserves_sqlstate() {
        let err = regexp_split_to_array(vec![Value::Text("abc".into()), Value::Text("[".into())])
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid regular expression: brackets [] not balanced"
        );
        assert_eq!(
            err.downcast_ref::<SqlError>()
                .expect("sql error")
                .sqlstate(),
            "2201B"
        );
    }

    #[test]
    fn test_regexp_split_to_array_extended_and_case_insensitive_flags() {
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("Ab".into()),
                Value::Text("a b".into()),
                Value::Text("ix".into()),
            ])
            .unwrap(),
            Value::Array(vec![Value::Text("".into()), Value::Text("".into())])
        );
    }

    #[test]
    fn test_regexp_split_to_array_accepts_case_quote_and_tight_flags() {
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("A".into()),
                Value::Text("a".into()),
                Value::Text("c".into()),
            ])
            .unwrap(),
            Value::Array(vec![Value::Text("A".into())])
        );
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("a.b".into()),
                Value::Text(".".into()),
                Value::Text("q".into()),
            ])
            .unwrap(),
            Value::Array(vec![Value::Text("a".into()), Value::Text("b".into())])
        );
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("(a)".into()),
                Value::Text("(".into()),
                Value::Text("t".into()),
            ])
            .unwrap_err()
            .to_string(),
            "invalid regular expression: parentheses () not balanced"
        );
    }

    #[test]
    fn test_regexp_split_to_array_uses_pg_newline_modes() {
        assert_eq!(
            regexp_split_to_array(vec![Value::Text("a\nb".into()), Value::Text(".".into()),])
                .unwrap(),
            Value::Array(vec![
                Value::Text("".into()),
                Value::Text("".into()),
                Value::Text("".into()),
                Value::Text("".into()),
            ])
        );
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("a\nb".into()),
                Value::Text(".".into()),
                Value::Text("p".into()),
            ])
            .unwrap(),
            Value::Array(vec![
                Value::Text("".into()),
                Value::Text("\n".into()),
                Value::Text("".into()),
            ])
        );
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("a\nb".into()),
                Value::Text("^".into()),
                Value::Text("w".into()),
            ])
            .unwrap(),
            Value::Array(vec![Value::Text("a\n".into()), Value::Text("b".into())])
        );
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("a\nb".into()),
                Value::Text("^".into()),
                Value::Text("s".into()),
            ])
            .unwrap(),
            Value::Array(vec![Value::Text("a\nb".into())])
        );
    }

    #[test]
    fn test_regexp_split_to_array_newline_flags_use_last_wins_semantics() {
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("a\nb".into()),
                Value::Text(".".into()),
                Value::Text("sn".into()),
            ])
            .unwrap(),
            Value::Array(vec![
                Value::Text("".into()),
                Value::Text("\n".into()),
                Value::Text("".into()),
            ])
        );
        assert_eq!(
            regexp_split_to_array(vec![
                Value::Text("a\nb".into()),
                Value::Text(".".into()),
                Value::Text("ns".into()),
            ])
            .unwrap(),
            Value::Array(vec![
                Value::Text("".into()),
                Value::Text("".into()),
                Value::Text("".into()),
                Value::Text("".into()),
            ])
        );
    }

    #[test]
    fn test_regexp_split_to_array_drops_zero_width_boundary_empties_like_pg() {
        assert_eq!(
            regexp_split_to_array(vec![Value::Text("abc".into()), Value::Text("^".into()),])
                .unwrap(),
            Value::Array(vec![Value::Text("abc".into())])
        );
        assert_eq!(
            regexp_split_to_array(vec![Value::Text("abc".into()), Value::Text("$".into()),])
                .unwrap(),
            Value::Array(vec![Value::Text("abc".into())])
        );
    }
}
