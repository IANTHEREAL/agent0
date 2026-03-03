//! COPY command parsing: `parse_copy_command` (FROM STDIN) and
//! `parse_copy_to_command` (TO STDOUT).

use super::super::super::errors::error_info;
use super::super::super::query_parser::strip_leading_whitespace_and_comments;
use super::super::DynamicPgHandler;
use pgwire::error::ErrorInfo;
use sqlparser::ast::{CopySource, CopyTarget, Ident, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// Skip ASCII whitespace and SQL comments (`/* ... */`, `-- ...\n`).
/// Returns the remaining unconsumed slice.
fn skip_ws_and_comments(s: &str) -> &str {
    let mut rest = s;
    loop {
        // Skip whitespace
        rest = rest.trim_start();
        if rest.starts_with("--") {
            // Line comment: skip to end of line
            rest = match rest.find('\n') {
                Some(pos) => &rest[pos + 1..],
                None => "",
            };
        } else if rest.starts_with("/*") {
            // Block comment: skip to closing */
            rest = match rest[2..].find("*/") {
                Some(pos) => &rest[pos + 4..],
                None => return rest, // unterminated — bail out
            };
        } else {
            break;
        }
    }
    rest
}

/// Parse a single SQL identifier (quoted or unquoted) from the front of `s`.
/// Returns `Some((ident_text, remainder))` or `None` if no valid identifier found.
///
/// - **Quoted**: `"..."` with `""` as escaped quote. Returns the full `"..."` text.
/// - **Unquoted**: `[a-zA-Z_][a-zA-Z0-9_$]*`. Returns the matched text.
fn parse_ident(s: &str) -> Option<(&str, &str)> {
    if s.starts_with('"') {
        // Quoted identifier: scan for unescaped closing quote
        let mut i = 1;
        let bytes = s.as_bytes();
        while i < bytes.len() {
            if bytes[i] == b'"' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    // Escaped quote "", skip both
                    i += 2;
                } else {
                    // Closing quote found
                    return Some((&s[..i + 1], &s[i + 1..]));
                }
            } else {
                i += 1;
            }
        }
        None // unterminated quoted identifier
    } else {
        // Unquoted identifier: [a-zA-Z_][a-zA-Z0-9_$]*
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            return None;
        }
        let first = bytes[0];
        if !first.is_ascii_alphabetic() && first != b'_' {
            return None;
        }
        let len = bytes
            .iter()
            .skip(1)
            .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
            .count()
            + 1;
        Some((&s[..len], &s[len..]))
    }
}

/// Match a case-insensitive keyword at the front of `s`.
/// Returns the remainder after the keyword, or `None` if no match.
fn match_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let kw_len = keyword.len();
    if s.len() >= kw_len && s[..kw_len].eq_ignore_ascii_case(keyword) {
        Some(&s[kw_len..])
    } else {
        None
    }
}

fn is_known_copy_with_option(option: &str) -> bool {
    option.eq_ignore_ascii_case("FORMAT")
        || option.eq_ignore_ascii_case("DELIMITER")
        || option.eq_ignore_ascii_case("NULL")
        || option.eq_ignore_ascii_case("HEADER")
        || option.eq_ignore_ascii_case("QUOTE")
        || option.eq_ignore_ascii_case("ESCAPE")
        || option.eq_ignore_ascii_case("FORCE_QUOTE")
        || option.eq_ignore_ascii_case("FORCE_NOT_NULL")
        || option.eq_ignore_ascii_case("FREEZE")
        || option.eq_ignore_ascii_case("ENCODING")
}

