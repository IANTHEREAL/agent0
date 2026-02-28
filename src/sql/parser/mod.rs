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
use sqlparser::ast::{SelectItem, Statement, WildcardAdditionalOptions};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use preprocess::{
    extract_create_index_with_params as extract_create_index_with_params_impl, preprocess_sql,
};
use tokenizer::{skip_ws_comments_forward, tokenize_sql_for_rewrite, TokenKind};

/// Parse a SQL string into AST statements
pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let dialect = PostgreSqlDialect {};
    let preprocessed = preprocess_sql(sql);
    match Parser::parse_sql(&dialect, &preprocessed) {
        Ok(stmts) => Ok(stmts),
        Err(e) => {
            if let Some(stmts) = parse_insert_returning_wildcard_fallback(&dialect, &preprocessed) {
                return Ok(stmts);
            }
            Err(anyhow!("SQL parse error: {}", e))
        }
    }
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
    let mut stmts = Parser::parse_sql(dialect, &rewritten).ok()?;
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
