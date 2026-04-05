//! SQL parser wrapper using sqlparser-rs.
//!
//! Provides `parse_sql`, the single entry-point for converting SQL text into
//! AST statements. Internally applies a set of parse-time preprocessing shims
//! and operator rewrites to work around sqlparser-rs limitations for
//! PostgreSQL-specific syntax.

mod operator_rewrite;
mod preprocess;
mod tokenizer;

#[cfg(test)]
mod tests;

use anyhow::{anyhow, Result};
use sqlparser::ast::{DataType, Expr, SelectItem, Statement, WildcardAdditionalOptions};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};

use super::error::SqlError;

use preprocess::{
    extract_create_index_with_params as extract_create_index_with_params_impl, preprocess_sql,
};
use tokenizer::{skip_ws_comments_forward, tokenize_sql_for_rewrite, TokenKind};

/// Parse a SQL string into AST statements.
///
/// Custom typed-string literals (`mood 'happy'`) are parsed by our sqlparser
/// fork as `Expr::TypedString { Custom(mood), "happy" }`. We normalize these
/// to `Expr::Cast { 'happy', Custom(mood) }` so that ALL downstream code
/// uses the existing Cast infrastructure — no per-consumer TypedString handling
/// needed.
///
/// Builtin TypedStrings (DATE, TIMESTAMP, etc.) are left as-is because the
/// analyzer already handles them natively.
pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let dialect = PostgreSqlDialect {};
    let preprocessed = preprocess_sql(sql).map_err(SqlError::Syntax)?;
    match parse_sql_with_pg_named_arg_compat(&dialect, &preprocessed) {
        Ok(mut stmts) => {
            for stmt in &mut stmts {
                normalize_custom_typed_strings(stmt);
            }
            Ok(stmts)
        }
        Err(e) => {
            if let Some(mut stmts) =
                parse_insert_returning_wildcard_fallback(&dialect, &preprocessed)
            {
                for stmt in &mut stmts {
                    normalize_custom_typed_strings(stmt);
                }
                return Ok(stmts);
            }
            let hint = fullwidth_character_hint(sql);
            match hint {
                Some(h) => Err(anyhow!("SQL parse error: {}\nHINT: {}", e, h)),
                None => Err(anyhow!("SQL parse error: {}", e)),
            }
        }
    }
}

/// Normalize `Expr::TypedString { Custom(type), value }` → `Expr::Cast`
/// so downstream code only needs to handle Cast for custom types.
/// Builtin TypedStrings (DATE, TIMESTAMP, etc.) are left as-is.
fn normalize_custom_typed_strings(stmt: &mut Statement) {
    use core::ops::ControlFlow;
    use sqlparser::ast::visit_expressions_mut;

    let _ = visit_expressions_mut(stmt, |expr| {
        if matches!(
            expr,
            Expr::TypedString {
                data_type: DataType::Custom(..),
                ..
            }
        ) {
            let Expr::TypedString { data_type, value } =
                std::mem::replace(expr, Expr::Value(sqlparser::ast::Value::Null))
            else {
                unreachable!()
            };
            *expr = Expr::Cast {
                expr: Box::new(Expr::Value(sqlparser::ast::Value::SingleQuotedString(
                    value,
                ))),
                data_type,
                format: None,
            };
        }
        ControlFlow::<()>::Continue(())
    });
}

fn parse_sql_with_pg_named_arg_compat(
    dialect: &PostgreSqlDialect,
    sql: &str,
) -> std::result::Result<Vec<Statement>, sqlparser::parser::ParserError> {
    let mut tokens = Tokenizer::new(dialect, sql).tokenize_with_location()?;
    for tok in &mut tokens {
        if tok.token == Token::DuckAssignment {
            // PostgreSQL accepts `:=` as named-arg syntax in function calls.
            // sqlparser 0.40 only recognizes `=>` for FunctionArg::Named.
            tok.token = Token::RArrow;
        }
    }
    Parser::new(dialect)
        .with_tokens_with_locations(tokens)
        .parse_statements()
}

/// Extract raw `CREATE INDEX ... WITH (...)` payloads in source order.
///
/// Returned vector contains one entry per `CREATE INDEX` statement:
/// - `Some(raw_params)` when `WITH (...)` is present
/// - `None` when absent
///
/// This side-channel exists because sqlparser 0.40 can't parse CREATE INDEX
/// storage parameters yet.
pub(crate) fn extract_create_index_with_params(sql: &str) -> Vec<Option<String>> {
    extract_create_index_with_params_impl(sql)
}