/// Consume exactly one COPY WITH option value token.
/// Returns the remainder after the token.
fn consume_copy_with_value(s: &str) -> Option<&str> {
    fn consume_quoted_token(s: &str, quote: u8) -> Option<&str> {
        let bytes = s.as_bytes();
        let mut i = 1usize;
        while i < bytes.len() {
            if bytes[i] == quote {
                if i + 1 < bytes.len() && bytes[i + 1] == quote {
                    i += 2;
                } else {
                    return Some(&s[i + 1..]);
                }
            } else {
                i += 1;
            }
        }
        None
    }

    fn consume_parenthesized_token(s: &str) -> Option<&str> {
        let bytes = s.as_bytes();
        let mut i = 1usize;
        let mut paren_depth = 1usize;
        while i < bytes.len() {
            let rest = &s[i..];
            if rest.starts_with("--") {
                if let Some(pos) = rest.find('\n') {
                    i += pos + 1;
                    continue;
                }
                return None;
            }
            if let Some(stripped) = rest.strip_prefix("/*") {
                let pos = stripped.find("*/")?;
                i += pos + 4;
                continue;
            }

            match bytes[i] {
                b'(' => {
                    paren_depth += 1;
                    i += 1;
                }
                b')' => {
                    paren_depth -= 1;
                    i += 1;
                    if paren_depth == 0 {
                        return Some(&s[i..]);
                    }
                }
                b'\'' => {
                    let rest = consume_quoted_token(&s[i..], b'\'')?;
                    i = s.len() - rest.len();
                }
                b'"' => {
                    let rest = consume_quoted_token(&s[i..], b'"')?;
                    i = s.len() - rest.len();
                }
                _ => i += 1,
            }
        }
        None
    }

    let s = skip_ws_and_comments(s);
    let first = *s.as_bytes().first()?;
    match first {
        b'\'' => consume_quoted_token(s, b'\''),
        b'"' => consume_quoted_token(s, b'"'),
        b'(' => consume_parenthesized_token(s),
        b',' | b')' => None,
        _ => {
            let bytes = s.as_bytes();
            let mut i = 0usize;
            while i < bytes.len() {
                let rest = &s[i..];
                if bytes[i].is_ascii_whitespace()
                    || bytes[i] == b','
                    || bytes[i] == b')'
                    || rest.starts_with("--")
                    || rest.starts_with("/*")
                {
                    break;
                }
                i += 1;
            }
            if i == 0 {
                None
            } else {
                Some(&s[i..])
            }
        }
    }
}

/// Validate `WITH (...)` clause syntax/options for COPY fast-path.
/// Returns remainder after the closing `)` when valid.
fn parse_valid_copy_with_clause(mut s: &str) -> Option<&str> {
    s = s.strip_prefix('(')?;
    let mut saw_option = false;
    loop {
        s = skip_ws_and_comments(s);
        if let Some(rest) = s.strip_prefix(')') {
            return if saw_option { Some(rest) } else { None };
        }

        let (option, rest_after_option) = parse_ident(s)?;
        if option.starts_with('"') || !is_known_copy_with_option(option) {
            return None;
        }

        let mut value_input = skip_ws_and_comments(rest_after_option);
        if let Some(after_eq) = value_input.strip_prefix('=') {
            value_input = skip_ws_and_comments(after_eq);
        }

        let rest_after_value = consume_copy_with_value(value_input)?;
        let rest_after_value = skip_ws_and_comments(rest_after_value);
        if !rest_after_value.starts_with(',') && !rest_after_value.starts_with(')') {
            return None;
        }

        s = rest_after_value;
        saw_option = true;

        if let Some(rest) = s.strip_prefix(',') {
            s = skip_ws_and_comments(rest);
            if s.starts_with(')') {
                return None;
            }
            continue;
        }
        if let Some(rest) = s.strip_prefix(')') {
            return Some(rest);
        }
        return None;
    }
}

