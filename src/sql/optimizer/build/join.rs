//! Join operator builders: type conversion, condition extraction, correlated-ref detection.

use anyhow::{anyhow, Result};

use crate::sql::analyzer::types::{JoinCondition, JoinType, TypedExpr, TypedExprKind};
use crate::sql::expr::classify::has_correlated_ref;
use crate::sql::operators::JoinType as OpJoinType;
use crate::sql::optimizer::physical_plan::{PhysicalNode, PhysicalPlan};
use crate::types::DataType;

/// Convert analyzer JoinType to operator JoinType.
pub(super) fn convert_join_type(jt: &JoinType) -> OpJoinType {
    match jt {
        JoinType::Inner => OpJoinType::Inner,
        JoinType::Left => OpJoinType::Left,
        JoinType::Right => OpJoinType::Right,
        JoinType::Full => OpJoinType::Full,
        JoinType::Cross => OpJoinType::Cross,
    }
}

pub(super) fn join_condition_has_correlated_ref(condition: &JoinCondition) -> bool {
    match condition {
        JoinCondition::On(expr) => has_correlated_ref(expr),
        JoinCondition::Using(_) | JoinCondition::None => false,
    }
}

pub(super) fn plan_has_correlated_refs(plan: &PhysicalPlan) -> bool {
    match &plan.node {
        PhysicalNode::SeqScan { .. } | PhysicalNode::IndexScan { .. } | PhysicalNode::Empty => {
            false
        }
        PhysicalNode::Values { rows } => rows.iter().flatten().any(has_correlated_ref),
        PhysicalNode::TableFunction { args, .. } => args.iter().any(|arg| match arg {
            crate::sql::analyzer::types::TypedFunctionArg::Positional(expr) => {
                has_correlated_ref(expr)
            }
            crate::sql::analyzer::types::TypedFunctionArg::Named { expr, .. } => {
                has_correlated_ref(expr)
            }
        }),
        PhysicalNode::Filter { predicate, input } => {
            has_correlated_ref(predicate) || plan_has_correlated_refs(input)
        }
        PhysicalNode::Project { projections, input } => {
            projections.iter().any(|p| has_correlated_ref(&p.expr))
                || plan_has_correlated_refs(input)
        }
        PhysicalNode::HashAggregate {
            group_by,
            projections,
            input,
        }
        | PhysicalNode::StreamAggregate {
            group_by,
            projections,
            input,
        } => {
            group_by.iter().any(has_correlated_ref)
                || projections.iter().any(|p| has_correlated_ref(&p.expr))
                || plan_has_correlated_refs(input)
        }
        PhysicalNode::Sort { order_by, input }
        | PhysicalNode::TopNSort {
            order_by, input, ..
        } => {
            order_by.iter().any(|o| has_correlated_ref(&o.expr)) || plan_has_correlated_refs(input)
        }
        PhysicalNode::Limit {
            limit,
            offset,
            input,
        } => {
            limit.as_ref().is_some_and(has_correlated_ref)
                || offset.as_ref().is_some_and(has_correlated_ref)
                || plan_has_correlated_refs(input)
        }
        PhysicalNode::Distinct { input } => plan_has_correlated_refs(input),
        PhysicalNode::DistinctOn { on_exprs, input } => {
            on_exprs.iter().any(has_correlated_ref) || plan_has_correlated_refs(input)
        }
        PhysicalNode::Window {
            window_functions,
            input,
        } => {
            window_functions.iter().any(|wf| {
                wf.arg_expr.as_ref().is_some_and(has_correlated_ref)
                    || wf.partition_by.iter().any(has_correlated_ref)
                    || wf.order_by.iter().any(|o| has_correlated_ref(&o.expr))
                    || wf.offset_expr.as_ref().is_some_and(has_correlated_ref)
                    || wf
                        .default_value_expr
                        .as_ref()
                        .is_some_and(has_correlated_ref)
                    || wf.filter_expr.as_ref().is_some_and(has_correlated_ref)
            }) || plan_has_correlated_refs(input)
        }
        PhysicalNode::NestedLoopJoin {
            left,
            right,
            condition,
            ..
        }
        | PhysicalNode::HashJoin {
            left,
            right,
            condition,
            ..
        } => {
            join_condition_has_correlated_ref(condition)
                || plan_has_correlated_refs(left)
                || plan_has_correlated_refs(right)
        }
        PhysicalNode::HashSemiJoin {
            left,
            right,
            condition,
            ..
        } => {
            join_condition_has_correlated_ref(condition)
                || plan_has_correlated_refs(left)
                || plan_has_correlated_refs(right)
        }
        PhysicalNode::SetOperation { left, right, .. } => {
            plan_has_correlated_refs(left) || plan_has_correlated_refs(right)
        }
        PhysicalNode::Subquery { subplan, .. } => plan_has_correlated_refs(subplan),
    }
}

/// Extract the ON condition expression from a JoinCondition for NLJ.
pub(super) fn extract_on_condition(condition: &JoinCondition) -> Option<TypedExpr> {
    match condition {
        JoinCondition::On(expr) => Some(expr.clone()),
        JoinCondition::Using(cols) => {
            // Build equality conjunction: col1_left = col1_right AND col2_left = col2_right ...
            let mut result: Option<TypedExpr> = None;
            for col in cols {
                let eq = TypedExpr {
                    kind: TypedExprKind::BinaryOp {
                        left: Box::new(TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: col.left_index,
                                column_name: col.name.clone(),
                            },
                            data_type: col.data_type.clone(),
                        }),
                        op: crate::sql::analyzer::types::BinaryOp::Eq,
                        right: Box::new(TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: col.right_index,
                                column_name: col.name.clone(),
                            },
                            data_type: col.data_type.clone(),
                        }),
                    },
                    data_type: DataType::Boolean,
                };
                result = Some(match result {
                    None => eq,
                    Some(prev) => TypedExpr {
                        kind: TypedExprKind::BinaryOp {
                            left: Box::new(prev),
                            op: crate::sql::analyzer::types::BinaryOp::And,
                            right: Box::new(eq),
                        },
                        data_type: DataType::Boolean,
                    },
                });
            }
            result
        }
        JoinCondition::None => None,
    }
}

/// Extract equi-join key indices from a JoinCondition for hash join.
///
/// Delegates to `join_keys::try_extract_equi_keys` for validated, rebased indices.
/// Returns (left_key_indices, right_key_indices, optional_residual_filter).
pub(super) fn extract_hash_join_keys(
    condition: &JoinCondition,
    left_width: usize,
) -> Result<(Vec<usize>, Vec<usize>, Option<TypedExpr>)> {
    match crate::sql::optimizer::join_keys::try_extract_equi_keys(condition, left_width) {
        Some((left_keys, right_keys)) => Ok((left_keys, right_keys, None)),
        None => match condition {
            JoinCondition::None => Ok((vec![], vec![], None)),
            _ => Err(anyhow!(
                "HashJoin requires equi-join keys but got non-equi condition"
            )),
        },
    }
}
