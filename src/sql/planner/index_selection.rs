//! Access path selection and expression/partial index matching
//!
//! Selects the best access path (full table scan, B-tree index scan variants)
//! for a given filter expression. Supports both AST and TypedExpr paths.
//! GIN predicate extraction helpers remain test-only until a real GIN execution
//! operator is implemented.

use std::collections::HashMap;

#[cfg(test)]
use sqlparser::ast::JsonOperator;
use sqlparser::ast::{BinaryOperator, Expr};

use super::predicate::analyze_predicates;
#[cfg(test)]
use super::scan_type::extract_column_ref;
use super::scan_type::{
    coerce_index_predicate_value, estimate_selectivity, extract_typed_conjuncts,
    normalize_expr_for_match, normalize_expr_string, parse_predicate_expr,
    typed_expr_to_canonical_sql,
};
use super::{AccessPath, PredicateInfo, PredicateOp, ScanType};
use crate::sql::expr::bridge::eval_const_ast_expr;
use crate::types::{IndexDef, TableSchema, Value};
use crate::worker::types::IndexState;

/// Choose the best access path for an optional AST filter expression.
///
/// **Legacy:** Only used by the EXPLAIN AST path for non-SELECT statements.
/// The primary path uses
/// [`choose_best_access_path_for_typed_filter`] which operates on the Analyzer's
/// TypedExpr IR.
pub fn choose_best_access_path_for_filter(
    _db_id: u64,
    schema: &TableSchema,
    filter: Option<&Expr>,
    estimated_table_rows: usize,
) -> AccessPath {
    let Some(filter_expr) = filter else {
        return AccessPath {
            scan_type: ScanType::FullTableScan,
            cost: estimated_table_rows as f64,
        };
    };

    let predicates = analyze_predicates(filter_expr);
    choose_best_access_path_with_filter(
        schema,
        &predicates,
        Some(filter_expr),
        estimated_table_rows,
    )
}

/// Choose the best access path using a typed filter expression (from the Analyzer).
///
/// This is the TypedExpr equivalent of [`choose_best_access_path_for_filter`].
/// Supports B-tree index selection, expression-index matching, and partial-index
/// predicate implication.
pub fn choose_best_access_path_for_typed_filter(
    _db_id: u64,
    schema: &TableSchema,
    filter: Option<&crate::sql::analyzer::types::TypedExpr>,
    estimated_table_rows: usize,
) -> AccessPath {
    let Some(filter_expr) = filter else {
        return AccessPath {
            scan_type: ScanType::FullTableScan,
            cost: estimated_table_rows as f64,
        };
    };

    let predicates = super::predicate::analyze_typed_predicates(filter_expr);
    choose_best_access_path_with_typed_filter(
        schema,
        &predicates,
        filter_expr,
        estimated_table_rows,
    )
}

/// Choose the best B-tree access path for a typed filter expression.
///
/// Used by the CBO physical planner for access-path selection.
/// This is the explicit runtime-safe entry point (full-table + B-tree variants).
///
/// While GIN access-path planning is disabled, this is behaviorally equivalent
/// to [`choose_best_access_path_for_typed_filter`]. It remains separate to keep
/// the runtime contract explicit and prevent future planner/executor drift when
/// GIN is reintroduced.
///
/// Covers: B-tree point lookup, range scan, bounded-range scan, in-list scan,
/// expression-index matching, and partial-index predicate implication.
pub fn choose_btree_access_path_for_typed_filter(
    schema: &TableSchema,
    filter: &crate::sql::analyzer::types::TypedExpr,
    estimated_table_rows: usize,
) -> AccessPath {
    let predicates = super::predicate::analyze_typed_predicates(filter);
    choose_best_access_path_with_typed_filter(schema, &predicates, filter, estimated_table_rows)
}

#[cfg(test)]
pub fn choose_best_access_path(
    schema: &TableSchema,
    predicates: &[PredicateInfo],
    estimated_table_rows: usize,
) -> AccessPath {
    choose_best_access_path_with_filter(schema, predicates, None, estimated_table_rows)
}

