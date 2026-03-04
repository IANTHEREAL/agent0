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
    #[allow(clippy::result_large_err)]
    pub(in crate::protocol::handler) fn parse_copy_command(
        query: &str,
    ) -> Result<Option<(String, Vec<String>)>, ErrorInfo> {
        // Keep COPY FROM STDIN semantics in one parser implementation.
        Self::parse_copy_from_stdin_via_sqlparser(query)
    }

    /// Parse COPY FROM STDIN using sqlparser.
    /// Returns `(table_name, columns)` or `Ok(None)` if the query is not a COPY FROM STDIN.
    #[allow(clippy::result_large_err)]
    pub(in crate::protocol::handler) fn parse_copy_from_stdin_via_sqlparser(
        query: &str,
    ) -> Result<Option<(String, Vec<String>)>, ErrorInfo> {
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
        let stmts = Parser::parse_sql(&dialect, &query_with_semi)
            .map_err(|e| error_info("42601", e.to_string()))?;
        let Some(stmt) = stmts.first() else {
            return Ok(None);
        };

        let Statement::Copy {
            source, to, target, ..
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

        Ok(Some((table_str, col_strs)))
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
