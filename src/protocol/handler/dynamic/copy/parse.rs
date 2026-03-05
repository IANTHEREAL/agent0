//! COPY command parsing: `parse_copy_command` (FROM STDIN) and
//! `parse_copy_to_command` (TO STDOUT).

use super::super::super::errors::error_info;
use super::super::super::query_parser::strip_leading_whitespace_and_comments;
use super::super::DynamicPgHandler;
use pgwire::error::ErrorInfo;
use sqlparser::ast::{CopySource, CopyTarget, Ident, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

impl DynamicPgHandler {
    /// Parse a COPY FROM STDIN command.
    /// Returns `(table_name, columns, header)` or `Ok(None)` if the query is
    /// not a COPY FROM STDIN.  The `header` flag is `true` when the HEADER
    /// option is present, indicating the first data line must be skipped.
    #[allow(clippy::result_large_err)]
    pub(in crate::protocol::handler) fn parse_copy_command(
        query: &str,
    ) -> Result<Option<(String, Vec<String>, bool)>, ErrorInfo> {
        // Keep COPY FROM STDIN semantics in one parser implementation.
        Self::parse_copy_from_stdin_via_sqlparser(query)
    }

    /// Parse COPY FROM STDIN using sqlparser.
    /// Returns `(table_name, columns, header)` or `Ok(None)` if the query is not a COPY FROM STDIN.
    #[allow(clippy::result_large_err)]
    pub(in crate::protocol::handler) fn parse_copy_from_stdin_via_sqlparser(
        query: &str,
    ) -> Result<Option<(String, Vec<String>, bool)>, ErrorInfo> {
        let Some(query_trimmed) = strip_leading_whitespace_and_comments(query) else {
            return Ok(None);
        };

        // Quick pre-check: must start with COPY
        match query_trimmed.get(..4) {
            Some(prefix) if prefix.eq_ignore_ascii_case("COPY") => {}
            _ => return Ok(None),
        }

        // sqlparser requires a trailing semicolon for COPY FROM STDIN
        // (it expects data lines after the statement in non-terminated form).
        // If the query ends with a line comment (`-- ...` with no trailing newline),
        // we must insert a newline before the semicolon so it doesn't land inside
        // the comment. PG 17.7 accepts `COPY t FROM STDIN -- comment` just fine.
        let query_with_semi = if query_trimmed.trim_end().ends_with(';') {
            query_trimmed.to_string()
        } else if Self::ends_with_line_comment(query_trimmed) {
            format!("{}\n;", query_trimmed)
        } else {
            format!("{};", query_trimmed)
        };

        let dialect = PostgreSqlDialect {};
        // db9-specific: error message text comes from sqlparser and may differ
        // from PostgreSQL's canonical syntax error wording. SQLSTATE 42601 is correct.
        let stmts = match Parser::parse_sql(&dialect, &query_with_semi) {
            Ok(stmts) => stmts,
            Err(first_err) => {
                // sqlparser 0.40 doesn't support bare `HEADER` after STDIN
                // (PG 17 does). Normalize `HEADER` → `CSV HEADER` and retry.
                if let Some(normalized) = Self::normalize_bare_header_after_stdin(&query_with_semi)
                {
                    // Reject duplicate HEADER: PG 17.7 returns 42601
                    // "conflicting or redundant options".
                    if has_duplicate_header_after_stdin(&query_with_semi) {
                        return Err(error_info(
                            "42601",
                            "conflicting or redundant options".to_string(),
                        ));
                    }
                    Parser::parse_sql(&dialect, &normalized)
                        .map_err(|_| error_info("42601", first_err.to_string()))?
                } else {
                    return Err(error_info("42601", first_err.to_string()));
                }
            }
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
            ..
        } = stmt
        else {
            return Ok(None);
        };

        if *to || !matches!(target, CopyTarget::Stdin) {
            return Ok(None);
        }

        let CopySource::Table {
            table_name,
            columns,
        } = source
        else {
            return Ok(None);
        };

        fn format_ident(ident: &Ident) -> String {
            if ident.quote_style.is_some() {
                let escaped = ident.value.replace('"', "\"\"");
                format!("\"{}\"", escaped)
            } else {
                ident.value.clone()
            }
        }

        let table_str = match table_name.0.as_slice() {
            [table] => format_ident(table),
            [schema, table] => format!("{}.{}", format_ident(schema), format_ident(table)),
            _ => {
                let canonical_name = table_name
                    .0
                    .iter()
                    .map(|ident| {
                        if ident.quote_style.is_some() {
                            ident.value.clone()
                        } else {
                            ident.value.to_lowercase()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(".");
                return Err(error_info(
                    "0A000",
                    format!(
                        "cross-database references are not implemented: \"{}\"",
                        canonical_name
                    ),
                ));
            }
        };

        let col_strs: Vec<String> = columns.iter().map(format_ident).collect();

        let header = options
            .iter()
            .any(|o| matches!(o, sqlparser::ast::CopyOption::Header(true)))
            || legacy_options.iter().any(|o| {
                matches!(
                    o,
                    sqlparser::ast::CopyLegacyOption::Csv(csv_opts)
                        if csv_opts.iter().any(|c| matches!(c, sqlparser::ast::CopyLegacyCsvOption::Header))
                )
            });

        Ok(Some((table_str, col_strs, header)))
    }

    /// Normalize bare `HEADER` option after `FROM STDIN` into `CSV HEADER` so
    /// that sqlparser 0.40 can parse it. PostgreSQL 17 accepts `COPY t FROM
    /// STDIN HEADER` as valid syntax, but sqlparser only recognises HEADER as a
    /// sub-option of the `CSV` legacy keyword.
    ///
    /// Returns the rewritten query if bare HEADER was found, `None` otherwise.
    fn normalize_bare_header_after_stdin(query: &str) -> Option<String> {
        let upper = query.to_ascii_uppercase();
        let stdin_kw = b"STDIN";
        let mut search_from = 0;
        loop {
            let abs = find_keyword_outside_comments(upper.as_bytes(), stdin_kw, search_from)?;
            // Accept whitespace or end-of-block-comment (`*/`) as a word
            // boundary before STDIN.  `FROM/*c*/STDIN` ends the comment with
            // `*/`, so the byte before STDIN is `/` preceded by `*`.
            let is_word_boundary = upper.as_bytes()[abs - 1].is_ascii_whitespace()
                || (abs >= 2
                    && upper.as_bytes()[abs - 2] == b'*'
                    && upper.as_bytes()[abs - 1] == b'/');
            if abs == 0 || !is_word_boundary {
                search_from = abs + stdin_kw.len();
                continue;
            }
            let after_stdin = abs + stdin_kw.len();
            // Skip whitespace AND SQL comments after STDIN.
            let skip = skip_whitespace_and_comments(&upper[after_stdin..]);
            let rest = &upper[after_stdin + skip..];
            if let Some(rest_after_header) = rest.strip_prefix("HEADER") {
                // Ensure HEADER is a full keyword (not e.g. "HEADERX").
                let is_word_boundary = rest_after_header.is_empty()
                    || rest_after_header.starts_with(';')
                    || rest_after_header.starts_with(char::is_whitespace)
                    || rest_after_header.starts_with('-') // -- comment
                    || rest_after_header.starts_with('/'); // /* comment */
                if is_word_boundary {
                    // Insert "CSV " before "HEADER" in the original query.
                    let header_offset =
                        after_stdin + skip_whitespace_and_comments(&query[after_stdin..]);
                    let mut result = String::with_capacity(query.len() + 4);
                    result.push_str(&query[..header_offset]);
                    result.push_str("CSV ");
                    result.push_str(&query[header_offset..]);
                    return Some(result);
                }
            }
            search_from = abs + stdin_kw.len();
            continue;
        }
    }

    /// Returns `true` if the query ends with a `--` line comment that has no
    /// trailing newline. Uses sqlparser's tokenizer to correctly handle all
    /// string literal forms (standard `'...'`, escape `E'...'`, dollar-quoted,
    /// etc.) without re-implementing string scanning.
    fn ends_with_line_comment(query: &str) -> bool {
        let dialect = PostgreSqlDialect {};
        let Ok(tokens) = Tokenizer::new(&dialect, query).tokenize() else {
            return false;
        };
        // Find the last non-whitespace token; if it's a SingleLineComment, the
        // query ends with an unterminated line comment.
        let last_significant = tokens.iter().rev().find(|t| {
            !matches!(
                t,
                Token::Whitespace(Whitespace::Space | Whitespace::Tab | Whitespace::Newline)
            )
        });
        matches!(
            last_significant,
            Some(Token::Whitespace(Whitespace::SingleLineComment { .. }))
        )
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

/// Find the next occurrence of `keyword` at or after `start`, skipping over
/// SQL comment bodies (`/* ... */` with nesting, `-- ...\n`), double-quoted
/// identifiers (`"..."`), and single-quoted string literals (`'...'`).
/// Returns the absolute byte offset, or `None` if not found.
fn find_keyword_outside_comments(bytes: &[u8], keyword: &[u8], start: usize) -> Option<usize> {
    let len = bytes.len();
    let kw_len = keyword.len();
    let mut i = start;
    while i + kw_len <= len {
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Block comment — skip until closing */, handling nesting.
            i += 2;
            let mut depth = 1u32;
            while i < len && depth > 0 {
                if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if i + 1 < len && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            // Line comment — skip until newline.
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            if i < len {
                i += 1; // skip the newline
            }
        } else if bytes[i] == b'"' {
            // Double-quoted identifier — skip until closing `"`, with `""` escape.
            i += 1;
            while i < len {
                if bytes[i] == b'"' {
                    i += 1;
                    if i < len && bytes[i] == b'"' {
                        i += 1; // escaped `""`, continue
                    } else {
                        break; // closing quote
                    }
                } else {
                    i += 1;
                }
            }
        } else if bytes[i] == b'\'' {
            // Single-quoted string literal — skip until closing `'`, with `''` escape.
            i += 1;
            while i < len {
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < len && bytes[i] == b'\'' {
                        i += 1; // escaped `''`, continue
                    } else {
                        break; // closing quote
                    }
                } else {
                    i += 1;
                }
            }
        } else if bytes[i..i + kw_len] == *keyword {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// Return the number of leading bytes that are whitespace or SQL comments.
/// Handles `/* ... */` block comments (with nesting) and `-- ...` line comments.
fn skip_whitespace_and_comments(s: &str) -> usize {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Block comment — may be nested (PG supports nested block comments).
            i += 2;
            let mut depth = 1u32;
            while i < len && depth > 0 {
                if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if i + 1 < len && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            // Line comment — skip until newline.
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
        } else {
            break;
        }
    }
    i
}

/// Returns `true` if the options region after `FROM STDIN` contains the keyword
/// `HEADER` more than once, indicating a duplicate/conflicting option.
///
/// The scan is anchored to start **after** the `FROM STDIN` clause so that
/// column names (e.g. `header`) and table names (e.g. `stdin_log`) in the
/// column list do not participate in the duplicate count.
fn has_duplicate_header_after_stdin(query: &str) -> bool {
    let upper = query.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let header_kw = b"HEADER";
    let from_kw = b"FROM";
    let stdin_kw = b"STDIN";

    // Locate `FROM STDIN` with word boundaries on both keywords.
    let mut from_search = 0;
    let search_start = loop {
        let Some(from_pos) = find_keyword_outside_comments(bytes, from_kw, from_search) else {
            return false;
        };
        // Word-boundary check on FROM.
        let fb = from_pos == 0
            || (!bytes[from_pos - 1].is_ascii_alphanumeric() && bytes[from_pos - 1] != b'_');
        let fa_end = from_pos + from_kw.len();
        let fa = fa_end >= bytes.len()
            || (!bytes[fa_end].is_ascii_alphanumeric() && bytes[fa_end] != b'_');
        if fb && fa {
            // Skip whitespace / comments between FROM and STDIN.
            let gap = skip_whitespace_and_comments(&upper[fa_end..]);
            let stdin_start = fa_end + gap;
            if stdin_start + stdin_kw.len() <= bytes.len()
                && bytes[stdin_start..stdin_start + stdin_kw.len()] == *stdin_kw
            {
                let sa_end = stdin_start + stdin_kw.len();
                let sa = sa_end >= bytes.len()
                    || (!bytes[sa_end].is_ascii_alphanumeric() && bytes[sa_end] != b'_');
                if sa {
                    break sa_end;
                }
            }
        }
        from_search = from_pos + from_kw.len();
    };
    let mut search_from = search_start;
    let mut count = 0u32;
    while let Some(pos) = find_keyword_outside_comments(bytes, header_kw, search_from) {
        let before_ok =
            pos == 0 || (!bytes[pos - 1].is_ascii_alphanumeric() && bytes[pos - 1] != b'_');
        let after_end = pos + header_kw.len();
        let after_ok = after_end >= bytes.len()
            || (!bytes[after_end].is_ascii_alphanumeric() && bytes[after_end] != b'_');
        if before_ok && after_ok {
            count += 1;
            if count >= 2 {
                return true;
            }
        }
        search_from = pos + header_kw.len();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_keyword_skips_double_quoted_identifier() {
        // "STDIN" inside a double-quoted identifier must be ignored.
        let input = b"COPY \"x STDIN y\" FROM STDIN;";
        let upper: Vec<u8> = input.iter().map(|b| b.to_ascii_uppercase()).collect();
        let result = find_keyword_outside_comments(&upper, b"STDIN", 0);
        // Should find the bare STDIN after FROM, not the one inside quotes.
        let expected = upper.windows(5).rposition(|w| w == b"STDIN").unwrap();
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn find_keyword_skips_single_quoted_string() {
        let input = b"COPY t FROM 'STDIN' STDIN;";
        let upper: Vec<u8> = input.iter().map(|b| b.to_ascii_uppercase()).collect();
        let result = find_keyword_outside_comments(&upper, b"STDIN", 0);
        let expected = upper.windows(5).rposition(|w| w == b"STDIN").unwrap();
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn find_keyword_skips_escaped_quotes() {
        // Double-quoted identifier with escaped `""` inside.
        let input = b"COPY \"a\"\"STDIN\" FROM STDIN;";
        let upper: Vec<u8> = input.iter().map(|b| b.to_ascii_uppercase()).collect();
        let result = find_keyword_outside_comments(&upper, b"STDIN", 0);
        let expected = upper.windows(5).rposition(|w| w == b"STDIN").unwrap();
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn no_false_duplicate_header_with_header_column_name() {
        // `COPY stdin_log (header) FROM STDIN HEADER;` — column name `header`
        // and table name `stdin_log` must NOT cause a false duplicate rejection.
        let query = "COPY stdin_log (header) FROM STDIN HEADER;";
        assert!(
            !has_duplicate_header_after_stdin(query),
            "should NOT detect duplicate HEADER when 'header' is a column name"
        );
    }

    #[test]
    fn duplicate_header_still_rejected() {
        // Genuine duplicate must still be caught.
        let query = "COPY t FROM STDIN HEADER HEADER;";
        assert!(
            has_duplicate_header_after_stdin(query),
            "should detect genuine duplicate HEADER"
        );
    }

    #[test]
    fn normalize_skips_stdin_inside_quoted_table_name() {
        // `COPY "x STDIN HEADER y" FROM STDIN HEADER;` — the normalizer must
        // not match the STDIN inside the quoted identifier.
        let query = r#"COPY "x STDIN HEADER y" FROM STDIN HEADER;"#;
        let result =
            DynamicPgHandler::normalize_bare_header_after_stdin(query).expect("should normalize");
        // The HEADER after the real STDIN should be prefixed with CSV.
        assert!(result.contains("CSV HEADER"), "got: {result}");
        // The quoted identifier must remain untouched.
        assert!(
            result.contains(r#""x STDIN HEADER y""#),
            "quoted ident modified: {result}"
        );
    }
}
