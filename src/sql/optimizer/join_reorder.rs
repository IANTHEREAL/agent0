//! Cost-based join reordering via DPccp (n ≤ 8) or greedy (n > 8).
//!
//! Flattens inner/cross join trees, classifies predicates, and reconstructs
//! an optimal join order using table statistics for cardinality estimates.
//!
//! Safety gates:
//! - Unresolved subqueries in the candidate subtree → skip reorder
//! - Correlated refs in RHS of join → treat as opaque base relation
//! - Outer joins / USING joins → not flattened (subtrees recursively reordered)

use std::collections::{HashMap, HashSet};

use super::logical_plan::{LogicalNode, LogicalPlan, PlanSchema};
use super::physical_planner::PlanningContext;
use super::rewrite::{collect_column_indices, conjuncts_to_predicate, split_conjunction};
use super::selectivity;
use super::statistics::TableStatistics;
use crate::sql::analyzer::types::{
    reindex_typed_expr, BinaryOp, JoinCondition, JoinType, TypedExpr, TypedExprKind,
};
use crate::sql::expr::classify::{has_correlated_ref, has_unresolved_subquery};
use crate::sql::expr::traverse::map_children;
use crate::types::DataType;

const DEFAULT_ESTIMATED_ROWS: usize = 1000;
const DEFAULT_JOIN_SEL: f64 = 0.1;
// DPccp limit: above this, fall back to greedy
const DPCCP_MAX_RELS: usize = 8;

// ── Public entry point ──────────────────────────────────────────

/// Recursively reorder joins in a logical plan tree.
///
/// For each inner/cross join group, flattens the tree, classifies predicates,
/// and finds an optimal join order using DPccp (≤8 rels) or greedy (>8 rels).
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
fn logical_has_unresolved_subquery(plan: &LogicalPlan) -> bool {
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
fn logical_has_correlated_refs(plan: &LogicalPlan) -> bool {
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

// ── Flattening ──────────────────────────────────────────────────

/// A base relation in the flattened join group.
#[derive(Debug, Clone)]
struct BaseRelation {
    /// Unique ID in this join group (index into the rels vector).
    id: usize,
    /// The logical plan subtree for this relation.
    plan: LogicalPlan,
    /// Column offset in the root join schema.
    col_offset: usize,
    /// Number of output columns.
    width: usize,
}

/// Flatten an inner/cross join tree into base relations and raw predicates.
///
/// `root_offset` tracks the cumulative column offset from the root of the
/// join group, so ON conditions can be lifted to root-level indices.
fn flatten_recursive(
    plan: LogicalPlan,
    rels: &mut Vec<BaseRelation>,
    preds: &mut Vec<TypedExpr>,
    root_offset: usize,
) {
    match plan.node {
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } if matches!(join_type, JoinType::Inner | JoinType::Cross)
            && !matches!(condition, JoinCondition::Using(..))
            && !logical_has_correlated_refs(&right) =>
        {
            let left_width = left.schema.columns.len();
            flatten_recursive(*left, rels, preds, root_offset);
            flatten_recursive(*right, rels, preds, root_offset + left_width);
            if let JoinCondition::On(expr) = condition {
                let lifted = if root_offset > 0 {
                    lift_typed_expr(&expr, root_offset)
                } else {
                    expr
                };
                preds.push(lifted);
            }
        }
        _ => {
            // Opaque base relation
            let width = plan.schema.columns.len();
            let id = rels.len();
            rels.push(BaseRelation {
                id,
                plan,
                col_offset: root_offset,
                width,
            });
        }
    }
}

/// Lift column indices by adding `offset` to `ColumnRef.column_index` at scope_depth 0.
///
/// Inverse of `reindex_typed_expr` — used to convert local ON conditions
/// to root-level join-group indices.
fn lift_typed_expr(expr: &TypedExpr, offset: usize) -> TypedExpr {
    let kind = match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            column_name,
        } if *scope_depth == 0 => TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index: column_index + offset,
            column_name: column_name.clone(),
        },
        _ => map_children(expr, &mut |child| lift_typed_expr(child, offset)),
    };
    TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    }
}

// ── Predicate classification ────────────────────────────────────

/// A join edge between two base relations.
#[derive(Debug, Clone)]
struct JoinEdge {
    /// Bitmask of involved relations (exactly 2 bits set).
    rels: u64,
    /// The predicate conjunct.
    predicate: TypedExpr,
    /// Whether this is a pure equi-join predicate (bare Var = Var across rels).
    is_equi: bool,
}

/// Result of classifying all predicates.
struct Classification {
    /// Two-relation edges.
    edges: Vec<JoinEdge>,
    /// Single-relation filters, keyed by rel_id.
    base_local_filters: HashMap<usize, Vec<TypedExpr>>,
    /// Predicates referencing 0 or 3+ relations.
    remaining: Vec<TypedExpr>,
}

/// Determine which base relation a column index belongs to.
fn find_rel_for_column(col_idx: usize, rels: &[BaseRelation]) -> Option<usize> {
    for rel in rels {
        if col_idx >= rel.col_offset && col_idx < rel.col_offset + rel.width {
            return Some(rel.id);
        }
    }
    None
}

/// Check if a predicate is a pure equi-join predicate:
/// bare `ColumnRef = ColumnRef` where the two columns reference different relations.
fn is_pure_equi_join_pred(expr: &TypedExpr, rels: &[BaseRelation]) -> bool {
    if let TypedExprKind::BinaryOp { left, op, right } = &expr.kind {
        if *op != BinaryOp::Eq {
            return false;
        }
        // Both sides must be bare ColumnRef at scope_depth 0
        let left_col = match &left.kind {
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index,
                ..
            } => Some(*column_index),
            _ => None,
        };
        let right_col = match &right.kind {
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index,
                ..
            } => Some(*column_index),
            _ => None,
        };
        if let (Some(l_idx), Some(r_idx)) = (left_col, right_col) {
            let l_rel = find_rel_for_column(l_idx, rels);
            let r_rel = find_rel_for_column(r_idx, rels);
            if let (Some(lr), Some(rr)) = (l_rel, r_rel) {
                return lr != rr;
            }
        }
    }
    false
}

/// Classify all predicates into edges, base-local filters, and remaining.
fn classify_predicates(conjuncts: &[TypedExpr], rels: &[BaseRelation]) -> Classification {
    let mut edges = Vec::new();
    let mut base_local: HashMap<usize, Vec<TypedExpr>> = HashMap::new();
    let mut remaining = Vec::new();

    for conj in conjuncts {
        let indices = collect_column_indices(conj);

        // Find which relations are referenced
        let mut rel_set: HashSet<usize> = HashSet::new();
        for &idx in &indices {
            if let Some(rel_id) = find_rel_for_column(idx, rels) {
                rel_set.insert(rel_id);
            }
        }

        match rel_set.len() {
            1 => {
                let rel_id = *rel_set.iter().next().unwrap();
                // Localize: subtract the base col_offset
                let local_pred = reindex_typed_expr(conj, rels[rel_id].col_offset);
                base_local.entry(rel_id).or_default().push(local_pred);
            }
            2 => {
                let mut iter = rel_set.iter();
                let r1 = *iter.next().unwrap();
                let r2 = *iter.next().unwrap();
                let rel_mask = (1u64 << r1) | (1u64 << r2);
                let is_equi = is_pure_equi_join_pred(conj, rels);
                edges.push(JoinEdge {
                    rels: rel_mask,
                    predicate: conj.clone(),
                    is_equi,
                });
            }
            _ => {
                // 0 or 3+ relations
                remaining.push(conj.clone());
            }
        }
    }

    Classification {
        edges,
        base_local_filters: base_local,
        remaining,
    }
}

// ── Cost model ──────────────────────────────────────────────────

