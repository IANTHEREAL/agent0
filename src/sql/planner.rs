//! Query planner for cost-based optimization
//!
//! This module provides query planning and optimization capabilities:
//! - Cost-based index selection
//! - Predicate analysis
//! - Access path selection

use std::collections::HashMap;

use sqlparser::ast::{BinaryOperator, Expr, JsonOperator};

use super::expr::bridge::eval_const_ast_expr;
use super::names::normalize_ident;
use super::operators::HashJoinConfig;
use crate::types::{DataType, IndexDef, TableSchema, Value};

/// Result of join algorithm selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinAlgorithmChoice {
    /// Classic nested-loop join (O(N×M)).
    NestedLoop,
    /// Hash join for equi-joins (O(N+M)).
    HashJoin {
        /// Whether the logical left input is chosen as the build side.
        left_is_build: bool,
        /// Join key column indices in the logical left input.
        left_key_indices: Vec<usize>,
        /// Join key column indices in the logical right input.
        right_key_indices: Vec<usize>,
    },
}

/// Choose join algorithm based on join condition and row-count estimates.
///
/// This only selects hash join for simple equi-join conditions of the form:
/// `a.col = b.col [AND ...]`.
pub fn choose_join_algorithm(
    join_condition: Option<&Expr>,
    left_schema: &TableSchema,
    right_schema: &TableSchema,
    left_row_estimate: usize,
    right_row_estimate: usize,
    config: &HashJoinConfig,
) -> JoinAlgorithmChoice {
    let Some(cond) = join_condition else {
        return JoinAlgorithmChoice::NestedLoop;
    };

    let Some((left_keys, right_keys)) = extract_equi_join_keys(cond, left_schema, right_schema)
    else {
        return JoinAlgorithmChoice::NestedLoop;
    };

    if !hash_join_keys_type_compatible(left_schema, right_schema, &left_keys, &right_keys) {
        return JoinAlgorithmChoice::NestedLoop;
    }

    if left_row_estimate + right_row_estimate < config.min_rows_threshold {
        return JoinAlgorithmChoice::NestedLoop;
    }

    let left_is_build = left_row_estimate <= right_row_estimate;
    JoinAlgorithmChoice::HashJoin {
        left_is_build,
        left_key_indices: left_keys,
        right_key_indices: right_keys,
    }
}

fn hash_join_keys_type_compatible(
    left_schema: &TableSchema,
    right_schema: &TableSchema,
    left_keys: &[usize],
    right_keys: &[usize],
) -> bool {
    if left_keys.len() != right_keys.len() {
        return false;
    }

    left_keys
        .iter()
        .copied()
        .zip(right_keys.iter().copied())
        .all(|(li, ri)| {
            let Some(left_col) = left_schema.columns.get(li) else {
                return false;
            };
            let Some(right_col) = right_schema.columns.get(ri) else {
                return false;
            };
            hash_join_key_type_compatible(&left_col.data_type, &right_col.data_type)
        })
}

fn hash_join_key_type_compatible(left: &DataType, right: &DataType) -> bool {
    match (left, right) {
        (DataType::Int32, DataType::Int64) | (DataType::Int64, DataType::Int32) => true,
        (DataType::Numeric { .. }, DataType::Numeric { .. }) => true,
        (DataType::Timestamp, DataType::TimestampTz)
        | (DataType::TimestampTz, DataType::Timestamp) => true,
        _ => left == right,
    }
}

/// Extract equi-join key indices from a join condition expression.
///
/// Returns `(left_key_indices, right_key_indices)` if the expression is a conjunction of
/// equality predicates comparing one left column to one right column.
pub(crate) fn extract_equi_join_keys(
    expr: &Expr,
    left_schema: &TableSchema,
    right_schema: &TableSchema,
) -> Option<(Vec<usize>, Vec<usize>)> {
    match expr {
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::Eq => extract_column_pair(left, right, left_schema, right_schema),
            BinaryOperator::And => {
                let (mut lk1, mut rk1) = extract_equi_join_keys(left, left_schema, right_schema)?;
                let (lk2, rk2) = extract_equi_join_keys(right, left_schema, right_schema)?;
                lk1.extend(lk2);
                rk1.extend(rk2);
                Some((lk1, rk1))
            }
            _ => None,
        },
        Expr::Nested(inner) => extract_equi_join_keys(inner, left_schema, right_schema),
        _ => None,
    }
}

fn extract_column_pair(
    left_expr: &Expr,
    right_expr: &Expr,
    left_schema: &TableSchema,
    right_schema: &TableSchema,
) -> Option<(Vec<usize>, Vec<usize>)> {
    let left_col = extract_column_ref(left_expr)?;
    let right_col = extract_column_ref(right_expr)?;

    if let (Some(li), Some(ri)) = (
        resolve_column_index(left_schema, &left_col),
        resolve_column_index(right_schema, &right_col),
    ) {
        return Some((vec![li], vec![ri]));
    }

    if let (Some(li), Some(ri)) = (
        resolve_column_index(left_schema, &right_col),
        resolve_column_index(right_schema, &left_col),
    ) {
        return Some((vec![li], vec![ri]));
    }

    None
}

/// Split a join condition into equi-join keys and residual filter.
///
/// This function separates the conjuncts of an AND expression into:
/// 1. Equi-join predicates (e.g., `a.id = b.id`) that can be used for hash join
/// 2. Residual predicates (e.g., `a.val > 10`) that must be applied as a filter
///
/// Returns `(left_key_indices, right_key_indices, residual_filter)`.
/// If no equi-join keys are found, returns `None`.
#[cfg(test)]
pub(crate) fn split_join_condition(
    expr: &Expr,
    left_schema: &TableSchema,
    right_schema: &TableSchema,
) -> Option<(Vec<usize>, Vec<usize>, Option<Expr>)> {
    fn split_conjuncts(expr: &Expr, conjuncts: &mut Vec<Expr>) {
        match expr {
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => {
                split_conjuncts(left, conjuncts);
                split_conjuncts(right, conjuncts);
            }
            Expr::Nested(inner) => split_conjuncts(inner, conjuncts),
            other => conjuncts.push(other.clone()),
        }
    }

    let mut conjuncts = Vec::new();
    split_conjuncts(expr, &mut conjuncts);

    let mut left_keys = Vec::new();
    let mut right_keys = Vec::new();
    let mut residual_conjuncts = Vec::new();

    for conjunct in conjuncts {
        if let Expr::BinaryOp {
            left: left_expr,
            op: BinaryOperator::Eq,
            right: right_expr,
        } = &conjunct
        {
            if let Some((lk, rk)) =
                extract_column_pair(left_expr, right_expr, left_schema, right_schema)
            {
                left_keys.extend(lk);
                right_keys.extend(rk);
                continue;
            }
        }
        residual_conjuncts.push(conjunct);
    }

    if left_keys.is_empty() {
        return None;
    }

    let residual = if residual_conjuncts.is_empty() {
        None
    } else {
        Some(
            residual_conjuncts
                .into_iter()
                .reduce(|a, b| Expr::BinaryOp {
                    left: Box::new(a),
                    op: BinaryOperator::And,
                    right: Box::new(b),
                })
                .unwrap(),
        )
    };

    Some((left_keys, right_keys, residual))
}

#[derive(Debug, Clone)]
pub enum ScanType {
    FullTableScan,
    IndexScan {
        index_id: u64,
        index_name: String,
        values: Vec<Value>,
        estimated_rows: usize,
    },
    IndexRangeScan {
        index_id: u64,
        index_name: String,
        prefix_values: Vec<Value>,
        estimated_rows: usize,
    },
    IndexBoundedRangeScan {
        index_id: u64,
        index_name: String,
        prefix_values: Vec<Value>,
        range_start: Option<Value>,
        start_inclusive: bool,
        range_end: Option<Value>,
        end_inclusive: bool,
        estimated_rows: usize,
    },
    InListScan {
        index_id: u64,
        index_name: String,
        column_values: Vec<Vec<Value>>,
        estimated_rows: usize,
    },
    GinIndexScan {
        #[allow(dead_code)] // planned GIN index scan feature
        index_id: u64,
        index_name: String,
        #[allow(dead_code)] // planned GIN index scan feature
        column: String,
        #[allow(dead_code)] // planned GIN index scan feature
        pattern: Value,
        estimated_rows: usize,
    },
}

#[derive(Debug, Clone)]
pub struct AccessPath {
    pub scan_type: ScanType,
    pub cost: f64,
}

