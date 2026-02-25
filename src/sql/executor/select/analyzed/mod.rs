//! Analyzed SELECT execution path — the primary SELECT executor.
//!
//! All SELECT queries go through the Analyzer, producing `AnalyzedQuery` with
//! fully typed expressions. Async operations (subqueries, sequences) are
//! pre-materialized before building the operator tree.
//!
//! Handles FOR UPDATE/SHARE row locking and SELECT INTO natively.

use crate::model::{DataType, Row, TableSchema};
use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQueryBody, AnalyzedTableRef, AnalyzedTableRefKind, JoinCondition,
    TypedExpr, TypedExprKind, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use crate::sql::executor::core::Executor;
use crate::sql::expr::classify::needs_pre_materialization;
use crate::sql::expr::typed_eval::eval_const_usize;
use crate::sql::ExecuteResult;

use anyhow::{anyhow, Result};
use sqlparser::ast::{Query, SetExpr};
use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tikv_client::Transaction;

mod expr_runtime;
mod materialize;
mod materialize_catalog;
mod pipeline;
pub(crate) mod postprocess;
mod pre_materialize;
mod subquery;

use expr_runtime::*;
use postprocess::*;

impl Executor {
    /// Execute a query through the Analyzer path.
    ///
    /// This is the single execution path for all SELECT queries. The Analyzer
    /// produces fully typed expressions that are compiled into an operator tree.
    ///
    /// Returns a boxed future to keep the async state machine off callers'
    /// stack frames, bounding worker stack usage (#907).
    pub(crate) fn try_execute_analyzed<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        query: &'a Query,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        current_role: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<ExecuteResult>> + Send + 'a>> {
        Box::pin(async move {
            // Pre-expand views once to discover nested WITH scopes introduced by
            // view expansion (e.g. FROM (WITH ... SELECT ...) AS v) and materialize
            // those CTEs before analysis.
            let prepared_ctes = {
                let preexpanded_query =
                    crate::sql::executor::core::view_rewrite::expand_views_in_query(
                        self.store().as_ref(),
                        txn,
                        db_id,
                        search_path,
                        query,
                    )
                    .await?;
                let prepared = self
                    .build_nested_with_cte_context_with_base(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &preexpanded_query,
                        ctes,
                        current_role,
                    )
                    .await?;
                // Drop deep rewritten AST on a grown stack as soon as it is no longer needed.
                crate::sql::stack_safety::drop_on_grown_stack(preexpanded_query);
                prepared
            };

            let analyzed = self
                .analyze_then_rewrite_query(
                    txn,
                    db_id,
                    search_path,
                    query,
                    &prepared_ctes,
                    current_role,
                )
                .await?;

            let select_into_target = match &*query.body {
                SetExpr::Select(select) => select.into.as_ref().map(|into| into.name.clone()),
                _ => None,
            };

            // ── Single execution path: CBO optimizer pipeline ──────────
            let (result, _) = self
                .execute_via_optimizer(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    Cow::Owned(analyzed),
                    &prepared_ctes,
                    &query.locks,
                    None,
                    false,
                )
                .await?;

            // SELECT INTO post-processing: create table from result.
            if let Some(target_name) = select_into_target {
                return self
                    .create_table_from_result(txn, db_id, search_path, &target_name, result)
                    .await;
            }

            Ok(result)
        })
    }

    /// Execute a subquery (no locks) through the optimizer pipeline.
    ///
    /// Used for recursive execution: subqueries in FROM, correlated subqueries,
    /// set-operation branches, INSERT...SELECT, etc.
    ///
    /// Returns a boxed future to seal the recursion boundary
    /// (`transform_expr → execute_subquery → execute_via_optimizer`) and
    /// keep async frame sizes bounded (#907).
    pub(crate) fn execute_subquery<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        analyzed: &'a AnalyzedQuery,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<ExecuteResult>> + Send + 'a>> {
        Box::pin(async move {
            let (result, _) = self
                .execute_via_optimizer(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    Cow::Borrowed(analyzed),
                    ctes,
                    &[],
                    None,
                    false,
                )
                .await?;
            Ok(result)
        })
    }

