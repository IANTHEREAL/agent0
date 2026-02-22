//! Query planner for cost-based optimization
//!
//! This module provides query planning and optimization capabilities:
//! - Cost-based index selection
//! - Predicate analysis
//! - Access path selection

mod index_selection;
mod predicate;
mod scan_type;
#[cfg(test)]
mod tests;

// Re-export public types and functions so external code continues to work
// with `crate::sql::planner::Foo` paths unchanged.

pub use self::index_selection::{
    choose_best_access_path_for_filter, choose_best_access_path_for_typed_filter,
    choose_btree_access_path_for_typed_filter,
};
pub(crate) use self::predicate::collect_typed_eq_predicates;
pub use self::predicate::{analyze_predicates, analyze_typed_predicates};

use sqlparser::ast::{BinaryOperator, Expr};

use super::operators::HashJoinConfig;
use crate::types::{DataType, TableSchema, Value};

/// Result of join algorithm selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinAlgorithmChoice {
    /// Classic nested-loop join (O(N*M)).
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
    let left_col = scan_type::extract_column_ref(left_expr)?;
    let right_col = scan_type::extract_column_ref(right_expr)?;

    if let (Some(li), Some(ri)) = (
        scan_type::resolve_column_index(left_schema, &left_col),
        scan_type::resolve_column_index(right_schema, &right_col),
    ) {
        return Some((vec![li], vec![ri]));
    }

    if let (Some(li), Some(ri)) = (
        scan_type::resolve_column_index(left_schema, &right_col),
        scan_type::resolve_column_index(right_schema, &left_col),
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
