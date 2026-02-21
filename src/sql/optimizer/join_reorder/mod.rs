//! Cost-based join reordering via DPccp (n <= 8) or greedy (n > 8).
//!
//! Flattens inner/cross join trees, classifies predicates, and reconstructs
//! an optimal join order using table statistics for cardinality estimates.
//!
//! Safety gates:
//! - Unresolved subqueries in the candidate subtree -> skip reorder
//! - Correlated refs in RHS of join -> treat as opaque base relation
//! - Outer joins / USING joins -> not flattened (subtrees recursively reordered)

mod algorithms;
mod cost;
mod predicates;
#[cfg(test)]
mod tests;

use super::logical_plan::{LogicalNode, LogicalPlan};
use super::physical_planner::PlanningContext;
use super::rewrite::{conjuncts_to_predicate, split_conjunction};
use crate::sql::analyzer::types::{JoinCondition, JoinType, TypedExpr};
use crate::sql::expr::classify::{has_correlated_ref, has_unresolved_subquery};

use algorithms::{dpccp_optimize, greedy_optimize};
use predicates::{classify_predicates, flatten_recursive, BaseRelation};

const DPCCP_MAX_RELS: usize = 8;

// ── Public entry point ──────────────────────────────────────────

/// Recursively reorder joins in a logical plan tree.
///
/// For each inner/cross join group, flattens the tree, classifies predicates,
/// and finds an optimal join order using DPccp (<=8 rels) or greedy (>8 rels).
pub fn reorder_joins(plan: LogicalPlan, ctx: &PlanningContext) -> LogicalPlan {
    reorder_recursive(plan, ctx)
}

// ── Recursive traversal ─────────────────────────────────────────

fn reorder_recursive(plan: LogicalPlan, ctx: &PlanningContext) -> LogicalPlan {
    let schema = plan.schema;
    let node = match plan.node {
        // Inner/Cross join (non-USING): attempt to flatten and reorder
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } if is_flattenable_join(&join_type, &condition) => {
            let plan = LogicalPlan {
                node: LogicalNode::Join {
                    left,
                    right,
                    join_type,
                    condition,
                },
                schema: schema.clone(),
            };
            return try_reorder_join_group(plan, ctx);
        }
        // Filter above flattenable join: include filter preds in reorder
        LogicalNode::Filter { predicate, input }
            if matches!(&input.node, LogicalNode::Join { join_type, condition, .. }
                if is_flattenable_join(join_type, condition)) =>
        {
            let plan = LogicalPlan {
                node: LogicalNode::Filter { predicate, input },
                schema: schema.clone(),
            };
            return try_reorder_join_group(plan, ctx);
        }
        // Outer/USING joins: only recurse into children
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } => LogicalNode::Join {
            left: Box::new(reorder_recursive(*left, ctx)),
            right: Box::new(reorder_recursive(*right, ctx)),
            join_type,
            condition,
        },
        // Recurse into children for all other nodes
        LogicalNode::Filter { predicate, input } => LogicalNode::Filter {
            predicate,
            input: Box::new(reorder_recursive(*input, ctx)),
        },
        LogicalNode::Project { projections, input } => LogicalNode::Project {
            projections,
            input: Box::new(reorder_recursive(*input, ctx)),
        },
        LogicalNode::Aggregate {
            group_by,
            projections,
            input,
        } => LogicalNode::Aggregate {
            group_by,
            projections,
            input: Box::new(reorder_recursive(*input, ctx)),
        },
        LogicalNode::Sort { order_by, input } => LogicalNode::Sort {
            order_by,
            input: Box::new(reorder_recursive(*input, ctx)),
        },
        LogicalNode::Limit {
            limit,
            offset,
            input,
        } => LogicalNode::Limit {
            limit,
            offset,
            input: Box::new(reorder_recursive(*input, ctx)),
        },
        LogicalNode::Distinct { input } => LogicalNode::Distinct {
            input: Box::new(reorder_recursive(*input, ctx)),
        },
        LogicalNode::DistinctOn { on_exprs, input } => LogicalNode::DistinctOn {
            on_exprs,
            input: Box::new(reorder_recursive(*input, ctx)),
        },
        LogicalNode::Window {
            window_functions,
            input_col_count,
            input,
        } => LogicalNode::Window {
            window_functions,
            input_col_count,
            input: Box::new(reorder_recursive(*input, ctx)),
        },
        LogicalNode::SetOperation {
            op,
            all,
            left,
            right,
        } => LogicalNode::SetOperation {
            op,
            all,
            left: Box::new(reorder_recursive(*left, ctx)),
            right: Box::new(reorder_recursive(*right, ctx)),
        },
        // SemiJoin/AntiJoin: opaque barrier — recurse into left and right individually,
        // do NOT flatten into join groups.
        LogicalNode::SemiJoin {
            left,
            right,
            condition,
        } => LogicalNode::SemiJoin {
            left: Box::new(reorder_recursive(*left, ctx)),
            right: Box::new(reorder_recursive(*right, ctx)),
            condition,
        },
        LogicalNode::AntiJoin {
            left,
            right,
            condition,
        } => LogicalNode::AntiJoin {
            left: Box::new(reorder_recursive(*left, ctx)),
            right: Box::new(reorder_recursive(*right, ctx)),
            condition,
        },
        LogicalNode::Subquery { subplan, alias } => LogicalNode::Subquery {
            subplan: Box::new(reorder_recursive(*subplan, ctx)),
            alias,
        },
        // Leaf nodes — no children
        node @ (LogicalNode::Scan { .. }
        | LogicalNode::Values { .. }
        | LogicalNode::TableFunction { .. }
        | LogicalNode::Empty) => node,
    };
    LogicalPlan { node, schema }
}

