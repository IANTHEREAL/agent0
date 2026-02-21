//! Logical plan rewrite framework + predicate pushdown.
//!
//! Rewrites transform a `LogicalPlan` into a semantically equivalent plan
//! with better expected performance. Each rule is applied once in sequence.
//!
//! Currently implemented:
//! - **PredicatePushdown**: pushes WHERE filter predicates below Join and Sort
//!   nodes to reduce the number of rows entering those operators.
//! - **CrossJoinElimination**: absorbs cross-table equi-predicates from Filter
//!   into join ON conditions, converting Cross→Inner and enabling HashJoin.

use std::collections::HashSet;

use super::join_keys;
use super::logical_plan::{LogicalNode, LogicalPlan, PlanSchema};
use crate::sql::analyzer::types::{
    reindex_typed_expr, BinaryOp, JoinCondition, JoinType, TypedExpr, TypedExprKind,
};
use crate::sql::expr::classify::{has_correlated_ref, has_unresolved_subquery, is_volatile};
use crate::types::DataType;

mod decorrelate;

// ── Rewrite framework ──────────────────────────────────────────

/// A rewrite rule that transforms a logical plan.
trait LogicalRewriteRule {
    fn rewrite(&self, plan: LogicalPlan) -> LogicalPlan;
}

/// Apply all rewrite rules to a logical plan.
///
/// Called from `optimize()` between logical planning and physical planning.
/// When `planning_ctx` is provided, join reordering is enabled using table
/// statistics for cost-based decisions.
pub fn apply_rewrites(
    plan: LogicalPlan,
    planning_ctx: Option<&super::physical_planner::PlanningContext>,
) -> LogicalPlan {
    // Phase 0: Subquery decorrelation (EXISTS/NOT EXISTS → SemiJoin/AntiJoin)
    let mut current = decorrelate::SubqueryDecorrelation.rewrite(plan);
    // Phase 1: Predicate pushdown + cross-join elimination
    current = PredicatePushdown.rewrite(current);
    current = CrossJoinElimination.rewrite(current);

    // Phase 2: Join reordering (requires PlanningContext for cost estimation)
    if let Some(ctx) = planning_ctx {
        current = super::join_reorder::reorder_joins(current, ctx);
        // Second pushdown pass: re-push single-table filters that may now be
        // pushable after join tree restructuring
        current = PredicatePushdown.rewrite(current);
    }

    current
}

// ── Predicate pushdown ─────────────────────────────────────────

struct PredicatePushdown;

impl LogicalRewriteRule for PredicatePushdown {
    fn rewrite(&self, plan: LogicalPlan) -> LogicalPlan {
        rewrite_plan(plan)
    }
}

/// Bottom-up recursive rewrite: rewrite children first, then handle current node.
fn rewrite_plan(plan: LogicalPlan) -> LogicalPlan {
    // First, recursively rewrite children
    let plan = rewrite_children(plan);

    // Then, if current node is a Filter, try to push it down
    match plan.node {
        LogicalNode::Filter { predicate, input } => push_filter_down(predicate, *input),
        _ => plan,
    }
}