/// Try to tokenize `COPY [schema.]table [(col1, ...)] FROM stdin [WITH (...)]` from `query`.
///
/// Returns:
/// - `Ok(Some(...))` — valid COPY FROM STDIN recognised.
/// - `Ok(None)` — does not look like COPY FROM STDIN (safe fallthrough).
/// - `Err(msg)` — matched COPY … FROM STDIN but unexpected trailing tokens (syntax error).
fn parse_copy_from_stdin_tokens(query: &str) -> Result<Option<(String, Vec<String>)>, String> {
    // 1. Match "COPY" keyword
    let Some(rest) = match_keyword(query, "COPY") else {
        return Ok(None);
    };
    // Must have whitespace after COPY
    if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
        return Ok(None);
    }
    let rest = skip_ws_and_comments(rest);

    // 2. Parse first identifier (could be schema or table)
    let Some((ident1, rest)) = parse_ident(rest) else {
        return Ok(None);
    };

    // 3. Check for schema qualification: "."
    let (table_name, rest) = if let Some(rest) = rest.strip_prefix('.') {
        let Some((ident2, rest)) = parse_ident(rest) else {
            return Ok(None);
        };
        (format!("{}.{}", ident1, ident2), rest)
    } else {
        (ident1.to_string(), rest)
    };

    let rest = skip_ws_and_comments(rest);

    // 4. Optional column list: "(" col1, col2, ... ")"
    let (columns, rest) = if let Some(after_paren) = rest.strip_prefix('(') {
        let mut rest = skip_ws_and_comments(after_paren);
        let mut cols = Vec::new();
        loop {
            let Some((col, r)) = parse_ident(rest) else {
                return Ok(None);
            };
            cols.push(col.to_string());
            let r = skip_ws_and_comments(r);
            if let Some(after_comma) = r.strip_prefix(',') {
                rest = skip_ws_and_comments(after_comma);
            } else if let Some(after_close) = r.strip_prefix(')') {
                rest = after_close;
                break;
            } else {
                return Ok(None); // unexpected token inside column list
            }
        }
        let rest = skip_ws_and_comments(rest);
        (cols, rest)
    } else {
        (vec![], rest)
    };

    // 5. Match "FROM"
    let Some(rest) = match_keyword(rest, "FROM") else {
        return Ok(None);
    };
    if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
        return Ok(None);
    }
    let rest = skip_ws_and_comments(rest);

    // 6. Match "stdin" (case-insensitive)
    let Some(rest) = match_keyword(rest, "STDIN") else {
        return Ok(None);
    };

    // 7. Validate trailing tokens: only whitespace/comments/EOF, `;`, or `WITH (...)` allowed.
    let rest = skip_ws_and_comments(rest);
    if !rest.is_empty() && !rest.starts_with(';') {
        // WITH must be a complete keyword boundary (followed by whitespace, '(' or EOF).
        let Some(after_with) = match_keyword(rest, "WITH") else {
            return Err(format!(
                "syntax error at or near \"{}\"",
                rest.split_ascii_whitespace().next().unwrap_or(rest)
            ));
        };
        if !after_with.is_empty()
            && !after_with.starts_with(|c: char| c.is_ascii_whitespace() || c == '(')
            && !after_with.starts_with("/*")
            && !after_with.starts_with("--")
        {
            return Err(format!(
                "syntax error at or near \"{}\"",
                rest.split_ascii_whitespace().next().unwrap_or(rest)
            ));
        }

        // PG-compatible fast-path guard: after WITH, the next non-ws/comment token must be '('.
        let Some(after_with) = strip_leading_whitespace_and_comments(after_with) else {
            return Ok(None); // unclosed comment — fall through to full parser
        };
        if !after_with.starts_with('(') {
            return Err("syntax error: expected '(' after WITH in COPY statement".to_string());
        }

        // Validate WITH clause strictly for fast-path eligibility.
        // Unknown options / malformed syntax must fall through to full parser.
        let Some(after_clause) = parse_valid_copy_with_clause(after_with) else {
            return Ok(None);
        };
        let after_clause = skip_ws_and_comments(after_clause);
        if !after_clause.is_empty() && !after_clause.starts_with(';') {
            return Ok(None);
        }
    }

    Ok(Some((table_name, columns)))
}

impl DynamicPgHandler {
    #[allow(clippy::result_large_err)]
    pub(in crate::protocol::handler) fn parse_copy_command(
        query: &str,
    ) -> Result<Option<(String, Vec<String>)>, ErrorInfo> {
        fn is_valid_unquoted_ident(ident: &str) -> bool {
            let mut chars = ident.chars();
            let Some(first) = chars.next() else {
                return false;
            };
            if first != '_' && !first.is_ascii_alphabetic() {
                return false;
            }
            chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
        }

        let Some(query) = strip_leading_whitespace_and_comments(query) else {
            return Ok(None);
        };
        let query_upper = query.to_uppercase();
        // COPY FROM STDIN must start at statement start (after leading whitespace/comments).
        if !query_upper.starts_with("COPY")
            || !query_upper.contains("FROM")
            || !query_upper.contains("STDIN")
        {
            return Ok(None);
        }

        let Some((table_name, columns)) =
            parse_copy_from_stdin_tokens(query).map_err(|msg| error_info("42601", msg))?
        else {
            return Ok(None);
        };

        // Validate unquoted column names against identifier rules (PG parity).
        // Quoted columns (starting with `"`) are validated downstream by normalize_copy_ident.
        for col in &columns {
            if !col.starts_with('"') && !is_valid_unquoted_ident(col) {
                return Err(error_info(
                    "42602",
                    format!("Invalid identifier in COPY FROM STDIN: \"{}\"", col),
                ));
            }
        }

        Ok(Some((table_name, columns)))
    }

