//! Node-building helpers for the logical planner.
//!
//! Contains functions for constructing specific logical node types:
//! aggregate helpers, window function extraction, FROM/JOIN builders,
//! and aggregate-detection utilities.

use super::super::logical_plan::{LogicalNode, LogicalPlan, PlanSchema};
use super::super::window_rewrite::collect_window_calls_from_expr;
use super::LogicalPlanner;
use crate::sql::analyzer::types::{
    AnalyzedProjection, AnalyzedTableRef, AnalyzedTableRefKind, TypedExpr, TypedExprKind,
    TypedOrderByExpr,
};
use crate::sql::operators::AggregateExpr;
use anyhow::Result;

// ── Aggregate helpers ──────────────────────────────────────────────

/// Collect unique `AggregateExpr` from projection list (for rewriting).
pub(super) fn collect_aggregate_exprs(projections: &[AnalyzedProjection]) -> Vec<AggregateExpr> {
    let mut agg_exprs = Vec::new();
    let mut agg_names = Vec::new();
    let mut agg_types = Vec::new();
    for proj in projections {
        super::super::build::collect_agg_exprs_from(
            &proj.expr,
            &proj.output_name,
            &mut agg_exprs,
            &mut agg_names,
            &mut agg_types,
        );
    }
    agg_exprs
}

/// Build a "raw" aggregate projection for the aggregate + window path.
///
/// Returns `[group_by_col_0, ..., agg_call_0, agg_call_1, ...]` — each
/// item is either a group-by ColumnRef or a bare AggregateCall.  This
/// ensures:
/// - ALL aggregate functions are captured (including those inside window
///   expressions like `ROW_NUMBER() OVER (ORDER BY COUNT(*))`)
/// - `build_aggregate_operator` outputs clean group-by + aggregate columns
///   without adding a post-projection that can't evaluate WindowCall nodes
/// - The Window node and final Project handle the full expression mapping
pub(super) fn build_raw_aggregate_projection(
    group_by: &[TypedExpr],
    aggregate_exprs: &[AggregateExpr],
    full_projection: &[AnalyzedProjection],
) -> Vec<AnalyzedProjection> {
    let mut raw = Vec::new();

    // Group-by columns.
    for (i, gb) in group_by.iter().enumerate() {
        let name = match &gb.kind {
            TypedExprKind::ColumnRef { column_name, .. } => column_name.clone(),
            _ => format!("group_by_{}", i),
        };
        raw.push(AnalyzedProjection {
            expr: gb.clone(),
            output_name: name,
        });
    }

    // Bare aggregate calls (already deduplicated by collect_aggregate_exprs).
    for (i, ae) in aggregate_exprs.iter().enumerate() {
        // Find the original TypedExpr for this aggregate in the full projection
        // so we preserve the exact data type and expression structure.
        let agg_expr = find_aggregate_typed_expr(ae, full_projection);
        let name = format!("agg_{}", i);
        raw.push(AnalyzedProjection {
            expr: agg_expr,
            output_name: name,
        });
    }

    raw
}

/// Find the `TypedExpr` for a given `AggregateExpr` in the projection tree.
pub(super) fn find_aggregate_typed_expr(
    ae: &AggregateExpr,
    projection: &[AnalyzedProjection],
) -> TypedExpr {
    for proj in projection {
        if let Some(found) = find_agg_in_expr(&proj.expr, ae) {
            return found;
        }
    }
    // Fallback: reconstruct a minimal AggregateCall.
    // This shouldn't happen because collect_aggregate_exprs guarantees all
    // aggregates come from the projection, but be safe.
    TypedExpr {
        kind: TypedExprKind::AggregateCall {
            func: crate::sql::analyzer::types::ResolvedFunction {
                name: ae.func_name.clone(),
                kind: crate::sql::analyzer::types::FunctionKind::Builtin,
                return_type: ae
                    .arg
                    .as_ref()
                    .map_or(crate::model::DataType::Int64, |a| a.data_type.clone()),
            },
            args: ae.arg.iter().cloned().collect(),
            distinct: ae.distinct,
            filter: ae.filter.as_ref().map(|f| Box::new(f.clone())),
            order_by: ae.order_by.clone(),
        },
        data_type: ae
            .arg
            .as_ref()
            .map_or(crate::model::DataType::Int64, |a| a.data_type.clone()),
    }
}