/// Recursively rewrite all children of a plan node.
fn rewrite_children(plan: LogicalPlan) -> LogicalPlan {
    let schema = plan.schema;
    let node = match plan.node {
        LogicalNode::Filter { predicate, input } => LogicalNode::Filter {
            predicate,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Project { projections, input } => LogicalNode::Project {
            projections,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Aggregate {
            group_by,
            projections,
            input,
        } => LogicalNode::Aggregate {
            group_by,
            projections,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Sort { order_by, input } => LogicalNode::Sort {
            order_by,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Limit {
            limit,
            offset,
            input,
        } => LogicalNode::Limit {
            limit,
            offset,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Distinct { input } => LogicalNode::Distinct {
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::DistinctOn { on_exprs, input } => LogicalNode::DistinctOn {
            on_exprs,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Window {
            window_functions,
            input_col_count,
            input,
        } => LogicalNode::Window {
            window_functions,
            input_col_count,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } => LogicalNode::Join {
            left: Box::new(rewrite_plan(*left)),
            right: Box::new(rewrite_plan(*right)),
            join_type,
            condition,
        },
        LogicalNode::SetOperation {
            op,
            all,
            left,
            right,
        } => LogicalNode::SetOperation {
            op,
            all,
            left: Box::new(rewrite_plan(*left)),
            right: Box::new(rewrite_plan(*right)),
        },
        LogicalNode::SemiJoin {
            left,
            right,
            condition,
        } => LogicalNode::SemiJoin {
            left: Box::new(rewrite_plan(*left)),
            right: Box::new(rewrite_plan(*right)),
            condition,
        },
        LogicalNode::AntiJoin {
            left,
            right,
            condition,
        } => LogicalNode::AntiJoin {
            left: Box::new(rewrite_plan(*left)),
            right: Box::new(rewrite_plan(*right)),
            condition,
        },
        LogicalNode::Subquery { subplan, alias } => LogicalNode::Subquery {
            subplan: Box::new(rewrite_plan(*subplan)),
            alias,
        },
        // Leaf nodes — no children to rewrite
        node @ (LogicalNode::Scan { .. }
        | LogicalNode::Values { .. }
        | LogicalNode::TableFunction { .. }
        | LogicalNode::Empty) => node,
    };
    LogicalPlan { node, schema }
}

/// Try to push a filter predicate below the given plan node.
///
/// Returns the optimized plan — either with the filter pushed into children
/// or kept above as a Filter node.
fn push_filter_down(predicate: TypedExpr, input: LogicalPlan) -> LogicalPlan {
    match input.node {
        // Filter over Filter → merge predicates, recurse
        LogicalNode::Filter {
            predicate: inner_pred,
            input: inner_input,
        } => {
            let merged = and_expr(predicate, inner_pred);
            push_filter_down(merged, *inner_input)
        }

        // Filter over Join → classify and push conjuncts
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } => push_filter_through_join(predicate, *left, *right, join_type, condition, input.schema),

        // Filter over Sort → push through (sort preserves rows)
        LogicalNode::Sort {
            order_by,
            input: sort_input,
        } => {
            let pushed = push_filter_down(predicate, *sort_input);
            let schema = pushed.schema.clone();
            LogicalPlan {
                node: LogicalNode::Sort {
                    order_by,
                    input: Box::new(pushed),
                },
                schema,
            }
        }

        // SemiJoin / AntiJoin: output schema = left-only, push left-only predicates to left child
        LogicalNode::SemiJoin {
            left,
            right,
            condition,
        } => push_filter_through_semi_anti(predicate, *left, *right, condition, input.schema, false),
        LogicalNode::AntiJoin {
            left,
            right,
            condition,
        } => push_filter_through_semi_anti(predicate, *left, *right, condition, input.schema, true),

        // Barriers — never push through these
        LogicalNode::Project { .. }
        | LogicalNode::Aggregate { .. }
        | LogicalNode::Limit { .. }
        | LogicalNode::Distinct { .. }
        | LogicalNode::DistinctOn { .. }
        | LogicalNode::Window { .. }
        | LogicalNode::Scan { .. }
        | LogicalNode::Values { .. }
        | LogicalNode::TableFunction { .. }
        | LogicalNode::Empty
        | LogicalNode::SetOperation { .. }
        | LogicalNode::Subquery { .. } => {
            let schema = input.schema.clone();
            LogicalPlan {
                node: LogicalNode::Filter {
                    predicate,
                    input: Box::new(input),
                },
                schema,
            }
        }
    }
}

/// Push filter conjuncts through a Join node based on join type and column references.
fn push_filter_through_join(
    predicate: TypedExpr,
    left: LogicalPlan,
    right: LogicalPlan,
    join_type: JoinType,
    condition: JoinCondition,
    join_schema: PlanSchema,
) -> LogicalPlan {
    let left_width = left.schema.columns.len();
    let conjuncts = split_conjunction(predicate);

    let mut left_pushable = Vec::new();
    let mut right_pushable = Vec::new();
    let mut remaining = Vec::new();

    for conj in conjuncts {
        // Never push volatile, correlated, or subquery-containing predicates
        if is_volatile(&conj) || has_correlated_ref(&conj) || has_unresolved_subquery(&conj) {
            remaining.push(conj);
            continue;
        }

        let indices = collect_column_indices(&conj);

        // No column refs (e.g. constant true) — can push to either side
        if indices.is_empty() {
            // Push to left side by default for constant predicates
            match join_type {
                JoinType::Inner | JoinType::Cross => left_pushable.push(conj),
                JoinType::Left => left_pushable.push(conj),
                JoinType::Right => right_pushable.push(conj),
                JoinType::Full => remaining.push(conj),
            }
            continue;
        }

        let all_left = indices.iter().all(|&i| i < left_width);
        let all_right = !indices.is_empty() && indices.iter().all(|&i| i >= left_width);

        match join_type {
            JoinType::Inner | JoinType::Cross => {
                if all_left {
                    left_pushable.push(conj);
                } else if all_right {
                    right_pushable.push(conj);
                } else {
                    remaining.push(conj);
                }
            }
            JoinType::Left => {
                if all_left {
                    left_pushable.push(conj);
                } else {
                    // Right-only and cross-table stay above (nullable side)
                    remaining.push(conj);
                }
            }
            JoinType::Right => {
                if all_right {
                    right_pushable.push(conj);
                } else {
                    // Left-only and cross-table stay above (nullable side)
                    remaining.push(conj);
                }
            }
            JoinType::Full => {
                // Both sides are nullable — nothing can be pushed
                remaining.push(conj);
            }
        }
    }

    // Recursively push into children (handles nested joins in a single pass)
    let new_left = if left_pushable.is_empty() {
        left
    } else {
        let pred = conjuncts_to_predicate(left_pushable);
        push_filter_down(pred, left)
    };

    let new_right = if right_pushable.is_empty() {
        right
    } else {
        // Reindex right-side predicates: subtract left_width from column indices
        let reindexed: Vec<TypedExpr> = right_pushable
            .into_iter()
            .map(|e| reindex_typed_expr(&e, left_width))
            .collect();
        let pred = conjuncts_to_predicate(reindexed);
        push_filter_down(pred, right)
    };

    // Rebuild the join
    let mut result = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(new_left),
            right: Box::new(new_right),
            join_type,
            condition,
        },
        schema: join_schema,
    };

    // Wrap in Filter if there are remaining predicates
    if !remaining.is_empty() {
        let pred = conjuncts_to_predicate(remaining);
        let schema = result.schema.clone();
        result = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: pred,
                input: Box::new(result),
            },
            schema,
        };
    }

    result
}

/// Push filter predicates through a SemiJoin or AntiJoin node.
///
/// Output schema = left-side only. Only left-only predicates can be pushed.
/// Any predicate referencing column_index >= left_width is invalid (would be a bug)
/// since the output has no right-side columns — keep above unchanged as defensive guard.
fn push_filter_through_semi_anti(
    predicate: TypedExpr,
    left: LogicalPlan,
    right: LogicalPlan,
    condition: JoinCondition,
    join_schema: PlanSchema,
    is_anti: bool,
) -> LogicalPlan {
    let left_width = left.schema.columns.len();
    let conjuncts = split_conjunction(predicate);

    let mut left_pushable = Vec::new();
    let mut remaining = Vec::new();

    for conj in conjuncts {
        if is_volatile(&conj) || has_correlated_ref(&conj) || has_unresolved_subquery(&conj) {
            remaining.push(conj);
            continue;
        }

        let indices = collect_column_indices(&conj);
        let all_left = indices.is_empty() || indices.iter().all(|&i| i < left_width);

        if all_left {
            left_pushable.push(conj);
        } else {
            // Defensive: should never happen since output = left-only
            remaining.push(conj);
        }
    }

    let new_left = if left_pushable.is_empty() {
        left
    } else {
        let pred = conjuncts_to_predicate(left_pushable);
        push_filter_down(pred, left)
    };

    let mut result = LogicalPlan {
        node: if is_anti {
            LogicalNode::AntiJoin {
                left: Box::new(new_left),
                right: Box::new(right),
                condition,
            }
        } else {
            LogicalNode::SemiJoin {
                left: Box::new(new_left),
                right: Box::new(right),
                condition,
            }
        },
        schema: join_schema.clone(),
    };

    if !remaining.is_empty() {
        let pred = conjuncts_to_predicate(remaining);
        result = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: pred,
                input: Box::new(result),
            },
            schema: join_schema,
        };
    }

    result
}