    /// Materialize WITH CTEs from analyzed IR into runtime CTE bindings.
    ///
    /// This is used by prepared execution where we no longer have the original
    /// SQL AST but still need deterministic CTE materialization.
    #[allow(clippy::type_complexity)]
    pub(crate) fn build_cte_context_from_analyzed_with_base<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        analyzed: &'a AnalyzedQuery,
        base_ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<HashMap<String, (TableSchema, Vec<Row>)>>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut ctes: HashMap<String, (TableSchema, Vec<Row>)> = base_ctes.clone();
            for cte in &analyzed.ctes {
                let cte_scope = self
                    .build_cte_context_from_analyzed_with_base(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &cte.query,
                        &ctes,
                    )
                    .await?;
                let cte_result = self
                    .execute_subquery(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &cte.query,
                        &cte_scope,
                    )
                    .await?;
                match cte_result {
                    ExecuteResult::Select {
                        columns,
                        column_types,
                        rows,
                        timezone: _,
                    } => {
                        let col_names: Vec<String> = if cte.columns.is_empty() {
                            columns
                        } else {
                            cte.columns.iter().map(|(n, _, _)| n.clone()).collect()
                        };
                        let inferred_types: Vec<DataType> = if let Some(types) = column_types {
                            types
                        } else {
                            crate::model::infer_column_types_from_rows(&rows, col_names.len())
                        };
                        let schema = TableSchema {
                            table_id: 0,
                            name: cte.name.clone(),
                            columns: col_names
                                .iter()
                                .enumerate()
                                .map(|(idx, n)| crate::model::ColumnDef {
                                    name: n.clone(),
                                    // Index guard: unreachable when types and columns are aligned.
                                    data_type: inferred_types
                                        .get(idx)
                                        .cloned()
                                        .unwrap_or(DataType::Text),
                                    nullable: true,
                                    primary_key: false,
                                    unique: false,
                                    is_serial: false,
                                    default_expr: None,
                                    collation: None,
                                })
                                .collect(),
                            pk_constraint_name: None,
                            pk_indices: vec![],
                            indexes: vec![],
                            version: 1,
                            check_constraints: vec![],
                            foreign_keys: vec![],
                            owner: String::new(),
                            from_alias: None,
                        };
                        ctes.insert(cte.name.to_lowercase(), (schema, rows));
                    }
                    _ => return Err(anyhow!("CTE must be a SELECT query")),
                }
            }
            ctes = self
                .build_nested_ctes_from_analyzed_query_with_base(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    analyzed,
                    &ctes,
                )
                .await?;
            Ok(ctes)
        })
    }

    /// Materialize CTE runtime bindings for nested (non-root) analyzed queries.
    ///
    /// Traverses subqueries in FROM/join trees, typed expressions, set-operation
    /// branches, and query-level ORDER BY/LIMIT/OFFSET.
    #[allow(clippy::type_complexity)]
    fn build_nested_ctes_from_analyzed_query_with_base<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        analyzed: &'a AnalyzedQuery,
        base_ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<HashMap<String, (TableSchema, Vec<Row>)>>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut ctes = base_ctes.clone();
            let nested_queries = collect_immediate_nested_analyzed_queries(analyzed);

            for nested_query in nested_queries {
                ctes = self
                    .build_cte_context_from_analyzed_with_base(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        nested_query,
                        &ctes,
                    )
                    .await?;
            }

            Ok(ctes)
        })
    }

    /// Execute a query through the CBO optimizer pipeline.
    ///
    /// This is the **single execution path** for ALL SELECT queries:
    /// `AnalyzedQuery → pre-materialize → optimize → build → execute → post-process`
    ///
    /// Handles all query shapes: single-table, multi-table joins, set operations,
    /// CTEs, VALUES, tableless SELECT, table functions, virtual catalog tables,
    /// subqueries, correlated subqueries, catalog-dependent functions,
    /// FOR UPDATE/SHARE row locking, and DISTINCT/DISTINCT ON.
    ///
    /// Returns a boxed future — this is the largest async state machine in the
    /// execution path (~8 `.await` points holding `AnalyzedQuery`,
    /// `PlanningContext`, `BuildContext`, `Vec<Row>`, etc.). Boxing it keeps
    /// async frame sizes bounded for all callers (#907).
    #[allow(clippy::type_complexity)]
    pub(crate) fn execute_via_optimizer<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        analyzed: Cow<'a, AnalyzedQuery>,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        locks: &'a [sqlparser::ast::LockClause],
        cached_plan: Option<&'a crate::sql::optimizer::physical_plan::PhysicalPlan>,
        capture_plan: bool,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<(
                        ExecuteResult,
                        Option<crate::sql::optimizer::physical_plan::PhysicalPlan>,
                    )>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            use crate::sql::expr::classify::needs_async;
            use crate::sql::optimizer::{BuildContext, PlanningContext};

            tracing::debug!(target: "optimizer", "routing query through CBO pipeline");

            let rt = ExprRuntime::new(self, db_id, search_path, ctes);

            // ── Step 1: Pre-materialize non-correlated async expressions ──
            // Resolves IN subquery → InList, EXISTS → bool, ScalarSubquery → constant,
            // ANY/ALL → expanded comparisons, ArraySubquery → array literal.
            // Correlated subqueries (scope_depth > 0) are left as-is.
            //
            // For Cow::Owned (normal SELECT): to_mut() is free — no clone.
            // For Cow::Borrowed (prepared/subquery): only clone when the query
            // actually contains expressions that need pre-materialization.
            let mut analyzed = analyzed;
            match &mut analyzed {
                Cow::Owned(ref mut owned) => {
                    self.pre_materialize_query_body(
                        owned,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                    )
                    .await?;
                }
                Cow::Borrowed(_) => {
                    if query_needs_pre_materialization(&analyzed) {
                        self.pre_materialize_query_body(
                            analyzed.to_mut(),
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    }
                }
            }

            // ── Step 2: Determine post-processing needs ──
            let mut has_async_projection = match &analyzed.body {
                AnalyzedQueryBody::Select(s) => s.projection.iter().any(|p| needs_async(&p.expr)),
                _ => false,
            };
            let has_async_order_by = analyzed.order_by.iter().any(|o| needs_async(&o.expr));
            let has_locks = !locks.is_empty();

            // ── Step 2a: GROUP BY + async projection special handling ──
            // Passthrough mode (replacing all projection with source columns) is
            // incompatible with GROUP BY because the Aggregate operator changes the
            // row layout. Instead, replace each async projection expression with its
            // primary dependency (the ColumnRef argument), and defer the async
            // function evaluation to post-processing.
            let has_group_by = match &analyzed.body {
                AnalyzedQueryBody::Select(s) => !s.group_by.is_empty(),
                _ => false,
            };
            // (output_col_index, deferred_async_expr_with_output_col_ref)
            let mut deferred_async_cols: Vec<(usize, TypedExpr)> = Vec::new();
            if has_group_by && has_async_projection {
                if let AnalyzedQueryBody::Select(ref mut select) = analyzed.to_mut().body {
                    for (i, proj) in select.projection.iter_mut().enumerate() {
                        if needs_async(&proj.expr) {
                            // Build a deferred expression that references output col `i`
                            // (the dependency value will be at this position after the
                            // optimizer evaluates the replacement expression).
                            let deferred = build_deferred_async_expr(&proj.expr, i);
                            deferred_async_cols.push((i, deferred));

                            // Replace the async expression with its primary dependency
                            // (the first ColumnRef argument). This lets the GROUP BY
                            // Aggregate operator include the dependency as a group key
                            // reference, producing the correct value in the output.
                            if let Some(dep) = extract_async_dependency(&proj.expr) {
                                proj.expr = dep;
                            } else {
                                // Fallback: NULL constant (value will be replaced in
                                // post-processing, but aggregate rewrite must still work).
                                proj.expr = TypedExpr {
                                    kind: TypedExprKind::Constant(crate::model::Value::Null),
                                    data_type: proj.expr.data_type.clone(),
                                };
                            }
                        }
                    }
                }
                // Passthrough is no longer needed for async projection — we handled it.
                has_async_projection = false;
            }

            let needs_passthrough = has_async_projection || has_locks;

            // Save the final output schema before any modifications.
            let final_output_schema = analyzed.output_schema.clone();

            // ── Step 3: Prepare passthrough mode (strip async parts) ──
            // When projection has async expressions or locks need raw rows,
            // replace projection with passthrough (all source columns) and
            // handle the real projection in post-processing.
            let mut original_proj_exprs: Option<Vec<TypedExpr>> = None;
            let mut base_schema: Option<TableSchema> = None;
            let mut deferred_order_by: Option<Vec<TypedOrderByExpr>> = None;
            let mut deferred_limit: Option<(Option<TypedExpr>, Option<TypedExpr>)> = None;

            if needs_passthrough {
                let a = analyzed.to_mut();
                if let AnalyzedQueryBody::Select(ref mut select) = a.body {
                    // Save original projection.
                    original_proj_exprs =
                        Some(select.projection.iter().map(|p| p.expr.clone()).collect());

                    // Build base schema from source columns.
                    let source_cols = collect_source_columns(select);
                    base_schema = Some(build_schema_from_columns("__base", &source_cols));

                    // Replace projection with passthrough.
                    select.projection = create_passthrough_projection(&source_cols);
                    a.output_schema = source_cols;
                }

                // Defer LIMIT/OFFSET for locking (scan all, lock, then paginate).
                let limit = a.limit.take();
                let offset = a.offset.take();
                if limit.is_some() || offset.is_some() {
                    deferred_limit = Some((limit, offset));
                }
            }

            // JOIN ON predicates (including correlated subqueries / catalog-dependent
            // functions) are evaluated inside join operators so outer join
            // null-extension semantics remain join-local and deterministic.

            if has_async_order_by {
                let a = analyzed.to_mut();
                deferred_order_by = Some(a.order_by.clone());
                a.order_by.clear();
                // Also defer LIMIT/OFFSET (ORDER BY must happen before LIMIT).
                if deferred_limit.is_none() {
                    let limit = a.limit.take();
                    let offset = a.offset.take();
                    if limit.is_some() || offset.is_some() {
                        deferred_limit = Some((limit, offset));
                    }
                }
            }

            // ── Step 4: Pre-load table schemas, stats, virtual table data ──
            let mut planning_ctx = PlanningContext::empty();
            let mut build_ctx = BuildContext::new();
            self.prepare_optimizer_contexts(
                txn,
                db_id,
                sequence_values,
                search_path,
                &analyzed,
                ctes,
                &mut planning_ctx,
                &mut build_ctx,
            )
            .await?;

            // ── Step 5: AnalyzedQuery → PhysicalPlan (or use cached) ──
            let (physical, captured_plan) = if let Some(cached) = cached_plan {
                (cached.clone(), None)
            } else {
                let optimized = crate::sql::stack_safety::with_grown_stack(|| {
                    crate::sql::optimizer::optimize(&analyzed, &planning_ctx)
                })?;
                let captured = if capture_plan {
                    Some(optimized.clone())
                } else {
                    None
                };
                (optimized, captured)
            };

            // ── Step 6: PhysicalPlan → BoxedOperator ──
            let mut operator = crate::sql::stack_safety::with_grown_stack(|| {
                physical.build_operators(&build_ctx)
            })?;

            // ── Step 7: Execute operator tree ──
            let mut rows = rt
                .run_operator_tree(&mut operator, txn, sequence_values)
                .await?;

            // ── Step 8: Post-processing ──

            // 8a: FOR UPDATE/SHARE locking (before projection, raw rows have PK).
            if has_locks {
                rows = self
                    .apply_row_locks(
                        rows,
                        locks,
                        &analyzed,
                        &build_ctx,
                        txn,
                        db_id,
                        &deferred_limit,
                    )
                    .await?;
            }

            // 8b: Apply original projection (async expressions + all expressions when passthrough).
            if let Some(ref proj_exprs) = original_proj_exprs {
                let schema = base_schema
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| build_output_schema(&analyzed));
                rows = rt
                    .project_rows(rows, proj_exprs, &schema, txn, sequence_values)
                    .await?;
            }

            // 8c: Deferred async projection for GROUP BY queries.
            // Each deferred entry has (output_col_idx, async_expr_with_output_col_ref).
            // The output row already contains the dependency value at col_idx;
            // we evaluate the async function with that value and replace the column.
            if !deferred_async_cols.is_empty() {
                let schema = build_output_schema(&analyzed);
                let qctx = crate::sql::query_context::QueryContext::from_task_locals();
                for row in &mut rows {
                    for (col_idx, async_expr) in &deferred_async_cols {
                        let materialized = self
                            .materialize_expr_for_row(
                                async_expr,
                                row,
                                None,
                                Some(&schema),
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                                &qctx,
                            )
                            .await?;
                        // materialize_expr_for_row returns a TypedExpr with the async
                        // function resolved to a Constant. Evaluate to get the value.
                        let val = crate::sql::expr::typed_eval::eval_typed_expr(
                            &materialized,
                            row,
                            &qctx,
                        )?;
                        if *col_idx < row.values.len() {
                            row.values[*col_idx] = val;
                        }
                    }
                }
            }

            // 8d: Deferred ORDER BY + LIMIT/OFFSET.
            if let Some(ref deferred_ob) = deferred_order_by {
                let limit = deferred_limit
                    .as_ref()
                    .and_then(|(l, _)| l.as_ref())
                    .map(|expr| eval_const_usize(expr, true))
                    .transpose()?;
                let offset = deferred_limit
                    .as_ref()
                    .and_then(|(_, o)| o.as_ref())
                    .map(|expr| eval_const_usize(expr, true))
                    .transpose()?
                    .unwrap_or(0);
                rows = sort_projected_rows(rows, deferred_ob, &final_output_schema, limit, offset)?;
            } else if let Some((ref limit_expr, ref offset_expr)) = deferred_limit {
                // LIMIT/OFFSET deferred for locking but no deferred ORDER BY.
                let limit = limit_expr
                    .as_ref()
                    .map(|expr| eval_const_usize(expr, true))
                    .transpose()?;
                let offset = offset_expr
                    .as_ref()
                    .map(|expr| eval_const_usize(expr, true))
                    .transpose()?
                    .unwrap_or(0);
                if limit.is_some() || offset > 0 {
                    let start = offset.min(rows.len());
                    let end = limit.map_or(rows.len(), |l| (start + l).min(rows.len()));
                    rows = rows[start..end].to_vec();
                }
            }

            // ── Step 9: Build result ──
            let columns: Vec<String> = final_output_schema
                .iter()
                .map(|(name, _, _)| name.clone())
                .collect();
            let column_types: Vec<DataType> = final_output_schema
                .iter()
                .map(|(_, dt, _)| dt.clone())
                .collect();

            Ok((
                ExecuteResult::Select {
                    columns,
                    column_types: Some(column_types),
                    rows,
                    timezone: crate::session_context::current_timezone(),
                },
                captured_plan,
            ))
        })
    }
}