fn choose_best_access_path_with_filter(
    schema: &TableSchema,
    predicates: &[PredicateInfo],
    filter: Option<&Expr>,
    estimated_table_rows: usize,
) -> AccessPath {
    let mut best_path = AccessPath {
        scan_type: ScanType::FullTableScan,
        cost: estimated_table_rows as f64,
    };

    for index in &schema.indexes {
        if !is_planner_usable_index(index) {
            continue;
        }

        if let Some(index_predicate) = index.predicate.as_deref() {
            let Some(filter_expr) = filter else {
                continue;
            };
            if !query_implies_index_predicate(filter_expr, index_predicate) {
                continue;
            }
        }

        if !index.expressions.is_empty() {
            if let Some(filter_expr) = filter {
                if let Some((scan_type, cost)) =
                    evaluate_expression_index(index, filter_expr, estimated_table_rows)
                {
                    if cost < best_path.cost {
                        best_path = AccessPath { scan_type, cost };
                    }
                }
            }
            if index.columns.is_empty() {
                continue;
            }
        }

        if let Some((scan_type, cost)) =
            evaluate_index(schema, index, predicates, estimated_table_rows)
        {
            if cost < best_path.cost {
                best_path = AccessPath { scan_type, cost };
            }
        }
    }

    best_path
}

/// Typed-filter version of [`choose_best_access_path_with_filter`].
///
/// Uses a [`TypedExpr`] filter for expression-index matching and partial-index
/// predicate implication, replacing the AST-based logic.
fn choose_best_access_path_with_typed_filter(
    schema: &TableSchema,
    predicates: &[PredicateInfo],
    typed_filter: &crate::sql::analyzer::types::TypedExpr,
    estimated_table_rows: usize,
) -> AccessPath {
    let mut best_path = AccessPath {
        scan_type: ScanType::FullTableScan,
        cost: estimated_table_rows as f64,
    };

    for index in &schema.indexes {
        if !is_planner_usable_index(index) {
            continue;
        }

        // Partial-index predicate implication: check that the query filter
        // implies the index predicate (e.g. WHERE status = 'active' implies
        // a partial index on status = 'active').
        if let Some(index_predicate) = index.predicate.as_deref() {
            if !query_implies_index_predicate_typed(typed_filter, index_predicate) {
                continue;
            }
        }

        // Expression-index matching (e.g. CREATE INDEX ON t (lower(name))).
        if !index.expressions.is_empty() {
            if let Some((scan_type, cost)) =
                evaluate_expression_index_typed(index, typed_filter, estimated_table_rows)
            {
                if cost < best_path.cost {
                    best_path = AccessPath { scan_type, cost };
                }
            }
            if index.columns.is_empty() {
                continue;
            }
        }

        // Regular B-tree column index matching.
        if let Some((scan_type, cost)) =
            evaluate_index(schema, index, predicates, estimated_table_rows)
        {
            if cost < best_path.cost {
                best_path = AccessPath { scan_type, cost };
            }
        }
    }

    best_path
}

fn is_planner_usable_index(index: &IndexDef) -> bool {
    if index.state != IndexState::Ready {
        return false;
    }
    if index.columns.is_empty() && index.expressions.is_empty() {
        return false;
    }
    index
        .method
        .as_deref()
        .map(|m| m.eq_ignore_ascii_case("btree"))
        .unwrap_or(true)
}

/// Compute index scan cost from estimated matching rows.
fn index_scan_cost(estimated_rows: usize) -> f64 {
    1.0 + estimated_rows as f64 * 0.5
}

fn evaluate_expression_index(
    index: &IndexDef,
    filter: &Expr,
    estimated_table_rows: usize,
) -> Option<(ScanType, f64)> {
    let values = match_expression_predicates(index, filter)?;
    let selectivity = estimate_selectivity(index, values.len(), true);
    let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
    let cost = index_scan_cost(estimated_rows);

    Some((
        ScanType::IndexScan {
            index_id: index.id,
            index_name: index.name.clone(),
            values,
            estimated_rows,
        },
        cost,
    ))
}

