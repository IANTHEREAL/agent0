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
            Err(anyhow!("SQL parse error: {}", e))
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