/// Search an expression tree for an `AggregateCall` matching the given `AggregateExpr`.
pub(super) fn find_agg_in_expr(expr: &TypedExpr, target: &AggregateExpr) -> Option<TypedExpr> {
    match &expr.kind {
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            filter,
            order_by,
        } => {
            if super::super::build::aggregate_identity_matches(
                target, func, args, *distinct, filter, order_by,
            ) {
                return Some(expr.clone());
            }
            None
        }
        TypedExprKind::BinaryOp { left, right, .. } => {
            find_agg_in_expr(left, target).or_else(|| find_agg_in_expr(right, target))
        }
        TypedExprKind::UnaryOp { operand, .. } | TypedExprKind::Cast { expr: operand, .. } => {
            find_agg_in_expr(operand, target)
        }
        TypedExprKind::FunctionCall { args, .. } => {
            args.iter().find_map(|a| find_agg_in_expr(a, target))
        }
        TypedExprKind::WindowCall {
            args,
            partition_by,
            order_by,
            ..
        } => args
            .iter()
            .chain(partition_by.iter())
            .find_map(|a| find_agg_in_expr(a, target))
            .or_else(|| {
                order_by
                    .iter()
                    .find_map(|ob| find_agg_in_expr(&ob.expr, target))
            }),
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => operand
            .as_ref()
            .and_then(|o| find_agg_in_expr(o, target))
            .or_else(|| {
                when_clauses.iter().find_map(|(w, t)| {
                    find_agg_in_expr(w, target).or_else(|| find_agg_in_expr(t, target))
                })
            })
            .or_else(|| {
                else_result
                    .as_ref()
                    .and_then(|e| find_agg_in_expr(e, target))
            }),
        TypedExprKind::Coalesce(args) | TypedExprKind::MinMax { args, .. } => {
            args.iter().find_map(|a| find_agg_in_expr(a, target))
        }
        TypedExprKind::NullIf(a, b) => {
            find_agg_in_expr(a, target).or_else(|| find_agg_in_expr(b, target))
        }
        _ => None,
    }
}

/// Find the original `TypedExpr` for an `AggregateExpr` in HAVING/ORDER BY trees.
pub(super) fn find_aggregate_in_having_orderby(
    ae: &AggregateExpr,
    having: Option<&TypedExpr>,
    order_by: &[TypedOrderByExpr],
) -> TypedExpr {
    if let Some(h) = having {
        if let Some(found) = find_agg_in_expr(h, ae) {
            return found;
        }
    }
    for ob in order_by {
        if let Some(found) = find_agg_in_expr(&ob.expr, ae) {
            return found;
        }
    }
    // Fallback: use the reconstruction from find_aggregate_typed_expr
    find_aggregate_typed_expr(ae, &[])
}

// ── Window helpers ─────────────────────────────────────────────────

/// Extract `WindowFunctionExpr` from a list of typed expressions.
///
/// `exprs` and `projection` must be the same length — `exprs[i]` is the
/// (possibly post-aggregate-rewritten) expression and `projection[i]`
/// provides the output name.
pub(super) fn extract_window_funcs(
    exprs: &[TypedExpr],
    projection: &[AnalyzedProjection],
) -> Vec<crate::sql::operators::WindowFunctionExpr> {
    let mut result = Vec::new();
    for (i, expr) in exprs.iter().enumerate() {
        let output_name = &projection[i].output_name;
        collect_window_calls_from_expr(expr, output_name, &mut result);
    }
    result
}

// ── FROM / JOIN builders ───────────────────────────────────────────

pub(crate) fn build_from(from: &[AnalyzedTableRef]) -> Result<LogicalPlan> {
    if from.is_empty() {
        return Ok(LogicalPlan::empty(PlanSchema::from_columns(vec![])));
    }

    let mut plan = build_table_ref(&from[0])?;

    // Additional FROM items → cross joins
    for table_ref in from.iter().skip(1) {
        let right = build_table_ref(table_ref)?;
        let mut combined_cols = plan.schema.columns.clone();
        combined_cols.extend(right.schema.columns.clone());
        let schema = PlanSchema::from_columns(combined_cols);
        plan = LogicalPlan {
            node: LogicalNode::Join {
                left: Box::new(plan),
                right: Box::new(right),
                join_type: crate::sql::analyzer::types::JoinType::Cross,
                condition: crate::sql::analyzer::types::JoinCondition::None,
            },
            schema,
        };
    }

    Ok(plan)
}

