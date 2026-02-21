//! Row estimation, NDV lookup, and join cardinality cost model.
//!
//! Mirrors the physical planner's estimation logic for use during
//! join reordering.

use crate::sql::analyzer::types::{JoinCondition, JoinType, TypedExpr, TypedExprKind};

use super::super::logical_plan::{LogicalNode, LogicalPlan};
use super::super::physical_planner::PlanningContext;
use super::super::selectivity;
use super::super::statistics::TableStatistics;
use super::predicates::BaseRelation;

pub(super) const DEFAULT_ESTIMATED_ROWS: usize = 1000;
pub(super) const DEFAULT_JOIN_SEL: f64 = 0.1;

// ── Subtree row estimation ──────────────────────────────────────

/// Estimate base row count from a LogicalPlan subtree.
///
/// Mirrors physical_planner.rs row estimation logic.
pub(super) fn estimate_subtree_rows(plan: &LogicalPlan, ctx: &PlanningContext) -> usize {
    match &plan.node {
        LogicalNode::Scan { table_name, alias } => {
            let key = super::super::schema_map_key(table_name, alias.as_deref());
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
pub(super) fn resolve_stats_logical<'a>(
    plan: &LogicalPlan,
    ctx: &'a PlanningContext,
) -> Option<&'a TableStatistics> {
    match &plan.node {
        LogicalNode::Scan { table_name, alias } => {
            let key = super::super::schema_map_key(table_name, alias.as_deref());
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
        super::super::join_keys::try_extract_equi_keys(condition, left_width)
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

// ── Candidate row estimation ────────────────────────────────────

/// Estimate output rows for a candidate join based on raw (root-level) equi predicates.
pub(super) fn estimate_candidate_rows(
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