    #[allow(clippy::result_large_err)]
    pub(in crate::protocol::handler) fn parse_copy_to_command(
        query: &str,
    ) -> Result<
        Option<(
            String,
            Vec<String>,
            crate::protocol::copy_format::CopyOptions,
        )>,
        ErrorInfo,
    > {
        fn unsupported_copy_to_stdout_syntax() -> ErrorInfo {
            error_info(
                "0A000",
                "Unsupported COPY TO STDOUT syntax. Supported: COPY [schema.]table [(col1, col2, ...)] TO STDOUT [WITH (options)]",
            )
        }

        fn is_valid_unquoted_ident(ident: &str) -> bool {
            let mut chars = ident.chars();
            let Some(first) = chars.next() else {
                return false;
            };
            if first != '_' && !first.is_ascii_alphabetic() {
                return false;
            }
            chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        }

        let Some(query_trimmed) = strip_leading_whitespace_and_comments(query) else {
            return Ok(None);
        };

        match query_trimmed.get(..4) {
            Some(prefix) if prefix.eq_ignore_ascii_case("COPY") => {}
            _ => return Ok(None),
        }

        let dialect = PostgreSqlDialect {};
        let Ok(stmts) = Parser::parse_sql(&dialect, query_trimmed) else {
            return Ok(None);
        };
        let Some(stmt) = stmts.first() else {
            return Ok(None);
        };

        let Statement::Copy {
            source,
            to,
            target,
            options,
            legacy_options,
            values,
        } = stmt
        else {
            return Ok(None);
        };

        if !*to || !matches!(target, CopyTarget::Stdout) {
            return Ok(None);
        }

        if stmts.len() != 1 || !legacy_options.is_empty() || !values.is_empty() {
            return Err(unsupported_copy_to_stdout_syntax());
        }

        let copy_opts = crate::protocol::copy_format::CopyOptions::from_copy_options(options)
            .map_err(|e| error_info("0A000", e))?;

        if copy_opts.format == crate::protocol::copy_format::CopyFormat::Parquet {
            return Err(error_info(
                "0A000",
                "COPY TO with FORMAT parquet is not supported",
            ));
        }

        let CopySource::Table {
            table_name,
            columns,
        } = source
        else {
            return Err(unsupported_copy_to_stdout_syntax());
        };

        let (schema_ident, table_ident) = match table_name.0.as_slice() {
            [table] => (None, table),
            [schema, table] => (Some(schema), table),
            _ => return Err(unsupported_copy_to_stdout_syntax()),
        };

        let validate_ident = |ident: &Ident| -> Result<(), ErrorInfo> {
            if ident.quote_style.is_some() {
                return Err(unsupported_copy_to_stdout_syntax());
            }
            if !is_valid_unquoted_ident(&ident.value) {
                return Err(error_info(
                    "42602",
                    format!("Invalid identifier in COPY TO STDOUT: \"{}\"", ident.value),
                ));
            }
            Ok(())
        };

        if let Some(schema) = schema_ident {
            validate_ident(schema)?;
        }
        validate_ident(table_ident)?;
        for col in columns.iter() {
            validate_ident(col)?;
        }

        let table_name = match schema_ident {
            Some(schema) => format!("{}.{}", schema.value, table_ident.value),
            None => table_ident.value.clone(),
        };
        let columns = columns.iter().map(|c| c.value.clone()).collect();

        Ok(Some((table_name, columns, copy_opts)))
    }
}