#[derive(Debug, Clone)]
pub struct PredicateInfo {
    pub column: String,
    pub op: PredicateOp,
    pub value: Value,
    pub in_values: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PredicateOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    IsNull,
    IsNotNull,
}

pub fn analyze_predicates(expr: &Expr) -> Vec<PredicateInfo> {
    let mut predicates = Vec::new();
    collect_predicates(expr, &mut predicates);
    predicates
}

/// Choose the best access path for an optional AST filter expression.
///
/// **Legacy:** Only used by the EXPLAIN AST fallback path (non-SELECT statements
/// and analysis error recovery).  The primary path uses
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
    let mut best = choose_best_access_path_with_filter(
        schema,
        &predicates,
        Some(filter_expr),
        estimated_table_rows,
    );

    if let Some(gin_path) = choose_gin_access_path(schema, filter_expr, estimated_table_rows) {
        if gin_path.cost < best.cost {
            best = gin_path;
        }
    }

    best
}

/// Choose the best access path using a typed filter expression (from the Analyzer).
///
/// This is the TypedExpr equivalent of [`choose_best_access_path_for_filter`].
/// Supports B-tree index selection, GIN index selection, expression-index matching,
/// and partial-index predicate implication — full parity with the AST path.
pub fn choose_best_access_path_for_typed_filter(
    _db_id: u64,
    schema: &TableSchema,
    filter: Option<&super::analyzer::types::TypedExpr>,
    estimated_table_rows: usize,
) -> AccessPath {
    let Some(filter_expr) = filter else {
        return AccessPath {
            scan_type: ScanType::FullTableScan,
            cost: estimated_table_rows as f64,
        };
    };

    let predicates = analyze_typed_predicates(filter_expr);
    let mut best = choose_best_access_path_with_typed_filter(
        schema,
        &predicates,
        filter_expr,
        estimated_table_rows,
    );

    // GIN index selection (@@, @>, <@ operators on JSONB/Array/Tsvector columns).
    if let Some(gin_path) = choose_gin_access_path_typed(schema, filter_expr, estimated_table_rows)
    {
        if gin_path.cost < best.cost {
            best = gin_path;
        }
    }

    best
}

/// Extract [`PredicateInfo`] from a [`TypedExpr`] tree.
///
/// Simpler than the AST version: column names and constant values are already
/// resolved by the Analyzer.
pub fn analyze_typed_predicates(expr: &super::analyzer::types::TypedExpr) -> Vec<PredicateInfo> {
    let mut predicates = Vec::new();
    collect_typed_predicates(expr, &mut predicates);
    predicates
}

fn collect_typed_predicates(
    expr: &super::analyzer::types::TypedExpr,
    predicates: &mut Vec<PredicateInfo>,
) {
    use super::analyzer::types::{BinaryOp as TypedBinaryOp, IsTestKind, TypedExprKind};

    match &expr.kind {
        TypedExprKind::BinaryOp { left, op, right } => match op {
            TypedBinaryOp::And => {
                collect_typed_predicates(left, predicates);
                collect_typed_predicates(right, predicates);
            }
            TypedBinaryOp::Or => {} // can't use for index selection
            _ => {
                let pred_op = match op {
                    TypedBinaryOp::Eq => Some(PredicateOp::Eq),
                    TypedBinaryOp::NotEq => Some(PredicateOp::Ne),
                    TypedBinaryOp::Lt => Some(PredicateOp::Lt),
                    TypedBinaryOp::LtEq => Some(PredicateOp::Le),
                    TypedBinaryOp::Gt => Some(PredicateOp::Gt),
                    TypedBinaryOp::GtEq => Some(PredicateOp::Ge),
                    _ => None,
                };
                if let Some(pred_op) = pred_op {
                    // Try col OP const or const OP col
                    if let Some(pred) = extract_typed_simple_predicate(left, right, pred_op.clone())
                    {
                        predicates.push(pred);
                    } else if let Some(pred) =
                        extract_typed_simple_predicate(right, left, flip_pred_op(pred_op))
                    {
                        predicates.push(pred);
                    }
                }
            }
        },
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => {
            if let TypedExprKind::ColumnRef { column_name, .. } = &inner.kind {
                let op = match (test, negated) {
                    (IsTestKind::Null, false) => Some(PredicateOp::IsNull),
                    (IsTestKind::Null, true) => Some(PredicateOp::IsNotNull),
                    _ => None,
                };
                if let Some(op) = op {
                    predicates.push(PredicateInfo {
                        column: column_name.to_lowercase(),
                        op,
                        value: Value::Null,
                        in_values: vec![],
                    });
                }
            }
        }
        TypedExprKind::InList {
            expr: inner,
            list,
            negated: false,
        } => {
            if let TypedExprKind::ColumnRef { column_name, .. } = &inner.kind {
                let values: Vec<Value> = list
                    .iter()
                    .filter_map(|e| {
                        if let TypedExprKind::Constant(v) = &e.kind {
                            Some(v.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                if values.len() == list.len() && !values.is_empty() {
                    predicates.push(PredicateInfo {
                        column: column_name.to_lowercase(),
                        op: PredicateOp::In,
                        value: values[0].clone(),
                        in_values: values,
                    });
                }
            }
        }
        _ => {}
    }
}

fn extract_typed_simple_predicate(
    maybe_col: &super::analyzer::types::TypedExpr,
    maybe_val: &super::analyzer::types::TypedExpr,
    op: PredicateOp,
) -> Option<PredicateInfo> {
    use super::analyzer::types::TypedExprKind;
    if let TypedExprKind::ColumnRef { column_name, .. } = &maybe_col.kind {
        if let TypedExprKind::Constant(val) = &maybe_val.kind {
            return Some(PredicateInfo {
                column: column_name.to_lowercase(),
                op,
                value: val.clone(),
                in_values: vec![],
            });
        }
    }
    None
}

fn flip_pred_op(op: PredicateOp) -> PredicateOp {
    match op {
        PredicateOp::Lt => PredicateOp::Gt,
        PredicateOp::Le => PredicateOp::Ge,
        PredicateOp::Gt => PredicateOp::Lt,
        PredicateOp::Ge => PredicateOp::Le,
        other => other,
    }
}

fn collect_predicates(expr: &Expr, predicates: &mut Vec<PredicateInfo>) {
    match expr {
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And => {
                collect_predicates(left, predicates);
                collect_predicates(right, predicates);
            }
            BinaryOperator::Or => {}
            BinaryOperator::Eq => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Eq) {
                    predicates.push(pred);
                }
            }
            BinaryOperator::NotEq => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Ne) {
                    predicates.push(pred);
                }
            }
            BinaryOperator::Lt => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Lt) {
                    predicates.push(pred);
                }
            }
            BinaryOperator::LtEq => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Le) {
                    predicates.push(pred);
                }
            }
            BinaryOperator::Gt => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Gt) {
                    predicates.push(pred);
                }
            }
            BinaryOperator::GtEq => {
                if let Some(pred) = extract_simple_predicate(left, right, PredicateOp::Ge) {
                    predicates.push(pred);
                }
            }
            _ => {}
        },
        Expr::IsNull(inner) => {
            if let Expr::Identifier(ident) = &**inner {
                predicates.push(PredicateInfo {
                    column: normalize_ident(ident),
                    op: PredicateOp::IsNull,
                    value: Value::Null,
                    in_values: Vec::new(),
                });
            }
        }
        Expr::IsNotNull(inner) => {
            if let Expr::Identifier(ident) = &**inner {
                predicates.push(PredicateInfo {
                    column: normalize_ident(ident),
                    op: PredicateOp::IsNotNull,
                    value: Value::Null,
                    in_values: Vec::new(),
                });
            }
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            if *negated {
                return;
            }
            if let Expr::Identifier(ident) = &**expr {
                let column = normalize_ident(ident);
                let Ok(low_value) = eval_const_ast_expr(low) else {
                    return;
                };
                let Ok(high_value) = eval_const_ast_expr(high) else {
                    return;
                };
                predicates.push(PredicateInfo {
                    column: column.clone(),
                    op: PredicateOp::Ge,
                    value: low_value,
                    in_values: Vec::new(),
                });
                predicates.push(PredicateInfo {
                    column,
                    op: PredicateOp::Le,
                    value: high_value,
                    in_values: Vec::new(),
                });
            }
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            if *negated {
                return;
            }
            if let Expr::Identifier(ident) = &**expr {
                let mut values = Vec::with_capacity(list.len());
                for item in list {
                    let Ok(v) = eval_const_ast_expr(item) else {
                        return;
                    };
                    values.push(v);
                }

                if !values.is_empty() {
                    predicates.push(PredicateInfo {
                        column: normalize_ident(ident),
                        op: PredicateOp::In,
                        value: values[0].clone(),
                        in_values: values,
                    });
                }
            }
        }
        Expr::Nested(e) => collect_predicates(e, predicates),
        _ => {}
    }
}