fn match_expression_predicates(index: &IndexDef, filter: &Expr) -> Option<Vec<Value>> {
    if index.expressions.is_empty() {
        return None;
    }

    let filter_conjuncts = extract_conjuncts(filter);
    let mut values = Vec::with_capacity(index.expressions.len());

    for expr_str in &index.expressions {
        let expr_ast = parse_predicate_expr(expr_str)?;
        let normalized_expr = normalize_expr_for_match(&expr_ast);
        let mut matched_value = None;

        for conjunct in &filter_conjuncts {
            if let Expr::BinaryOp {
                left,
                op: BinaryOperator::Eq,
                right,
            } = conjunct
            {
                let left_norm = normalize_expr_for_match(left);
                let right_norm = normalize_expr_for_match(right);

                if left_norm == normalized_expr {
                    if let Ok(value) = eval_const_ast_expr(right) {
                        matched_value = Some(value);
                        break;
                    }
                }
                if right_norm == normalized_expr {
                    if let Ok(value) = eval_const_ast_expr(left) {
                        matched_value = Some(value);
                        break;
                    }
                }
            }
        }

        values.push(matched_value?);
    }

    Some(values)
}

fn query_implies_index_predicate(filter: &Expr, index_predicate: &str) -> bool {
    let Some(index_pred_expr) = parse_predicate_expr(index_predicate) else {
        return false;
    };

    let query_conjuncts = extract_conjuncts(filter)
        .iter()
        .map(normalize_expr_for_match)
        .collect::<Vec<_>>();

    extract_conjuncts(&index_pred_expr)
        .iter()
        .map(normalize_expr_for_match)
        .all(|idx_conj| query_conjuncts.iter().any(|q| q == &idx_conj))
}

pub(super) fn extract_conjuncts(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut result = extract_conjuncts(left);
            result.extend(extract_conjuncts(right));
            result
        }
        Expr::Nested(inner) => extract_conjuncts(inner),
        other => vec![other.clone()],
    }
}

fn evaluate_index(
    schema: &TableSchema,
    index: &IndexDef,
    predicates: &[PredicateInfo],
    estimated_table_rows: usize,
) -> Option<(ScanType, f64)> {
    let eq_predicate_map: HashMap<&str, &PredicateInfo> = predicates
        .iter()
        .filter(|p| p.op == PredicateOp::Eq)
        .map(|p| (p.column.as_str(), p))
        .collect();

    let mut prefix_values = Vec::new();

    for col in &index.columns {
        if let Some(pred) = eq_predicate_map.get(col.as_str()) {
            prefix_values.push(coerce_index_predicate_value(schema, col, &pred.value));
        } else {
            break;
        }
    }

    if prefix_values.len() == index.columns.len() {
        let selectivity = estimate_selectivity(index, prefix_values.len(), true);
        let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
        let cost = index_scan_cost(estimated_rows);

        return Some((
            ScanType::IndexScan {
                index_id: index.id,
                index_name: index.name.clone(),
                values: prefix_values,
                estimated_rows,
            },
            cost,
        ));
    }

    let Some(next_col) = index.columns.get(prefix_values.len()) else {
        return None;
    };

    if let Some(in_pred) = predicates.iter().find(|p| {
        p.op == PredicateOp::In
            && p.column.eq_ignore_ascii_case(next_col)
            && !p.in_values.is_empty()
    }) {
        let mut column_values = Vec::with_capacity(in_pred.in_values.len());
        for in_value in &in_pred.in_values {
            let mut lookup = prefix_values.clone();
            lookup.push(coerce_index_predicate_value(schema, next_col, in_value));
            column_values.push(lookup);
        }

        let table_rows = estimated_table_rows.max(1);
        let selectivity = ((in_pred.in_values.len() as f64) * (1.0 / table_rows as f64)).min(0.5);
        let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
        let cost = index_scan_cost(estimated_rows);

        return Some((
            ScanType::InListScan {
                index_id: index.id,
                index_name: index.name.clone(),
                column_values,
                estimated_rows,
            },
            cost,
        ));
    }

    let lower_inclusive = predicates
        .iter()
        .find(|p| p.column.eq_ignore_ascii_case(next_col) && p.op == PredicateOp::Ge);
    let lower_exclusive = predicates
        .iter()
        .find(|p| p.column.eq_ignore_ascii_case(next_col) && p.op == PredicateOp::Gt);
    let upper_inclusive = predicates
        .iter()
        .find(|p| p.column.eq_ignore_ascii_case(next_col) && p.op == PredicateOp::Le);
    let upper_exclusive = predicates
        .iter()
        .find(|p| p.column.eq_ignore_ascii_case(next_col) && p.op == PredicateOp::Lt);

    let (range_start, start_inclusive) = if let Some(pred) = lower_inclusive {
        (
            Some(coerce_index_predicate_value(schema, next_col, &pred.value)),
            true,
        )
    } else if let Some(pred) = lower_exclusive {
        (
            Some(coerce_index_predicate_value(schema, next_col, &pred.value)),
            false,
        )
    } else {
        (None, true)
    };

    let (range_end, end_inclusive) = if let Some(pred) = upper_inclusive {
        (
            Some(coerce_index_predicate_value(schema, next_col, &pred.value)),
            true,
        )
    } else if let Some(pred) = upper_exclusive {
        (
            Some(coerce_index_predicate_value(schema, next_col, &pred.value)),
            false,
        )
    } else {
        (None, true)
    };

    if range_start.is_some() || range_end.is_some() {
        let two_sided = range_start.is_some() && range_end.is_some();
        let selectivity = if two_sided { 0.1 } else { 0.3 };
        let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
        let cost = index_scan_cost(estimated_rows);

        return Some((
            ScanType::IndexBoundedRangeScan {
                index_id: index.id,
                index_name: index.name.clone(),
                prefix_values,
                range_start,
                start_inclusive,
                range_end,
                end_inclusive,
                estimated_rows,
            },
            cost,
        ));
    }

    if prefix_values.is_empty() {
        return None;
    }

    let selectivity = estimate_selectivity(index, prefix_values.len(), false);
    let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
    let cost = index_scan_cost(estimated_rows);

    Some((
        ScanType::IndexRangeScan {
            index_id: index.id,
            index_name: index.name.clone(),
            prefix_values,
            estimated_rows,
        },
        cost,
    ))
}

