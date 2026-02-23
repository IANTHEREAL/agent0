//! Access path selection and expression/partial index matching
//!
//! Selects the best access path (full table scan, B-tree index scan variants)
//! for a given typed filter expression. Supports expression-index matching and
//! partial-index predicate implication.

use super::scan_type::{
    coerce_index_predicate_value, estimate_selectivity, extract_typed_conjuncts,
    normalize_expr_for_match, normalize_expr_string, parse_predicate_expr,
    typed_expr_to_canonical_sql,
};
use super::{AccessPath, PredicateInfo, PredicateOp, ScanType};
use crate::types::{IndexDef, TableSchema, Value};
use crate::worker::types::IndexState;

/// Choose the best B-tree access path for a typed filter expression.
///
/// Used by the CBO physical planner for access-path selection.
/// This is the explicit runtime-safe entry point (full-table + B-tree variants).
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

/// Typed-filter version of access path selection.
///
/// Uses a [`TypedExpr`] filter for expression-index matching and partial-index
/// predicate implication.
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

fn evaluate_index(
    schema: &TableSchema,
    index: &IndexDef,
    predicates: &[PredicateInfo],
    estimated_table_rows: usize,
) -> Option<(ScanType, f64)> {
    let eq_predicate_map: std::collections::HashMap<&str, &PredicateInfo> = predicates
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
        },
        cost,
    ))
}

/// Check if a typed query filter implies a partial-index predicate.
///
/// The index predicate is stored as a string, so we parse it to AST and normalize.
/// The query conjuncts come from the TypedExpr tree via
/// [`typed_expr_to_canonical_sql`] + [`normalize_expr_string`].
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

fn extract_conjuncts(expr: &sqlparser::ast::Expr) -> Vec<sqlparser::ast::Expr> {
    match expr {
        sqlparser::ast::Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::And,
            right,
        } => {
            let mut result = extract_conjuncts(left);
            result.extend(extract_conjuncts(right));
            result
        }
        sqlparser::ast::Expr::Nested(inner) => extract_conjuncts(inner),
        other => vec![other.clone()],
    }
}

/// Evaluate expression-index applicability using a typed filter.
///
/// For each expression in the index, check if any typed filter conjunct
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
