//! Join operator building helpers for the analyzed SELECT path.

use crate::sql::analyzer::types::{
    BinaryOp as TypedBinaryOp, JoinCondition, JoinType as AnalyzerJoinType, ResolvedUsingColumn,
    TypedExpr, TypedExprKind,
};
use crate::sql::expr::typed_eval::eval_const_usize as eval_const_usize_shared;
use crate::sql::operators::{
    BoxedOperator, HashJoinConfig, HashJoinOperator, HashJoinType, JoinType as OpJoinType,
    NestedLoopJoinOperator,
};
use crate::model::{DataType, TableSchema};
use anyhow::Result;

/// Build a synthetic TableSchema for set operation intermediate results.
///
/// Used to wrap materialized rows from left/right branches into
/// TableScanOperators that the SetOperationOperator can consume.
pub(super) fn build_set_op_schema(columns: &[String], types: &[DataType]) -> TableSchema {
    TableSchema::virtual_table(
        "set_op",
        columns
            .iter()
            .zip(types.iter())
            .map(|(name, dt)| crate::model::ColumnDef::new(name.clone(), dt.clone(), true))
            .collect(),
    )
}

/// Evaluate a constant TypedExpr to a usize (for LIMIT/OFFSET).
/// Handles plain constants and constant casts (e.g., `0::int8`).
pub(super) fn eval_const_usize(expr: &TypedExpr) -> Result<usize> {
    eval_const_usize_shared(expr, true)
}

// ── JOIN helper functions ───────────────────────────────────────

// All AnalyzedTableRef kinds are now supported (tables, joins, subqueries, functions).

/// Build a join operator from left/right children and the join spec.
pub(super) fn build_join_operator(
    left_op: BoxedOperator,
    right_op: BoxedOperator,
    join_type: AnalyzerJoinType,
    condition: &JoinCondition,
    left_col_count: usize,
) -> Result<BoxedOperator> {
    match condition {
        JoinCondition::On(expr) => {
            let (left_keys, right_keys, residual) =
                extract_typed_equi_join_keys(expr, left_col_count);

            if !left_keys.is_empty()
                && types_compatible_for_hash_join(&left_op, &right_op, &left_keys, &right_keys)
            {
                // Hash join: equi-join keys for hash lookup, residual as filter.
                let left_is_build = true; // Simple heuristic: left is build side.
                let hash_type = to_hash_join_type(join_type);
                Ok(Box::new(HashJoinOperator::new(
                    left_op,
                    right_op,
                    hash_type,
                    left_keys,
                    right_keys,
                    left_is_build,
                    residual,
                    HashJoinConfig::default(),
                )))
            } else {
                // Nested loop join with full condition.
                Ok(Box::new(NestedLoopJoinOperator::new(
                    left_op,
                    right_op,
                    to_op_join_type(join_type),
                    Some(expr.clone()),
                )))
            }
        }

        JoinCondition::Using(using_cols) => {
            let (left_keys, right_keys) = using_to_hash_join_keys(using_cols, left_col_count);

            if types_compatible_for_hash_join(&left_op, &right_op, &left_keys, &right_keys) {
                let left_is_build = true;
                let hash_type = to_hash_join_type(join_type);
                Ok(Box::new(HashJoinOperator::new(
                    left_op,
                    right_op,
                    hash_type,
                    left_keys,
                    right_keys,
                    left_is_build,
                    None,
                    HashJoinConfig::default(),
                )))
            } else {
                // Fall back to nested loop with synthesized equality condition.
                let condition = using_to_typed_condition(using_cols, left_col_count);
                Ok(Box::new(NestedLoopJoinOperator::new(
                    left_op,
                    right_op,
                    to_op_join_type(join_type),
                    Some(condition),
                )))
            }
        }

        JoinCondition::None => {
            // Cross join.
            Ok(Box::new(NestedLoopJoinOperator::new(
                left_op,
                right_op,
                to_op_join_type(join_type),
                None,
            )))
        }
    }
}

/// Extract equi-join keys from a TypedExpr ON condition.
///
/// Walks the AND-tree looking for `left_col = right_col` patterns.
/// Returns (left_key_indices, right_key_indices, residual_filter).
/// Key indices are relative to their respective child operators.
pub(super) fn extract_typed_equi_join_keys(
    expr: &TypedExpr,
    left_col_count: usize,
) -> (Vec<usize>, Vec<usize>, Option<TypedExpr>) {
    let mut left_keys = Vec::new();
    let mut right_keys = Vec::new();
    let mut residual_parts = Vec::new();

    collect_and_conjuncts(expr, &mut |conjunct| {
        if let TypedExprKind::BinaryOp {
            op: TypedBinaryOp::Eq,
            left,
            right,
        } = &conjunct.kind
        {
            if let (
                TypedExprKind::ColumnRef {
                    column_index: li, ..
                },
                TypedExprKind::ColumnRef {
                    column_index: ri, ..
                },
            ) = (&left.kind, &right.kind)
            {
                if *li < left_col_count && *ri >= left_col_count {
                    left_keys.push(*li);
                    right_keys.push(*ri - left_col_count);
                    return;
                } else if *ri < left_col_count && *li >= left_col_count {
                    left_keys.push(*ri);
                    right_keys.push(*li - left_col_count);
                    return;
                }
            }
        }
        residual_parts.push(conjunct.clone());
    });

    let residual = residual_parts.into_iter().reduce(|a, b| TypedExpr {
        kind: TypedExprKind::BinaryOp {
            op: TypedBinaryOp::And,
            left: Box::new(a),
            right: Box::new(b),
        },
        data_type: DataType::Boolean,
    });

    (left_keys, right_keys, residual)
}