/// Estimate base row count from a LogicalPlan subtree.
///
/// Mirrors physical_planner.rs row estimation logic.
fn estimate_subtree_rows(plan: &LogicalPlan, ctx: &PlanningContext) -> usize {
    match &plan.node {
        LogicalNode::Scan { table_name, alias } => {
            let key = super::schema_map_key(table_name, alias.as_deref());
            ctx.get_stats(&key)
                .map(|s| s.row_count)
                .unwrap_or(DEFAULT_ESTIMATED_ROWS)
        }
        LogicalNode::Empty => 1,
        LogicalNode::Values { rows } => rows.len(),
        LogicalNode::TableFunction { .. } => DEFAULT_ESTIMATED_ROWS,

        // Transparent: pass through with optional adjustment
        LogicalNode::Filter { predicate, input } => {
            let child_rows = estimate_subtree_rows(input, ctx);
            // Try stats-based selectivity
            if let Some(stats) = resolve_stats_logical(input, ctx) {
                let sel = selectivity::estimate_selectivity(predicate, stats);
                (child_rows as f64 * sel).ceil() as usize
            } else {
                (child_rows / 3).max(1)
            }
        }
        LogicalNode::Project { input, .. }
        | LogicalNode::Sort { input, .. }
        | LogicalNode::Distinct { input }
        | LogicalNode::DistinctOn { input, .. }
        | LogicalNode::Window { input, .. } => estimate_subtree_rows(input, ctx),

        LogicalNode::Limit { limit, input, .. } => {
            let child_rows = estimate_subtree_rows(input, ctx);
            if let Some(limit_expr) = limit {
                extract_constant_usize(limit_expr)
                    .map(|l| l.min(child_rows))
                    .unwrap_or(child_rows)
            } else {
                child_rows
            }
        }

        LogicalNode::Aggregate { input, .. } => {
            let child_rows = estimate_subtree_rows(input, ctx);
            (child_rows / 10).max(1)
        }

        // Joins: estimate recursively
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
            ..
        } => {
            let left_rows = estimate_subtree_rows(left, ctx);
            let right_rows = estimate_subtree_rows(right, ctx);
            estimate_join_rows_logical(
                left, right, left_rows, right_rows, join_type, condition, ctx,
            )
        }

        LogicalNode::SetOperation { left, right, .. } => {
            estimate_subtree_rows(left, ctx) + estimate_subtree_rows(right, ctx)
        }
        LogicalNode::Subquery { subplan, .. } => estimate_subtree_rows(subplan, ctx),
    }
}

/// Walk a logical subtree to find base-table stats (mirrors physical_planner::resolve_stats).
fn resolve_stats_logical<'a>(
    plan: &LogicalPlan,
    ctx: &'a PlanningContext,
) -> Option<&'a TableStatistics> {
    match &plan.node {
        LogicalNode::Scan { table_name, alias } => {
            let key = super::schema_map_key(table_name, alias.as_deref());
            ctx.get_stats(&key)
        }
        LogicalNode::Aggregate { .. } => None,
        LogicalNode::Filter { input, .. }
        | LogicalNode::Project { input, .. }
        | LogicalNode::Sort { input, .. }
        | LogicalNode::Limit { input, .. }
        | LogicalNode::Distinct { input }
        | LogicalNode::DistinctOn { input, .. }
        | LogicalNode::Window { input, .. } => resolve_stats_logical(input, ctx),
        _ => None,
    }
}

/// Estimate join output rows (mirrors physical_planner::estimate_join_rows).
fn estimate_join_rows_logical(
    left: &LogicalPlan,
    right: &LogicalPlan,
    left_rows: usize,
    right_rows: usize,
    join_type: &JoinType,
    condition: &JoinCondition,
    ctx: &PlanningContext,
) -> usize {
    let left_width = left.schema.columns.len();

    let sel = if let Some((left_keys, right_keys)) =
        super::join_keys::try_extract_equi_keys(condition, left_width)
    {
        let left_stats = resolve_stats_logical(left, ctx);
        let right_stats = resolve_stats_logical(right, ctx);

        match (left_stats, right_stats) {
            (Some(ls), Some(rs)) => {
                let mut s = 1.0;
                for (&lk, &rk) in left_keys.iter().zip(right_keys.iter()) {
                    let l_name = left.schema.columns.get(lk).map(|(n, _)| n.as_str());
                    let r_name = right.schema.columns.get(rk).map(|(n, _)| n.as_str());

                    let l_ndv = l_name
                        .and_then(|n| selectivity::get_column_stats(ls, n))
                        .map(|c| selectivity::n_distinct_raw(c, ls.row_count));
                    let r_ndv = r_name
                        .and_then(|n| selectivity::get_column_stats(rs, n))
                        .map(|c| selectivity::n_distinct_raw(c, rs.row_count));

                    s *= match (l_ndv, r_ndv) {
                        (Some(l), Some(r)) => 1.0 / l.max(r).max(1.0),
                        _ => DEFAULT_JOIN_SEL,
                    };
                }
                s
            }
            _ => DEFAULT_JOIN_SEL,
        }
    } else {
        1.0 // Non-equi or cross — Cartesian
    };

    let inner_est = ((left_rows as f64) * (right_rows as f64) * sel).ceil() as usize;

    let min_rows = match join_type {
        JoinType::Left => left_rows,
        JoinType::Right => right_rows,
        JoinType::Full => left_rows.max(right_rows),
        _ => 0,
    };

    inner_est.max(min_rows).max(1)
}

fn extract_constant_usize(expr: &TypedExpr) -> Option<usize> {
    match &expr.kind {
        TypedExprKind::Constant(crate::types::Value::Int32(v)) => Some(*v as usize),
        TypedExprKind::Constant(crate::types::Value::Int64(v)) => Some(*v as usize),
        _ => None,
    }
}

// ── DPccp DP state ──────────────────────────────────────────────

/// A candidate partial join plan tracked during DP.
#[derive(Clone)]
struct DpEntry {
    /// Bitmask of included base relations.
    set: u64,
    /// The constructed logical plan for this subset.
    plan: LogicalPlan,
    /// Estimated output rows.
    rows: usize,
    /// Cumulative cost.
    cost: f64,
    /// Column mapping: `col_map[root_col_idx] = local_col_idx` in this plan.
    col_map: HashMap<usize, usize>,
}

// ── DPccp optimizer ─────────────────────────────────────────────

fn dpccp_optimize(
    rels: &[BaseRelation],
    edges: &[JoinEdge],
    remaining: &[TypedExpr],
    ctx: &PlanningContext,
    root_schema: &PlanSchema,
) -> LogicalPlan {
    let n = rels.len();
    let full_set = (1u64 << n) - 1;

    // Initialize DP table with single-relation entries
    let mut dp: HashMap<u64, DpEntry> = HashMap::new();
    for rel in rels {
        let mask = 1u64 << rel.id;
        let rows = estimate_subtree_rows(&rel.plan, ctx);
        let cost = rows as f64 * 0.01 + 1.0;
        let mut col_map = HashMap::new();
        for i in 0..rel.width {
            col_map.insert(rel.col_offset + i, i);
        }
        dp.insert(
            mask,
            DpEntry {
                set: mask,
                plan: rel.plan.clone(),
                rows,
                cost,
                col_map,
            },
        );
    }

    // Build adjacency set from edges
    let edge_graph = build_edge_graph(edges, n);

    // Enumerate subsets in increasing size
    for size in 2..=n {
        for subset in SubsetIter::new(full_set, size) {
            // Try all ways to split subset into (s1, s2) where s1 < s2
            // and s1, s2 are connected via at least one edge
            for s1 in SubsetIter::non_empty_subsets(subset) {
                let s2 = subset & !s1;
                if s2 == 0 || s1 >= s2 {
                    continue; // Avoid duplicates: ensure s1 < s2
                }
                // Both must be in DP table
                let (e1, e2) = match (dp.get(&s1), dp.get(&s2)) {
                    (Some(a), Some(b)) => (a.clone(), b.clone()),
                    _ => continue,
                };
                // Must be connected by at least one edge
                if !subsets_connected(s1, s2, &edge_graph) {
                    continue;
                }
                let candidate = build_join_candidate(&e1, &e2, edges, ctx, rels);
                // Update DP table if this is better
                let existing = dp.get(&subset);
                if existing.is_none() || candidate.cost < existing.unwrap().cost {
                    dp.insert(subset, candidate);
                }
            }
        }
    }

    // Build final plan
    if let Some(entry) = dp.get(&full_set) {
        finalize_plan(entry, remaining, root_schema, rels)
    } else {
        // Disconnected graph: find connected components and stitch
        stitch_disconnected_components(&dp, rels, edges, remaining, ctx, root_schema, n)
    }
}

// ── Greedy optimizer ────────────────────────────────────────────