// ── Cross-join elimination ───────────────────────────────────

struct CrossJoinElimination;

impl LogicalRewriteRule for CrossJoinElimination {
    fn rewrite(&self, plan: LogicalPlan) -> LogicalPlan {
        eliminate_cross_joins(plan)
    }
}

/// Bottom-up recursive: rewrite children first, then check if current node is
/// `Filter over Inner/Cross join`.
fn eliminate_cross_joins(plan: LogicalPlan) -> LogicalPlan {
    // First, recursively rewrite children
    let plan = elim_rewrite_children(plan);

    // Then, check: Filter over Inner/Cross join?
    let LogicalPlan { node, schema } = plan;
    match node {
        LogicalNode::Filter { predicate, input } => {
            let LogicalPlan {
                node: inner_node,
                schema: inner_schema,
            } = *input;
            match inner_node {
                LogicalNode::Join {
                    left,
                    right,
                    join_type,
                    condition,
                } if matches!(join_type, JoinType::Inner | JoinType::Cross) => {
                    absorb_equi_into_join(
                        predicate,
                        *left,
                        *right,
                        join_type,
                        condition,
                        inner_schema,
                    )
                }
                other => LogicalPlan {
                    node: LogicalNode::Filter {
                        predicate,
                        input: Box::new(LogicalPlan {
                            node: other,
                            schema: inner_schema,
                        }),
                    },
                    schema,
                },
            }
        }
        other => LogicalPlan {
            node: other,
            schema,
        },
    }
}