/// Check if any expression in the analyzed query needs pre-materialization.
///
/// This mirrors the scope of `pre_materialize_query_body` (pipeline.rs) to
/// determine whether a `Cow::Borrowed` query must be cloned. For `Cow::Owned`
/// callers this check is unnecessary (to_mut() on Owned is free).
pub(crate) fn query_needs_pre_materialization(analyzed: &AnalyzedQuery) -> bool {
    if analyzed
        .order_by
        .iter()
        .any(|ob| needs_pre_materialization(&ob.expr))
    {
        return true;
    }
    match &analyzed.body {
        AnalyzedQueryBody::Select(select) => {
            select
                .projection
                .iter()
                .any(|p| needs_pre_materialization(&p.expr))
                || select
                    .where_clause
                    .as_ref()
                    .is_some_and(needs_pre_materialization)
                || select
                    .having
                    .as_ref()
                    .is_some_and(needs_pre_materialization)
                || select.group_by.iter().any(needs_pre_materialization)
                || matches!(
                    &select.distinct,
                    AnalyzedDistinct::DistinctOn(exprs) if exprs.iter().any(needs_pre_materialization)
                )
                || select.from.iter().any(table_ref_needs_pre_materialization)
        }
        AnalyzedQueryBody::SetOperation { left, right, .. } => {
            query_needs_pre_materialization(left) || query_needs_pre_materialization(right)
        }
        AnalyzedQueryBody::Values(rows) => rows
            .iter()
            .any(|row| row.iter().any(needs_pre_materialization)),
    }
}