fn greedy_optimize(
    rels: &[BaseRelation],
    edges: &[JoinEdge],
    remaining: &[TypedExpr],
    ctx: &PlanningContext,
    root_schema: &PlanSchema,
) -> LogicalPlan {
    let n = rels.len();

    // Initialize candidates
    let mut candidates: Vec<DpEntry> = rels
        .iter()
        .map(|rel| {
            let mask = 1u64 << rel.id;
            let rows = estimate_subtree_rows(&rel.plan, ctx);
            let cost = rows as f64 * 0.01 + 1.0;
            let mut col_map = HashMap::new();
            for i in 0..rel.width {
                col_map.insert(rel.col_offset + i, i);
            }
            DpEntry {
                set: mask,
                plan: rel.plan.clone(),
                rows,
                cost,
                col_map,
            }
        })
        .collect();

    let edge_graph = build_edge_graph(edges, n);

    // Greedily merge the cheapest connected pair
    while candidates.len() > 1 {
        let mut best_cost = f64::MAX;
        let mut best_i = 0;
        let mut best_j = 1;
        let mut best_candidate: Option<DpEntry> = None;

        for i in 0..candidates.len() {
            for j in (i + 1)..candidates.len() {
                // Prefer connected pairs
                if !subsets_connected(candidates[i].set, candidates[j].set, &edge_graph) {
                    continue;
                }
                let c = build_join_candidate(&candidates[i], &candidates[j], edges, ctx, rels);
                if c.cost < best_cost {
                    best_cost = c.cost;
                    best_i = i;
                    best_j = j;
                    best_candidate = Some(c);
                }
            }
        }

        if best_candidate.is_none() {
            // No connected pairs left: stitch disconnected components with cross join.
            // Sort ascending by row count and merge smallest-first to minimize
            // intermediate result sizes (matches stitch_disconnected_components).
            candidates.sort_by_key(|c| c.rows);
            let mut result = candidates.remove(0);
            for other in candidates.drain(..) {
                result = build_cross_join_candidate(&result, &other);
            }
            candidates.push(result);
            break;
        }

        let merged = best_candidate.unwrap();
        // Remove j first (higher index), then i
        candidates.remove(best_j);
        candidates.remove(best_i);
        candidates.push(merged);
    }

    let entry = &candidates[0];
    finalize_plan(entry, remaining, root_schema, rels)
}

// ── Join candidate builder ──────────────────────────────────────

/// Build a candidate join of two DP entries.
fn build_join_candidate(
    left: &DpEntry,
    right: &DpEntry,
    edges: &[JoinEdge],
    ctx: &PlanningContext,
    rels: &[BaseRelation],
) -> DpEntry {
    let combined_set = left.set | right.set;

    // Collect applicable edges: edge.rels ⊆ combined AND touches both sides
    let mut equi_preds: Vec<TypedExpr> = Vec::new();
    let mut residual_preds: Vec<TypedExpr> = Vec::new();

    for edge in edges {
        if (edge.rels & combined_set) == edge.rels
            && (edge.rels & left.set) != 0
            && (edge.rels & right.set) != 0
        {
            if edge.is_equi {
                equi_preds.push(edge.predicate.clone());
            } else {
                residual_preds.push(edge.predicate.clone());
            }
        }
    }

    // Build col_map for the combined plan
    let left_width = left.plan.schema.columns.len();
    let mut new_col_map = HashMap::new();
    for (&orig, &local) in &left.col_map {
        new_col_map.insert(orig, local);
    }
    for (&orig, &local) in &right.col_map {
        new_col_map.insert(orig, left_width + local);
    }

    // Build join schema
    let mut combined_cols = left.plan.schema.columns.clone();
    combined_cols.extend(right.plan.schema.columns.clone());
    let join_schema = PlanSchema::from_columns(combined_cols);

    // Remap equi predicates to local indices
    let remapped_equi: Vec<TypedExpr> = equi_preds
        .iter()
        .map(|p| remap_expr(p, &new_col_map))
        .collect();

    // Build the join node
    let (join_type, condition) = if !remapped_equi.is_empty() {
        (
            JoinType::Inner,
            JoinCondition::On(conjuncts_to_predicate(remapped_equi)),
        )
    } else {
        (JoinType::Inner, JoinCondition::None)
    };

    let mut plan = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left.plan.clone()),
            right: Box::new(right.plan.clone()),
            join_type,
            condition: condition.clone(),
        },
        schema: join_schema.clone(),
    };

    // Wrap with residual filter if needed
    if !residual_preds.is_empty() {
        let remapped_residual: Vec<TypedExpr> = residual_preds
            .iter()
            .map(|p| remap_expr(p, &new_col_map))
            .collect();
        let pred = conjuncts_to_predicate(remapped_residual);
        plan = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: pred,
                input: Box::new(plan),
            },
            schema: join_schema,
        };
    }

    // Estimate rows and cost
    let left_rows = left.rows;
    let right_rows = right.rows;
    let output_rows = estimate_candidate_rows(
        &left.plan,
        &right.plan,
        left_rows,
        right_rows,
        &equi_preds,
        ctx,
        rels,
    );
    let cost = left.cost + right.cost + output_rows as f64 * 0.01;

    DpEntry {
        set: combined_set,
        plan,
        rows: output_rows,
        cost,
        col_map: new_col_map,
    }
}

/// Build a cross join candidate (for disconnected components).
fn build_cross_join_candidate(left: &DpEntry, right: &DpEntry) -> DpEntry {
    let combined_set = left.set | right.set;
    let left_width = left.plan.schema.columns.len();

    let mut new_col_map = HashMap::new();
    for (&orig, &local) in &left.col_map {
        new_col_map.insert(orig, local);
    }
    for (&orig, &local) in &right.col_map {
        new_col_map.insert(orig, left_width + local);
    }

    let mut combined_cols = left.plan.schema.columns.clone();
    combined_cols.extend(right.plan.schema.columns.clone());
    let join_schema = PlanSchema::from_columns(combined_cols);

    let plan = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left.plan.clone()),
            right: Box::new(right.plan.clone()),
            join_type: JoinType::Inner,
            condition: JoinCondition::None,
        },
        schema: join_schema,
    };

    let output_rows = left.rows.saturating_mul(right.rows);
    let cost = left.cost + right.cost + output_rows as f64 * 0.01;

    DpEntry {
        set: combined_set,
        plan,
        rows: output_rows,
        cost,
        col_map: new_col_map,
    }
}

/// Estimate output rows for a candidate join based on raw (root-level) equi predicates.
fn estimate_candidate_rows(
    left_plan: &LogicalPlan,
    right_plan: &LogicalPlan,
    left_rows: usize,
    right_rows: usize,
    equi_preds: &[TypedExpr],
    ctx: &PlanningContext,
    rels: &[BaseRelation],
) -> usize {
    if equi_preds.is_empty() {
        // Cross join
        return left_rows.saturating_mul(right_rows).max(1);
    }

    let left_stats = resolve_stats_logical(left_plan, ctx);
    let right_stats = resolve_stats_logical(right_plan, ctx);

    let mut sel = 1.0;
    for pred in equi_preds {
        if let TypedExprKind::BinaryOp { left, right, .. } = &pred.kind {
            if let (
                TypedExprKind::ColumnRef {
                    column_index: l_idx,
                    ..
                },
                TypedExprKind::ColumnRef {
                    column_index: r_idx,
                    ..
                },
            ) = (&left.kind, &right.kind)
            {
                // Figure out which side each column is on
                let l_ndv = get_ndv_for_root_col(*l_idx, rels, ctx);
                let r_ndv = get_ndv_for_root_col(*r_idx, rels, ctx);

                sel *= match (l_ndv, r_ndv) {
                    (Some(l), Some(r)) => 1.0 / l.max(r).max(1.0),
                    _ => DEFAULT_JOIN_SEL,
                };
                continue;
            }
        }
        sel *= DEFAULT_JOIN_SEL;
    }

    ((left_rows as f64) * (right_rows as f64) * sel)
        .ceil()
        .max(1.0) as usize
}