/// Parse-compatibility fallback for INSERT ... RETURNING *.
///
/// sqlparser-rs currently rejects `RETURNING *` in INSERT statements for some
/// query-source forms (e.g. INSERT ... SELECT ... RETURNING *). We keep this
/// fallback intentionally narrow to preserve parser strictness elsewhere.
fn parse_insert_returning_wildcard_fallback(
    dialect: &PostgreSqlDialect,
    sql: &str,
) -> Option<Vec<Statement>> {
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return None;
    }

    let mut depth_paren = 0i32;
    let mut depth_bracket = 0i32;
    let mut returning_idx: Option<usize> = None;
    for (idx, tok) in tokens.iter().enumerate() {
        if matches!(
            tok.kind,
            TokenKind::Whitespace | TokenKind::Comment | TokenKind::StringLiteral
        ) {
            continue;
        }

        match tok.text.as_str() {
            "(" => depth_paren += 1,
            ")" => {
                if depth_paren > 0 {
                    depth_paren -= 1;
                }
            }
            "[" => depth_bracket += 1,
            "]" => {
                if depth_bracket > 0 {
                    depth_bracket -= 1;
                }
            }
            _ => {}
        }

        if depth_paren == 0
            && depth_bracket == 0
            && tok.kind == TokenKind::Word
            && tok.text.eq_ignore_ascii_case("RETURNING")
        {
            returning_idx = Some(idx);
        }
    }

    let returning_idx = returning_idx?;
    let star_idx = skip_ws_comments_forward(&tokens, returning_idx + 1, tokens.len());
    if star_idx >= tokens.len() || tokens[star_idx].text != "*" {
        return None;
    }

    // Keep fallback single-statement only. Allow an optional trailing semicolon.
    let mut tail_idx = skip_ws_comments_forward(&tokens, star_idx + 1, tokens.len());
    if tail_idx < tokens.len() {
        if tokens[tail_idx].text != ";" {
            return None;
        }
        tail_idx = skip_ws_comments_forward(&tokens, tail_idx + 1, tokens.len());
        if tail_idx != tokens.len() {
            return None;
        }
    }

    let mut rewritten = sql.to_string();
    rewritten.replace_range(tokens[returning_idx].start..tokens[star_idx].end, "");
    let mut stmts = parse_sql_with_pg_named_arg_compat(dialect, &rewritten).ok()?;
    if stmts.len() != 1 {
        return None;
    }

    match &mut stmts[0] {
        Statement::Insert { returning, .. } => {
            *returning = Some(vec![SelectItem::Wildcard(
                WildcardAdditionalOptions::default(),
            )]);
            Some(stmts)
        }
        _ => None,
    }
}

/// Check if a SQL string contains fullwidth ASCII characters (U+FF01..U+FF5E)
/// outside of string literals, and return a hint message if so.
/// Check if a Unicode codepoint is in the fullwidth ASCII range (U+FF01..U+FF5E).
///
/// With the PG-compatible tokenizer, all fullwidth characters are now valid
/// identifier characters. This function is used for the parse-error hint:
/// if a query fails to parse and contains fullwidth characters, the hint
/// alerts the user about potential encoding issues.
fn is_fullwidth_ascii(code: u32) -> bool {
    (0xFF01..=0xFF5E).contains(&code)
}

fn fullwidth_character_hint(sql: &str) -> Option<String> {
    let mut found = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            // Line comment: skip to end of line
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            // Block comment (with nesting): skip to matching */
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                let mut depth = 1u32;
                while i < bytes.len() && depth > 0 {
                    if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            // Single-quoted string literal (with '' escape, and \' in E-strings)
            b'\'' => {
                // Check if preceded by E/e (escape string constant)
                let is_escape_string = i > 0
                    && matches!(bytes[i - 1], b'E' | b'e')
                    && (i < 2 || !bytes[i - 2].is_ascii_alphanumeric());
                i += 1;
                while i < bytes.len() {
                    if is_escape_string && bytes[i] == b'\\' {
                        i += 2; // skip backslash + next char
                    } else if bytes[i] == b'\'' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\'' {
                            i += 1; // escaped ''
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            // Double-quoted identifier
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'"' {
                            i += 1; // escaped ""
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            // Dollar-quoted string: $tag$...$tag$
            b'$' => {
                let tag_start = i;
                i += 1;
                // Find the end of the opening tag
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'$' {
                    let tag = &bytes[tag_start..=i];
                    i += 1;
                    // Scan for matching closing tag
                    while i + tag.len() <= bytes.len() {
                        if &bytes[i..i + tag.len()] == tag {
                            i += tag.len();
                            break;
                        }
                        i += 1;
                    }
                }
                // If tag didn't close or wasn't a valid dollar-quote, we just
                // advanced past the $ chars — safe to continue scanning.
            }
            _ => {
                // After byte-level scanning (comments, strings, dollar-quotes),
                // i may land mid-UTF-8 sequence. Re-align to a char boundary.
                if !sql.is_char_boundary(i) {
                    i += 1;
                    continue;
                }
                // Decode a single UTF-8 character at position i
                let rest = &sql[i..];
                if let Some(c) = rest.chars().next() {
                    let code = c as u32;
                    if is_fullwidth_ascii(code) {
                        let ascii_equiv = char::from_u32(code - 0xFF01 + 0x21).unwrap();
                        if !found.iter().any(|&(fw, _)| fw == c) {
                            found.push((c, ascii_equiv));
                        }
                    }
                    i += c.len_utf8();
                } else {
                    i += 1;
                }
                continue;
            }
        }
    }

    if found.is_empty() {
        return None;
    }

    let examples: Vec<String> = found
        .iter()
        .take(3)
        .map(|(fw, ascii)| format!("{fw} (U+{:04X}) → {ascii}", *fw as u32))
        .collect();

    Some(format!(
        "Your query contains fullwidth Unicode characters that look like ASCII characters \
         but are different: {}. Check your client application's character encoding settings.",
        examples.join(", ")
    ))
}