/// Check if a table ref tree contains JOIN ON conditions that need pre-materialization.
fn table_ref_needs_pre_materialization(tr: &AnalyzedTableRef) -> bool {
    match &tr.kind {
        AnalyzedTableRefKind::Join {
            left,
            right,
            condition,
            ..
        } => {
            table_ref_needs_pre_materialization(left)
                || table_ref_needs_pre_materialization(right)
                || matches!(condition, JoinCondition::On(expr) if needs_pre_materialization(expr))
        }
        _ => false,
    }
}

/// Compile-time guards for #907: these three functions MUST return
/// `Pin<Box<dyn Future<...>>>`, not opaque `impl Future` (from `async fn`).
/// Reverting any of them to `async fn` makes this a type error at `cargo build`.
#[allow(
    dead_code,
    unreachable_code,
    unused_variables,
    clippy::let_underscore_future,
    clippy::type_complexity
)]
mod _stack_overflow_signature_guards_907 {
    use super::*;

    fn _guard_execute_via_optimizer(
        e: &Executor,
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
        analyzed: AnalyzedQuery,
    ) {
        let ctes = HashMap::new();
        let _: Pin<
            Box<
                dyn Future<
                        Output = Result<(
                            ExecuteResult,
                            Option<crate::sql::optimizer::physical_plan::PhysicalPlan>,
                        )>,
                    > + Send
                    + '_,
            >,
        > = e.execute_via_optimizer(
            txn,
            0,
            seq,
            &[],
            Cow::Owned(analyzed),
            &ctes,
            &[],
            None,
            false,
        );
    }

