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

use super::logical_plan::{LogicalNode, LogicalPlan, PlanSchema};
use super::window_rewrite::{
    collect_window_calls_from_expr, contains_window, rewrite_for_post_window,
};
use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedProjection, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef,
    AnalyzedTableRefKind, TypedExpr, TypedExprKind, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use crate::sql::operators::AggregateExpr;
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
        let mut plan = Self::build_body(&query.body, &query.output_schema, &query.order_by)?;

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
        let mut plan = Self::build_from(&select.from)?;

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
            || has_aggregates(&select.projection)
            || select.having.is_some()
        {
            // ── Aggregate path ──
            // Extract aggregate metadata needed for rewriting.  Must run on the
            // FULL projection (including window items) so that aggregates inside
            // window expressions (e.g. LAG(COUNT(*))) are captured.
            let group_by = &select.group_by;
            let group_by_count = group_by.len();
            let mut aggregate_exprs = Self::collect_aggregate_exprs(&select.projection);

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
                    let typed_expr = Self::find_aggregate_in_having_orderby(
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

            let agg_projection = if has_win {
                // Build a raw projection [group_by_cols..., agg_calls...] so the
                // Aggregate outputs clean columns without a post-projection that
                // would choke on WindowCall nodes.
                // Include extended projection so HAVING/ORDER BY aggregates are found.
                let mut extended = select.projection.to_vec();
                extended.extend(extra_agg_projections);
                Self::build_raw_aggregate_projection(group_by, &aggregate_exprs, &extended)
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
                    Self::extract_window_funcs(&rewritten_proj, &select.projection);
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
            }
        } else if has_win {
            // ── Non-aggregate + Window path ──
            // Window is inserted before Sort/Project.

            // Extract window functions from BOTH projection and ORDER BY.
            let input_col_count = plan.schema.columns.len();
            let projection_exprs: Vec<TypedExpr> =
                select.projection.iter().map(|p| p.expr.clone()).collect();
            let mut window_functions =
                Self::extract_window_funcs(&projection_exprs, &select.projection);
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

    /// Collect unique AggregateExpr from projection list (for rewriting).
    fn collect_aggregate_exprs(
        projections: &[crate::sql::analyzer::types::AnalyzedProjection],
    ) -> Vec<AggregateExpr> {
        let mut agg_exprs = Vec::new();
        let mut agg_names = Vec::new();
        let mut agg_types = Vec::new();
        for proj in projections {
            super::build::collect_agg_exprs_from(
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
    fn build_raw_aggregate_projection(
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
            let agg_expr = Self::find_aggregate_typed_expr(ae, full_projection);
            let name = format!("agg_{}", i);
            raw.push(AnalyzedProjection {
                expr: agg_expr,
                output_name: name,
            });
        }

        raw
    }

    /// Find the TypedExpr for a given AggregateExpr in the projection tree.
    fn find_aggregate_typed_expr(
        ae: &AggregateExpr,
        projection: &[AnalyzedProjection],
    ) -> TypedExpr {
        for proj in projection {
            if let Some(found) = Self::find_agg_in_expr(&proj.expr, ae) {
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
                        .map_or(crate::types::DataType::Int64, |a| a.data_type.clone()),
                },
                args: ae.arg.iter().cloned().collect(),
                distinct: ae.distinct,
                filter: ae.filter.as_ref().map(|f| Box::new(f.clone())),
                order_by: ae.order_by.clone(),
            },
            data_type: ae
                .arg
                .as_ref()
                .map_or(crate::types::DataType::Int64, |a| a.data_type.clone()),
        }
    }

    /// Search an expression tree for an AggregateCall matching the given AggregateExpr.
    fn find_agg_in_expr(expr: &TypedExpr, target: &AggregateExpr) -> Option<TypedExpr> {
        match &expr.kind {
            TypedExprKind::AggregateCall {
                func,
                args,
                distinct,
                filter,
                order_by,
            } => {
                if super::build::aggregate_identity_matches(
                    target, func, args, *distinct, filter, order_by,
                ) {
                    return Some(expr.clone());
                }
                None
            }
            TypedExprKind::BinaryOp { left, right, .. } => Self::find_agg_in_expr(left, target)
                .or_else(|| Self::find_agg_in_expr(right, target)),
            TypedExprKind::UnaryOp { operand, .. } | TypedExprKind::Cast { expr: operand, .. } => {
                Self::find_agg_in_expr(operand, target)
            }
            TypedExprKind::FunctionCall { args, .. } => {
                args.iter().find_map(|a| Self::find_agg_in_expr(a, target))
            }
            TypedExprKind::WindowCall {
                args,
                partition_by,
                order_by,
                ..
            } => args
                .iter()
                .chain(partition_by.iter())
                .find_map(|a| Self::find_agg_in_expr(a, target))
                .or_else(|| {
                    order_by
                        .iter()
                        .find_map(|ob| Self::find_agg_in_expr(&ob.expr, target))
                }),
            TypedExprKind::Case {
                operand,
                when_clauses,
                else_result,
            } => operand
                .as_ref()
                .and_then(|o| Self::find_agg_in_expr(o, target))
                .or_else(|| {
                    when_clauses.iter().find_map(|(w, t)| {
                        Self::find_agg_in_expr(w, target)
                            .or_else(|| Self::find_agg_in_expr(t, target))
                    })
                })
                .or_else(|| {
                    else_result
                        .as_ref()
                        .and_then(|e| Self::find_agg_in_expr(e, target))
                }),
            TypedExprKind::Coalesce(args) | TypedExprKind::MinMax { args, .. } => {
                args.iter().find_map(|a| Self::find_agg_in_expr(a, target))
            }
            TypedExprKind::NullIf(a, b) => {
                Self::find_agg_in_expr(a, target).or_else(|| Self::find_agg_in_expr(b, target))
            }
            _ => None,
        }
    }

    /// Find the original TypedExpr for an AggregateExpr in HAVING/ORDER BY trees.
    fn find_aggregate_in_having_orderby(
        ae: &AggregateExpr,
        having: Option<&TypedExpr>,
        order_by: &[TypedOrderByExpr],
    ) -> TypedExpr {
        if let Some(h) = having {
            if let Some(found) = Self::find_agg_in_expr(h, ae) {
                return found;
            }
        }
        for ob in order_by {
            if let Some(found) = Self::find_agg_in_expr(&ob.expr, ae) {
                return found;
            }
        }
        // Fallback: use the reconstruction from find_aggregate_typed_expr
        Self::find_aggregate_typed_expr(ae, &[])
    }

    /// Extract `WindowFunctionExpr` from a list of typed expressions.
    ///
    /// `exprs` and `projection` must be the same length — `exprs[i]` is the
    /// (possibly post-aggregate-rewritten) expression and `projection[i]`
    /// provides the output name.
    fn extract_window_funcs(
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

    fn build_from(from: &[AnalyzedTableRef]) -> Result<LogicalPlan> {
        if from.is_empty() {
            return Ok(LogicalPlan::empty(PlanSchema::from_columns(vec![])));
        }

        let mut plan = Self::build_table_ref(&from[0])?;

        // Additional FROM items → cross joins
        for table_ref in from.iter().skip(1) {
            let right = Self::build_table_ref(table_ref)?;
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
                let subplan = Self::build(subquery)?;
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
                let left_plan = Self::build_table_ref(left)?;
                let right_plan = Self::build_table_ref(right)?;
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
}

/// Check if any projection item contains an aggregate function call.
fn has_aggregates(projections: &[crate::sql::analyzer::types::AnalyzedProjection]) -> bool {
    projections.iter().any(|p| expr_has_aggregate(&p.expr))
}

pub(crate) fn expr_has_aggregate(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::AggregateCall { .. } => true,
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
            operand.as_ref().map_or(false, |e| expr_has_aggregate(e))
                || when_clauses
                    .iter()
                    .any(|(w, t)| expr_has_aggregate(w) || expr_has_aggregate(t))
                || else_result
                    .as_ref()
                    .map_or(false, |e| expr_has_aggregate(e))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::*;
    use crate::types::DataType;

    fn simple_column(name: &str, dt: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: name.to_string(),
            },
            data_type: dt.clone(),
        }
    }

    fn simple_constant(v: crate::types::Value, dt: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(v),
            data_type: dt,
        }
    }

    fn simple_projection(name: &str, dt: DataType) -> AnalyzedProjection {
        AnalyzedProjection {
            expr: simple_column(name, dt),
            output_name: name.to_string(),
        }
    }

    /// Single-table SELECT: SELECT id, name FROM users WHERE id = 1
    #[test]
    fn test_single_table_select() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![
                    simple_projection("id", DataType::Int64),
                    simple_projection("name", DataType::Text),
                ],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "users".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![
                                ("id".to_string(), DataType::Int64, false),
                                ("name".to_string(), DataType::Text, true),
                            ],
                        },
                    },
                    alias: None,
                }],
                where_clause: Some(TypedExpr {
                    kind: TypedExprKind::BinaryOp {
                        left: Box::new(simple_column("id", DataType::Int64)),
                        op: BinaryOp::Eq,
                        right: Box::new(simple_constant(
                            crate::types::Value::Int64(1),
                            DataType::Int64,
                        )),
                    },
                    data_type: DataType::Boolean,
                }),
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![
                ("id".to_string(), DataType::Int64),
                ("name".to_string(), DataType::Text),
            ],
        };

        let plan = LogicalPlanner::build(&query).unwrap();

        // Should be: Project → Filter → Scan
        assert!(matches!(plan.node, LogicalNode::Project { .. }));
        if let LogicalNode::Project { input, .. } = &plan.node {
            assert!(matches!(input.node, LogicalNode::Filter { .. }));
            if let LogicalNode::Filter { input, .. } = &input.node {
                assert!(matches!(input.node, LogicalNode::Scan { .. }));
            }
        }
        assert_eq!(plan.schema.num_columns(), 2);
    }

    /// Tableless query: SELECT 1
    #[test]
    fn test_tableless_select() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: simple_constant(crate::types::Value::Int32(1), DataType::Int32),
                    output_name: "?column?".to_string(),
                }],
                from: vec![],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("?column?".to_string(), DataType::Int32)],
        };

        let plan = LogicalPlanner::build(&query).unwrap();

        // Should be: Project → Empty
        assert!(matches!(plan.node, LogicalNode::Project { .. }));
        if let LogicalNode::Project { input, .. } = &plan.node {
            assert!(matches!(input.node, LogicalNode::Empty));
        }
    }

    /// Query with ORDER BY and LIMIT: SELECT * FROM t ORDER BY id LIMIT 10
    #[test]
    fn test_order_by_limit() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![simple_projection("id", DataType::Int64)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "t".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![("id".to_string(), DataType::Int64, false)],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![TypedOrderByExpr {
                expr: simple_column("id", DataType::Int64),
                asc: true,
                nulls_first: false,
            }],
            limit: Some(simple_constant(
                crate::types::Value::Int64(10),
                DataType::Int64,
            )),
            offset: None,
            output_schema: vec![("id".to_string(), DataType::Int64)],
        };

        let plan = LogicalPlanner::build(&query).unwrap();

        // Should be: Limit → Project → Sort → Scan
        assert!(matches!(plan.node, LogicalNode::Limit { .. }));
        if let LogicalNode::Limit { input, .. } = &plan.node {
            assert!(
                matches!(input.node, LogicalNode::Project { .. }),
                "expected Project, got {:?}",
                std::mem::discriminant(&input.node)
            );
            if let LogicalNode::Project { input, .. } = &input.node {
                assert!(
                    matches!(input.node, LogicalNode::Sort { .. }),
                    "expected Sort, got {:?}",
                    std::mem::discriminant(&input.node)
                );
            }
        }
    }

    /// Set operation: SELECT id FROM a UNION SELECT id FROM b
    #[test]
    fn test_set_operation() {
        let left = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![simple_projection("id", DataType::Int64)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "a".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![("id".to_string(), DataType::Int64, false)],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("id".to_string(), DataType::Int64)],
        };
        let right = left.clone();

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::SetOperation {
                op: SetOpKind::Union,
                all: false,
                left: Box::new(left),
                right: Box::new(right),
            },
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("id".to_string(), DataType::Int64)],
        };

        let plan = LogicalPlanner::build(&query).unwrap();
        assert!(matches!(plan.node, LogicalNode::SetOperation { .. }));
    }

    /// Query with GROUP BY: SELECT status, count(*) FROM orders GROUP BY status
    #[test]
    fn test_group_by() {
        let count_agg = TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: ResolvedFunction {
                    name: "count".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Int64,
        };

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![
                    simple_projection("status", DataType::Text),
                    AnalyzedProjection {
                        expr: count_agg,
                        output_name: "count".to_string(),
                    },
                ],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "orders".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![
                                ("id".to_string(), DataType::Int64, false),
                                ("status".to_string(), DataType::Text, false),
                            ],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![simple_column("status", DataType::Text)],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![
                ("status".to_string(), DataType::Text),
                ("count".to_string(), DataType::Int64),
            ],
        };

        let plan = LogicalPlanner::build(&query).unwrap();

        // Should be: Aggregate → Scan (no separate Project when GROUP BY is present)
        assert!(matches!(plan.node, LogicalNode::Aggregate { .. }));
        if let LogicalNode::Aggregate { input, .. } = &plan.node {
            assert!(matches!(input.node, LogicalNode::Scan { .. }));
        }
    }

    /// DISTINCT query
    #[test]
    fn test_distinct() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![simple_projection("name", DataType::Text)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "t".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![("name".to_string(), DataType::Text, true)],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::Distinct,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("name".to_string(), DataType::Text)],
        };

        let plan = LogicalPlanner::build(&query).unwrap();

        // Should be: Distinct → Project → Scan
        assert!(matches!(plan.node, LogicalNode::Distinct { .. }));
    }

    /// Aggregate + ORDER BY: rewrite ORDER BY to post-aggregate indices
    #[test]
    fn test_aggregate_order_by_rewrite() {
        let count_agg = TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: ResolvedFunction {
                    name: "count".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Int64,
        };

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![
                    simple_projection("status", DataType::Text),
                    AnalyzedProjection {
                        expr: count_agg.clone(),
                        output_name: "count".to_string(),
                    },
                ],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "orders".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![
                                ("id".to_string(), DataType::Int64, false),
                                ("status".to_string(), DataType::Text, false),
                            ],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![simple_column("status", DataType::Text)],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            // ORDER BY count(*) DESC — uses scan-scope aggregate expr
            order_by: vec![TypedOrderByExpr {
                expr: count_agg,
                asc: false,
                nulls_first: false,
            }],
            limit: None,
            offset: None,
            output_schema: vec![
                ("status".to_string(), DataType::Text),
                ("count".to_string(), DataType::Int64),
            ],
        };

        let plan = LogicalPlanner::build(&query).unwrap();

        // Should be: Sort(rewritten) → Aggregate → Scan
        assert!(matches!(plan.node, LogicalNode::Sort { .. }));
        if let LogicalNode::Sort {
            order_by, input, ..
        } = &plan.node
        {
            assert_eq!(order_by.len(), 1);
            // The rewritten ORDER BY should be a ColumnRef to post-aggregate index 1
            // (group_by_count=1, agg_index=0 → 1+0 = 1)
            if let TypedExprKind::ColumnRef { column_index, .. } = &order_by[0].expr.kind {
                assert_eq!(*column_index, 1);
            } else {
                panic!("expected ColumnRef in rewritten ORDER BY");
            }
            assert!(matches!(input.node, LogicalNode::Aggregate { .. }));
        }
    }

    /// Aggregate + HAVING: rewrite HAVING to post-aggregate indices
    #[test]
    fn test_aggregate_having_rewrite() {
        let count_agg = TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: ResolvedFunction {
                    name: "count".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Int64,
        };

        let having_expr = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(count_agg.clone()),
                op: BinaryOp::Gt,
                right: Box::new(simple_constant(
                    crate::types::Value::Int64(5),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        };

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![
                    simple_projection("status", DataType::Text),
                    AnalyzedProjection {
                        expr: count_agg,
                        output_name: "count".to_string(),
                    },
                ],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "orders".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![
                                ("id".to_string(), DataType::Int64, false),
                                ("status".to_string(), DataType::Text, false),
                            ],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![simple_column("status", DataType::Text)],
                having: Some(having_expr),
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![
                ("status".to_string(), DataType::Text),
                ("count".to_string(), DataType::Int64),
            ],
        };

        let plan = LogicalPlanner::build(&query).unwrap();

        // Should be: Filter(rewritten HAVING) → Aggregate → Scan
        assert!(matches!(plan.node, LogicalNode::Filter { .. }));
        if let LogicalNode::Filter {
            predicate, input, ..
        } = &plan.node
        {
            // HAVING predicate should be rewritten: COUNT(*) → ColumnRef(1)
            if let TypedExprKind::BinaryOp { left, .. } = &predicate.kind {
                if let TypedExprKind::ColumnRef { column_index, .. } = &left.kind {
                    assert_eq!(*column_index, 1);
                } else {
                    panic!("expected ColumnRef in rewritten HAVING left side");
                }
            } else {
                panic!("expected BinaryOp in rewritten HAVING");
            }
            assert!(matches!(input.node, LogicalNode::Aggregate { .. }));
        }
    }
}