fn extract_simple_predicate(left: &Expr, right: &Expr, op: PredicateOp) -> Option<PredicateInfo> {
    if let Expr::Identifier(ident) = left {
        if let Ok(val) = eval_const_ast_expr(right) {
            return Some(PredicateInfo {
                column: normalize_ident(ident),
                op,
                value: val,
                in_values: Vec::new(),
            });
        }
    }
    if let Expr::Identifier(ident) = right {
        if let Ok(val) = eval_const_ast_expr(left) {
            let reversed_op = match op {
                PredicateOp::Lt => PredicateOp::Gt,
                PredicateOp::Le => PredicateOp::Ge,
                PredicateOp::Gt => PredicateOp::Lt,
                PredicateOp::Ge => PredicateOp::Le,
                other => other,
            };
            return Some(PredicateInfo {
                column: normalize_ident(ident),
                op: reversed_op,
                value: val,
                in_values: Vec::new(),
            });
        }
    }
    None
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
    typed_filter: &super::analyzer::types::TypedExpr,
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
    if index.columns.is_empty() && index.expressions.is_empty() {
        return false;
    }
    index
        .method
        .as_deref()
        .map(|m| m.eq_ignore_ascii_case("btree"))
        .unwrap_or(true)
}

fn evaluate_expression_index(
    index: &IndexDef,
    filter: &Expr,
    estimated_table_rows: usize,
) -> Option<(ScanType, f64)> {
    let values = match_expression_predicates(index, filter)?;
    let selectivity = estimate_selectivity(index, values.len(), true);
    let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
    let cost = 1.0 + estimated_rows as f64 * 0.5;

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

fn parse_predicate_expr(expr_str: &str) -> Option<Expr> {
    let sql = format!("SELECT {}", expr_str);
    let stmts = super::parse_sql(&sql).ok()?;
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

fn extract_conjuncts(expr: &Expr) -> Vec<Expr> {
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

fn normalize_expr_for_match(expr: &Expr) -> String {
    normalize_expr_string(expr.to_string())
}

fn normalize_expr_string(mut input: String) -> String {
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
        let cost = 1.0 + estimated_rows as f64 * 0.5;

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
        let cost = 1.0 + estimated_rows as f64 * 0.5;

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
        let cost = 1.0 + estimated_rows as f64 * 0.5;

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
    let cost = 1.0 + estimated_rows as f64 * 0.5;

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

fn coerce_index_predicate_value(schema: &TableSchema, col: &str, value: &Value) -> Value {
    if let Some(col_def) = schema
        .columns
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(col))
    {
        super::value_coercion::coerce_value_for_column(value.clone(), col_def)
            .unwrap_or_else(|_| value.clone())
    } else {
        value.clone()
    }
}

fn estimate_selectivity(index: &IndexDef, matched_cols: usize, full_match: bool) -> f64 {
    let base_selectivity = if index.unique && full_match {
        1.0 / 1000000.0
    } else {
        0.1_f64.powi(matched_cols as i32)
    };

    base_selectivity.max(0.0001)
}

// ── TypedExpr expression-index and partial-index support ──────────

/// Render a [`Value`] as a SQL literal string.
///
/// Text values are single-quoted (e.g. `'hello'`), numbers are unquoted,
/// booleans are `true`/`false`, NULL is `NULL`.  This must match the way
/// sqlparser renders literals so that normalization produces identical strings
/// for the AST and TypedExpr code paths.
fn value_to_sql_literal(v: &Value) -> String {
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
/// against index definitions parsed via `parse_predicate_expr` → `normalize_expr_for_match`.
fn typed_expr_to_canonical_sql(expr: &super::analyzer::types::TypedExpr) -> String {
    use super::analyzer::types::TypedExprKind;

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
            let temp = super::analyzer::types::TypedExpr {
                kind: other_kind.clone(),
                data_type: expr.data_type.clone(),
            };
            format!("{}", temp)
        }
    }
}

/// Extract conjuncts from a typed expression tree (AND decomposition).
fn extract_typed_conjuncts(
    expr: &super::analyzer::types::TypedExpr,
) -> Vec<&super::analyzer::types::TypedExpr> {
    use super::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExprKind};

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

/// Check if a typed query filter implies a partial-index predicate.
///
/// Mirrors [`query_implies_index_predicate`]: the index predicate is stored as a
/// string, so we parse it to AST and normalize.  The query conjuncts come from the
/// TypedExpr tree via [`typed_expr_to_canonical_sql`] + [`normalize_expr_string`].
fn query_implies_index_predicate_typed(
    filter: &super::analyzer::types::TypedExpr,
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
    filter: &super::analyzer::types::TypedExpr,
    estimated_table_rows: usize,
) -> Option<(ScanType, f64)> {
    let values = match_expression_predicates_typed(index, filter)?;
    let selectivity = estimate_selectivity(index, values.len(), true);
    let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
    let cost = 1.0 + estimated_rows as f64 * 0.5;

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
/// 1. Parse to AST → `normalize_expr_for_match` (the "index side")
/// 2. For each typed filter conjunct of the form `lhs = rhs`:
///    - Canonicalize lhs/rhs via `typed_expr_to_canonical_sql` → `normalize_expr_string`
///    - If one side matches the index expression, the other must be a constant value
fn match_expression_predicates_typed(
    index: &IndexDef,
    filter: &super::analyzer::types::TypedExpr,
) -> Option<Vec<Value>> {
    use super::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExprKind};

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
fn choose_gin_access_path(
    schema: &TableSchema,
    filter_expr: &Expr,
    estimated_table_rows: usize,
) -> Option<AccessPath> {
    let (column, pattern) = extract_gin_contains_predicate(filter_expr)?;

    let col_idx = schema.column_index(&column)?;
    let col_type = &schema.columns.get(col_idx)?.data_type;

    let is_gin_compatible = matches!(
        col_type,
        DataType::Json | DataType::Jsonb | DataType::Array(_) | DataType::Tsvector
    );
    if !is_gin_compatible {
        return None;
    }

    let index = schema.indexes.iter().find(|idx| {
        idx.method
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case("gin"))
            .unwrap_or(false)
            && idx.columns.len() == 1
            && idx.expressions.is_empty()
            && idx.predicate.is_none()
            && idx.columns[0].eq_ignore_ascii_case(&column)
    })?;

    let selectivity = 0.01_f64;
    let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
    let cost = 0.5 + (estimated_rows as f64 * 0.2);

    Some(AccessPath {
        scan_type: ScanType::GinIndexScan {
            index_id: index.id,
            index_name: index.name.clone(),
            column,
            pattern,
            estimated_rows,
        },
        cost,
    })
}

fn extract_gin_contains_predicate(expr: &Expr) -> Option<(String, Value)> {
    let predicates = collect_gin_predicates(expr);
    if predicates.is_empty() {
        return None;
    }

    predicates
        .into_iter()
        .min_by_key(|(_, value)| gin_predicate_priority(value))
}

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

fn is_gin_binary_operator(op: &BinaryOperator, expected: &str) -> bool {
    matches!(op, BinaryOperator::PGCustomBinaryOperator(parts) if parts.join("") == expected)
}

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

fn gin_predicate_priority(value: &Value) -> u8 {
    match value {
        Value::Tsquery(_) => 0,
        Value::Array(_) => 1,
        Value::Json(_) | Value::Jsonb(_) => 2,
        _ => 3,
    }
}

// ── TypedExpr GIN index support ──────────────────────────────────────

/// Choose GIN access path from a [`TypedExpr`] filter tree.
///
/// Mirrors [`choose_gin_access_path`] but works on the Analyzer's typed IR
/// instead of AST nodes.  Recognizes `BinaryOp` nodes with operators:
/// - `TsMatch` (`@@`) — full-text search
/// - `JsonContains` / `ArrayContains` (`@>`) — containment
fn choose_gin_access_path_typed(
    schema: &TableSchema,
    filter_expr: &super::analyzer::types::TypedExpr,
    estimated_table_rows: usize,
) -> Option<AccessPath> {
    let (column, pattern) = extract_gin_contains_predicate_typed(filter_expr)?;

    let col_idx = schema.column_index(&column)?;
    let col_type = &schema.columns.get(col_idx)?.data_type;

    let is_gin_compatible = matches!(
        col_type,
        DataType::Json | DataType::Jsonb | DataType::Array(_) | DataType::Tsvector
    );
    if !is_gin_compatible {
        return None;
    }

    let index = schema.indexes.iter().find(|idx| {
        idx.method
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case("gin"))
            .unwrap_or(false)
            && idx.columns.len() == 1
            && idx.expressions.is_empty()
            && idx.predicate.is_none()
            && idx.columns[0].eq_ignore_ascii_case(&column)
    })?;

    let selectivity = 0.01_f64;
    let estimated_rows = ((estimated_table_rows as f64) * selectivity).max(1.0) as usize;
    let cost = 0.5 + (estimated_rows as f64 * 0.2);

    Some(AccessPath {
        scan_type: ScanType::GinIndexScan {
            index_id: index.id,
            index_name: index.name.clone(),
            column,
            pattern,
            estimated_rows,
        },
        cost,
    })
}

/// Extract the best GIN-eligible predicate from a typed expression tree.
///
/// Mirrors [`extract_gin_contains_predicate`].
fn extract_gin_contains_predicate_typed(
    expr: &super::analyzer::types::TypedExpr,
) -> Option<(String, Value)> {
    let predicates = collect_gin_predicates_typed(expr);
    if predicates.is_empty() {
        return None;
    }
    predicates
        .into_iter()
        .min_by_key(|(_, value)| gin_predicate_priority(value))
}

/// Collect all GIN-eligible (column, pattern) pairs from a typed expression tree.
///
/// Mirrors [`collect_gin_predicates`].
fn collect_gin_predicates_typed(expr: &super::analyzer::types::TypedExpr) -> Vec<(String, Value)> {
    use super::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExprKind};

    match &expr.kind {
        TypedExprKind::BinaryOp { left, op, right } => match op {
            TypedBinaryOp::And => {
                let mut predicates = collect_gin_predicates_typed(left);
                predicates.extend(collect_gin_predicates_typed(right));
                predicates
            }
            TypedBinaryOp::TsMatch | TypedBinaryOp::JsonContains | TypedBinaryOp::ArrayContains => {
                // left should be a column ref, right should be a constant
                match (&left.kind, &right.kind) {
                    (
                        TypedExprKind::ColumnRef { column_name, .. },
                        TypedExprKind::Constant(pattern),
                    ) => vec![(column_name.to_lowercase(), pattern.clone())],
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnRef {
    qualifier: Option<String>,
    name: String,
}

fn extract_column_ref(expr: &Expr) -> Option<ColumnRef> {
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

fn resolve_column_index(schema: &TableSchema, col: &ColumnRef) -> Option<usize> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::Ident;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn make_eq_expr(col: &str, val: i32) -> Expr {
        Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new(col))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                val.to_string(),
                false,
            ))),
        }
    }

    fn schema_with_cols(name: &str, cols: &[&str]) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            table_id: 1,
            columns: cols
                .iter()
                .enumerate()
                .map(|(i, c)| crate::types::ColumnDef {
                    name: c.to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: true,
                    primary_key: i == 0,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                })
                .collect(),
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    #[test]
    fn test_extract_equi_join_keys_single() {
        let left = schema_with_cols("l", &["id", "v"]);
        let right = schema_with_cols("r", &["user_id", "v"]);

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("user_id"))),
        };

        let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
        assert_eq!(lk, vec![0]);
        assert_eq!(rk, vec![0]);
    }

    #[test]
    fn test_extract_equi_join_keys_swapped_sides() {
        let left = schema_with_cols("l", &["id"]);
        let right = schema_with_cols("r", &["user_id"]);

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("user_id"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("id"))),
        };

        let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
        assert_eq!(lk, vec![0]);
        assert_eq!(rk, vec![0]);
    }

    #[test]
    fn test_extract_equi_join_keys_matches_qualified_output_schema() {
        let left = schema_with_cols("join", &["a.id", "a.v", "b.user_id"]);
        let right = schema_with_cols("c", &["id"]);

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("a"),
                Ident::new("id"),
            ])),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("c"),
                Ident::new("id"),
            ])),
        };

        let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
        assert_eq!(lk, vec![0]);
        assert_eq!(rk, vec![0]);
    }

    #[test]
    fn test_extract_equi_join_keys_multi_key_and() {
        let left = schema_with_cols("l", &["a", "b"]);
        let right = schema_with_cols("r", &["x", "y"]);

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier(Ident::new("a"))),
                op: BinaryOperator::Eq,
                right: Box::new(Expr::Identifier(Ident::new("x"))),
            }),
            op: BinaryOperator::And,
            right: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier(Ident::new("b"))),
                op: BinaryOperator::Eq,
                right: Box::new(Expr::Identifier(Ident::new("y"))),
            }),
        };

        let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
        assert_eq!(lk, vec![0, 1]);
        assert_eq!(rk, vec![0, 1]);
    }

    #[test]
    fn test_extract_equi_join_keys_qualified_refs_against_unqualified_schemas() {
        let left = schema_with_cols("l", &["id"]);
        let right = schema_with_cols("r", &["user_id"]);

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("l"),
                Ident::new("id"),
            ])),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("r"),
                Ident::new("user_id"),
            ])),
        };

        let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
        assert_eq!(lk, vec![0]);
        assert_eq!(rk, vec![0]);
    }

    #[test]
    fn test_extract_equi_join_keys_unqualified_ref_against_qualified_schema_unique() {
        let left = schema_with_cols("join", &["a.id", "a.v"]);
        let right = schema_with_cols("c", &["id"]);

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("c"),
                Ident::new("id"),
            ])),
        };

        let (lk, rk) = extract_equi_join_keys(&expr, &left, &right).unwrap();
        assert_eq!(lk, vec![0]);
        assert_eq!(rk, vec![0]);
    }

    #[test]
    fn test_choose_join_algorithm_with_qualified_left_schema() {
        let left = schema_with_cols("join", &["a.id", "a.v", "b.id"]);
        let right = schema_with_cols("c", &["id"]);

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("a"),
                Ident::new("id"),
            ])),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("c"),
                Ident::new("id"),
            ])),
        };

        let cfg = HashJoinConfig {
            max_memory_bytes: 1,
            min_rows_threshold: 1,
        };

        match choose_join_algorithm(Some(&expr), &left, &right, 1000, 1000, &cfg) {
            JoinAlgorithmChoice::HashJoin {
                left_key_indices,
                right_key_indices,
                ..
            } => {
                assert_eq!(left_key_indices, vec![0]);
                assert_eq!(right_key_indices, vec![0]);
            }
            other => panic!("expected HashJoin, got {:?}", other),
        }
    }

    #[test]
    fn test_choose_join_algorithm_threshold_and_build_side() {
        let left = schema_with_cols("l", &["id"]);
        let right = schema_with_cols("r", &["id"]);

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("id"))),
        };

        let cfg = HashJoinConfig {
            max_memory_bytes: 1,
            min_rows_threshold: 100,
        };

        assert_eq!(
            choose_join_algorithm(Some(&expr), &left, &right, 10, 10, &cfg),
            JoinAlgorithmChoice::NestedLoop
        );

        match choose_join_algorithm(Some(&expr), &left, &right, 1000, 10, &cfg) {
            JoinAlgorithmChoice::HashJoin { left_is_build, .. } => assert!(!left_is_build),
            other => panic!("expected HashJoin, got {:?}", other),
        }
    }

    #[test]
    fn test_choose_join_algorithm_type_mismatch_falls_back() {
        let left = TableSchema::new(
            "l".to_string(),
            1,
            vec![crate::types::ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            vec![0],
        );
        let right = TableSchema::new(
            "r".to_string(),
            2,
            vec![crate::types::ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            vec![0],
        );

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("id"))),
        };

        let cfg = HashJoinConfig {
            max_memory_bytes: 1,
            min_rows_threshold: 0,
        };

        assert_eq!(
            choose_join_algorithm(Some(&expr), &left, &right, 1000, 1000, &cfg),
            JoinAlgorithmChoice::NestedLoop
        );
    }

    #[test]
    fn test_analyze_predicates_simple_eq() {
        let expr = make_eq_expr("id", 42);
        let predicates = analyze_predicates(&expr);
        assert_eq!(predicates.len(), 1);
        assert_eq!(predicates[0].column, "id");
        assert_eq!(predicates[0].op, PredicateOp::Eq);
    }

    #[test]
    fn test_analyze_predicates_and() {
        let expr = Expr::BinaryOp {
            left: Box::new(make_eq_expr("a", 1)),
            op: BinaryOperator::And,
            right: Box::new(make_eq_expr("b", 2)),
        };
        let predicates = analyze_predicates(&expr);
        assert_eq!(predicates.len(), 2);
    }

    #[test]
    fn test_choose_full_scan_no_index() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let path = choose_best_access_path(&schema, &[], 1000);
        assert!(matches!(path.scan_type, ScanType::FullTableScan));
    }

    #[test]
    fn test_choose_index_scan() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_a".to_string(),
                columns: vec!["a".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let predicates = vec![PredicateInfo {
            column: "a".to_string(),
            op: PredicateOp::Eq,
            value: Value::Int32(1),
            in_values: Vec::new(),
        }];
        let path = choose_best_access_path(&schema, &predicates, 1000);
        assert!(matches!(path.scan_type, ScanType::IndexScan { .. }));
    }

    #[test]
    fn test_skip_partial_index_scan() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_a_partial".to_string(),
                columns: vec!["a".to_string()],
                unique: false,
                method: None,
                predicate: Some("a = 10".to_string()),
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let predicates = vec![PredicateInfo {
            column: "a".to_string(),
            op: PredicateOp::Eq,
            value: Value::Int32(1),
            in_values: Vec::new(),
        }];
        let path = choose_best_access_path(&schema, &predicates, 1000);
        assert!(matches!(path.scan_type, ScanType::FullTableScan));
    }

    fn schema_for_index_filter_tests(
        columns: Vec<(&str, DataType)>,
        indexes: Vec<IndexDef>,
    ) -> TableSchema {
        TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: columns
                .into_iter()
                .map(|(name, data_type)| crate::types::ColumnDef {
                    name: name.to_string(),
                    data_type,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                })
                .collect(),
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes,
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    #[test]
    fn test_partial_index_exact_predicate_used() {
        let schema = schema_for_index_filter_tests(
            vec![("name", DataType::Text), ("status", DataType::Text)],
            vec![IndexDef {
                id: 1,
                name: "idx_name_active".to_string(),
                columns: vec!["name".to_string()],
                unique: false,
                method: None,
                predicate: Some("status = 'active'".to_string()),
                expressions: Vec::new(),
            }],
        );
        let filter = parse_where_expr("SELECT * FROM t WHERE status = 'active' AND name = 'foo'");
        let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
        assert!(matches!(path.scan_type, ScanType::IndexScan { .. }));
    }

    #[test]
    fn test_partial_index_missing_predicate_not_used() {
        let schema = schema_for_index_filter_tests(
            vec![("name", DataType::Text), ("status", DataType::Text)],
            vec![IndexDef {
                id: 1,
                name: "idx_name_active".to_string(),
                columns: vec!["name".to_string()],
                unique: false,
                method: None,
                predicate: Some("status = 'active'".to_string()),
                expressions: Vec::new(),
            }],
        );
        let filter = parse_where_expr("SELECT * FROM t WHERE name = 'foo'");
        let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
        assert!(matches!(path.scan_type, ScanType::FullTableScan));
    }

    #[test]
    fn test_partial_index_wrong_value_not_used() {
        let schema = schema_for_index_filter_tests(
            vec![("name", DataType::Text), ("status", DataType::Text)],
            vec![IndexDef {
                id: 1,
                name: "idx_name_active".to_string(),
                columns: vec!["name".to_string()],
                unique: false,
                method: None,
                predicate: Some("status = 'active'".to_string()),
                expressions: Vec::new(),
            }],
        );
        let filter = parse_where_expr("SELECT * FROM t WHERE status = 'inactive' AND name = 'foo'");
        let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
        assert!(matches!(path.scan_type, ScanType::FullTableScan));
    }

    #[test]
    fn test_partial_index_conjunct_subset_used() {
        let schema = schema_for_index_filter_tests(
            vec![
                ("a", DataType::Int32),
                ("b", DataType::Int32),
                ("c", DataType::Int32),
            ],
            vec![IndexDef {
                id: 1,
                name: "idx_a_partial".to_string(),
                columns: vec!["a".to_string()],
                unique: false,
                method: None,
                predicate: Some("a = 1 AND b = 2".to_string()),
                expressions: Vec::new(),
            }],
        );
        let filter = parse_where_expr("SELECT * FROM t WHERE a = 1 AND b = 2 AND c = 3");
        let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
        assert!(matches!(path.scan_type, ScanType::IndexScan { .. }));
    }

    #[test]
    fn test_partial_index_incomplete_conjunct_not_used() {
        let schema = schema_for_index_filter_tests(
            vec![("a", DataType::Int32), ("b", DataType::Int32)],
            vec![IndexDef {
                id: 1,
                name: "idx_a_partial".to_string(),
                columns: vec!["a".to_string()],
                unique: false,
                method: None,
                predicate: Some("a = 1 AND b = 2".to_string()),
                expressions: Vec::new(),
            }],
        );
        let filter = parse_where_expr("SELECT * FROM t WHERE a = 1");
        let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
        assert!(matches!(path.scan_type, ScanType::FullTableScan));
    }

    #[test]
    fn test_expression_index_lower_used() {
        let schema = schema_for_index_filter_tests(
            vec![("name", DataType::Text)],
            vec![IndexDef {
                id: 1,
                name: "idx_lower_name".to_string(),
                columns: vec![],
                unique: false,
                method: None,
                predicate: None,
                expressions: vec!["lower(name)".to_string()],
            }],
        );
        let filter = parse_where_expr("SELECT * FROM t WHERE lower(name) = 'foo'");
        let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
        assert!(matches!(path.scan_type, ScanType::IndexScan { .. }));
    }

    #[test]
    fn test_expression_index_case_insensitive_match() {
        let schema = schema_for_index_filter_tests(
            vec![("name", DataType::Text)],
            vec![IndexDef {
                id: 1,
                name: "idx_lower_name".to_string(),
                columns: vec![],
                unique: false,
                method: None,
                predicate: None,
                expressions: vec!["LOWER(name)".to_string()],
            }],
        );
        let filter = parse_where_expr("SELECT * FROM t WHERE lower(name) = 'bar'");
        let path = choose_best_access_path_for_filter(0, &schema, Some(&filter), 1000);
        assert!(matches!(path.scan_type, ScanType::IndexScan { .. }));
    }

    #[test]
    fn test_choose_gin_index_scan_for_jsonb_contains() {
        use crate::types::ColumnDef;
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![ColumnDef {
                name: "metadata".to_string(),
                data_type: DataType::Jsonb,
                nullable: true,
                default_expr: None,
                primary_key: false,
                unique: false,
                is_serial: false,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 7,
                name: "idx_meta".to_string(),
                columns: vec!["metadata".to_string()],
                unique: false,
                method: Some("gin".to_string()),
                predicate: None,
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(
            &dialect,
            "SELECT * FROM t WHERE metadata @> '{\"type\":\"pdf\"}'",
        )
        .unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let filter = select.selection.as_ref().expect("WHERE exists");

        let path = choose_best_access_path_for_filter(0, &schema, Some(filter), 1000);
        match path.scan_type {
            ScanType::GinIndexScan { index_id, .. } => assert_eq!(index_id, 7),
            other => panic!("expected GinIndexScan, got {:?}", other),
        }
    }

    #[test]
    fn test_gin_predicate_selection_single() {
        let filter = parse_where_expr("SELECT * FROM t WHERE metadata @> '{\"type\":\"pdf\"}'");
        let (column, _) = extract_gin_contains_predicate(&filter).expect("expected GIN predicate");
        assert_eq!(column, "metadata");
    }

    #[test]
    fn test_gin_predicate_selection_prefers_fts() {
        let filter = parse_where_expr(
            "SELECT * FROM t WHERE metadata @> '{\"type\":\"pdf\"}' AND document @@ to_tsquery('invoice')",
        );
        let (column, pattern) =
            extract_gin_contains_predicate(&filter).expect("expected GIN predicate");
        assert_eq!(column, "document");
        assert!(matches!(pattern, Value::Tsquery(_)));
    }

    #[test]
    fn test_analyze_predicates_comparison_ops() {
        let lt_expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("x"))),
            op: BinaryOperator::Lt,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                "10".to_string(),
                false,
            ))),
        };
        let predicates = analyze_predicates(&lt_expr);
        assert_eq!(predicates.len(), 1);
        assert_eq!(predicates[0].op, PredicateOp::Lt);

        let gt_expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("y"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                "5".to_string(),
                false,
            ))),
        };
        let predicates = analyze_predicates(&gt_expr);
        assert_eq!(predicates.len(), 1);
        assert_eq!(predicates[0].op, PredicateOp::Gt);
    }

    fn parse_where_expr(sql: &str) -> Expr {
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        select.selection.expect("WHERE exists")
    }

    fn is_int_value(v: &Value, expected: i64) -> bool {
        matches!(v, Value::Int32(n) if i64::from(*n) == expected)
            || matches!(v, Value::Int64(n) if *n == expected)
    }

    #[test]
    fn test_analyze_predicates_between() {
        let expr = parse_where_expr("SELECT * FROM t WHERE x BETWEEN 5 AND 10");
        let mut predicates = Vec::new();
        collect_predicates(&expr, &mut predicates);

        let ge = predicates
            .iter()
            .find(|p| p.column == "x" && p.op == PredicateOp::Ge)
            .expect("missing x >= 5");
        assert!(is_int_value(&ge.value, 5));

        let le = predicates
            .iter()
            .find(|p| p.column == "x" && p.op == PredicateOp::Le)
            .expect("missing x <= 10");
        assert!(is_int_value(&le.value, 10));
    }

    #[test]
    fn test_analyze_predicates_in_list() {
        let expr = parse_where_expr("SELECT * FROM t WHERE x IN (1, 2, 3)");
        let mut predicates = Vec::new();
        collect_predicates(&expr, &mut predicates);

        let in_pred = predicates
            .iter()
            .find(|p| p.column == "x" && p.op == PredicateOp::In)
            .expect("missing x IN predicate");
        assert_eq!(in_pred.in_values.len(), 3);
        assert!(is_int_value(&in_pred.in_values[0], 1));
        assert!(is_int_value(&in_pred.in_values[1], 2));
        assert!(is_int_value(&in_pred.in_values[2], 3));
    }

    #[test]
    fn test_analyze_predicates_nested() {
        let nested = Expr::Nested(Box::new(make_eq_expr("id", 1)));
        let predicates = analyze_predicates(&nested);
        assert_eq!(predicates.len(), 1);
        assert_eq!(predicates[0].column, "id");
    }

    #[test]
    fn test_analyze_predicates_is_null() {
        let is_null = Expr::IsNull(Box::new(Expr::Identifier(Ident::new("col"))));
        let predicates = analyze_predicates(&is_null);
        assert_eq!(predicates.len(), 1);
        assert_eq!(predicates[0].op, PredicateOp::IsNull);
    }

    #[test]
    fn test_analyze_predicates_is_not_null() {
        let is_not_null = Expr::IsNotNull(Box::new(Expr::Identifier(Ident::new("col"))));
        let predicates = analyze_predicates(&is_not_null);
        assert_eq!(predicates.len(), 1);
        assert_eq!(predicates[0].op, PredicateOp::IsNotNull);
    }

    #[test]
    fn test_choose_unique_index_over_non_unique() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![
                IndexDef {
                    id: 1,
                    name: "idx_a".to_string(),
                    columns: vec!["a".to_string()],
                    unique: false,
                    method: None,
                    predicate: None,
                    expressions: Vec::new(),
                },
                IndexDef {
                    id: 2,
                    name: "idx_a_unique".to_string(),
                    columns: vec!["a".to_string()],
                    unique: true,
                    method: None,
                    predicate: None,
                    expressions: Vec::new(),
                },
            ],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let predicates = vec![PredicateInfo {
            column: "a".to_string(),
            op: PredicateOp::Eq,
            value: Value::Int32(1),
            in_values: Vec::new(),
        }];
        let path = choose_best_access_path(&schema, &predicates, 10000);
        if let ScanType::IndexScan { index_name, .. } = path.scan_type {
            assert_eq!(index_name, "idx_a_unique");
        } else {
            panic!("Expected IndexScan");
        }
    }

    #[test]
    fn test_choose_composite_index() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![
                IndexDef {
                    id: 1,
                    name: "idx_a".to_string(),
                    columns: vec!["a".to_string()],
                    unique: false,
                    method: None,
                    predicate: None,
                    expressions: Vec::new(),
                },
                IndexDef {
                    id: 2,
                    name: "idx_ab".to_string(),
                    columns: vec!["a".to_string(), "b".to_string()],
                    unique: false,
                    method: None,
                    predicate: None,
                    expressions: Vec::new(),
                },
            ],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let predicates = vec![
            PredicateInfo {
                column: "a".to_string(),
                op: PredicateOp::Eq,
                value: Value::Int32(1),
                in_values: Vec::new(),
            },
            PredicateInfo {
                column: "b".to_string(),
                op: PredicateOp::Eq,
                value: Value::Int32(2),
                in_values: Vec::new(),
            },
        ];
        let path = choose_best_access_path(&schema, &predicates, 10000);
        if let ScanType::IndexScan {
            index_name, values, ..
        } = path.scan_type
        {
            assert_eq!(index_name, "idx_ab");
            assert_eq!(values.len(), 2);
        } else {
            panic!("Expected IndexScan on composite index");
        }
    }

    #[test]
    fn test_index_range_scan_partial_match() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_abc".to_string(),
                columns: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let predicates = vec![PredicateInfo {
            column: "a".to_string(),
            op: PredicateOp::Eq,
            value: Value::Int32(1),
            in_values: Vec::new(),
        }];
        let path = choose_best_access_path(&schema, &predicates, 10000);
        match path.scan_type {
            ScanType::IndexRangeScan { prefix_values, .. } => {
                assert_eq!(prefix_values.len(), 1);
            }
            _ => panic!("Expected IndexRangeScan for partial match"),
        }
    }

    #[test]
    fn test_choose_range_scan_gt() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![crate::types::ColumnDef {
                name: "val".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_val".to_string(),
                columns: vec!["val".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let predicates = vec![PredicateInfo {
            column: "val".to_string(),
            op: PredicateOp::Gt,
            value: Value::Int32(5),
            in_values: Vec::new(),
        }];

        let path = choose_best_access_path(&schema, &predicates, 100000);
        assert!(matches!(
            path.scan_type,
            ScanType::IndexBoundedRangeScan { .. }
        ));
    }

    #[test]
    fn test_choose_range_scan_between() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![crate::types::ColumnDef {
                name: "val".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_val".to_string(),
                columns: vec!["val".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let predicates = vec![
            PredicateInfo {
                column: "val".to_string(),
                op: PredicateOp::Ge,
                value: Value::Int32(5),
                in_values: Vec::new(),
            },
            PredicateInfo {
                column: "val".to_string(),
                op: PredicateOp::Le,
                value: Value::Int32(10),
                in_values: Vec::new(),
            },
        ];

        let path = choose_best_access_path(&schema, &predicates, 100000);
        match path.scan_type {
            ScanType::IndexBoundedRangeScan {
                range_start,
                range_end,
                ..
            } => {
                assert!(matches!(range_start, Some(Value::Int32(5))));
                assert!(matches!(range_end, Some(Value::Int32(10))));
            }
            other => panic!("expected IndexBoundedRangeScan, got {:?}", other),
        }
    }

    #[test]
    fn test_choose_range_scan_composite_prefix_plus_range() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "a".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "b".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_ab".to_string(),
                columns: vec!["a".to_string(), "b".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let predicates = vec![
            PredicateInfo {
                column: "a".to_string(),
                op: PredicateOp::Eq,
                value: Value::Int32(1),
                in_values: Vec::new(),
            },
            PredicateInfo {
                column: "b".to_string(),
                op: PredicateOp::Gt,
                value: Value::Int32(5),
                in_values: Vec::new(),
            },
        ];

        let path = choose_best_access_path(&schema, &predicates, 100000);
        match path.scan_type {
            ScanType::IndexBoundedRangeScan {
                prefix_values,
                range_start,
                range_end,
                ..
            } => {
                assert_eq!(prefix_values.len(), 1);
                assert!(matches!(prefix_values[0], Value::Int32(1)));
                assert!(matches!(range_start, Some(Value::Int32(5))));
                assert!(range_end.is_none());
            }
            other => panic!("expected IndexBoundedRangeScan, got {:?}", other),
        }
    }

    #[test]
    fn test_choose_in_list_scan() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![crate::types::ColumnDef {
                name: "val".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_val".to_string(),
                columns: vec!["val".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let predicates = vec![PredicateInfo {
            column: "val".to_string(),
            op: PredicateOp::In,
            value: Value::Null,
            in_values: vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)],
        }];

        let path = choose_best_access_path(&schema, &predicates, 100000);
        match path.scan_type {
            ScanType::InListScan { column_values, .. } => {
                assert_eq!(column_values.len(), 3);
                assert!(matches!(column_values[0][0], Value::Int32(1)));
                assert!(matches!(column_values[1][0], Value::Int32(2)));
                assert!(matches!(column_values[2][0], Value::Int32(3)));
            }
            other => panic!("expected InListScan, got {:?}", other),
        }
    }

    #[test]
    fn test_range_scan_cost_less_than_full_scan() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![crate::types::ColumnDef {
                name: "val".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_val".to_string(),
                columns: vec!["val".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let predicates = vec![PredicateInfo {
            column: "val".to_string(),
            op: PredicateOp::Gt,
            value: Value::Int32(5),
            in_values: Vec::new(),
        }];

        let path = choose_best_access_path(&schema, &predicates, 100000);
        assert!(matches!(
            path.scan_type,
            ScanType::IndexBoundedRangeScan { .. }
        ));
        assert!(path.cost < 100000_f64);
    }

    #[test]
    fn test_predicate_op_reversed() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                "10".to_string(),
                false,
            ))),
            op: BinaryOperator::Lt,
            right: Box::new(Expr::Identifier(Ident::new("x"))),
        };
        let predicates = analyze_predicates(&expr);
        assert_eq!(predicates.len(), 1);
        assert_eq!(predicates[0].column, "x");
        assert_eq!(predicates[0].op, PredicateOp::Gt);
    }

    #[test]
    fn test_choose_join_algorithm_no_condition() {
        let left = schema_with_cols("l", &["id"]);
        let right = schema_with_cols("r", &["id"]);
        let cfg = HashJoinConfig::default();

        assert_eq!(
            choose_join_algorithm(None, &left, &right, 1000, 1000, &cfg),
            JoinAlgorithmChoice::NestedLoop
        );
    }

    #[test]
    fn test_choose_join_algorithm_non_equi_returns_nested_loop() {
        let left = schema_with_cols("l", &["id"]);
        let right = schema_with_cols("r", &["id"]);
        let cfg = HashJoinConfig::default();

        let gt_expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Identifier(Ident::new("id"))),
        };

        assert_eq!(
            choose_join_algorithm(Some(&gt_expr), &left, &right, 1000, 1000, &cfg),
            JoinAlgorithmChoice::NestedLoop
        );
    }

    #[test]
    fn test_choose_join_algorithm_smaller_table_is_build() {
        let left = schema_with_cols("l", &["id"]);
        let right = schema_with_cols("r", &["id"]);
        let cfg = HashJoinConfig {
            max_memory_bytes: usize::MAX,
            min_rows_threshold: 0,
        };

        let eq_expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("id"))),
        };

        match choose_join_algorithm(Some(&eq_expr), &left, &right, 100, 10000, &cfg) {
            JoinAlgorithmChoice::HashJoin { left_is_build, .. } => assert!(left_is_build),
            other => panic!("expected HashJoin, got {:?}", other),
        }

        match choose_join_algorithm(Some(&eq_expr), &left, &right, 10000, 100, &cfg) {
            JoinAlgorithmChoice::HashJoin { left_is_build, .. } => assert!(!left_is_build),
            other => panic!("expected HashJoin, got {:?}", other),
        }
    }

    #[test]
    fn test_extract_equi_join_keys_non_eq_returns_none() {
        let left = schema_with_cols("l", &["id"]);
        let right = schema_with_cols("r", &["id"]);

        let gt_expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Identifier(Ident::new("id"))),
        };

        assert!(extract_equi_join_keys(&gt_expr, &left, &right).is_none());
    }

    #[test]
    fn test_extract_equi_join_keys_nested_expression() {
        let left = schema_with_cols("l", &["id"]);
        let right = schema_with_cols("r", &["user_id"]);

        let nested = Expr::Nested(Box::new(Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("user_id"))),
        }));

        let (lk, rk) = extract_equi_join_keys(&nested, &left, &right).unwrap();
        assert_eq!(lk, vec![0]);
        assert_eq!(rk, vec![0]);
    }

    #[test]
    fn test_full_table_scan_better_for_high_selectivity() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_a".to_string(),
                columns: vec!["a".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let path_no_pred = choose_best_access_path(&schema, &[], 10);
        assert!(matches!(path_no_pred.scan_type, ScanType::FullTableScan));
    }

    #[test]
    fn test_multi_join_hash_join_selection_with_qualified_left_schema() {
        // Simulate multi-join scenario: after first join (a JOIN b), the left schema
        // has qualified columns like "a.id", "a.v", "b.id", "b.user_id".
        // The second join (... JOIN c ON b.id = c.b_id) should still be able to
        // extract equi-join keys and select hash join.
        let left_after_first_join = schema_with_cols("join", &["a.id", "a.v", "b.id", "b.user_id"]);
        let right_c = schema_with_cols("c", &["id", "b_id"]);

        // Original condition: b.id = c.b_id
        let original_condition = Expr::BinaryOp {
            left: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("b"),
                Ident::new("id"),
            ])),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("c"),
                Ident::new("b_id"),
            ])),
        };

        let cfg = HashJoinConfig {
            max_memory_bytes: 1,
            min_rows_threshold: 1,
        };

        // This should return HashJoin, not NestedLoop
        match choose_join_algorithm(
            Some(&original_condition),
            &left_after_first_join,
            &right_c,
            1000,
            1000,
            &cfg,
        ) {
            JoinAlgorithmChoice::HashJoin {
                left_key_indices,
                right_key_indices,
                ..
            } => {
                // b.id is at index 2 in the left schema
                assert_eq!(left_key_indices, vec![2]);
                // b_id is at index 1 in the right schema
                assert_eq!(right_key_indices, vec![1]);
            }
            other => panic!(
                "Expected HashJoin for multi-join with qualified left schema, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_split_join_condition_pure_equi_join() {
        let left = schema_with_cols("l", &["id", "val"]);
        let right = schema_with_cols("r", &["user_id"]);

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Identifier(Ident::new("user_id"))),
        };

        let (lk, rk, residual) = split_join_condition(&expr, &left, &right).unwrap();
        assert_eq!(lk, vec![0]);
        assert_eq!(rk, vec![0]);
        assert!(residual.is_none());
    }

    #[test]
    fn test_split_join_condition_with_residual() {
        let left = schema_with_cols("l", &["id", "val"]);
        let right = schema_with_cols("r", &["user_id", "amount"]);

        // id = user_id AND val > 10
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier(Ident::new("id"))),
                op: BinaryOperator::Eq,
                right: Box::new(Expr::Identifier(Ident::new("user_id"))),
            }),
            op: BinaryOperator::And,
            right: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier(Ident::new("val"))),
                op: BinaryOperator::Gt,
                right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                    "10".to_string(),
                    false,
                ))),
            }),
        };

        let (lk, rk, residual) = split_join_condition(&expr, &left, &right).unwrap();
        assert_eq!(lk, vec![0]);
        assert_eq!(rk, vec![0]);
        assert!(residual.is_some());
    }

    #[test]
    fn test_split_join_condition_multi_key_with_residual() {
        let left = schema_with_cols("l", &["id", "val", "category"]);
        let right = schema_with_cols("r", &["user_id", "amount", "cat"]);

        // id = user_id AND category = cat AND val > 10
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::BinaryOp {
                    left: Box::new(Expr::Identifier(Ident::new("id"))),
                    op: BinaryOperator::Eq,
                    right: Box::new(Expr::Identifier(Ident::new("user_id"))),
                }),
                op: BinaryOperator::And,
                right: Box::new(Expr::BinaryOp {
                    left: Box::new(Expr::Identifier(Ident::new("category"))),
                    op: BinaryOperator::Eq,
                    right: Box::new(Expr::Identifier(Ident::new("cat"))),
                }),
            }),
            op: BinaryOperator::And,
            right: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier(Ident::new("val"))),
                op: BinaryOperator::Gt,
                right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                    "10".to_string(),
                    false,
                ))),
            }),
        };

        let (lk, rk, residual) = split_join_condition(&expr, &left, &right).unwrap();
        assert_eq!(lk, vec![0, 2]);
        assert_eq!(rk, vec![0, 2]);
        assert!(residual.is_some());
    }

    #[test]
    fn test_split_join_condition_no_equi_keys_returns_none() {
        let left = schema_with_cols("l", &["id", "val"]);
        let right = schema_with_cols("r", &["user_id"]);

        // Only a non-equi predicate: val > 10
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("val"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                "10".to_string(),
                false,
            ))),
        };

        assert!(split_join_condition(&expr, &left, &right).is_none());
    }

    // ── Cross-path parity tests (TypedExpr vs AST) ──────────────────

    use super::super::analyzer::types::{
        BinaryOp as TypedBinaryOp, ResolvedFunction, TypedExpr, TypedExprKind,
    };

    fn typed_constant(v: Value, dt: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(v),
            data_type: dt,
        }
    }

    fn typed_column(name: &str, dt: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: name.to_string(),
            },
            data_type: dt,
        }
    }

    fn typed_binop(
        left: TypedExpr,
        op: TypedBinaryOp,
        right: TypedExpr,
        dt: DataType,
    ) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            },
            data_type: dt,
        }
    }

    fn gin_schema() -> TableSchema {
        TableSchema {
            name: "docs".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int64,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "body".to_string(),
                    data_type: DataType::Tsvector,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "data".to_string(),
                    data_type: DataType::Jsonb,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![
                IndexDef {
                    id: 1,
                    name: "idx_body_gin".to_string(),
                    columns: vec!["body".to_string()],
                    unique: false,
                    method: Some("gin".to_string()),
                    predicate: None,
                    expressions: Vec::new(),
                },
                IndexDef {
                    id: 2,
                    name: "idx_data_gin".to_string(),
                    columns: vec!["data".to_string()],
                    unique: false,
                    method: Some("gin".to_string()),
                    predicate: None,
                    expressions: Vec::new(),
                },
            ],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    #[test]
    fn test_gin_typed_tsmatch_selects_gin_index() {
        let schema = gin_schema();
        // body @@ to_tsquery('hello')
        let filter = typed_binop(
            typed_column("body", DataType::Tsvector),
            TypedBinaryOp::TsMatch,
            typed_constant(Value::Tsquery("hello".to_string()), DataType::Tsquery),
            DataType::Boolean,
        );

        let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
        match &path.scan_type {
            ScanType::GinIndexScan {
                index_name, column, ..
            } => {
                assert_eq!(index_name, "idx_body_gin");
                assert_eq!(column, "body");
            }
            other => panic!("expected GinIndexScan, got {:?}", other),
        }
    }

    #[test]
    fn test_gin_typed_json_contains_selects_gin_index() {
        let schema = gin_schema();
        // data @> '{"key": "val"}'::jsonb
        let filter = typed_binop(
            typed_column("data", DataType::Jsonb),
            TypedBinaryOp::JsonContains,
            typed_constant(
                Value::Jsonb(r#"{"key": "val"}"#.to_string()),
                DataType::Jsonb,
            ),
            DataType::Boolean,
        );

        let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
        match &path.scan_type {
            ScanType::GinIndexScan {
                index_name, column, ..
            } => {
                assert_eq!(index_name, "idx_data_gin");
                assert_eq!(column, "data");
            }
            other => panic!("expected GinIndexScan, got {:?}", other),
        }
    }

    #[test]
    fn test_gin_typed_no_gin_index_falls_back() {
        // Schema without GIN index
        let schema = TableSchema {
            name: "plain".to_string(),
            table_id: 1,
            columns: vec![crate::types::ColumnDef {
                name: "body".to_string(),
                data_type: DataType::Tsvector,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let filter = typed_binop(
            typed_column("body", DataType::Tsvector),
            TypedBinaryOp::TsMatch,
            typed_constant(Value::Tsquery("hello".to_string()), DataType::Tsquery),
            DataType::Boolean,
        );

        let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
        assert!(matches!(path.scan_type, ScanType::FullTableScan));
    }

    #[test]
    fn test_expression_index_typed_lower() {
        // Schema with expression index on lower(name)
        let schema = TableSchema {
            name: "users".to_string(),
            table_id: 1,
            columns: vec![crate::types::ColumnDef {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_lower_name".to_string(),
                columns: vec![],
                unique: false,
                method: None,
                predicate: None,
                expressions: vec!["lower(name)".to_string()],
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        // WHERE lower(name) = 'alice'
        let lower_call = TypedExpr {
            kind: TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "lower".to_string(),
                    kind: super::super::analyzer::types::FunctionKind::Builtin,
                    return_type: DataType::Text,
                },
                args: vec![typed_column("name", DataType::Text)],
                order_by: vec![],
                filter: None,
            },
            data_type: DataType::Text,
        };
        let filter = typed_binop(
            lower_call,
            TypedBinaryOp::Eq,
            typed_constant(Value::Text("alice".to_string()), DataType::Text),
            DataType::Boolean,
        );

        let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
        match &path.scan_type {
            ScanType::IndexScan {
                index_name, values, ..
            } => {
                assert_eq!(index_name, "idx_lower_name");
                assert_eq!(values, &[Value::Text("alice".to_string())]);
            }
            other => panic!("expected IndexScan for expression index, got {:?}", other),
        }
    }

    #[test]
    fn test_partial_index_typed_exact_predicate() {
        // Schema with partial index: WHERE status = 'active'
        let schema = TableSchema {
            name: "orders".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int64,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "status".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_active_orders".to_string(),
                columns: vec!["id".to_string()],
                unique: false,
                method: None,
                predicate: Some("status = 'active'".to_string()),
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        // WHERE status = 'active' AND id = 42
        let filter = typed_binop(
            typed_binop(
                typed_column("status", DataType::Text),
                TypedBinaryOp::Eq,
                typed_constant(Value::Text("active".to_string()), DataType::Text),
                DataType::Boolean,
            ),
            TypedBinaryOp::And,
            typed_binop(
                typed_column("id", DataType::Int64),
                TypedBinaryOp::Eq,
                typed_constant(Value::Int64(42), DataType::Int64),
                DataType::Boolean,
            ),
            DataType::Boolean,
        );

        let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
        match &path.scan_type {
            ScanType::IndexScan {
                index_name, values, ..
            } => {
                assert_eq!(index_name, "idx_active_orders");
                assert_eq!(values, &[Value::Int64(42)]);
            }
            other => panic!("expected IndexScan for partial index, got {:?}", other),
        }
    }

    #[test]
    fn test_partial_index_typed_missing_predicate() {
        // Same schema as above, but query doesn't include the partial predicate
        let schema = TableSchema {
            name: "orders".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int64,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "status".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_active_orders".to_string(),
                columns: vec!["id".to_string()],
                unique: false,
                method: None,
                predicate: Some("status = 'active'".to_string()),
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        // WHERE id = 42 (missing status = 'active')
        let filter = typed_binop(
            typed_column("id", DataType::Int64),
            TypedBinaryOp::Eq,
            typed_constant(Value::Int64(42), DataType::Int64),
            DataType::Boolean,
        );

        let path = choose_best_access_path_for_typed_filter(0, &schema, Some(&filter), 10000);
        // Should NOT use the partial index since the predicate isn't satisfied
        assert!(
            matches!(path.scan_type, ScanType::FullTableScan),
            "expected FullTableScan when partial predicate not satisfied, got {:?}",
            path.scan_type
        );
    }

    #[test]
    fn test_canonicalizer_parity_with_ast_lower() {
        // Verify that typed_expr_to_canonical_sql produces strings that normalize
        // to the same value as AST normalize_expr_for_match for common patterns
        let lower_call = TypedExpr {
            kind: TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: "lower".to_string(),
                    kind: super::super::analyzer::types::FunctionKind::Builtin,
                    return_type: DataType::Text,
                },
                args: vec![typed_column("name", DataType::Text)],
                order_by: vec![],
                filter: None,
            },
            data_type: DataType::Text,
        };

        let typed_canonical = normalize_expr_string(typed_expr_to_canonical_sql(&lower_call));
        let ast_expr = parse_predicate_expr("lower(name)").unwrap();
        let ast_canonical = normalize_expr_for_match(&ast_expr);
        assert_eq!(typed_canonical, ast_canonical);
    }

    #[test]
    fn test_canonicalizer_parity_with_ast_cast() {
        // CAST(x AS TEXT)
        let cast_expr = TypedExpr {
            kind: TypedExprKind::Cast {
                expr: Box::new(typed_column("x", DataType::Int64)),
                target_type: DataType::Text,
                cast_context: crate::sql::types::CastContext::Explicit,
            },
            data_type: DataType::Text,
        };

        let typed_canonical = normalize_expr_string(typed_expr_to_canonical_sql(&cast_expr));
        // AST: CAST(x AS TEXT) — sqlparser emits this same form
        let ast_expr = parse_predicate_expr("CAST(x AS TEXT)").unwrap();
        let ast_canonical = normalize_expr_for_match(&ast_expr);
        assert_eq!(typed_canonical, ast_canonical);
    }
}