    fn _guard_execute_subquery(
        e: &Executor,
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
        analyzed: &AnalyzedQuery,
    ) {
        let ctes = HashMap::new();
        let _: Pin<Box<dyn Future<Output = Result<ExecuteResult>> + Send + '_>> =
            e.execute_subquery(txn, 0, seq, &[], analyzed, &ctes);
    }

    fn _guard_try_execute_analyzed(
        e: &Executor,
        txn: &mut Transaction,
        seq: &mut HashMap<String, i64>,
        query: &Query,
    ) {
        let ctes = HashMap::new();
        let _: Pin<Box<dyn Future<Output = Result<ExecuteResult>> + Send + '_>> =
            e.try_execute_analyzed(txn, 0, seq, &[], query, &ctes, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use crate::sql::analyzer::types::{
        AnalyzedDistinct, AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind,
        BinaryOp as TypedBinaryOp,
    };
    use crate::sql::expr::typed_eval::eval_typed_expr;
    use crate::sql::query_context::QueryContext;
    use crate::sql::types::CastContext;
    use std::sync::Arc;

    fn test_qctx() -> QueryContext {
        QueryContext::new(
            1,
            Arc::from("testdb"),
            Arc::from("testuser"),
            0,
            0,
            Arc::from("UTC"),
        )
    }

    fn const_int(v: i32) -> TypedExpr {
        TypedExpr::new(TypedExprKind::Constant(Value::Int32(v)), DataType::Int32)
    }

    fn values_query() -> AnalyzedQuery {
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Values(vec![vec![const_int(1)]]),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("v".to_string(), DataType::Int32, None)],
        }
    }