/// Check if a typed query filter implies a partial-index predicate.
///
/// Mirrors [`query_implies_index_predicate`]: the index predicate is stored as a
/// string, so we parse it to AST and normalize.  The query conjuncts come from the
/// TypedExpr tree via [`typed_expr_to_canonical_sql`] + [`normalize_expr_string`].
fn query_implies_index_predicate_typed(
    filter: &crate::sql::analyzer::types::TypedExpr,
    index_predicate: &str,
) -> bool {
    let Some(index_pred_expr) = parse_predicate_expr(index_predicate) else {
        return false;
    };

    let query_conjuncts: Vec<String> = extract_typed_conjuncts(filter)
        .iter()
        .map(|c| normalize_expr_string(typed_expr_to_canonical_sql(c)))
        .collect();

    extract_conjuncts(&index_pred_expr)
        .iter()
        .map(normalize_expr_for_match)
        .all(|idx_conj| query_conjuncts.iter().any(|q| q == &idx_conj))
}

/// Evaluate expression-index applicability using a typed filter.
///
/// Mirrors [`evaluate_expression_index`] + [`match_expression_predicates`]:
/// for each expression in the index, check if any typed filter conjunct
/// of the form `expr = constant` matches (after canonical normalization).
fn evaluate_expression_index_typed(
    index: &IndexDef,
    filter: &crate::sql::analyzer::types::TypedExpr,
    estimated_table_rows: usize,
) -> Option<(ScanType, f64)> {
    let values = match_expression_predicates_typed(index, filter)?;
    let selectivity = estimate_selectivity(index, values.len(), true);
    let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
    let cost = index_scan_cost(estimated_rows);

    Some((
        ScanType::IndexScan {
            index_id: index.id,
            index_name: index.name.clone(),
            values,
            estimated_rows,
        },
        cost,
    ))
}