/// Get NDV for a root-level column index by tracing back to the base relation.
fn get_ndv_for_root_col(
    root_col: usize,
    rels: &[BaseRelation],
    ctx: &PlanningContext,
) -> Option<f64> {
    let rel = rels
        .iter()
        .find(|r| root_col >= r.col_offset && root_col < r.col_offset + r.width)?;
    let local_idx = root_col - rel.col_offset;
    let col_name = rel
        .plan
        .schema
        .columns
        .get(local_idx)
        .map(|(n, _)| n.as_str())?;
    let stats = resolve_stats_logical(&rel.plan, ctx)?;
    let col_stats = selectivity::get_column_stats(stats, col_name)?;
    Some(selectivity::n_distinct_raw(col_stats, stats.row_count))
}

// ── Edge graph and connectivity ─────────────────────────────────

/// Build adjacency: `edge_graph[i]` is the set of relations adjacent to relation i.
fn build_edge_graph(edges: &[JoinEdge], n: usize) -> Vec<u64> {
    let mut graph = vec![0u64; n];
    for edge in edges {
        let bits: Vec<usize> = (0..n).filter(|&i| edge.rels & (1u64 << i) != 0).collect();
        if bits.len() == 2 {
            graph[bits[0]] |= 1u64 << bits[1];
            graph[bits[1]] |= 1u64 << bits[0];
        }
    }
    graph
}

/// Check if two subsets are connected by at least one edge.
fn subsets_connected(s1: u64, s2: u64, edge_graph: &[u64]) -> bool {
    for i in 0..64 {
        if s1 & (1u64 << i) == 0 {
            continue;
        }
        if edge_graph[i] & s2 != 0 {
            return true;
        }
    }
    false
}

/// Find connected components in the full relation set.
fn find_connected_components(n: usize, edge_graph: &[u64]) -> Vec<u64> {
    let mut visited = 0u64;
    let mut components = Vec::new();

    for start in 0..n {
        if visited & (1u64 << start) != 0 {
            continue;
        }
        let mut component = 0u64;
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            if component & (1u64 << node) != 0 {
                continue;
            }
            component |= 1u64 << node;
            visited |= 1u64 << node;
            for neighbor in 0..n {
                if edge_graph[node] & (1u64 << neighbor) != 0 && component & (1u64 << neighbor) == 0
                {
                    stack.push(neighbor);
                }
            }
        }
        components.push(component);
    }

    components
}

/// Handle disconnected graph: optimize each component independently, stitch with cross joins.
fn stitch_disconnected_components(
    dp: &HashMap<u64, DpEntry>,
    rels: &[BaseRelation],
    edges: &[JoinEdge],
    remaining: &[TypedExpr],
    ctx: &PlanningContext,
    root_schema: &PlanSchema,
    n: usize,
) -> LogicalPlan {
    let edge_graph = build_edge_graph(edges, n);
    let components = find_connected_components(n, &edge_graph);

    // For each component, get the DP entry or build from single rel
    let mut component_entries: Vec<DpEntry> = Vec::new();
    for &comp in &components {
        if let Some(entry) = dp.get(&comp) {
            component_entries.push(entry.clone());
        } else {
            // Single-relation component
            for i in 0..n {
                if comp == (1u64 << i) {
                    if let Some(entry) = dp.get(&comp) {
                        component_entries.push(entry.clone());
                    }
                }
            }
        }
    }

    // Sort by ascending rows for deterministic, efficient cross-join order
    component_entries.sort_by_key(|e| e.rows);

    // Stitch together with cross joins
    let mut result = component_entries.remove(0);
    for other in component_entries {
        result = build_cross_join_candidate(&result, &other);
    }

    finalize_plan(&result, remaining, root_schema, rels)
}

// ── Remap expressions ───────────────────────────────────────────

/// Remap a root-level expression to local indices using a column map.
fn remap_expr(expr: &TypedExpr, col_map: &HashMap<usize, usize>) -> TypedExpr {
    let kind = match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index,
            column_name,
        } => {
            let new_idx = col_map.get(column_index).copied().unwrap_or(*column_index);
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: new_idx,
                column_name: column_name.clone(),
            }
        }
        _ => map_children(expr, &mut |child| remap_expr(child, col_map)),
    };
    TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    }
}

// ── Finalization ────────────────────────────────────────────────

/// Build the final plan from a DP entry: apply remaining predicates and remap.
fn finalize_plan(
    entry: &DpEntry,
    remaining: &[TypedExpr],
    root_schema: &PlanSchema,
    rels: &[BaseRelation],
) -> LogicalPlan {
    let mut plan = entry.plan.clone();

    // Apply remaining predicates (0-rel or 3+-rel)
    if !remaining.is_empty() {
        let remapped: Vec<TypedExpr> = remaining
            .iter()
            .map(|p| remap_expr(p, &entry.col_map))
            .collect();
        let pred = conjuncts_to_predicate(remapped);
        let schema = plan.schema.clone();
        plan = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: pred,
                input: Box::new(plan),
            },
            schema,
        };
    }

    // Check if columns need reordering
    let total_root_cols: usize = rels.iter().map(|r| r.width).sum();
    let needs_remap =
        (0..total_root_cols).any(|i| entry.col_map.get(&i).copied().unwrap_or(i) != i);

    if needs_remap {
        // Build a project node that reorders columns back to original order
        let mut projections = Vec::new();
        let mut output_cols = Vec::new();
        for root_idx in 0..total_root_cols {
            let local_idx = entry.col_map.get(&root_idx).copied().unwrap_or(root_idx);
            let (col_name, col_type) = &plan.schema.columns[local_idx];
            projections.push(crate::sql::analyzer::types::AnalyzedProjection {
                expr: TypedExpr {
                    kind: TypedExprKind::ColumnRef {
                        scope_depth: 0,
                        column_index: local_idx,
                        column_name: col_name.clone(),
                    },
                    data_type: col_type.clone(),
                },
                output_name: col_name.clone(),
            });
            output_cols.push((col_name.clone(), col_type.clone()));
        }
        plan = plan.project(projections, PlanSchema::from_columns(output_cols));
    }

    // Ensure output schema matches root
    plan.schema = root_schema.clone();
    plan
}

// ── Subset iteration helpers ────────────────────────────────────

struct SubsetIter {
    universe: u64,
    target_size: usize,
    current: Option<u64>,
}

impl SubsetIter {
    fn new(universe: u64, target_size: usize) -> Self {
        // Find the first subset of the given size
        let first = first_subset_of_size(universe, target_size);
        Self {
            universe,
            target_size,
            current: first,
        }
    }

    /// Iterate all non-empty subsets of a given set.
    fn non_empty_subsets(set: u64) -> NonEmptySubsetIter {
        NonEmptySubsetIter {
            set,
            current: Some(set), // Start with the full set
            started: false,
        }
    }
}

impl Iterator for SubsetIter {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        let result = self.current?;
        // Find next subset of the same size using Gosper's hack
        self.current = next_subset_of_size(result, self.universe, self.target_size);
        Some(result)
    }
}

struct NonEmptySubsetIter {
    set: u64,
    current: Option<u64>,
    started: bool,
}

impl Iterator for NonEmptySubsetIter {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        if !self.started {
            self.started = true;
            // Start from (set - 1) & set, the first proper subset
            let first = (self.set.wrapping_sub(1)) & self.set;
            if first == 0 {
                return None;
            }
            self.current = Some(first);
            return self.current;
        }
        let c = self.current?;
        let next = (c.wrapping_sub(1)) & self.set;
        if next == 0 {
            self.current = None;
            return None;
        }
        self.current = Some(next);
        Some(next)
    }
}

/// Find the first subset of `universe` with exactly `target_size` bits set.
fn first_subset_of_size(universe: u64, target_size: usize) -> Option<u64> {
    if target_size == 0 {
        return Some(0);
    }
    // Collect bits in universe
    let bits: Vec<u32> = (0..64).filter(|&i| universe & (1u64 << i) != 0).collect();
    if bits.len() < target_size {
        return None;
    }
    // First subset = lowest `target_size` bits
    let mut result = 0u64;
    for i in 0..target_size {
        result |= 1u64 << bits[i];
    }
    Some(result)
}