    fn query_with_cte(name: &str) -> AnalyzedQuery {
        let cte = crate::sql::analyzer::types::AnalyzedCte {
            name: name.to_string(),
            query: values_query(),
            columns: vec![("v".to_string(), DataType::Int32, None)],
            materialized: None,
        };
        AnalyzedQuery {
            ctes: vec![cte],
            body: AnalyzedQueryBody::Values(vec![vec![const_int(2)]]),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("v".to_string(), DataType::Int32, None)],
        }
    }

    #[test]
    fn any_all_rhs_uses_implicit_cast_when_comparison_type_differs() {
        let rhs = build_any_all_rhs_constant_expr(
            &DataType::Int32,
            &DataType::Text,
            Value::Text("1".into()),
        );

        match rhs.kind {
            TypedExprKind::Cast {
                target_type,
                cast_context,
                ..
            } => {
                assert_eq!(target_type, DataType::Int32);
                assert_eq!(cast_context, CastContext::Implicit);
            }
            other => panic!("expected Cast rhs for mixed-type AnyAll, got {:?}", other),
        }
    }

    #[test]
    fn any_all_rhs_cast_avoids_runtime_cross_type_compare_error() {
        let rhs = build_any_all_rhs_constant_expr(
            &DataType::Int32,
            &DataType::Text,
            Value::Text("1".into()),
        );
        let expr = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int32(1)),
                    DataType::Int32,
                )),
                op: TypedBinaryOp::Eq,
                right: Box::new(rhs),
            },
            DataType::Boolean,
        );

        let result = eval_typed_expr(&expr, &Row::new(vec![]), &test_qctx()).unwrap();
        assert_eq!(result, Value::Boolean(true));
    }

    #[test]
    fn collect_immediate_nested_queries_finds_table_and_expr_subqueries() {
        let from_subquery = query_with_cte("from_cte");
        let expr_subquery = query_with_cte("expr_cte");
        let root = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![crate::sql::analyzer::types::AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ScalarSubquery(Box::new(expr_subquery)),
                        DataType::Int32,
                    ),
                    output_name: "x".to_string(),
                }],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(from_subquery)),
                    alias: Some("sq".to_string()),
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("x".to_string(), DataType::Int32, None)],
        };

        let nested = collect_immediate_nested_analyzed_queries(&root);
        let cte_names: Vec<String> = nested
            .iter()
            .map(|q| q.ctes.first().map(|c| c.name.clone()).unwrap_or_default())
            .collect();
        assert_eq!(
            cte_names,
            vec!["from_cte".to_string(), "expr_cte".to_string()]
        );
    }

    #[test]
    fn collect_immediate_nested_queries_finds_set_operation_branches() {
        let left = query_with_cte("left_cte");
        let right = query_with_cte("right_cte");
        let root = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::SetOperation {
                op: crate::sql::analyzer::types::SetOpKind::Union,
                all: true,
                left: Box::new(left),
                right: Box::new(right),
            },
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("v".to_string(), DataType::Int32, None)],
        };

        let nested = collect_immediate_nested_analyzed_queries(&root);
        let cte_names: Vec<String> = nested
            .iter()
            .map(|q| q.ctes.first().map(|c| c.name.clone()).unwrap_or_default())
            .collect();
        assert_eq!(
            cte_names,
            vec!["left_cte".to_string(), "right_cte".to_string()]
        );
    }

    #[test]
    fn collect_immediate_nested_queries_excludes_descendants() {
        let grandchild = query_with_cte("grandchild_cte");
        let child = AnalyzedQuery {
            ctes: vec![crate::sql::analyzer::types::AnalyzedCte {
                name: "child_cte".to_string(),
                query: values_query(),
                columns: vec![("v".to_string(), DataType::Int32, None)],
                materialized: None,
            }],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![crate::sql::analyzer::types::AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::ScalarSubquery(Box::new(grandchild)),
                        DataType::Int32,
                    ),
                    output_name: "v".to_string(),
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
            output_schema: vec![("v".to_string(), DataType::Int32, None)],
        };
        let root = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(child)),
                    alias: Some("child".to_string()),
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        };

        let nested = collect_immediate_nested_analyzed_queries(&root);
        assert_eq!(nested.len(), 1);
        let first_name = nested[0]
            .ctes
            .first()
            .map(|c| c.name.as_str())
            .unwrap_or_default();
        assert_eq!(first_name, "child_cte");
    }
}
