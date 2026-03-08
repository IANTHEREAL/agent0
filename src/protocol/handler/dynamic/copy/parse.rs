//! COPY command parsing: `parse_copy_command` (FROM STDIN) and
//! `parse_copy_to_command` (TO STDOUT).

use super::super::super::errors::error_info;
use super::super::super::query_parser::strip_leading_whitespace_and_comments;
use super::super::DynamicPgHandler;
use pgwire::error::ErrorInfo;
use sqlparser::ast::{
    CopyLegacyCsvOption, CopyLegacyOption, CopyOption, CopySource, CopyTarget, Ident, Statement,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};
use std::collections::HashSet;

impl DynamicPgHandler {
    /// Parse a COPY FROM STDIN command.
    /// Returns `(table_name, columns, header)` or `Ok(None)` if the query is
    /// not a COPY FROM STDIN.  The `header` flag is `true` when the HEADER
    /// option is present, indicating the first data line must be skipped.
    #[allow(clippy::result_large_err)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::protocol::handler) fn parse_copy_command(
        query: &str,
    ) -> Result<Option<(String, Vec<String>, bool)>, ErrorInfo> {
        // Keep COPY FROM STDIN semantics in one parser implementation.
        Self::parse_copy_command_with_options(query)
            .map(|opt| opt.map(|(table, columns, copy_opts)| (table, columns, copy_opts.header)))
    }

    /// Parse a COPY FROM STDIN command.
    /// Returns `(table_name, columns, copy_opts)` or `Ok(None)` if the query is
    /// not a COPY FROM STDIN.
    #[allow(clippy::result_large_err)]
    pub(in crate::protocol::handler) fn parse_copy_command_with_options(
        query: &str,
    ) -> Result<
        Option<(
            String,
            Vec<String>,
            crate::protocol::copy_format::CopyOptions,
        )>,
        ErrorInfo,
    > {
        Self::parse_copy_from_stdin_via_sqlparser_with_options(query)
    }

    /// Parse COPY FROM STDIN using sqlparser.
    /// Returns `(table_name, columns, header)` or `Ok(None)` if the query is not a COPY FROM STDIN.
    #[allow(clippy::result_large_err)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::protocol::handler) fn parse_copy_from_stdin_via_sqlparser(
        query: &str,
    ) -> Result<Option<(String, Vec<String>, bool)>, ErrorInfo> {
        Self::parse_copy_from_stdin_via_sqlparser_with_options(query)
            .map(|opt| opt.map(|(table, columns, copy_opts)| (table, columns, copy_opts.header)))
    }

    /// Parse COPY FROM STDIN using sqlparser.
    /// Returns `(table_name, columns, copy_opts)` or `Ok(None)` if the query is not a COPY FROM STDIN.
    #[allow(clippy::result_large_err)]
    pub(in crate::protocol::handler) fn parse_copy_from_stdin_via_sqlparser_with_options(
        query: &str,
    ) -> Result<
        Option<(
            String,
            Vec<String>,
            crate::protocol::copy_format::CopyOptions,
        )>,
        ErrorInfo,
    > {
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
                if let Some(parsed) =
                    parse_copy_from_stdin_with_legacy_header_fallback(&query_with_semi)?
                {
                    return Ok(Some(parsed));
                }
                return Err(error_info("42601", first_err.to_string()));
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

        validate_copy_from_stdin_options(options, legacy_options)?;

        let CopySource::Table {
            table_name,
            columns,
        } = source
        else {
            return Ok(None);
        };

        let table_str = format_copy_table_name(table_name.0.as_slice())?;
        let col_strs: Vec<String> = columns.iter().map(format_copy_ident).collect();
        let copy_opts = copy_options_from_parsed(options, legacy_options)?;
        Ok(Some((table_str, col_strs, copy_opts)))
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

fn format_copy_ident(ident: &Ident) -> String {
    if ident.quote_style.is_some() {
        let escaped = ident.value.replace('"', "\"\"");
        format!("\"{}\"", escaped)
    } else {
        ident.value.clone()
    }
}

#[allow(clippy::result_large_err)]
fn format_copy_table_name(parts: &[Ident]) -> Result<String, ErrorInfo> {
    match parts {
        [table] => Ok(format_copy_ident(table)),
        [schema, table] => Ok(format!(
            "{}.{}",
            format_copy_ident(schema),
            format_copy_ident(table)
        )),
        _ => {
            let canonical_name = parts
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
            Err(error_info(
                "0A000",
                format!(
                    "cross-database references are not implemented: \"{}\"",
                    canonical_name
                ),
            ))
        }
    }
}

fn token_is_unquoted_keyword(token: Option<&Token>, keyword: &str) -> bool {
    matches!(
        token,
        Some(Token::Word(word))
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(keyword)
    )
}

fn syntax_error_at_or_near_token(token: &Token) -> ErrorInfo {
    error_info("42601", format!("syntax error at or near \"{}\"", token))
}

fn consume_optional_as(tokens: &[Token], idx: &mut usize) {
    if token_is_unquoted_keyword(tokens.get(*idx), "AS") {
        *idx += 1;
    }
}

#[allow(clippy::result_large_err)]
fn parse_legacy_copy_option_string(tokens: &[Token], idx: &mut usize) -> Result<String, ErrorInfo> {
    let Some(token) = tokens.get(*idx) else {
        return Err(error_info("42601", "syntax error at end of input"));
    };
    match token {
        Token::SingleQuotedString(s)
        | Token::EscapedStringLiteral(s)
        | Token::NationalStringLiteral(s) => {
            *idx += 1;
            Ok(s.clone())
        }
        _ => Err(syntax_error_at_or_near_token(token)),
    }
}

#[allow(clippy::result_large_err)]
fn parse_legacy_copy_char_option(
    tokens: &[Token],
    idx: &mut usize,
    option_name: &str,
) -> Result<u8, ErrorInfo> {
    let raw = parse_legacy_copy_option_string(tokens, idx)?;
    let mut chars = raw.chars();
    let Some(c) = chars.next() else {
        return Err(error_info(
            "0A000",
            format!(
                "COPY {} must be a single one-byte character, got: '{}'",
                option_name, raw
            ),
        ));
    };
    if chars.next().is_some() || !c.is_ascii() {
        return Err(error_info(
            "0A000",
            format!(
                "COPY {} must be a single one-byte character, got: '{}'",
                option_name, raw
            ),
        ));
    }
    Ok(c as u8)
}

#[allow(clippy::result_large_err)]
fn parse_copy_from_stdin_with_legacy_header_fallback(
    query: &str,
) -> Result<
    Option<(
        String,
        Vec<String>,
        crate::protocol::copy_format::CopyOptions,
    )>,
    ErrorInfo,
> {
    use crate::protocol::copy_format::{CopyFormat, CopyOptions};

    let dialect = PostgreSqlDialect {};
    let tokens = Tokenizer::new(&dialect, query)
        .tokenize()
        .map_err(|e| error_info("42601", e.to_string()))?;
    let tokens = tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let mut idx = 0usize;

    if !token_is_unquoted_keyword(tokens.get(idx), "COPY") {
        return Ok(None);
    }
    idx += 1;

    let Some(Token::Word(first_table)) = tokens.get(idx) else {
        return Ok(None);
    };
    let mut table_parts = vec![Ident {
        value: first_table.value.clone(),
        quote_style: first_table.quote_style,
    }];
    idx += 1;

    while matches!(tokens.get(idx), Some(Token::Period)) {
        idx += 1;
        let Some(Token::Word(next_part)) = tokens.get(idx) else {
            let Some(tok) = tokens.get(idx.saturating_sub(1)) else {
                return Err(error_info("42601", "syntax error at end of input"));
            };
            return Err(syntax_error_at_or_near_token(tok));
        };
        table_parts.push(Ident {
            value: next_part.value.clone(),
            quote_style: next_part.quote_style,
        });
        idx += 1;
    }

    let mut columns: Vec<String> = Vec::new();
    if matches!(tokens.get(idx), Some(Token::LParen)) {
        idx += 1;
        loop {
            let Some(token) = tokens.get(idx) else {
                return Err(error_info("42601", "syntax error at end of input"));
            };
            let Token::Word(col_word) = token else {
                return Err(syntax_error_at_or_near_token(token));
            };
            columns.push(format_copy_ident(&Ident {
                value: col_word.value.clone(),
                quote_style: col_word.quote_style,
            }));
            idx += 1;
            match tokens.get(idx) {
                Some(Token::Comma) => {
                    idx += 1;
                }
                Some(Token::RParen) => {
                    idx += 1;
                    break;
                }
                Some(tok) => return Err(syntax_error_at_or_near_token(tok)),
                None => return Err(error_info("42601", "syntax error at end of input")),
            }
        }
    }

    if !token_is_unquoted_keyword(tokens.get(idx), "FROM") {
        return Ok(None);
    }
    idx += 1;
    if !token_is_unquoted_keyword(tokens.get(idx), "STDIN") {
        return Ok(None);
    }
    idx += 1;

    let mut copy_opts = CopyOptions::default();
    let mut delimiter_set = false;
    let mut null_string_set = false;
    let mut seen_legacy: HashSet<CopyOptionKind> = HashSet::new();
    let mut saw_any_legacy = false;
    let mut saw_header = false;

    if token_is_unquoted_keyword(tokens.get(idx), "WITH") {
        idx += 1;
        if matches!(tokens.get(idx), Some(Token::LParen)) {
            // Parenthesized WITH (...) options are handled by sqlparser itself.
            return Ok(None);
        }
    }

    while idx < tokens.len() {
        if matches!(tokens.get(idx), Some(Token::SemiColon)) {
            idx += 1;
            break;
        }
        let Some(token) = tokens.get(idx) else {
            break;
        };
        let Token::Word(word) = token else {
            return Err(syntax_error_at_or_near_token(token));
        };
        if word.quote_style.is_some() {
            return Err(syntax_error_at_or_near_token(token));
        }

        let keyword = word.value.to_ascii_uppercase();
        if keyword == "WITH" {
            // Mixed old/new COPY options syntax, e.g. HEADER WITH (...).
            return Err(error_info("42601", "syntax error at or near \"WITH\""));
        }

        saw_any_legacy = true;
        match keyword.as_str() {
            "HEADER" => {
                if !seen_legacy.insert(CopyOptionKind::Header) {
                    return Err(copy_conflicting_or_redundant_options_error());
                }
                copy_opts.header = true;
                saw_header = true;
                idx += 1;
            }
            "CSV" => {
                if !seen_legacy.insert(CopyOptionKind::Csv) {
                    return Err(copy_conflicting_or_redundant_options_error());
                }
                copy_opts.format = CopyFormat::Csv;
                idx += 1;
            }
            "DELIMITER" => {
                if !seen_legacy.insert(CopyOptionKind::Delimiter) {
                    return Err(copy_conflicting_or_redundant_options_error());
                }
                idx += 1;
                consume_optional_as(&tokens, &mut idx);
                copy_opts.delimiter =
                    parse_legacy_copy_char_option(&tokens, &mut idx, "delimiter")?;
                delimiter_set = true;
            }
            "NULL" => {
                if !seen_legacy.insert(CopyOptionKind::Null) {
                    return Err(copy_conflicting_or_redundant_options_error());
                }
                idx += 1;
                consume_optional_as(&tokens, &mut idx);
                copy_opts.null_string = parse_legacy_copy_option_string(&tokens, &mut idx)?;
                null_string_set = true;
            }
            "QUOTE" => {
                if !seen_legacy.insert(CopyOptionKind::Quote) {
                    return Err(copy_conflicting_or_redundant_options_error());
                }
                idx += 1;
                consume_optional_as(&tokens, &mut idx);
                copy_opts.quote = parse_legacy_copy_char_option(&tokens, &mut idx, "quote")?;
            }
            "ESCAPE" => {
                if !seen_legacy.insert(CopyOptionKind::Escape) {
                    return Err(copy_conflicting_or_redundant_options_error());
                }
                idx += 1;
                consume_optional_as(&tokens, &mut idx);
                copy_opts.escape = parse_legacy_copy_char_option(&tokens, &mut idx, "escape")?;
            }
            "BINARY" => {
                return Err(error_info("0A000", "COPY FORMAT binary is not supported"));
            }
            _ => return Err(syntax_error_at_or_near_token(token)),
        }
    }

    if idx < tokens.len() {
        let Some(tok) = tokens.get(idx) else {
            return Err(error_info("42601", "syntax error at end of input"));
        };
        return Err(syntax_error_at_or_near_token(tok));
    }

    if !saw_any_legacy || !saw_header {
        return Ok(None);
    }

    if copy_opts.format == CopyFormat::Csv {
        if !delimiter_set {
            copy_opts.delimiter = b',';
        }
        if !null_string_set {
            copy_opts.null_string.clear();
        }
    }
    if copy_opts.format != CopyFormat::Csv {
        if seen_legacy.contains(&CopyOptionKind::Quote) {
            return Err(error_info("0A000", "COPY QUOTE requires CSV mode"));
        }
        if seen_legacy.contains(&CopyOptionKind::Escape) {
            return Err(error_info("0A000", "COPY ESCAPE requires CSV mode"));
        }
    }

    let table_name = format_copy_table_name(&table_parts)?;
    Ok(Some((table_name, columns, copy_opts)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CopyOptionKind {
    Format,
    Freeze,
    Delimiter,
    Null,
    Header,
    Quote,
    Escape,
    ForceQuote,
    ForceNotNull,
    ForceNull,
    Encoding,
    Binary,
    Csv,
}

fn modern_option_kind(opt: &CopyOption) -> CopyOptionKind {
    match opt {
        CopyOption::Format(_) => CopyOptionKind::Format,
        CopyOption::Freeze(_) => CopyOptionKind::Freeze,
        CopyOption::Delimiter(_) => CopyOptionKind::Delimiter,
        CopyOption::Null(_) => CopyOptionKind::Null,
        CopyOption::Header(_) => CopyOptionKind::Header,
        CopyOption::Quote(_) => CopyOptionKind::Quote,
        CopyOption::Escape(_) => CopyOptionKind::Escape,
        CopyOption::ForceQuote(_) => CopyOptionKind::ForceQuote,
        CopyOption::ForceNotNull(_) => CopyOptionKind::ForceNotNull,
        CopyOption::ForceNull(_) => CopyOptionKind::ForceNull,
        CopyOption::Encoding(_) => CopyOptionKind::Encoding,
    }
}

fn legacy_option_kind(opt: &CopyLegacyOption) -> CopyOptionKind {
    match opt {
        CopyLegacyOption::Binary => CopyOptionKind::Binary,
        CopyLegacyOption::Delimiter(_) => CopyOptionKind::Delimiter,
        CopyLegacyOption::Null(_) => CopyOptionKind::Null,
        CopyLegacyOption::Csv(_) => CopyOptionKind::Csv,
    }
}

fn legacy_csv_option_kind(opt: &CopyLegacyCsvOption) -> CopyOptionKind {
    match opt {
        CopyLegacyCsvOption::Header => CopyOptionKind::Header,
        CopyLegacyCsvOption::Quote(_) => CopyOptionKind::Quote,
        CopyLegacyCsvOption::Escape(_) => CopyOptionKind::Escape,
        CopyLegacyCsvOption::ForceQuote(_) => CopyOptionKind::ForceQuote,
        CopyLegacyCsvOption::ForceNotNull(_) => CopyOptionKind::ForceNotNull,
    }
}

fn copy_conflicting_or_redundant_options_error() -> ErrorInfo {
    error_info("42601", "conflicting or redundant options")
}

fn has_duplicate_copy_options(options: &[CopyOption]) -> bool {
    let mut seen: HashSet<CopyOptionKind> = HashSet::new();
    options
        .iter()
        .any(|opt| !seen.insert(modern_option_kind(opt)))
}

fn has_duplicate_copy_legacy_options(legacy_options: &[CopyLegacyOption]) -> bool {
    let mut seen_legacy: HashSet<CopyOptionKind> = HashSet::new();
    for opt in legacy_options {
        let key = legacy_option_kind(opt);
        if !seen_legacy.insert(key) {
            return true;
        }
        if let CopyLegacyOption::Csv(csv_opts) = opt {
            let mut seen_csv: HashSet<CopyOptionKind> = HashSet::new();
            for csv_opt in csv_opts {
                if !seen_csv.insert(legacy_csv_option_kind(csv_opt)) {
                    return true;
                }
            }
        }
    }
    false
}

fn mixed_modern_legacy_copy_options_syntax_error(legacy_options: &[CopyLegacyOption]) -> ErrorInfo {
    let near = match legacy_options.first() {
        Some(CopyLegacyOption::Binary) => "BINARY",
        Some(CopyLegacyOption::Delimiter(_)) => "DELIMITER",
        Some(CopyLegacyOption::Null(_)) => "NULL",
        Some(CopyLegacyOption::Csv(_)) => "CSV",
        None => "COPY",
    };
    error_info("42601", format!("syntax error at or near \"{near}\""))
}

#[allow(clippy::result_large_err)]
fn validate_copy_from_stdin_options(
    options: &[CopyOption],
    legacy_options: &[CopyLegacyOption],
) -> Result<(), ErrorInfo> {
    if !options.is_empty() && !legacy_options.is_empty() {
        return Err(mixed_modern_legacy_copy_options_syntax_error(
            legacy_options,
        ));
    }
    if has_duplicate_copy_options(options) || has_duplicate_copy_legacy_options(legacy_options) {
        return Err(copy_conflicting_or_redundant_options_error());
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn copy_options_from_parsed(
    options: &[CopyOption],
    legacy_options: &[CopyLegacyOption],
) -> Result<crate::protocol::copy_format::CopyOptions, ErrorInfo> {
    if !options.is_empty() {
        return crate::protocol::copy_format::CopyOptions::from_copy_options(options)
            .map_err(|e| error_info("0A000", e));
    }

    use crate::protocol::copy_format::{CopyFormat, CopyOptions};

    let mut copy_opts = CopyOptions::default();
    let mut delimiter_set = false;
    let mut null_string_set = false;

    for opt in legacy_options {
        match opt {
            CopyLegacyOption::Binary => {
                return Err(error_info("0A000", "COPY FORMAT binary is not supported"));
            }
            CopyLegacyOption::Delimiter(c) => {
                if !c.is_ascii() {
                    return Err(error_info(
                        "0A000",
                        format!(
                            "COPY delimiter must be a single one-byte character, got: '{}'",
                            c
                        ),
                    ));
                }
                copy_opts.delimiter = *c as u8;
                delimiter_set = true;
            }
            CopyLegacyOption::Null(s) => {
                copy_opts.null_string = s.clone();
                null_string_set = true;
            }
            CopyLegacyOption::Csv(csv_opts) => {
                copy_opts.format = CopyFormat::Csv;
                for csv_opt in csv_opts {
                    match csv_opt {
                        CopyLegacyCsvOption::Header => {
                            copy_opts.header = true;
                        }
                        CopyLegacyCsvOption::Quote(c) => {
                            if !c.is_ascii() {
                                return Err(error_info(
                                    "0A000",
                                    format!(
                                        "COPY quote must be a single one-byte character, got: '{}'",
                                        c
                                    ),
                                ));
                            }
                            copy_opts.quote = *c as u8;
                        }
                        CopyLegacyCsvOption::Escape(c) => {
                            if !c.is_ascii() {
                                return Err(error_info(
                                    "0A000",
                                    format!(
                                        "COPY escape must be a single one-byte character, got: '{}'",
                                        c
                                    ),
                                ));
                            }
                            copy_opts.escape = *c as u8;
                        }
                        CopyLegacyCsvOption::ForceQuote(_)
                        | CopyLegacyCsvOption::ForceNotNull(_) => {}
                    }
                }
            }
        }
    }

    if copy_opts.format == CopyFormat::Csv {
        if !delimiter_set {
            copy_opts.delimiter = b',';
        }
        if !null_string_set {
            copy_opts.null_string.clear();
        }
    }

    Ok(copy_opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_header_mixed_with_parenthesized_with_rejected() {
        let err =
            DynamicPgHandler::parse_copy_command("COPY t FROM STDIN HEADER WITH (FORMAT csv)")
                .unwrap_err();
        assert_eq!(err.code, "42601");
        assert!(
            err.message.contains("syntax error"),
            "unexpected message: {}",
            err.message
        );
    }

    #[test]
    fn bare_header_keeps_text_format() {
        let (_, _, opts) =
            DynamicPgHandler::parse_copy_command_with_options("COPY t FROM STDIN HEADER")
                .unwrap()
                .expect("COPY should parse");
        assert_eq!(opts.format, crate::protocol::copy_format::CopyFormat::Text);
        assert!(opts.header);
        assert_eq!(opts.delimiter, b'\t');
        assert_eq!(opts.null_string, "\\N");
    }

    #[test]
    fn bare_header_with_delimiter_keeps_text_format() {
        let (_, _, opts) = DynamicPgHandler::parse_copy_command_with_options(
            "COPY t FROM STDIN HEADER DELIMITER '|'",
        )
        .unwrap()
        .expect("COPY should parse");
        assert_eq!(opts.format, crate::protocol::copy_format::CopyFormat::Text);
        assert!(opts.header);
        assert_eq!(opts.delimiter, b'|');
    }

    #[test]
    fn bare_header_after_comments_is_accepted() {
        let (_, _, opts) = DynamicPgHandler::parse_copy_command_with_options(
            "COPY t /* ignored */ FROM STDIN -- options\nHEADER",
        )
        .unwrap()
        .expect("COPY should parse");
        assert!(opts.header);
    }

    #[test]
    fn duplicate_modern_copy_options_rejected() {
        for sql in [
            "COPY t FROM STDIN WITH (FORMAT csv, FORMAT text)",
            "COPY t FROM STDIN WITH (DELIMITER ',', DELIMITER '|')",
            "COPY t FROM STDIN WITH (HEADER true, HEADER false)",
        ] {
            let err = DynamicPgHandler::parse_copy_from_stdin_via_sqlparser(sql).unwrap_err();
            assert_eq!(err.code, "42601", "sql: {sql}");
            assert_eq!(
                err.message, "conflicting or redundant options",
                "sql: {sql}"
            );
        }
    }

    #[test]
    fn mixed_modern_legacy_copy_options_rejected_as_syntax_error() {
        let err = DynamicPgHandler::parse_copy_from_stdin_via_sqlparser(
            "COPY t FROM STDIN WITH (FORMAT csv) CSV",
        )
        .unwrap_err();
        assert_eq!(err.code, "42601");
        assert!(
            err.message.contains("syntax error"),
            "unexpected message: {}",
            err.message
        );
    }
}