fn build_table_ref(table_ref: &AnalyzedTableRef) -> Result<LogicalPlan> {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { name, schema } => {
            let plan_schema = PlanSchema::from_columns(
                schema
                    .columns
                    .iter()
                    .map(|(name, dt, _nullable)| (name.clone(), dt.clone()))
                    .collect(),
            );
            Ok(LogicalPlan::scan(
                name.clone(),
                table_ref.alias.clone(),
                plan_schema,
            ))
        }
        AnalyzedTableRefKind::Subquery(subquery) => {
            let subplan = LogicalPlanner::build(subquery)?;
            let schema = subplan.schema.clone();
            Ok(LogicalPlan {
                node: LogicalNode::Subquery {
                    subplan: Box::new(subplan),
                    alias: table_ref.alias.clone(),
                },
                schema,
            })
        }
        AnalyzedTableRefKind::Join {
            left,
            right,
            join_type,
            condition,
            left_col_start,
        } => {
            let left_plan = build_table_ref(left)?;
            let right_plan = build_table_ref(right)?;
            let mut combined_cols = left_plan.schema.columns.clone();
            combined_cols.extend(right_plan.schema.columns.clone());
            let schema = PlanSchema::from_columns(combined_cols);
            // Normalize ON condition indices from global (analyzer scope) to local
            // (relative to this join's combined schema). left_col_start is the global
            // offset where this join's left child begins.
            let normalized_condition =
                crate::sql::analyzer::types::reindex_join_condition(condition, *left_col_start);
            Ok(LogicalPlan {
                node: LogicalNode::Join {
                    left: Box::new(left_plan),
                    right: Box::new(right_plan),
                    join_type: *join_type,
                    condition: normalized_condition,
                },
                schema,
            })
        }
        AnalyzedTableRefKind::Function {
            func,
            args,
            output_columns,
        } => {
            let plan_schema = PlanSchema::from_columns(output_columns.clone());
            Ok(LogicalPlan {
                node: LogicalNode::TableFunction {
                    function_name: func.name.clone(),
                    args: args.clone(),
                    alias: table_ref.alias.clone(),
                },
                schema: plan_schema,
            })
        }
    }
}

// ── Aggregate detection ────────────────────────────────────────────

/// Check if any projection item contains an aggregate function call.
pub(super) fn has_aggregates(projections: &[AnalyzedProjection]) -> bool {
    projections.iter().any(|p| expr_has_aggregate(&p.expr))
}

pub(crate) fn expr_has_aggregate(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::AggregateCall { .. } => true,
        TypedExprKind::IsTest { expr, .. } => expr_has_aggregate(expr),
        TypedExprKind::Between {
            expr, low, high, ..
        } => expr_has_aggregate(expr) || expr_has_aggregate(low) || expr_has_aggregate(high),
        TypedExprKind::InList { expr, list, .. } => {
            expr_has_aggregate(expr) || list.iter().any(expr_has_aggregate)
        }
        TypedExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_has_aggregate(expr)
                || expr_has_aggregate(pattern)
                || escape.as_ref().is_some_and(|e| expr_has_aggregate(e))
        }
        TypedExprKind::SimilarTo {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_has_aggregate(expr)
                || expr_has_aggregate(pattern)
                || escape.as_ref().is_some_and(|e| expr_has_aggregate(e))
        }
        TypedExprKind::BinaryOp { left, right, .. } => {
            expr_has_aggregate(left) || expr_has_aggregate(right)
        }
        TypedExprKind::UnaryOp { operand, .. } => expr_has_aggregate(operand),
        TypedExprKind::Cast { expr, .. } => expr_has_aggregate(expr),
        TypedExprKind::FunctionCall { args, .. } => args.iter().any(expr_has_aggregate),
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand.as_ref().is_some_and(|e| expr_has_aggregate(e))
                || when_clauses
                    .iter()
                    .any(|(w, t)| expr_has_aggregate(w) || expr_has_aggregate(t))
                || else_result.as_ref().is_some_and(|e| expr_has_aggregate(e))
        }
        TypedExprKind::AnyAll { expr, .. } => expr_has_aggregate(expr),
        TypedExprKind::Coalesce(args) => args.iter().any(expr_has_aggregate),
        TypedExprKind::NullIf(a, b) => expr_has_aggregate(a) || expr_has_aggregate(b),
        TypedExprKind::MinMax { args, .. } => args.iter().any(expr_has_aggregate),
        TypedExprKind::ArrayLiteral(items) | TypedExprKind::Row(items) => {
            items.iter().any(expr_has_aggregate)
        }
        TypedExprKind::ArrayIndex { array, index } => {
            expr_has_aggregate(array) || expr_has_aggregate(index)
        }
        TypedExprKind::JsonAccess { expr, path, .. } => {
            expr_has_aggregate(expr) || expr_has_aggregate(path)
        }
        _ => false,
    }
}
