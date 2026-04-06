//! Re-export all types from the `db9-model` crate.
//!
//! This module exists so that existing `use crate::model::…` imports continue
//! to work without modification throughout the main crate.  New code may use
//! `use db9_model::…` directly.

pub use db9_model::date;
pub use db9_model::timestamp;
pub use db9_model::*;

/// Hydrate the runtime predicate conjunct caches for all indexes in a schema.
///
/// Lives in the main crate (not `db9-model`) because it depends on the
/// project's canonical SQL parser which applies parse-compat rewrites.
pub fn hydrate_runtime_caches(schema: &mut TableSchema) {
    for index in &mut schema.indexes {
        index.cached_predicate_conjuncts =
            build_predicate_conjunct_cache(index.predicate.as_deref());
    }
}

pub fn build_predicate_conjunct_cache(predicate: Option<&str>) -> Option<Vec<String>> {
    let predicate = predicate?.trim();
    if predicate.is_empty() {
        return None;
    }

    let sql = format!("SELECT {}", predicate);
    let stmts = crate::sql::parse_sql(&sql).ok()?;
    let sqlparser::ast::Statement::Query(query) = stmts.into_iter().next()? else {
        return None;
    };
    let sqlparser::ast::SetExpr::Select(select) = *query.body else {
        return None;
    };
    let sqlparser::ast::SelectItem::UnnamedExpr(expr) = select.projection.into_iter().next()?
    else {
        return None;
    };

    Some(
        extract_predicate_conjuncts(&expr)
            .into_iter()
            .map(|conjunct| normalize_predicate_conjunct(conjunct.to_string()))
            .collect(),
    )
}

fn extract_predicate_conjuncts(expr: &sqlparser::ast::Expr) -> Vec<sqlparser::ast::Expr> {
    match expr {
        sqlparser::ast::Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::And,
            right,
        } => {
            let mut result = extract_predicate_conjuncts(left);
            result.extend(extract_predicate_conjuncts(right));
            result
        }
        sqlparser::ast::Expr::Nested(inner) => extract_predicate_conjuncts(inner),
        other => vec![other.clone()],
    }
}

fn normalize_predicate_conjunct(mut input: String) -> String {
    input = input.trim().to_string();
    while has_wrapping_parentheses(&input) {
        input = input[1..input.len().saturating_sub(1)].trim().to_string();
    }
    input
        .replace('"', "")
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn has_wrapping_parentheses(input: &str) -> bool {
    if input.len() < 2 || !input.starts_with('(') || !input.ends_with(')') {
        return false;
    }

    let mut depth = 0_i32;
    for (idx, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && idx + 1 < input.len() {
                    return false;
                }
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }

    depth == 0
}
