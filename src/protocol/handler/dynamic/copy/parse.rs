//! COPY command parsing: `parse_copy_command` (FROM STDIN) and
//! `parse_copy_to_command` (TO STDOUT).

use super::super::super::errors::error_info;
use super::super::super::query_parser::strip_leading_whitespace_and_comments;
use super::super::DynamicPgHandler;
use pgwire::error::ErrorInfo;
use sqlparser::ast::{CopySource, CopyTarget, Ident, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

impl DynamicPgHandler {
    pub(in crate::protocol::handler) fn parse_copy_command(
        query: &str,
    ) -> Option<(String, Vec<String>)> {
        let query = strip_leading_whitespace_and_comments(query)?;
        let query_upper = query.to_uppercase();
        // COPY FROM STDIN must start at statement start (after leading whitespace/comments).
        if !query_upper.starts_with("COPY")
            || !query_upper.contains("FROM")
            || !query_upper.contains("STDIN")
        {
            return None;
        }

        // Single regex: COPY [schema.]table_name [(col1, col2, ...)] FROM stdin
        let re = regex::Regex::new(
            r"(?i)^COPY\s+(?:(\w+)\.)?(\w+)\s*(?:\(([^)]+)\)\s+|\s+)FROM\s+stdin",
        )
        .ok()?;
        let caps = re.captures(query)?;
        let schema = caps.get(1).map(|m| m.as_str().to_string());
        let table = caps.get(2)?.as_str().to_string();
        let table_name = match schema {
            Some(s) => format!("{}.{}", s, table),
            None => table,
        };
        let columns = match caps.get(3) {
            Some(cols) => cols
                .as_str()
                .split(',')
                .map(|s| s.trim().to_string())
                .collect(),
            None => vec![],
        };
        Some((table_name, columns))
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