/// Check if a join is flattenable (Inner/Cross, non-USING).
fn is_flattenable_join(join_type: &JoinType, condition: &JoinCondition) -> bool {
    matches!(join_type, JoinType::Inner | JoinType::Cross)
        && !matches!(condition, JoinCondition::Using(..))
}

/// Count the number of base relations that `flatten_recursive` would produce.
/// Mirrors the same flattening conditions without actually extracting.
fn count_flattenable_relations(plan: &LogicalPlan) -> usize {
    match &plan.node {
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } if matches!(join_type, JoinType::Inner | JoinType::Cross)
            && !matches!(condition, JoinCondition::Using(..))
            && !logical_has_correlated_refs(right) =>
        {
            count_flattenable_relations(left) + count_flattenable_relations(right)
        }
        _ => 1,
    }
}

// ── Join group reorder attempt ──────────────────────────────────

/// Try to reorder a join group rooted at the given plan.
///
/// The plan may be either a Join node or a Filter-above-Join.
/// If safety gates block reordering, returns the plan with children
/// recursively reordered instead.
fn try_reorder_join_group(plan: LogicalPlan, ctx: &PlanningContext) -> LogicalPlan {
    // Safety gate: check entire subtree for unresolved subqueries
    if logical_has_unresolved_subquery(&plan) {
        tracing::debug!("Join reorder skipped: unresolved subquery in subtree");
        return reorder_children_only(plan, ctx);
    }

    // Extract optional top filter and the join node beneath
    let (top_filter_pred, join_plan) = match plan.node {
        LogicalNode::Filter { predicate, input }
            if matches!(input.node, LogicalNode::Join { .. }) =>
        {
            (Some(predicate), *input)
        }
        LogicalNode::Join { .. } => (None, plan),
        _ => return reorder_children_only(plan, ctx),
    };

    let root_schema = join_plan.schema.clone();

    // Guard: u64 bitmask can only represent up to 64 relations.
    // Check before flattening to preserve the original join structure
    // (ON conditions, join types). After flattening the original tree is
    // lost, so we must decide here.
    if count_flattenable_relations(&join_plan) >= 64 {
        tracing::debug!("Join reorder skipped: relation count exceeds u64 bitmask limit");
        let plan = if let Some(pred) = top_filter_pred {
            LogicalPlan {
                node: LogicalNode::Filter {
                    predicate: pred,
                    input: Box::new(join_plan),
                },
                schema: root_schema,
            }
        } else {
            join_plan
        };
        return reorder_children_only(plan, ctx);
    }

    // Flatten the join group
    let mut rels: Vec<BaseRelation> = Vec::new();
    let mut raw_preds: Vec<TypedExpr> = Vec::new();
    flatten_recursive(join_plan, &mut rels, &mut raw_preds, 0);

    // Add top filter predicates
    if let Some(top_pred) = top_filter_pred {
        let conjuncts = split_conjunction(top_pred);
        raw_preds.extend(conjuncts);
    }

    // Single relation: nothing to reorder.
    // Use reorder_children_only (NOT reorder_recursive) to avoid infinite
    // recursion when flatten_recursive collapsed a flattenable join into a
    // single opaque base relation (e.g. correlated RHS blocked flattening).
    if rels.len() < 2 {
        let mut result = rels.into_iter().next().unwrap().plan;
        if !raw_preds.is_empty() {
            let pred = conjuncts_to_predicate(raw_preds);
            result = LogicalPlan {
                node: LogicalNode::Filter {
                    predicate: pred,
                    input: Box::new(result),
                },
                schema: root_schema,
            };
        }
        return reorder_children_only(result, ctx);
    }

    let n = rels.len();

    // Split raw predicates into individual conjuncts and classify
    let all_conjuncts: Vec<TypedExpr> = raw_preds.into_iter().flat_map(split_conjunction).collect();

    let classification = classify_predicates(&all_conjuncts, &rels);

    // Attach local filters to base relations
    for (rel_id, local_filters) in &classification.base_local_filters {
        if !local_filters.is_empty() {
            let rel = &mut rels[*rel_id];
            let pred = conjuncts_to_predicate(local_filters.clone());
            let schema = rel.plan.schema.clone();
            rel.plan = LogicalPlan {
                node: LogicalNode::Filter {
                    predicate: pred,
                    input: Box::new(rel.plan.clone()),
                },
                schema,
            };
        }
    }

    // Recursively reorder within base relations (for nested join groups)
    for rel in &mut rels {
        rel.plan = reorder_recursive(rel.plan.clone(), ctx);
    }

    // Run DP or greedy
    let result = if n <= DPCCP_MAX_RELS {
        dpccp_optimize(
            &rels,
            &classification.edges,
            &classification.remaining,
            ctx,
            &root_schema,
        )
    } else {
        greedy_optimize(
            &rels,
            &classification.edges,
            &classification.remaining,
            ctx,
            &root_schema,
        )
    };

    result
}