/// Walk an AND-tree, calling f for each leaf conjunct.
pub(super) fn collect_and_conjuncts(expr: &TypedExpr, f: &mut impl FnMut(&TypedExpr)) {
    if let TypedExprKind::BinaryOp {
        op: TypedBinaryOp::And,
        left,
        right,
    } = &expr.kind
    {
        collect_and_conjuncts(left, f);
        collect_and_conjuncts(right, f);
    } else {
        f(expr);
    }
}

/// Convert USING columns to hash join key indices.
///
/// Both left_index and right_index are LOCAL to their respective operators
/// (relative to the join's left/right children, not global scope).
pub(super) fn using_to_hash_join_keys(
    cols: &[ResolvedUsingColumn],
    _left_col_count: usize,
) -> (Vec<usize>, Vec<usize>) {
    let left_keys: Vec<usize> = cols.iter().map(|c| c.left_index).collect();
    let right_keys: Vec<usize> = cols.iter().map(|c| c.right_index).collect();
    (left_keys, right_keys)
}

/// Synthesize a TypedExpr equality condition from USING columns.
/// Used for NestedLoopJoin when hash join isn't applicable.
/// Indices are LOCAL: left_index is 0-based within left, right_index within right.
/// For NLJ concat row [left || right], right position = left_col_count + right_index.
pub(super) fn using_to_typed_condition(
    cols: &[ResolvedUsingColumn],
    left_col_count: usize,
) -> TypedExpr {
    let equalities: Vec<TypedExpr> = cols
        .iter()
        .map(|c| TypedExpr {
            kind: TypedExprKind::BinaryOp {
                op: TypedBinaryOp::Eq,
                left: Box::new(TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: c.left_index,
                        column_name: c.name.clone(),
                    },
                    data_type: c.data_type.clone(),
                }),
                right: Box::new(TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: left_col_count + c.right_index,
                        column_name: c.name.clone(),
                    },
                    data_type: c.data_type.clone(),
                }),
            },
            data_type: DataType::Boolean,
        })
        .collect();

    equalities
        .into_iter()
        .reduce(|a, b| TypedExpr {
            kind: TypedExprKind::BinaryOp {
                op: TypedBinaryOp::And,
                left: Box::new(a),
                right: Box::new(b),
            },
            data_type: DataType::Boolean,
        })
        .expect("USING clause must have at least one column")
}

/// Check if hash join key types are compatible (same type for hashing).
pub(super) fn types_compatible_for_hash_join(
    left: &BoxedOperator,
    right: &BoxedOperator,
    left_keys: &[usize],
    right_keys: &[usize],
) -> bool {
    for (li, ri) in left_keys.iter().zip(right_keys.iter()) {
        let lt = &left.schema().columns[*li].data_type;
        let rt = &right.schema().columns[*ri].data_type;
        // Conservative: require exact type match. The Analyzer inserts casts
        // for compatible types, so mismatches here are truly incompatible.
        if lt != rt {
            return false;
        }
    }
    true
}

/// Convert Analyzer JoinType to operator JoinType.
pub(super) fn to_op_join_type(jt: AnalyzerJoinType) -> OpJoinType {
    match jt {
        AnalyzerJoinType::Inner => OpJoinType::Inner,
        AnalyzerJoinType::Left => OpJoinType::Left,
        AnalyzerJoinType::Right => OpJoinType::Right,
        AnalyzerJoinType::Full => OpJoinType::Full,
        AnalyzerJoinType::Cross => OpJoinType::Cross,
    }
}

/// Convert Analyzer JoinType to HashJoinType.
pub(super) fn to_hash_join_type(jt: AnalyzerJoinType) -> HashJoinType {
    match jt {
        AnalyzerJoinType::Inner | AnalyzerJoinType::Cross => HashJoinType::Inner,
        AnalyzerJoinType::Left => HashJoinType::Left,
        AnalyzerJoinType::Right => HashJoinType::Right,
        AnalyzerJoinType::Full => HashJoinType::Full,
    }
}

// ── Aggregate helper functions ──────────────────────────────

// ── Window helper functions ─────────────────────────────────