/// Recursively rewrite all children of a plan node (for cross-join elimination).
fn elim_rewrite_children(plan: LogicalPlan) -> LogicalPlan {
    let schema = plan.schema;
    let node = match plan.node {
        LogicalNode::Filter { predicate, input } => LogicalNode::Filter {
            predicate,
            input: Box::new(eliminate_cross_joins(*input)),
        },
        LogicalNode::Project { projections, input } => LogicalNode::Project {
            projections,
            input: Box::new(eliminate_cross_joins(*input)),
        },
        LogicalNode::Aggregate {
            group_by,
            projections,
            input,
        } => LogicalNode::Aggregate {
            group_by,
            projections,
            input: Box::new(eliminate_cross_joins(*input)),
        },
        LogicalNode::Sort { order_by, input } => LogicalNode::Sort {
            order_by,
            input: Box::new(eliminate_cross_joins(*input)),
        },
        LogicalNode::Limit {
            limit,
            offset,
            input,
        } => LogicalNode::Limit {
            limit,
            offset,
            input: Box::new(eliminate_cross_joins(*input)),
        },
        LogicalNode::Distinct { input } => LogicalNode::Distinct {
            input: Box::new(eliminate_cross_joins(*input)),
        },
        LogicalNode::DistinctOn { on_exprs, input } => LogicalNode::DistinctOn {
            on_exprs,
            input: Box::new(eliminate_cross_joins(*input)),
        },
        LogicalNode::Window {
            window_functions,
            input_col_count,
            input,
        } => LogicalNode::Window {
            window_functions,
            input_col_count,
            input: Box::new(eliminate_cross_joins(*input)),
        },
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } => LogicalNode::Join {
            left: Box::new(eliminate_cross_joins(*left)),
            right: Box::new(eliminate_cross_joins(*right)),
            join_type,
            condition,
        },
        LogicalNode::SetOperation {
            op,
            all,
            left,
            right,
        } => LogicalNode::SetOperation {
            op,
            all,
            left: Box::new(eliminate_cross_joins(*left)),
            right: Box::new(eliminate_cross_joins(*right)),
        },
        LogicalNode::SemiJoin {
            left,
            right,
            condition,
        } => LogicalNode::SemiJoin {
            left: Box::new(eliminate_cross_joins(*left)),
            right: Box::new(eliminate_cross_joins(*right)),
            condition,
        },
        LogicalNode::AntiJoin {
            left,
            right,
            condition,
        } => LogicalNode::AntiJoin {
            left: Box::new(eliminate_cross_joins(*left)),
            right: Box::new(eliminate_cross_joins(*right)),
            condition,
        },
        LogicalNode::Subquery { subplan, alias } => LogicalNode::Subquery {
            subplan: Box::new(eliminate_cross_joins(*subplan)),
            alias,
        },
        // Leaf nodes — no children to rewrite
        node @ (LogicalNode::Scan { .. }
        | LogicalNode::Values { .. }
        | LogicalNode::TableFunction { .. }
        | LogicalNode::Empty) => node,
    };
    LogicalPlan { node, schema }
}