/// Reorder only children of a plan node (when the root can't be reordered).
fn reorder_children_only(plan: LogicalPlan, ctx: &PlanningContext) -> LogicalPlan {
    let schema = plan.schema;
    let node = match plan.node {
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } => LogicalNode::Join {
            left: Box::new(reorder_recursive(*left, ctx)),
            right: Box::new(reorder_recursive(*right, ctx)),
            join_type,
            condition,
        },
        LogicalNode::Filter { predicate, input } => LogicalNode::Filter {
            predicate,
            input: Box::new(reorder_recursive(*input, ctx)),
        },
        other => other,
    };
    LogicalPlan { node, schema }
}

// ── Safety scanners ─────────────────────────────────────────────

/// Check if a LogicalPlan subtree contains any unresolved subquery.
pub(super) fn logical_has_unresolved_subquery(plan: &LogicalPlan) -> bool {
    match &plan.node {
        LogicalNode::Scan { .. } | LogicalNode::Empty => false,

        LogicalNode::Values { rows } => rows.iter().flatten().any(has_unresolved_subquery),
        LogicalNode::TableFunction { args, .. } => args.iter().any(|arg| match arg {
            crate::sql::analyzer::types::TypedFunctionArg::Positional(expr) => {
                has_unresolved_subquery(expr)
            }
            crate::sql::analyzer::types::TypedFunctionArg::Named { expr, .. } => {
                has_unresolved_subquery(expr)
            }
        }),

        LogicalNode::Filter { predicate, input } => {
            has_unresolved_subquery(predicate) || logical_has_unresolved_subquery(input)
        }
        LogicalNode::Project { projections, input } => {
            projections.iter().any(|p| has_unresolved_subquery(&p.expr))
                || logical_has_unresolved_subquery(input)
        }
        LogicalNode::Aggregate {
            group_by,
            projections,
            input,
        } => {
            group_by.iter().any(has_unresolved_subquery)
                || projections.iter().any(|p| has_unresolved_subquery(&p.expr))
                || logical_has_unresolved_subquery(input)
        }
        LogicalNode::Sort { order_by, input } => {
            order_by.iter().any(|o| has_unresolved_subquery(&o.expr))
                || logical_has_unresolved_subquery(input)
        }
        LogicalNode::Limit {
            limit,
            offset,
            input,
        } => {
            limit.as_ref().is_some_and(has_unresolved_subquery)
                || offset.as_ref().is_some_and(has_unresolved_subquery)
                || logical_has_unresolved_subquery(input)
        }
        LogicalNode::Distinct { input } => logical_has_unresolved_subquery(input),
        LogicalNode::DistinctOn { on_exprs, input } => {
            on_exprs.iter().any(has_unresolved_subquery) || logical_has_unresolved_subquery(input)
        }
        LogicalNode::Window {
            window_functions,
            input,
            ..
        } => {
            window_functions.iter().any(|wf| {
                wf.arg_expr.as_ref().is_some_and(has_unresolved_subquery)
                    || wf.partition_by.iter().any(has_unresolved_subquery)
                    || wf.order_by.iter().any(|o| has_unresolved_subquery(&o.expr))
                    || wf.offset_expr.as_ref().is_some_and(has_unresolved_subquery)
                    || wf
                        .default_value_expr
                        .as_ref()
                        .is_some_and(has_unresolved_subquery)
                    || wf.filter_expr.as_ref().is_some_and(has_unresolved_subquery)
            }) || logical_has_unresolved_subquery(input)
        }
        LogicalNode::Join {
            left,
            right,
            condition,
            ..
        }
        | LogicalNode::SemiJoin {
            left,
            right,
            condition,
        }
        | LogicalNode::AntiJoin {
            left,
            right,
            condition,
        } => {
            join_condition_has(condition, has_unresolved_subquery)
                || logical_has_unresolved_subquery(left)
                || logical_has_unresolved_subquery(right)
        }
        LogicalNode::SetOperation { left, right, .. } => {
            logical_has_unresolved_subquery(left) || logical_has_unresolved_subquery(right)
        }
        LogicalNode::Subquery { subplan, .. } => logical_has_unresolved_subquery(subplan),
    }
}

