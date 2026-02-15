//! Join operator building helpers for the analyzed SELECT path.

use crate::sql::analyzer::types::{
    BinaryOp as TypedBinaryOp, JoinCondition, JoinType as AnalyzerJoinType, ResolvedUsingColumn,
    TypedExpr, TypedExprKind,
};
use crate::sql::operators::{
    BoxedOperator, HashJoinConfig, HashJoinOperator, HashJoinType, JoinType as OpJoinType,
    NestedLoopJoinOperator,
};
use crate::types::{DataType, TableSchema, Value};
use anyhow::{anyhow, Result};

/// Build a synthetic TableSchema for set operation intermediate results.
///
/// Used to wrap materialized rows from left/right branches into
/// TableScanOperators that the SetOperationOperator can consume.
pub(super) fn build_set_op_schema(columns: &[String], types: &[DataType]) -> TableSchema {
    TableSchema {
        name: "set_op".to_string(),
        table_id: 0,
        columns: columns
            .iter()
            .zip(types.iter())
            .map(|(name, dt)| crate::types::ColumnDef {
                name: name.clone(),
                data_type: dt.clone(),
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
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    }
}

/// Evaluate a constant TypedExpr to a usize (for LIMIT/OFFSET).
pub(super) fn eval_const_usize(expr: &TypedExpr) -> Result<usize> {
    match &expr.kind {
        TypedExprKind::Constant(Value::Int32(n)) => {
            if *n < 0 {
                Err(anyhow!("LIMIT/OFFSET must not be negative"))
            } else {
                Ok(*n as usize)
            }
        }
        TypedExprKind::Constant(Value::Int64(n)) => {
            if *n < 0 {
                Err(anyhow!("LIMIT/OFFSET must not be negative"))
            } else {
                Ok(*n as usize)
            }
        }
        TypedExprKind::Constant(Value::Null) => Ok(0),
        _ => Err(anyhow!("LIMIT/OFFSET must be a constant integer")),
    }
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

// ── Nested JOIN reindexing ──────────────────────────────────

/// Reindex a JoinCondition from global column indices to local indices.
///
/// When the analyzer builds `A JOIN (B JOIN C ON ...)`, the ON condition for
/// the inner join has column indices that are global (relative to the full
/// FROM clause). The executor builds each join as a standalone operator pair,
/// so indices need to be local (starting from 0 for the inner join's left).
pub(super) fn reindex_join_condition(condition: &JoinCondition, offset: usize) -> JoinCondition {
    match condition {
        JoinCondition::On(expr) => JoinCondition::On(reindex_typed_expr(expr, offset)),
        // USING already uses local indices (computed at analysis time).
        other => other.clone(),
    }
}

/// Recursively clone a TypedExpr, subtracting `offset` from all
/// `ColumnRef.column_index` where `scope_depth == 0` (current-scope refs only).
fn reindex_typed_expr(expr: &TypedExpr, offset: usize) -> TypedExpr {
    let kind = match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            column_name,
        } => {
            if *scope_depth == 0 {
                TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: column_index.saturating_sub(offset),
                    column_name: column_name.clone(),
                }
            } else {
                expr.kind.clone()
            }
        }
        TypedExprKind::BinaryOp { left, right, op } => TypedExprKind::BinaryOp {
            left: Box::new(reindex_typed_expr(left, offset)),
            op: op.clone(),
            right: Box::new(reindex_typed_expr(right, offset)),
        },
        TypedExprKind::UnaryOp { operand, op } => TypedExprKind::UnaryOp {
            operand: Box::new(reindex_typed_expr(operand, offset)),
            op: op.clone(),
        },
        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => TypedExprKind::Cast {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            target_type: target_type.clone(),
            cast_context: cast_context.clone(),
        },
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => TypedExprKind::IsTest {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            test: test.clone(),
            negated: *negated,
        },
        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => TypedExprKind::Between {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            low: Box::new(reindex_typed_expr(low, offset)),
            high: Box::new(reindex_typed_expr(high, offset)),
            negated: *negated,
        },
        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => TypedExprKind::InList {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            list: list.iter().map(|e| reindex_typed_expr(e, offset)).collect(),
            negated: *negated,
        },
        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => TypedExprKind::Like {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            pattern: Box::new(reindex_typed_expr(pattern, offset)),
            escape: escape
                .as_ref()
                .map(|e| Box::new(reindex_typed_expr(e, offset))),
            case_insensitive: *case_insensitive,
            negated: *negated,
        },
        TypedExprKind::SimilarTo {
            expr: inner,
            pattern,
            escape,
            negated,
        } => TypedExprKind::SimilarTo {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            pattern: Box::new(reindex_typed_expr(pattern, offset)),
            escape: escape
                .as_ref()
                .map(|e| Box::new(reindex_typed_expr(e, offset))),
            negated: *negated,
        },
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => TypedExprKind::Case {
            operand: operand
                .as_ref()
                .map(|e| Box::new(reindex_typed_expr(e, offset))),
            when_clauses: when_clauses
                .iter()
                .map(|(w, t)| (reindex_typed_expr(w, offset), reindex_typed_expr(t, offset)))
                .collect(),
            else_result: else_result
                .as_ref()
                .map(|e| Box::new(reindex_typed_expr(e, offset))),
        },
        TypedExprKind::Coalesce(args) => {
            TypedExprKind::Coalesce(args.iter().map(|a| reindex_typed_expr(a, offset)).collect())
        }
        TypedExprKind::NullIf(a, b) => TypedExprKind::NullIf(
            Box::new(reindex_typed_expr(a, offset)),
            Box::new(reindex_typed_expr(b, offset)),
        ),
        TypedExprKind::MinMax { args, is_greatest } => TypedExprKind::MinMax {
            args: args.iter().map(|a| reindex_typed_expr(a, offset)).collect(),
            is_greatest: *is_greatest,
        },
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => TypedExprKind::FunctionCall {
            func: func.clone(),
            args: args.iter().map(|a| reindex_typed_expr(a, offset)).collect(),
            order_by: order_by.clone(),
            filter: filter
                .as_ref()
                .map(|f| Box::new(reindex_typed_expr(f, offset))),
        },
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            order_by,
            filter,
        } => TypedExprKind::AggregateCall {
            func: func.clone(),
            args: args.iter().map(|a| reindex_typed_expr(a, offset)).collect(),
            distinct: *distinct,
            order_by: order_by.clone(),
            filter: filter
                .as_ref()
                .map(|f| Box::new(reindex_typed_expr(f, offset))),
        },
        TypedExprKind::ArrayIndex { array, index } => TypedExprKind::ArrayIndex {
            array: Box::new(reindex_typed_expr(array, offset)),
            index: Box::new(reindex_typed_expr(index, offset)),
        },
        TypedExprKind::JsonAccess {
            expr: inner,
            path,
            operator,
        } => TypedExprKind::JsonAccess {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            path: Box::new(reindex_typed_expr(path, offset)),
            operator: operator.clone(),
        },
        // Leaf/opaque nodes: subqueries, constants, etc. — no reindexing needed.
        _ => expr.kind.clone(),
    };
    TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    }
}

// ── Aggregate helper functions ──────────────────────────────

// ── Window helper functions ─────────────────────────────────