/// Check if a single conjunct is a cross-table equi-predicate.
///
/// Delegates to `join_keys::try_extract_equi_keys` — the same extractor used
/// by physical_planner (algorithm selection) and build.rs (operator construction).
/// This ensures zero semantic drift between rewrite-time and execution-time.
fn is_absorbable_equi(expr: &TypedExpr, left_width: usize) -> bool {
    let probe = JoinCondition::On(expr.clone());
    join_keys::try_extract_equi_keys(&probe, left_width).is_some()
}

fn absorb_equi_into_join(
    predicate: TypedExpr,
    left: LogicalPlan,
    right: LogicalPlan,
    original_join_type: JoinType,
    existing_condition: JoinCondition,
    join_schema: PlanSchema,
) -> LogicalPlan {
    // Skip USING (already has equi-join semantics from analyzer)
    if matches!(existing_condition, JoinCondition::Using(_)) {
        return rebuild_filter_join(
            predicate,
            left,
            right,
            original_join_type,
            existing_condition,
            join_schema,
        );
    }

    let left_width = left.schema.columns.len();
    let conjuncts = split_conjunction(predicate);

    let mut equi_preds = Vec::new();
    let mut remaining = Vec::new();
    for conj in conjuncts {
        if is_absorbable_equi(&conj, left_width) {
            equi_preds.push(conj);
        } else {
            remaining.push(conj);
        }
    }

    if equi_preds.is_empty() {
        // No cross-table equalities — reconstruct unchanged
        let pred = conjuncts_to_predicate(remaining);
        return rebuild_filter_join(
            pred,
            left,
            right,
            original_join_type,
            existing_condition,
            join_schema,
        );
    }

    // Build candidate ON condition, deduplicating against existing ON keys
    let candidate_on = match &existing_condition {
        JoinCondition::On(existing) => {
            // Deduplicate: only add equi_preds whose (left,right) key pairs are not
            // already in existing ON. Must compare pairwise tuples — independent set
            // membership would false-positive e.g. (a,b) against existing {(a,a),(b,b)}.
            let existing_pairs: HashSet<(usize, usize)> = if let Some((elk, erk)) =
                join_keys::try_extract_equi_keys(&existing_condition, left_width)
            {
                elk.into_iter().zip(erk).collect()
            } else {
                HashSet::new()
            };
            let mut new_equi = Vec::new();
            for ep in &equi_preds {
                let probe = JoinCondition::On(ep.clone());
                if let Some((lk, rk)) = join_keys::try_extract_equi_keys(&probe, left_width) {
                    let already_present = lk
                        .iter()
                        .zip(rk.iter())
                        .all(|(l, r)| existing_pairs.contains(&(*l, *r)));
                    if !already_present {
                        new_equi.push(ep.clone());
                    }
                    // If already present, drop the duplicate (don't add to remaining either)
                } else {
                    new_equi.push(ep.clone());
                }
            }
            if new_equi.is_empty() {
                // All equi_preds were duplicates — nothing to absorb
                if remaining.is_empty() {
                    // No remaining predicates — just return the join unchanged
                    return LogicalPlan {
                        node: LogicalNode::Join {
                            left: Box::new(left),
                            right: Box::new(right),
                            join_type: original_join_type,
                            condition: existing_condition,
                        },
                        schema: join_schema,
                    };
                }
                let pred = conjuncts_to_predicate(remaining);
                return rebuild_filter_join(
                    pred,
                    left,
                    right,
                    original_join_type,
                    existing_condition,
                    join_schema,
                );
            }
            equi_preds = new_equi;
            let mut all = vec![existing.clone()];
            all.extend(equi_preds.clone());
            conjuncts_to_predicate(all)
        }
        JoinCondition::None => conjuncts_to_predicate(equi_preds.clone()),
        JoinCondition::Using(_) => unreachable!(),
    };

    // Safety check: for INNER joins with existing ON, verify the merged
    // condition is still pure-equi. If not, absorption would degrade
    // HashJoin eligibility — put equi_preds back into remaining.
    let should_absorb = match &existing_condition {
        JoinCondition::None => true, // Cross join — always safe
        JoinCondition::On(_) => {
            // Only absorb if merged ON stays pure-equi
            let probe = JoinCondition::On(candidate_on.clone());
            join_keys::try_extract_equi_keys(&probe, left_width).is_some()
        }
        JoinCondition::Using(_) => unreachable!(),
    };

    if !should_absorb {
        // Put equi_preds back into remaining, keep original condition
        remaining.extend(equi_preds);
        let pred = conjuncts_to_predicate(remaining);
        return rebuild_filter_join(
            pred,
            left,
            right,
            original_join_type,
            existing_condition,
            join_schema,
        );
    }

    let mut result = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Inner, // Cross + equi → Inner
            condition: JoinCondition::On(candidate_on),
        },
        schema: join_schema,
    };

    if !remaining.is_empty() {
        let pred = conjuncts_to_predicate(remaining);
        let schema = result.schema.clone();
        result = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: pred,
                input: Box::new(result),
            },
            schema,
        };
    }
    result
}