/// Match expression-index expressions against typed filter conjuncts.
///
/// For each index expression string:
/// 1. Parse to AST -> `normalize_expr_for_match` (the "index side")
/// 2. For each typed filter conjunct of the form `lhs = rhs`:
///    - Canonicalize lhs/rhs via `typed_expr_to_canonical_sql` -> `normalize_expr_string`
///    - If one side matches the index expression, the other must be a constant value
fn match_expression_predicates_typed(
    index: &IndexDef,
    filter: &crate::sql::analyzer::types::TypedExpr,
) -> Option<Vec<Value>> {
    use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExprKind};

    if index.expressions.is_empty() {
        return None;
    }

    let filter_conjuncts = extract_typed_conjuncts(filter);
    let mut values = Vec::with_capacity(index.expressions.len());

    for expr_str in &index.expressions {
        let expr_ast = parse_predicate_expr(expr_str)?;
        let normalized_expr = normalize_expr_for_match(&expr_ast);
        let mut matched_value = None;

        for conjunct in &filter_conjuncts {
            if let TypedExprKind::BinaryOp {
                left,
                op: TypedBinaryOp::Eq,
                right,
            } = &conjunct.kind
            {
                let left_norm = normalize_expr_string(typed_expr_to_canonical_sql(left));
                let right_norm = normalize_expr_string(typed_expr_to_canonical_sql(right));

                if left_norm == normalized_expr {
                    if let TypedExprKind::Constant(v) = &right.kind {
                        matched_value = Some(v.clone());
                        break;
                    }
                }
                if right_norm == normalized_expr {
                    if let TypedExprKind::Constant(v) = &left.kind {
                        matched_value = Some(v.clone());
                        break;
                    }
                }
            }
        }

        values.push(matched_value?);
    }

    Some(values)
}

// ---- GIN predicate extraction (test-only helper) ----

#[cfg(test)]
pub(super) fn extract_gin_contains_predicate(expr: &Expr) -> Option<(String, Value)> {
    let predicates = collect_gin_predicates(expr);
    if predicates.is_empty() {
        return None;
    }

    predicates
        .into_iter()
        .min_by_key(|(_, value)| gin_predicate_priority(value))
}

#[cfg(test)]
fn collect_gin_predicates(expr: &Expr) -> Vec<(String, Value)> {
    match expr {
        Expr::Nested(inner) => collect_gin_predicates(inner),
        Expr::BinaryOp { left, op, right } if matches!(op, BinaryOperator::And) => {
            let mut predicates = collect_gin_predicates(left);
            predicates.extend(collect_gin_predicates(right));
            predicates
        }
        Expr::BinaryOp { left, op, right }
            if is_gin_binary_operator(op, "@@") || is_gin_binary_operator(op, "@>") =>
        {
            match (extract_column_ref(left), eval_const_ast_expr(right).ok()) {
                (Some(column), Some(pattern)) => vec![(column.name, pattern)],
                _ => Vec::new(),
            }
        }
        Expr::JsonAccess {
            left,
            operator,
            right,
        } if matches!(operator, JsonOperator::AtArrow) => {
            collect_json_access_gin_predicates(left, right)
        }
        Expr::JsonAccess {
            left,
            operator,
            right,
        } if matches!(operator, JsonOperator::AtAt) => {
            collect_json_access_gin_predicates(left, right)
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
fn is_gin_binary_operator(op: &BinaryOperator, expected: &str) -> bool {
    matches!(op, BinaryOperator::PGCustomBinaryOperator(parts) if parts.join("") == expected)
}

#[cfg(test)]
fn collect_json_access_gin_predicates(left: &Expr, right: &Expr) -> Vec<(String, Value)> {
    let Some(column) = extract_column_ref(left).map(|col| col.name) else {
        return Vec::new();
    };

    match right {
        Expr::BinaryOp {
            left: rhs_left,
            op,
            right: rhs_right,
        } if matches!(op, BinaryOperator::And) => {
            let mut predicates = Vec::new();
            if let Ok(pattern) = eval_const_ast_expr(rhs_left) {
                predicates.push((column, pattern));
            }
            predicates.extend(collect_gin_predicates(rhs_right));
            predicates
        }
        _ => match eval_const_ast_expr(right).ok() {
            Some(pattern) => vec![(column, pattern)],
            None => Vec::new(),
        },
    }
}

#[cfg(test)]
fn gin_predicate_priority(value: &Value) -> u8 {
    match value {
        Value::Tsquery(_) => 0,
        Value::Array(_) => 1,
        Value::Json(_) | Value::Jsonb(_) => 2,
        _ => 3,
    }
}