/// Find the next subset of `universe` with `target_size` bits, using Gosper's hack variant.
fn next_subset_of_size(current: u64, universe: u64, target_size: usize) -> Option<u64> {
    // Collect universe bits in order
    let bits: Vec<u32> = (0..64).filter(|&i| universe & (1u64 << i) != 0).collect();
    let n = bits.len();
    if target_size > n {
        return None;
    }

    // Convert current to index combination
    let mut indices: Vec<usize> = Vec::new();
    for (pos, &bit) in bits.iter().enumerate() {
        if current & (1u64 << bit) != 0 {
            indices.push(pos);
        }
    }

    // Find next combination
    if !next_combination(&mut indices, n) {
        return None;
    }

    let mut result = 0u64;
    for &idx in &indices {
        result |= 1u64 << bits[idx];
    }
    Some(result)
}

/// Advance a combination to the next in lexicographic order.
/// Returns false if no more combinations exist.
fn next_combination(indices: &mut [usize], n: usize) -> bool {
    let k = indices.len();
    if k == 0 {
        return false;
    }
    // Find rightmost element that can be incremented
    let mut i = k;
    loop {
        if i == 0 {
            return false;
        }
        i -= 1;
        if indices[i] < n - k + i {
            indices[i] += 1;
            for j in (i + 1)..k {
                indices[j] = indices[j - 1] + 1;
            }
            return true;
        }
    }
}

