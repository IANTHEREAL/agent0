//! Logical planner: `AnalyzedQuery → LogicalPlan`.
//!
//! Pure structural translation — no optimization. Each SQL clause maps
//! to exactly one logical node in a fixed order.
//!
//! **Non-aggregate path** (Sort on full-width rows, then narrow):
//! ```text
//! FROM       → Scan / Join / Subquery
//! WHERE      → Filter
//! [WINDOW]   → Window        ← appends window columns (if present)
//! ORDER BY   → Sort          ← on full-width (or post-window) rows
//! SELECT     → Project       ← narrows to output columns
//! DISTINCT   → Distinct / DistinctOn
//! ```
//!
//! **Aggregate path** (rewrite ORDER BY / HAVING for post-agg schema):
//! ```text
//! FROM       → Scan / Join / Subquery
//! WHERE      → Filter
//! GROUP BY   → Aggregate
//! HAVING     → Filter (rewritten)
//! [WINDOW]   → Window        ← appends window columns (if present)
//! ORDER BY   → Sort   (rewritten)
//! SELECT     → Project       ← only when window functions present
//! DISTINCT   → Distinct / DistinctOn
//! ```
//!
//! Since the eligibility gate has been removed (single execution path),
//! all rewrite operations return `Result` so that failures are propagated
//! as errors rather than panicking.

pub(crate) mod nodes;

use super::logical_plan::{LogicalNode, LogicalPlan, PlanSchema};
use super::window_rewrite::{
    collect_window_calls_from_expr, contains_window, rewrite_for_post_window,
};
use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedProjection, AnalyzedQueryBody, AnalyzedSelect, TypedExpr,
    TypedExprKind, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use crate::types::DataType;
use anyhow::Result;

/// Builds a [`LogicalPlan`] from an [`AnalyzedQuery`].
pub struct LogicalPlanner;

impl LogicalPlanner {
    /// Build a logical plan from an analyzed query.
    ///
    /// Returns `Err` if an aggregate rewrite fails (e.g. HAVING expression
    /// references a column that is neither a GROUP BY key nor an aggregate).
    pub fn build(query: &AnalyzedQuery) -> Result<LogicalPlan> {
        let schema_no_collation: Vec<(String, DataType)> = query
            .output_schema
            .iter()
            .map(|(name, dt, _)| (name.clone(), dt.clone()))
            .collect();
        let mut plan = Self::build_body(&query.body, &schema_no_collation, &query.order_by)?;

        // LIMIT / OFFSET
        if query.limit.is_some() || query.offset.is_some() {
            plan = plan.limit(query.limit.clone(), query.offset.clone());
        }

        Ok(plan)
    }

    fn build_body(
        body: &AnalyzedQueryBody,
        output_schema: &[(String, DataType)],
        order_by: &[TypedOrderByExpr],
    ) -> Result<LogicalPlan> {
        match body {
            AnalyzedQueryBody::Select(select) => {
                Self::build_select(select, output_schema, order_by)
            }
            AnalyzedQueryBody::Values(rows) => {
                let schema = PlanSchema::from_columns(output_schema.to_vec());
                let mut plan = LogicalPlan {
                    node: LogicalNode::Values { rows: rows.clone() },
                    schema,
                };
                // For Values / SetOperation, Sort stays on top (original behavior).
                if !order_by.is_empty() {
                    plan = plan.sort(order_by.to_vec());
                }
                Ok(plan)
            }
            AnalyzedQueryBody::SetOperation {
                op,
                all,
                left,
                right,
            } => {
                let left_plan = Self::build(left)?;
                let right_plan = Self::build(right)?;
                let schema = PlanSchema::from_columns(output_schema.to_vec());
                let mut plan = LogicalPlan {
                    node: LogicalNode::SetOperation {
                        op: *op,
                        all: *all,
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                    },
                    schema,
                };
                // For Values / SetOperation, Sort stays on top (original behavior).
                if !order_by.is_empty() {
                    plan = plan.sort(order_by.to_vec());
                }
                Ok(plan)
            }
        }
    }