/// Check if a LogicalPlan subtree contains any correlated reference.
pub(super) fn logical_has_correlated_refs(plan: &LogicalPlan) -> bool {
    match &plan.node {
        LogicalNode::Scan { .. } | LogicalNode::Empty => false,

        LogicalNode::Values { rows } => rows.iter().flatten().any(has_correlated_ref),
        LogicalNode::TableFunction { args, .. } => args.iter().any(|arg| match arg {
            crate::sql::analyzer::types::TypedFunctionArg::Positional(expr) => {
                has_correlated_ref(expr)
            }
            crate::sql::analyzer::types::TypedFunctionArg::Named { expr, .. } => {
                has_correlated_ref(expr)
            }
        }),

        LogicalNode::Filter { predicate, input } => {
            has_correlated_ref(predicate) || logical_has_correlated_refs(input)
        }
        LogicalNode::Project { projections, input } => {
            projections.iter().any(|p| has_correlated_ref(&p.expr))
                || logical_has_correlated_refs(input)
        }
        LogicalNode::Aggregate {
            group_by,
            projections,
            input,
        } => {
            group_by.iter().any(has_correlated_ref)
                || projections.iter().any(|p| has_correlated_ref(&p.expr))
                || logical_has_correlated_refs(input)
        }
        LogicalNode::Sort { order_by, input } => {
            order_by.iter().any(|o| has_correlated_ref(&o.expr))
                || logical_has_correlated_refs(input)
        }
        LogicalNode::Limit {
            limit,
            offset,
            input,
        } => {
            limit.as_ref().is_some_and(has_correlated_ref)
                || offset.as_ref().is_some_and(has_correlated_ref)
                || logical_has_correlated_refs(input)
        }
        LogicalNode::Distinct { input } => logical_has_correlated_refs(input),
        LogicalNode::DistinctOn { on_exprs, input } => {
            on_exprs.iter().any(has_correlated_ref) || logical_has_correlated_refs(input)
        }
        LogicalNode::Window {
            window_functions,
            input,
            ..
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
            }) || logical_has_correlated_refs(input)
        }
        LogicalNode::Join {
            left,
            right,
            condition,
            ..
        }
        | LogicalNode::SemiJoin {
            left,
            right,
            condition,
        }
        | LogicalNode::AntiJoin {
            left,
            right,
            condition,
        } => {
            join_condition_has(condition, has_correlated_ref)
                || logical_has_correlated_refs(left)
                || logical_has_correlated_refs(right)
        }
        LogicalNode::SetOperation { left, right, .. } => {
            logical_has_correlated_refs(left) || logical_has_correlated_refs(right)
        }
        LogicalNode::Subquery { subplan, .. } => logical_has_correlated_refs(subplan),
    }
}

fn join_condition_has(condition: &JoinCondition, check: fn(&TypedExpr) -> bool) -> bool {
    match condition {
        JoinCondition::On(expr) => check(expr),
        JoinCondition::Using(_) | JoinCondition::None => false,
    }
}