/// Rebuild a Filter over Join unchanged (helper to avoid duplication).
fn rebuild_filter_join(
    predicate: TypedExpr,
    left: LogicalPlan,
    right: LogicalPlan,
    join_type: JoinType,
    condition: JoinCondition,
    join_schema: PlanSchema,
) -> LogicalPlan {
    let join = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type,
            condition,
        },
        schema: join_schema.clone(),
    };
    LogicalPlan {
        node: LogicalNode::Filter {
            predicate,
            input: Box::new(join),
        },
        schema: join_schema,
    }
}

// ── Helpers (private to this module) ───────────────────────────

/// Split an expression on AND into a flat list of conjuncts.
pub(super) fn split_conjunction(expr: TypedExpr) -> Vec<TypedExpr> {
    match expr.kind {
        TypedExprKind::BinaryOp {
            left,
            op: BinaryOp::And,
            right,
        } => {
            let mut result = split_conjunction(*left);
            result.extend(split_conjunction(*right));
            result
        }
        _ => vec![expr],
    }
}

/// Recombine a list of conjuncts into a single AND expression.
///
/// Panics if the list is empty.
pub(super) fn conjuncts_to_predicate(mut conjuncts: Vec<TypedExpr>) -> TypedExpr {
    assert!(!conjuncts.is_empty(), "conjuncts must be non-empty");
    let mut result = conjuncts.pop().unwrap();
    while let Some(next) = conjuncts.pop() {
        result = and_expr(next, result);
    }
    result
}

/// Create an AND binary op node.
pub(super) fn and_expr(left: TypedExpr, right: TypedExpr) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(left),
            op: BinaryOp::And,
            right: Box::new(right),
        },
        data_type: DataType::Boolean,
    }
}

/// Collect all column indices referenced by an expression (scope_depth == 0 only).
///
/// Uses explicit recursive walk (not `expr_any`) because `expr_any` takes `&Fn`
/// which doesn't support the mutable `HashSet` capture needed for collection.
pub(super) fn collect_column_indices(expr: &TypedExpr) -> HashSet<usize> {
    let mut indices = HashSet::new();
    collect_column_indices_inner(expr, &mut indices);
    indices
}

fn collect_column_indices_inner(expr: &TypedExpr, indices: &mut HashSet<usize>) {
    // ColumnRef at scope_depth 0: collect it
    if let TypedExprKind::ColumnRef {
        scope_depth: 0,
        column_index,
        ..
    } = &expr.kind
    {
        indices.insert(*column_index);
    }
    // Recurse into children (for_each_child treats subqueries as opaque — correct)
    crate::sql::expr::traverse::for_each_child(expr, &mut |child| {
        collect_column_indices_inner(child, indices);
    });
}

#[cfg(test)]
mod tests;
