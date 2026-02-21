//! Index cost evaluation and utility functions
//!
//! Contains helpers for column reference extraction, expression normalization,
//! cost estimation, and TypedExpr-to-SQL canonicalization used by index selection.

use sqlparser::ast::Expr;

use crate::sql::names::normalize_ident;
use crate::types::{IndexDef, TableSchema, Value};

// ---- TypedExpr expression-index and partial-index support ----

/// Render a [`Value`] as a SQL literal string.
///
/// Text values are single-quoted (e.g. `'hello'`), numbers are unquoted,
/// booleans are `true`/`false`, NULL is `NULL`.  This must match the way
/// sqlparser renders literals so that normalization produces identical strings
/// for the AST and TypedExpr code paths.
pub(super) fn value_to_sql_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => format!("{}", b),
        Value::Int32(i) => format!("{}", i),
        Value::Int64(i) => format!("{}", i),
        Value::Float64(f) => format!("{}", f),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        _ => format!("{}", v),
    }
}

/// Semantic SQL canonicalizer for [`TypedExpr`].
///
/// Produces a normalized SQL string suitable for matching against expression-index
/// definitions.  Unlike `TypedExpr::Display` which is presentation-oriented:
/// - CAST uses proper SQL type names (not `{:?}` Debug format)
/// - ColumnRef uses the column name directly (lowercased)
/// - Function calls use canonical `func(args)` syntax
/// - Minimal parenthesization (only around binary ops)
///
/// The output is fed through [`normalize_expr_string`] for final comparison
/// against index definitions parsed via `parse_predicate_expr` -> `normalize_expr_for_match`.
pub(super) fn typed_expr_to_canonical_sql(expr: &crate::sql::analyzer::types::TypedExpr) -> String {
    use crate::sql::analyzer::types::TypedExprKind;

    match &expr.kind {
        TypedExprKind::Constant(v) => value_to_sql_literal(v),
        TypedExprKind::ColumnRef { column_name, .. } => column_name.to_lowercase(),
        TypedExprKind::BinaryOp { left, op, right } => {
            format!(
                "({} {} {})",
                typed_expr_to_canonical_sql(left),
                op,
                typed_expr_to_canonical_sql(right)
            )
        }
        TypedExprKind::UnaryOp { op, operand } => {
            format!("({}{})", op, typed_expr_to_canonical_sql(operand))
        }
        TypedExprKind::Cast {
            expr, target_type, ..
        } => {
            // Use DataType::Display which produces proper SQL names (e.g. "TEXT", "BIGINT")
            format!(
                "CAST({} AS {})",
                typed_expr_to_canonical_sql(expr),
                target_type
            )
        }
        TypedExprKind::FunctionCall { func, args, .. } => {
            let arg_strs: Vec<String> = args.iter().map(typed_expr_to_canonical_sql).collect();
            format!("{}({})", func.name.to_lowercase(), arg_strs.join(", "))
        }
        // For expression-index matching, other node types are unlikely to appear in
        // index definitions.  Fall back to Display for a best-effort string.
        other_kind => {
            let temp = crate::sql::analyzer::types::TypedExpr {
                kind: other_kind.clone(),
                data_type: expr.data_type.clone(),
            };
            format!("{}", temp)
        }
    }
}

/// Extract conjuncts from a typed expression tree (AND decomposition).
pub(super) fn extract_typed_conjuncts(
    expr: &crate::sql::analyzer::types::TypedExpr,
) -> Vec<&crate::sql::analyzer::types::TypedExpr> {
    use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExprKind};

    match &expr.kind {
        TypedExprKind::BinaryOp {
            left,
            op: TypedBinaryOp::And,
            right,
        } => {
            let mut result = extract_typed_conjuncts(left);
            result.extend(extract_typed_conjuncts(right));
            result
        }
        _ => vec![expr],
    }
}

pub(super) fn parse_predicate_expr(expr_str: &str) -> Option<Expr> {
    let sql = format!("SELECT {}", expr_str);
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
    Some(expr)
}

pub(super) fn normalize_expr_for_match(expr: &Expr) -> String {
    normalize_expr_string(expr.to_string())
}

pub(super) fn normalize_expr_string(mut input: String) -> String {
    input = input.trim().to_string();
    while has_wrapping_parentheses(&input) {
        input = input[1..input.len().saturating_sub(1)].trim().to_string();
    }
    let without_quotes = input.replace('"', "").to_lowercase();
    without_quotes
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

pub(super) fn coerce_index_predicate_value(
    schema: &TableSchema,
    col: &str,
    value: &Value,
) -> Value {
    if let Some(col_def) = schema
        .columns
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(col))
    {
        crate::sql::value_coercion::coerce_value_for_column(value.clone(), col_def)
            .unwrap_or_else(|_| value.clone())
    } else {
        value.clone()
    }
}

pub(super) fn estimate_selectivity(index: &IndexDef, matched_cols: usize, full_match: bool) -> f64 {
    let base_selectivity = if index.unique && full_match {
        1.0 / 1000000.0
    } else {
        0.1_f64.powi(matched_cols as i32)
    };

    base_selectivity.max(0.0001)
}

pub(super) fn eval_const_typed_expr(
    expr: &crate::sql::analyzer::types::TypedExpr,
) -> Option<Value> {
    use crate::sql::analyzer::types::TypedExprKind;

    match &expr.kind {
        TypedExprKind::Constant(v) => Some(v.clone()),
        _ => {
            let qctx = crate::sql::query_context::QueryContext::from_task_locals();
            let row = crate::types::Row::new(vec![]);
            crate::sql::expr::typed_eval::eval_typed_expr(expr, &row, &qctx).ok()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ColumnRef {
    pub(super) qualifier: Option<String>,
    pub(super) name: String,
}

pub(super) fn extract_column_ref(expr: &Expr) -> Option<ColumnRef> {
    match expr {
        Expr::Identifier(ident) => Some(ColumnRef {
            qualifier: None,
            name: normalize_ident(ident),
        }),
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let qualifier = parts
                .get(parts.len().saturating_sub(2))
                .map(normalize_ident);
            let name = parts.last().map(normalize_ident)?;
            Some(ColumnRef { qualifier, name })
        }
        Expr::Nested(inner) => extract_column_ref(inner),
        _ => None,
    }
}

pub(super) fn resolve_column_index(schema: &TableSchema, col: &ColumnRef) -> Option<usize> {
    if let Some(qualifier) = col.qualifier.as_deref() {
        let qualified = format!("{}.{}", qualifier, col.name);
        if let Some(idx) = schema.column_index(&qualified) {
            return Some(idx);
        }
        if let Some(idx) = schema
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&qualified))
        {
            return Some(idx);
        }
    }

    if let Some(idx) = schema.column_index(&col.name) {
        return Some(idx);
    }
    if let Some(idx) = schema
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(&col.name))
    {
        return Some(idx);
    }

    let mut match_idx: Option<usize> = None;
    for (idx, schema_col) in schema.columns.iter().enumerate() {
        let unqualified = schema_col
            .name
            .rsplit('.')
            .next()
            .unwrap_or(&schema_col.name);
        if unqualified.eq_ignore_ascii_case(&col.name) {
            if match_idx.is_some() {
                return None;
            }
            match_idx = Some(idx);
        }
    }
    match_idx
}