    fn build_select(
        select: &AnalyzedSelect,
        output_schema: &[(String, DataType)],
        order_by: &[TypedOrderByExpr],
    ) -> Result<LogicalPlan> {
        // 1. FROM clause → base plan
        let mut plan = nodes::build_from(&select.from)?;

        // 2. WHERE → Filter
        if let Some(predicate) = &select.where_clause {
            plan = plan.filter(predicate.clone());
        }

        // Detect window functions in projection or ORDER BY.
        let proj_has_win = select.projection.iter().any(|p| contains_window(&p.expr));
        let order_has_win = order_by.iter().any(|ob| contains_window(&ob.expr));
        let has_win = proj_has_win || order_has_win;

        let proj_schema = PlanSchema::from_columns(output_schema.to_vec());
        if !select.group_by.is_empty()
            || nodes::has_aggregates(&select.projection)
            || select.having.is_some()
        {
            // ── Aggregate path ──
            // Extract aggregate metadata needed for rewriting.  Must run on the
            // FULL projection (including window items) so that aggregates inside
            // window expressions (e.g. LAG(COUNT(*))) are captured.
            let group_by = &select.group_by;
            let group_by_count = group_by.len();
            let mut aggregate_exprs = nodes::collect_aggregate_exprs(&select.projection);

            // Also collect aggregates from HAVING and ORDER BY that may not
            // appear in the projection (e.g., SELECT dept GROUP BY dept
            // HAVING AVG(salary) > X).  These must be in aggregate_exprs
            // (for rewrite index mapping) AND in agg_projection (so the
            // Aggregate operator computes them).
            let mut extra_agg_projections = Vec::new();
            {
                let pre_count = aggregate_exprs.len();
                let mut names = Vec::new();
                let mut types = Vec::new();
                if let Some(having) = &select.having {
                    super::build::collect_agg_exprs_from(
                        having,
                        "__having_agg",
                        &mut aggregate_exprs,
                        &mut names,
                        &mut types,
                    );
                }
                for ob in order_by {
                    super::build::collect_agg_exprs_from(
                        &ob.expr,
                        "__order_agg",
                        &mut aggregate_exprs,
                        &mut names,
                        &mut types,
                    );
                }
                // Build synthetic AnalyzedProjection for each new aggregate.
                // The final Project will strip them from the output.
                for i in pre_count..aggregate_exprs.len() {
                    let ae = &aggregate_exprs[i];
                    let typed_expr = nodes::find_aggregate_in_having_orderby(
                        ae,
                        select.having.as_ref(),
                        order_by,
                    );
                    extra_agg_projections.push(AnalyzedProjection {
                        output_name: names[i - pre_count].clone(),
                        expr: typed_expr,
                    });
                }
            }

            let has_extra_agg = !extra_agg_projections.is_empty();
            let agg_projection = if has_win {
                // Build a raw projection [group_by_cols..., agg_calls...] so the
                // Aggregate outputs clean columns without a post-projection that
                // would choke on WindowCall nodes.
                // Include extended projection so HAVING/ORDER BY aggregates are found.
                let mut extended = select.projection.to_vec();
                extended.extend(extra_agg_projections);
                nodes::build_raw_aggregate_projection(group_by, &aggregate_exprs, &extended)
            } else {
                let mut proj = select.projection.clone();
                proj.extend(extra_agg_projections);
                proj
            };
            let agg_schema = PlanSchema::from_columns(
                agg_projection
                    .iter()
                    .map(|p| (p.output_name.clone(), p.expr.data_type.clone()))
                    .collect(),
            );
            plan = plan.aggregate(select.group_by.clone(), agg_projection, agg_schema);

            // HAVING → Filter (rewritten)
            if let Some(having) = &select.having {
                let rewritten = super::build::rewrite_post_aggregate_expr(
                    having,
                    group_by,
                    group_by_count,
                    &aggregate_exprs,
                )?;
                plan = plan.filter(rewritten);
            }

            if has_win {
                // ── Aggregate + Window path ──
                // Post-aggregate rewrite the full projection (keeping WindowCalls
                // intact — rewrite_post_aggregate_expr now recurses into WindowCall
                // children), then extract window functions and build Window node.
                let rewritten_proj: Vec<TypedExpr> = select
                    .projection
                    .iter()
                    .map(|p| {
                        super::build::rewrite_post_aggregate_expr(
                            &p.expr,
                            group_by,
                            group_by_count,
                            &aggregate_exprs,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;

                let rewritten_order: Vec<TypedOrderByExpr> = order_by
                    .iter()
                    .map(|ob| {
                        let expr = super::build::rewrite_post_aggregate_expr(
                            &ob.expr,
                            group_by,
                            group_by_count,
                            &aggregate_exprs,
                        )?;
                        Ok(TypedOrderByExpr {
                            expr,
                            asc: ob.asc,
                            nulls_first: ob.nulls_first,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;

                // Extract window functions from BOTH projection and ORDER BY.
                let input_col_count = plan.schema.columns.len();
                let mut window_functions =
                    nodes::extract_window_funcs(&rewritten_proj, &select.projection);
                for ob in &rewritten_order {
                    collect_window_calls_from_expr(
                        &ob.expr,
                        "order_by_window",
                        &mut window_functions,
                    );
                }

                // Build window output schema (input columns + window columns).
                let mut win_schema_cols = plan.schema.columns.clone();
                for wf in &window_functions {
                    win_schema_cols.push((wf.output_name.clone(), wf.output_type.clone()));
                }
                let win_schema = PlanSchema::from_columns(win_schema_cols);
                plan = plan.window(window_functions, win_schema);

                // Rewrite projection and ORDER BY: WindowCall → ColumnRef.
                // Use a shared counter so indices match the extraction order
                // (projection first, then ORDER BY).
                let mut win_counter = 0usize;
                let final_proj: Vec<AnalyzedProjection> = rewritten_proj
                    .iter()
                    .zip(select.projection.iter())
                    .map(|(expr, orig)| AnalyzedProjection {
                        expr: rewrite_for_post_window(expr, input_col_count, &mut win_counter),
                        output_name: orig.output_name.clone(),
                    })
                    .collect();
                let final_order: Vec<TypedOrderByExpr> = rewritten_order
                    .iter()
                    .map(|ob| TypedOrderByExpr {
                        expr: rewrite_for_post_window(&ob.expr, input_col_count, &mut win_counter),
                        asc: ob.asc,
                        nulls_first: ob.nulls_first,
                    })
                    .collect();

                // Sort → DistinctOn/Project/Distinct
                if !final_order.is_empty() {
                    plan = plan.sort(final_order);
                }
                match &select.distinct {
                    AnalyzedDistinct::DistinctOn(on_exprs) => {
                        // Rewrite on_exprs for post-aggregate positions.
                        let rewritten_on: Vec<TypedExpr> = on_exprs
                            .iter()
                            .map(|e| {
                                super::build::rewrite_post_aggregate_expr(
                                    e,
                                    group_by,
                                    group_by_count,
                                    &aggregate_exprs,
                                )
                            })
                            .collect::<Result<Vec<_>>>()?;
                        plan = plan.distinct_on(rewritten_on);
                        plan = plan.project(final_proj, proj_schema);
                    }
                    _ => {
                        plan = plan.project(final_proj, proj_schema);
                        if matches!(select.distinct, AnalyzedDistinct::Distinct) {
                            plan = plan.distinct();
                        }
                    }
                }
            } else {
                // ── Aggregate without windows ──
                // ORDER BY (rewritten)
                if !order_by.is_empty() {
                    let rewritten_order: Vec<TypedOrderByExpr> = order_by
                        .iter()
                        .map(|ob| {
                            let expr = super::build::rewrite_post_aggregate_expr(
                                &ob.expr,
                                group_by,
                                group_by_count,
                                &aggregate_exprs,
                            )?;
                            Ok(TypedOrderByExpr {
                                expr,
                                asc: ob.asc,
                                nulls_first: ob.nulls_first,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    plan = plan.sort(rewritten_order);
                }

                // DISTINCT / DISTINCT ON (on post-aggregate rows)
                match &select.distinct {
                    AnalyzedDistinct::All => {}
                    AnalyzedDistinct::Distinct => {
                        plan = plan.distinct();
                    }
                    AnalyzedDistinct::DistinctOn(on_exprs) => {
                        let rewritten_on: Vec<TypedExpr> = on_exprs
                            .iter()
                            .map(|e| {
                                super::build::rewrite_post_aggregate_expr(
                                    e,
                                    group_by,
                                    group_by_count,
                                    &aggregate_exprs,
                                )
                            })
                            .collect::<Result<Vec<_>>>()?;
                        plan = plan.distinct_on(rewritten_on);
                    }
                }

                // If HAVING/ORDER BY introduced extra aggregate columns that are
                // not in the user's SELECT, strip them with a final Project
                // that references only the first N columns of the aggregate output.
                if has_extra_agg {
                    let select_col_count = select.projection.len();
                    let strip_proj: Vec<AnalyzedProjection> = (0..select_col_count)
                        .map(|i| {
                            let col = &plan.schema.columns[i];
                            AnalyzedProjection {
                                output_name: col.0.clone(),
                                expr: TypedExpr {
                                    kind: TypedExprKind::ColumnRef {
                                        scope_depth: 0,
                                        column_index: i,
                                        column_name: col.0.clone(),
                                    },
                                    data_type: col.1.clone(),
                                },
                            }
                        })
                        .collect();
                    plan = plan.project(strip_proj, proj_schema.clone());
                }
            }
        } else if has_win {
            // ── Non-aggregate + Window path ──
            // Window is inserted before Sort/Project.

            // Extract window functions from BOTH projection and ORDER BY.
            let input_col_count = plan.schema.columns.len();
            let projection_exprs: Vec<TypedExpr> =
                select.projection.iter().map(|p| p.expr.clone()).collect();
            let mut window_functions =
                nodes::extract_window_funcs(&projection_exprs, &select.projection);
            for ob in order_by {
                collect_window_calls_from_expr(&ob.expr, "order_by_window", &mut window_functions);
            }

            // Build window output schema (input columns + window columns).
            let mut win_schema_cols = plan.schema.columns.clone();
            for wf in &window_functions {
                win_schema_cols.push((wf.output_name.clone(), wf.output_type.clone()));
            }
            let win_schema = PlanSchema::from_columns(win_schema_cols);
            plan = plan.window(window_functions, win_schema);

            // Rewrite projection and ORDER BY: WindowCall → ColumnRef.
            // Use a shared counter so indices match the extraction order
            // (projection first, then ORDER BY).
            let mut win_counter = 0usize;
            let rewritten_proj: Vec<AnalyzedProjection> = select
                .projection
                .iter()
                .map(|p| AnalyzedProjection {
                    expr: rewrite_for_post_window(&p.expr, input_col_count, &mut win_counter),
                    output_name: p.output_name.clone(),
                })
                .collect();
            let rewritten_order: Vec<TypedOrderByExpr> = order_by
                .iter()
                .map(|ob| TypedOrderByExpr {
                    expr: rewrite_for_post_window(&ob.expr, input_col_count, &mut win_counter),
                    asc: ob.asc,
                    nulls_first: ob.nulls_first,
                })
                .collect();

            // Sort → DistinctOn/Project/Distinct
            if !rewritten_order.is_empty() {
                plan = plan.sort(rewritten_order);
            }
            match &select.distinct {
                AnalyzedDistinct::DistinctOn(on_exprs) => {
                    // DISTINCT ON on_exprs reference pre-projection full-width rows.
                    // After Window, full-width rows are still present (Window only appends).
                    plan = plan.distinct_on(on_exprs.clone());
                    plan = plan.project(rewritten_proj, proj_schema);
                }
                _ => {
                    plan = plan.project(rewritten_proj, proj_schema);
                    if matches!(select.distinct, AnalyzedDistinct::Distinct) {
                        plan = plan.distinct();
                    }
                }
            }
        } else {
            // ── Non-aggregate, no window path ──
            // Sort on full-width rows (before projection narrows them).
            if !order_by.is_empty() {
                plan = plan.sort(order_by.to_vec());
            }

            // DISTINCT ON vs plain DISTINCT have different placement relative
            // to Project:
            //
            // - DISTINCT ON evaluates on_exprs against pre-projection full-width
            //   rows (the on_exprs may reference columns not in the output).
            //   Ordering: Sort → DistinctOn → Project
            //
            // - Plain DISTINCT deduplicates on the projected output columns.
            //   Ordering: Sort → Project → Distinct
            match &select.distinct {
                AnalyzedDistinct::DistinctOn(on_exprs) => {
                    // DistinctOn on full-width rows, then narrow via Project.
                    plan = plan.distinct_on(on_exprs.clone());
                    plan = plan.project(select.projection.clone(), proj_schema);
                }
                _ => {
                    // Project first (narrows columns), then optionally Distinct.
                    plan = plan.project(select.projection.clone(), proj_schema);
                    if matches!(select.distinct, AnalyzedDistinct::Distinct) {
                        plan = plan.distinct();
                    }
                }
            }
        }

        Ok(plan)
    }
}

#[cfg(test)]
mod tests;