// ── Unit tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::optimizer::logical_plan::PlanSchema;
    use crate::sql::optimizer::physical_plan::{PhysicalNode, PhysicalPlan};
    use crate::sql::optimizer::physical_planner::PhysicalPlanner;
    use crate::sql::optimizer::physical_planner::PlanningContext;
    use crate::sql::optimizer::statistics::{ColumnStatistics, TableStatistics};
    use std::sync::Arc;

    // ── Test helpers ─────────────────────────────────────────

    fn col_ref(index: usize, name: &str) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: index,
                column_name: name.to_string(),
            },
            data_type: DataType::Int64,
        }
    }

    fn correlated_col_ref(index: usize, name: &str) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 1,
                column_index: index,
                column_name: name.to_string(),
            },
            data_type: DataType::Int64,
        }
    }

    fn const_int(v: i64) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(crate::types::Value::Int64(v)),
            data_type: DataType::Int64,
        }
    }

    fn eq_expr(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::Eq,
                right: Box::new(right),
            },
            data_type: DataType::Boolean,
        }
    }

    fn gt_expr(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::Gt,
                right: Box::new(right),
            },
            data_type: DataType::Boolean,
        }
    }

    fn and_expr_helper(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        super::super::rewrite::and_expr(left, right)
    }

    fn make_scan(name: &str, alias: Option<&str>, cols: Vec<(&str, DataType)>) -> LogicalPlan {
        let columns: Vec<(String, DataType)> = cols
            .into_iter()
            .map(|(n, dt)| (n.to_string(), dt))
            .collect();
        LogicalPlan::scan(
            name.to_string(),
            alias.map(|s| s.to_string()),
            PlanSchema::from_columns(columns),
        )
    }

    fn make_inner_join(left: LogicalPlan, right: LogicalPlan) -> LogicalPlan {
        let mut combined = left.schema.columns.clone();
        combined.extend(right.schema.columns.clone());
        LogicalPlan {
            node: LogicalNode::Join {
                left: Box::new(left),
                right: Box::new(right),
                join_type: JoinType::Inner,
                condition: JoinCondition::None,
            },
            schema: PlanSchema::from_columns(combined),
        }
    }

    fn make_inner_join_on(left: LogicalPlan, right: LogicalPlan, on: TypedExpr) -> LogicalPlan {
        let mut combined = left.schema.columns.clone();
        combined.extend(right.schema.columns.clone());
        LogicalPlan {
            node: LogicalNode::Join {
                left: Box::new(left),
                right: Box::new(right),
                join_type: JoinType::Inner,
                condition: JoinCondition::On(on),
            },
            schema: PlanSchema::from_columns(combined),
        }
    }

    fn make_left_join_on(left: LogicalPlan, right: LogicalPlan, on: TypedExpr) -> LogicalPlan {
        let mut combined = left.schema.columns.clone();
        combined.extend(right.schema.columns.clone());
        LogicalPlan {
            node: LogicalNode::Join {
                left: Box::new(left),
                right: Box::new(right),
                join_type: JoinType::Left,
                condition: JoinCondition::On(on),
            },
            schema: PlanSchema::from_columns(combined),
        }
    }

    fn make_col_stats(null_fraction: f64, n_distinct: f64) -> ColumnStatistics {
        ColumnStatistics {
            null_fraction,
            n_distinct,
            avg_width: 4,
            most_common_vals: vec![],
            most_common_freqs: vec![],
            histogram_bounds: vec![],
            correlation: 0.0,
        }
    }

    fn make_table_stats(
        row_count: usize,
        columns: HashMap<String, ColumnStatistics>,
    ) -> Arc<TableStatistics> {
        Arc::new(TableStatistics {
            table_id: 1,
            row_count,
            last_analyzed: 1000,
            columns,
        })
    }

    fn ctx_with_stats(entries: Vec<(&str, usize, Vec<(&str, f64)>)>) -> PlanningContext {
        let mut ctx = PlanningContext::empty();
        for (name, rows, cols) in entries {
            let mut col_stats = HashMap::new();
            for (col_name, ndv) in cols {
                col_stats.insert(col_name.to_string(), make_col_stats(0.0, ndv));
            }
            ctx.table_stats
                .insert(name.to_string(), make_table_stats(rows, col_stats));
        }
        ctx
    }

    /// Extract the scan table_name from a plan, descending through Filter.
    fn extract_scan_name(plan: &LogicalPlan) -> Option<&str> {
        match &plan.node {
            LogicalNode::Scan { table_name, .. } => Some(table_name.as_str()),
            LogicalNode::Filter { input, .. } => extract_scan_name(input),
            _ => None,
        }
    }

    /// Collect all scan table names in DFS order.
    fn collect_scan_names(plan: &LogicalPlan) -> Vec<String> {
        let mut names = Vec::new();
        collect_scan_names_inner(plan, &mut names);
        names
    }

    fn collect_scan_names_inner(plan: &LogicalPlan, names: &mut Vec<String>) {
        match &plan.node {
            LogicalNode::Scan { table_name, .. } => names.push(table_name.clone()),
            LogicalNode::Filter { input, .. }
            | LogicalNode::Project { input, .. }
            | LogicalNode::Sort { input, .. }
            | LogicalNode::Limit { input, .. }
            | LogicalNode::Distinct { input }
            | LogicalNode::DistinctOn { input, .. }
            | LogicalNode::Window { input, .. }
            | LogicalNode::Aggregate { input, .. } => collect_scan_names_inner(input, names),
            LogicalNode::Join { left, right, .. }
            | LogicalNode::SetOperation { left, right, .. } => {
                collect_scan_names_inner(left, names);
                collect_scan_names_inner(right, names);
            }
            LogicalNode::Subquery { subplan, .. } => collect_scan_names_inner(subplan, names),
            _ => {}
        }
    }

    // ── Test 1: Index lifting correctness ────────────────────

    #[test]
    fn test_lift_typed_expr() {
        let expr = col_ref(0, "a");
        let lifted = lift_typed_expr(&expr, 3);
        if let TypedExprKind::ColumnRef { column_index, .. } = &lifted.kind {
            assert_eq!(*column_index, 3, "lifting by 3 should produce index 3");
        } else {
            panic!("expected ColumnRef");
        }
    }

    #[test]
    fn test_lift_preserves_correlated() {
        let expr = correlated_col_ref(0, "outer");
        let lifted = lift_typed_expr(&expr, 5);
        if let TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            ..
        } = &lifted.kind
        {
            assert_eq!(*scope_depth, 1, "scope_depth unchanged");
            assert_eq!(*column_index, 0, "correlated ref unchanged");
        }
    }

    #[test]
    fn test_lift_compound_expr() {
        // eq(col(0), col(1)) lifted by 2 → eq(col(2), col(3))
        let expr = eq_expr(col_ref(0, "a"), col_ref(1, "b"));
        let lifted = lift_typed_expr(&expr, 2);
        let indices = collect_column_indices(&lifted);
        assert!(indices.contains(&2) && indices.contains(&3));
        assert!(!indices.contains(&0) && !indices.contains(&1));
    }

    // ── Test 2: Subquery safety gate ─────────────────────────

    #[test]
    fn test_subquery_safety_gate() {
        let a = make_scan("a", None, vec![("id", DataType::Int64)]);
        let b = make_scan("b", None, vec![("id", DataType::Int64)]);
        let join = make_inner_join(a, b);

        // Create an InSubquery expression in the ON condition
        let subquery_pred = TypedExpr {
            kind: TypedExprKind::InSubquery {
                expr: Box::new(col_ref(0, "id")),
                subquery: Box::new(crate::sql::analyzer::types::AnalyzedQuery {
                    ctes: vec![],
                    body: crate::sql::analyzer::types::AnalyzedQueryBody::Values(vec![]),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![],
                }),
                negated: false,
            },
            data_type: DataType::Boolean,
        };

        let plan = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: subquery_pred,
                input: Box::new(join),
            },
            schema: PlanSchema::from_columns(vec![
                ("id".to_string(), DataType::Int64),
                ("id".to_string(), DataType::Int64),
            ]),
        };

        assert!(logical_has_unresolved_subquery(&plan));

        // Should skip reordering (no crash, returns plan unchanged in structure)
        let ctx = PlanningContext::empty();
        let result = reorder_joins(plan, &ctx);
        // Just verify it doesn't crash and preserves structure
        assert!(matches!(result.node, LogicalNode::Filter { .. }));
    }

    // ── Test 3: Correlated RHS barrier ───────────────────────

    #[test]
    fn test_correlated_rhs_barrier() {
        // Build a right subtree with a correlated ref
        let left = make_scan("a", None, vec![("id", DataType::Int64)]);
        let right_inner = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: eq_expr(col_ref(0, "id"), correlated_col_ref(0, "outer_id")),
                input: Box::new(make_scan("b", None, vec![("id", DataType::Int64)])),
            },
            schema: PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        };

        assert!(logical_has_correlated_refs(&right_inner));

        // When flattening, correlated RHS should cause entire join to be opaque
        let join = make_inner_join_on(
            left,
            right_inner,
            eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id")),
        );
        let mut rels = Vec::new();
        let mut preds = Vec::new();
        flatten_recursive(join, &mut rels, &mut preds, 0);

        // Whole join is opaque base relation — 1 rel, no extracted predicates
        assert_eq!(rels.len(), 1, "correlated RHS makes whole join opaque");
        assert!(preds.is_empty(), "no preds extracted from opaque join");
    }

    /// Regression: correlated RHS through full reorder_joins must not stack-overflow.
    /// The inner join is "flattenable" by type but the correlated RHS prevents
    /// actual flattening → 1 opaque rel. The fix ensures we don't re-enter
    /// reorder_recursive on the same node.
    #[test]
    fn test_correlated_rhs_no_infinite_recursion() {
        let left = make_scan("a", None, vec![("id", DataType::Int64)]);
        let right_inner = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: eq_expr(col_ref(0, "id"), correlated_col_ref(0, "outer_id")),
                input: Box::new(make_scan("b", None, vec![("id", DataType::Int64)])),
            },
            schema: PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
        };
        let join = make_inner_join_on(
            left,
            right_inner,
            eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id")),
        );

        let ctx = PlanningContext::empty();
        // This must terminate (previously caused infinite recursion / stack overflow)
        let result = reorder_joins(join, &ctx);
        // Structure preserved: still a Join at top
        assert!(matches!(result.node, LogicalNode::Join { .. }));
    }

    // ── Test 4: Conjunction splitting ────────────────────────

    #[test]
    fn test_conjunction_splitting() {
        let a = make_scan("a", None, vec![("id", DataType::Int64)]);
        let b = make_scan("b", None, vec![("id", DataType::Int64)]);
        let c = make_scan("c", None, vec![("id", DataType::Int64)]);

        // ON (a.id = b.id AND b.id > 5)
        let mixed_on = and_expr_helper(
            eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id")),
            gt_expr(col_ref(1, "b_id"), const_int(5)),
        );

        let ab = make_inner_join_on(a, b, mixed_on);
        let abc = make_inner_join_on(ab, c, eq_expr(col_ref(1, "b_id"), col_ref(2, "c_id")));

        let mut rels = Vec::new();
        let mut preds = Vec::new();
        flatten_recursive(abc, &mut rels, &mut preds, 0);

        assert_eq!(rels.len(), 3, "should flatten to 3 base relations");
        // Flattening collects raw ON exprs (not split yet):
        // - mixed_on (AND of 2 conjuncts) as 1 raw pred
        // - outer ON as 1 raw pred
        assert_eq!(preds.len(), 2, "should have 2 raw ON predicates");

        // After split_conjunction, should expand to 3 conjuncts
        let all_conjuncts: Vec<TypedExpr> = preds.into_iter().flat_map(split_conjunction).collect();
        assert_eq!(
            all_conjuncts.len(),
            3,
            "should have 3 conjuncts after splitting"
        );
    }

    // ── Test 5: Pure equi detection ─────────────────────────

    #[test]
    fn test_pure_equi_detection() {
        let rels = vec![
            BaseRelation {
                id: 0,
                plan: make_scan("a", None, vec![("id", DataType::Int64)]),
                col_offset: 0,
                width: 1,
            },
            BaseRelation {
                id: 1,
                plan: make_scan("b", None, vec![("id", DataType::Int64)]),
                col_offset: 1,
                width: 1,
            },
        ];

        // Bare Var = Var across rels → pure equi
        let equi = eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id"));
        assert!(is_pure_equi_join_pred(&equi, &rels));

        // Var = Const → not equi (single-table filter)
        let not_equi = eq_expr(col_ref(0, "a_id"), const_int(42));
        assert!(!is_pure_equi_join_pred(&not_equi, &rels));

        // Var > Var → not equi (wrong op)
        let gt = gt_expr(col_ref(0, "a_id"), col_ref(1, "b_id"));
        assert!(!is_pure_equi_join_pred(&gt, &rels));

        // Same-table Var = Var → not equi
        let same_table = eq_expr(col_ref(0, "a_id"), col_ref(0, "a_id2"));
        assert!(!is_pure_equi_join_pred(&same_table, &rels));

        // f(Var) = Var → not equi (not bare ColumnRef)
        let cast_expr = TypedExpr {
            kind: TypedExprKind::Cast {
                expr: Box::new(col_ref(0, "a_id")),
                target_type: DataType::Text,
                cast_context: crate::sql::types::CastContext::Explicit,
            },
            data_type: DataType::Text,
        };
        let cast_eq = eq_expr(cast_expr, col_ref(1, "b_id"));
        assert!(!is_pure_equi_join_pred(&cast_eq, &rels));
    }

    // ── Test 6: 1-rel conjunct attachment ────────────────────

    #[test]
    fn test_single_rel_attachment() {
        let rels = vec![
            BaseRelation {
                id: 0,
                plan: make_scan("a", None, vec![("id", DataType::Int64)]),
                col_offset: 0,
                width: 1,
            },
            BaseRelation {
                id: 1,
                plan: make_scan("b", None, vec![("id", DataType::Int64)]),
                col_offset: 1,
                width: 1,
            },
        ];

        let conjuncts = vec![
            eq_expr(col_ref(0, "a_id"), const_int(1)),       // 1-rel: a
            eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id")), // 2-rel: edge
            gt_expr(col_ref(1, "b_id"), const_int(5)),       // 1-rel: b
        ];

        let classification = classify_predicates(&conjuncts, &rels);

        assert_eq!(classification.edges.len(), 1, "should have 1 edge");
        assert_eq!(
            classification
                .base_local_filters
                .get(&0)
                .map(|v| v.len())
                .unwrap_or(0),
            1,
            "rel 0 should have 1 local filter"
        );
        assert_eq!(
            classification
                .base_local_filters
                .get(&1)
                .map(|v| v.len())
                .unwrap_or(0),
            1,
            "rel 1 should have 1 local filter"
        );
        assert!(
            classification.remaining.is_empty(),
            "no remaining predicates"
        );
    }

    // ── Test 7: DPccp optimal order ─────────────────────────

    #[test]
    fn test_dpccp_optimal_order() {
        // 3 tables: small (10), medium (1000), big (10000)
        // Chain join: big.id = medium.id AND medium.id = small.id
        // big↔medium and medium↔small are connected; big↔small have NO direct edge.
        //
        // Input order (suboptimal): (big CROSS medium) JOIN small
        //   All predicates on the outer ON clause.
        // Optimal: (medium ⋈ small) ⋈ big — cost 0.3
        //   step 1: medium(1000) ⋈ small(10), sel=1/1000, rows=10, cost=0.1
        //   step 2: result(10) ⋈ big(10000), sel=1/5000, rows=20, cost=0.3
        // Suboptimal: (big ⋈ medium) ⋈ small — cost 20.2
        //   step 1: big(10000) ⋈ medium(1000), sel=1/5000, rows=2000, cost=20.0
        //   step 2: result(2000) ⋈ small(10), sel=1/1000, rows=20, cost=20.2
        //
        // The key: big_t should NOT appear in the first (deepest) join.
        let big = make_scan("big_t", None, vec![("id", DataType::Int64)]);
        let medium = make_scan("medium_t", None, vec![("id", DataType::Int64)]);
        let small = make_scan("small_t", None, vec![("id", DataType::Int64)]);

        // big(0) JOIN medium(1) — cross join (no predicate)
        let bm = make_inner_join(big, medium);
        // (big, medium)(0,1) JOIN small(2) ON big.id=medium.id AND medium.id=small.id
        let plan = make_inner_join_on(
            bm,
            small,
            and_expr_helper(
                eq_expr(col_ref(0, "big_id"), col_ref(1, "med_id")),
                eq_expr(col_ref(1, "med_id"), col_ref(2, "small_id")),
            ),
        );

        let ctx = ctx_with_stats(vec![
            ("small_t", 10, vec![("id", 10.0)]),
            ("big_t", 10000, vec![("id", 5000.0)]),
            ("medium_t", 1000, vec![("id", 1000.0)]),
        ]);

        let result = reorder_joins(plan, &ctx);

        let names = collect_scan_names(&result);
        assert_eq!(names.len(), 3, "should have 3 scans");

        // Find the deepest join's direct children.
        fn deepest_join_children(plan: &LogicalPlan) -> Option<(Vec<String>, Vec<String>)> {
            match &plan.node {
                LogicalNode::Join { left, right, .. } => {
                    if let Some(deeper) = deepest_join_children(left) {
                        return Some(deeper);
                    }
                    if let Some(deeper) = deepest_join_children(right) {
                        return Some(deeper);
                    }
                    Some((collect_scan_names(left), collect_scan_names(right)))
                }
                LogicalNode::Filter { input, .. } | LogicalNode::Project { input, .. } => {
                    deepest_join_children(input)
                }
                _ => None,
            }
        }

        let (left_names, right_names) =
            deepest_join_children(&result).expect("should find a join node");
        let first_join_tables: Vec<&str> = left_names
            .iter()
            .chain(right_names.iter())
            .map(|s| s.as_str())
            .collect();

        // The first (deepest) join should pair the two smaller tables,
        // NOT include big_t.
        assert!(
            !first_join_tables.contains(&"big_t"),
            "first join should not involve big_t (10000 rows), got {:?}",
            first_join_tables
        );
    }

    // ── Test 8: Disconnected graph stitching ─────────────────

    #[test]
    fn test_disconnected_graph_stitching() {
        // Two disconnected pairs: (a JOIN b) CROSS JOIN (c JOIN d)
        let a = make_scan("a", None, vec![("id", DataType::Int64)]);
        let b = make_scan("b", None, vec![("id", DataType::Int64)]);
        let c = make_scan("c", None, vec![("id", DataType::Int64)]);
        let d = make_scan("d", None, vec![("id", DataType::Int64)]);

        let ab = make_inner_join_on(a, b, eq_expr(col_ref(0, "id"), col_ref(1, "id")));
        let cd = make_inner_join_on(c, d, eq_expr(col_ref(0, "id"), col_ref(1, "id")));
        let plan = make_inner_join(ab, cd);

        let ctx = ctx_with_stats(vec![
            ("a", 100, vec![("id", 100.0)]),
            ("b", 100, vec![("id", 100.0)]),
            ("c", 50, vec![("id", 50.0)]),
            ("d", 50, vec![("id", 50.0)]),
        ]);

        let result = reorder_joins(plan, &ctx);
        let names = collect_scan_names(&result);
        assert_eq!(names.len(), 4, "should have 4 scans");
    }

    // ── Test 9: Column remap correctness ─────────────────────

    #[test]
    fn test_column_remap_3way() {
        // FROM a(id), b(id), c(id) WHERE a.id = c.id AND b.id = c.id
        // Original order: a(0), b(1), c(2)
        // If reordered to a, c, b — output schema must still match a, b, c
        let a = make_scan("a", None, vec![("a_id", DataType::Int64)]);
        let b = make_scan("b", None, vec![("b_id", DataType::Int64)]);
        let c = make_scan("c", None, vec![("c_id", DataType::Int64)]);

        let ab = make_inner_join(a, b);
        let abc = make_inner_join_on(
            ab,
            c,
            and_expr_helper(
                eq_expr(col_ref(0, "a_id"), col_ref(2, "c_id")),
                eq_expr(col_ref(1, "b_id"), col_ref(2, "c_id")),
            ),
        );

        let ctx = ctx_with_stats(vec![
            ("a", 100, vec![("a_id", 100.0)]),
            ("b", 1000, vec![("b_id", 1000.0)]),
            ("c", 10, vec![("c_id", 10.0)]),
        ]);

        let result = reorder_joins(abc, &ctx);
        // Output schema should have 3 columns
        assert_eq!(
            result.schema.columns.len(),
            3,
            "output should have 3 columns"
        );
    }

    // ── Test 10: Self-join alias stats ───────────────────────

    #[test]
    fn test_self_join_alias_stats() {
        let a = make_scan("t", Some("a"), vec![("id", DataType::Int64)]);
        let b = make_scan("t", Some("b"), vec![("id", DataType::Int64)]);
        let plan = make_inner_join_on(a, b, eq_expr(col_ref(0, "id"), col_ref(1, "id")));

        let mut ctx = PlanningContext::empty();
        let mut cols = HashMap::new();
        cols.insert("id".to_string(), make_col_stats(0.0, 100.0));
        // Both aliases should have distinct keys
        ctx.table_stats.insert(
            super::super::schema_map_key("t", Some("a")),
            make_table_stats(1000, cols.clone()),
        );
        ctx.table_stats.insert(
            super::super::schema_map_key("t", Some("b")),
            make_table_stats(500, cols),
        );

        let result = reorder_joins(plan, &ctx);
        assert_eq!(result.schema.columns.len(), 2);
    }

    // ── Test 11: No-op cases ─────────────────────────────────

    #[test]
    fn test_noop_single_table() {
        let a = make_scan("a", None, vec![("id", DataType::Int64)]);
        let ctx = PlanningContext::empty();
        let result = reorder_joins(a.clone(), &ctx);
        assert!(matches!(result.node, LogicalNode::Scan { .. }));
    }

    #[test]
    fn test_noop_two_tables_already_optimal() {
        let a = make_scan("a", None, vec![("id", DataType::Int64)]);
        let b = make_scan("b", None, vec![("id", DataType::Int64)]);
        let plan = make_inner_join_on(a, b, eq_expr(col_ref(0, "id"), col_ref(1, "id")));

        // Both same size — order shouldn't change
        let ctx = ctx_with_stats(vec![
            ("a", 1000, vec![("id", 1000.0)]),
            ("b", 1000, vec![("id", 1000.0)]),
        ]);

        let result = reorder_joins(plan, &ctx);
        assert_eq!(result.schema.columns.len(), 2);
    }

    // ── Test 12: Physical plan build-side assertion ──────────

    #[test]
    fn test_physical_plan_build_side() {
        // Ensure that after reordering, HashJoin picks smaller side as build.
        // Input: big(10000) on left, small(10) on right — suboptimal.
        // After reorder, small should move to left and be the build side.
        let small = make_scan("small_t", None, vec![("id", DataType::Int64)]);
        let big = make_scan("big_t", None, vec![("id", DataType::Int64)]);

        // Intentionally put big on left
        let plan = make_inner_join_on(big, small, eq_expr(col_ref(0, "id"), col_ref(1, "id")));

        let ctx = ctx_with_stats(vec![
            ("small_t", 10, vec![("id", 10.0)]),
            ("big_t", 10000, vec![("id", 10000.0)]),
        ]);

        let reordered = reorder_joins(plan, &ctx);
        let physical = PhysicalPlanner::plan(&reordered, &ctx);

        // Walk the physical plan to find the HashJoin
        fn find_hash_join(plan: &PhysicalPlan) -> Option<bool> {
            match &plan.node {
                PhysicalNode::HashJoin { left_is_build, .. } => Some(*left_is_build),
                PhysicalNode::Filter { input, .. }
                | PhysicalNode::Project { input, .. }
                | PhysicalNode::Sort { input, .. }
                | PhysicalNode::Limit { input, .. } => find_hash_join(input),
                _ => None,
            }
        }

        let left_is_build = find_hash_join(&physical).expect("expected HashJoin in physical plan");
        // Physical planner sets left_is_build = (left_rows <= right_rows).
        // With 2 tables, DPccp keeps the enumeration order (big on left,
        // small on right). The physical planner independently picks the
        // smaller side as build: left_is_build = (10000 <= 10) = false,
        // meaning right/small is the build side — correct behavior.
        assert!(
            !left_is_build,
            "big(10000) on left, small(10) on right: left_is_build should be false \
             (right/small is build side); got left_is_build=true"
        );
    }

    // ── Test: Outer join unchanged ───────────────────────────

    #[test]
    fn test_outer_join_unchanged() {
        let a = make_scan("a", None, vec![("id", DataType::Int64)]);
        let b = make_scan("b", None, vec![("id", DataType::Int64)]);
        let plan = make_left_join_on(a, b, eq_expr(col_ref(0, "id"), col_ref(1, "id")));

        let ctx = PlanningContext::empty();
        let result = reorder_joins(plan, &ctx);

        // LEFT JOIN should not be flattened or reordered
        match &result.node {
            LogicalNode::Join { join_type, .. } => {
                assert_eq!(*join_type, JoinType::Left, "LEFT join preserved");
            }
            _ => panic!("expected Join node"),
        }
    }

    // ── Test: n>=64 preserves original join structure ─────────

    #[test]
    fn test_large_join_group_preserves_on_conditions() {
        // Build a chain of 65 inner joins with ON conditions.
        // count_flattenable_relations should detect >= 64 and skip reordering,
        // preserving every JoinCondition::On (no degradation to cross join).
        let mut plan = make_scan("t0", None, vec![("id", DataType::Int64)]);
        for i in 1..65 {
            let right = make_scan(&format!("t{}", i), None, vec![("id", DataType::Int64)]);
            // ON condition referencing left col 0 and right col (width of left)
            let left_width = plan.schema.columns.len();
            let on = eq_expr(col_ref(0, "id"), col_ref(left_width, "id"));
            plan = make_inner_join_on(plan, right, on);
        }

        // Verify the pre-check counts correctly
        assert_eq!(
            count_flattenable_relations(&plan),
            65,
            "should detect 65 flattenable relations"
        );

        let ctx = PlanningContext::empty();
        let result = reorder_joins(plan, &ctx);

        // Walk the result tree and verify no JoinCondition::None exists
        // at the top level (the n>=64 group should be intact).
        fn count_on_conditions(plan: &LogicalPlan) -> usize {
            match &plan.node {
                LogicalNode::Join {
                    left,
                    right,
                    condition,
                    ..
                } => {
                    let here = if matches!(condition, JoinCondition::On(..)) {
                        1
                    } else {
                        0
                    };
                    here + count_on_conditions(left) + count_on_conditions(right)
                }
                LogicalNode::Filter { input, .. } | LogicalNode::Project { input, .. } => {
                    count_on_conditions(input)
                }
                _ => 0,
            }
        }

        // All 64 ON conditions must be preserved (not degraded to None)
        assert_eq!(
            count_on_conditions(&result),
            64,
            "all 64 ON conditions must be preserved, not degraded to cross joins"
        );
    }

    // ── Test: Values with unresolved subquery triggers safety gate ──

    #[test]
    fn test_values_unresolved_subquery_safety_gate() {
        // A Values node containing an InSubquery expression should be
        // detected by logical_has_unresolved_subquery.
        let subquery_expr = TypedExpr {
            kind: TypedExprKind::InSubquery {
                expr: Box::new(col_ref(0, "id")),
                subquery: Box::new(crate::sql::analyzer::types::AnalyzedQuery {
                    ctes: vec![],
                    body: crate::sql::analyzer::types::AnalyzedQueryBody::Values(vec![]),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![],
                }),
                negated: false,
            },
            data_type: DataType::Boolean,
        };

        let values_plan = LogicalPlan {
            node: LogicalNode::Values {
                rows: vec![vec![subquery_expr]],
            },
            schema: PlanSchema::from_columns(vec![("v".to_string(), DataType::Boolean)]),
        };

        assert!(
            logical_has_unresolved_subquery(&values_plan),
            "Values with InSubquery must be detected"
        );
    }

    // ── Test: TableFunction with unresolved subquery triggers safety gate ──

    #[test]
    fn test_table_function_unresolved_subquery_safety_gate() {
        let subquery_expr = TypedExpr {
            kind: TypedExprKind::InSubquery {
                expr: Box::new(col_ref(0, "id")),
                subquery: Box::new(crate::sql::analyzer::types::AnalyzedQuery {
                    ctes: vec![],
                    body: crate::sql::analyzer::types::AnalyzedQueryBody::Values(vec![]),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![],
                }),
                negated: false,
            },
            data_type: DataType::Boolean,
        };

        let tf_plan = LogicalPlan {
            node: LogicalNode::TableFunction {
                function_name: "generate_series".to_string(),
                args: vec![crate::sql::analyzer::types::TypedFunctionArg::Positional(
                    subquery_expr,
                )],
                alias: None,
            },
            schema: PlanSchema::from_columns(vec![("v".to_string(), DataType::Int64)]),
        };

        assert!(
            logical_has_unresolved_subquery(&tf_plan),
            "TableFunction with InSubquery arg must be detected"
        );
    }

    // ── Test: Greedy disconnected stitching merges smallest-first ───

    #[test]
    fn test_greedy_disconnected_smallest_first() {
        // Build > 8 relations (to force greedy path) with two disconnected
        // groups. Verify the smallest component appears deepest (leftmost).
        //
        // Group 1: t0-t1-...-t8 (9 rels, star join centered on t0)
        // Group 2: s0 (1 rel, disconnected, tiny: 5 rows)
        // t0..t8 each have 100 rows.
        // Smallest-first stitching → s0 on the left of the cross join.
        let mut plan = make_scan("t0", None, vec![("id", DataType::Int64)]);
        for i in 1..9 {
            let right = make_scan(&format!("t{}", i), None, vec![("id", DataType::Int64)]);
            // Star: every table joins to t0.id (root col 0) via its own col
            let left_width = plan.schema.columns.len();
            let on = eq_expr(col_ref(0, "t0_id"), col_ref(left_width, "ti_id"));
            plan = make_inner_join_on(plan, right, on);
        }
        // Add disconnected s0 via cross join
        let s0 = make_scan("s0", None, vec![("id", DataType::Int64)]);
        plan = make_inner_join(plan, s0);

        let ctx = ctx_with_stats(vec![
            ("s0", 5, vec![("id", 5.0)]),
            ("t0", 100, vec![("id", 100.0)]),
            ("t1", 100, vec![("id", 100.0)]),
            ("t2", 100, vec![("id", 100.0)]),
            ("t3", 100, vec![("id", 100.0)]),
            ("t4", 100, vec![("id", 100.0)]),
            ("t5", 100, vec![("id", 100.0)]),
            ("t6", 100, vec![("id", 100.0)]),
            ("t7", 100, vec![("id", 100.0)]),
            ("t8", 100, vec![("id", 100.0)]),
        ]);

        let result = reorder_joins(plan, &ctx);
        let names = collect_scan_names(&result);
        assert_eq!(names.len(), 10, "should have 10 scans");

        // The smallest component (s0, 5 rows) should appear first in DFS order
        // because smallest-first stitching puts it on the left of the cross join.
        assert_eq!(
            names[0], "s0",
            "smallest component (s0) should be leftmost after smallest-first stitching, got {:?}",
            names
        );
    }
}
