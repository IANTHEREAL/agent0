//! Analyzed SELECT execution path — the primary SELECT executor.
//!
//! All SELECT queries go through the Analyzer, producing `AnalyzedQuery` with
//! fully typed expressions. Async operations (subqueries, sequences) are
//! pre-materialized before building the operator tree.
//!
//! Handles FOR UPDATE/SHARE row locking and SELECT INTO natively.

use crate::sql::analyzer::types::{
    reindex_join_condition, AnalyzedDistinct, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef,
    AnalyzedTableRefKind, BinaryOp as TypedBinaryOp, JoinCondition, SetOpKind, TypedExpr,
    TypedExprKind, TypedFunctionArg, TypedOrderByExpr,
};
use crate::sql::analyzer::{AnalyzedQuery, Analyzer};
use crate::sql::executor::core::catalog_prefetch::build_catalog_snapshot;
use crate::sql::executor::core::view_rewrite::expand_views_in_query;
use crate::sql::executor::core::Executor;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::operators::{
    BoxedOperator, DistinctOnOperator, DistinctOperator, FilterOperator, HashAggregateOperator,
    JoinType as OpJoinType, LimitOperator, NestedLoopJoinOperator, PhysicalPlanner,
    ProjectOperator, SetOperationOperator, SetOperationType, SortOperator, TableScanOperator,
    WindowOperator,
};
use crate::sql::sequences::resolve_sequence_full_name_from_value;
use crate::sql::ExecuteResult;
use crate::types::{DataType, Row, TableSchema, Value};

use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use sqlparser::ast::{FunctionArg, FunctionArgExpr, ObjectName, Query, SetExpr};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tikv_client::Transaction;

mod expr_runtime;
mod joins;
mod materialize;
pub(crate) mod query_plan;
mod rewrite;
mod subquery;

use expr_runtime::*;
use joins::*;

use query_plan::*;
use rewrite::*;
use subquery::*;

impl Executor {
    /// Execute a query through the Analyzer path.
    ///
    /// This is the single execution path for all SELECT queries. The Analyzer
    /// produces fully typed expressions that are compiled into an operator tree.
    pub(crate) async fn try_execute_analyzed(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        // Pre-analysis rewrite: expand views into derived subqueries.
        //
        // Ensures the Analyzer and planner see a single query tree, and the
        // executor never needs runtime view expansion in table loading.
        let expanded_query =
            expand_views_in_query(self.store().as_ref(), txn, db_id, search_path, query).await?;

        // Build CatalogSnapshot (async: fetches table schemas from store).
        let catalog = build_catalog_snapshot(
            self.store().as_ref(),
            txn,
            db_id,
            search_path,
            self.tenant_keyspace(),
            &expanded_query,
            ctes,
        )
        .await?;

        // ── SELECT privilege check ──────────────────────────────────
        // Check that the current role has SELECT privilege on every base
        // table referenced in this query.  Virtual catalog tables
        // (information_schema, pg_catalog) are exempt.
        if current_role.is_some() {
            for table_name in catalog.base_table_full_names() {
                self.require_table_privilege(
                    txn,
                    current_role,
                    crate::auth::Privilege::Select,
                    table_name,
                )
                .await?;
            }
        }

        // Run the Analyzer (sync: name resolution + type checking).
        let mut analyzer = Analyzer::new(&catalog);
        let analyzed = analyzer
            .analyze_query(&expanded_query)
            .map_err(SqlError::from)?;

        // Post-analysis rewrite: flatten simple view subqueries back to
        // direct table references so the optimizer and planner can use
        // index-aware scan strategies.
        let analyzed = crate::sql::rewriter::rewrite_query(analyzed);

        // ── CBO optimizer routing gate ──────────────────────────────
        // When `SET tipg.use_optimizer = on`, eligible queries are routed through
        // the new optimizer pipeline: AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator.
        // Phase 3: single-table and multi-table joins without locks or SELECT INTO.
        if crate::sql::query_context::QueryContext::use_optimizer()
            && expanded_query.locks.is_empty()
            && crate::sql::optimizer::eligibility::is_optimizer_eligible(&analyzed)
        {
            let result = self
                .execute_via_optimizer(txn, db_id, sequence_values, search_path, &analyzed, ctes)
                .await?;
            // SELECT INTO post-processing.
            if let SetExpr::Select(select) = &*expanded_query.body {
                if let Some(ref into) = select.into {
                    return self
                        .create_table_from_result(txn, db_id, search_path, &into.name, result)
                        .await;
                }
            }
            return Ok(result);
        }

        // Single-point routing gate: ALL capability / routing decisions are made here.
        let plan = plan_query(&analyzed, &expanded_query.locks).map_err(SqlError::from)?;
        tracing::debug!(target: "pipeline", "{}", plan.trace_summary());

        // Execute through the analyzed path, driven by the plan.
        let result = self
            .execute_analyzed_query(
                txn,
                db_id,
                sequence_values,
                search_path,
                &analyzed,
                &expanded_query.locks,
                ctes,
                &plan,
            )
            .await?;

        // SELECT INTO post-processing: create table from result.
        if let SetExpr::Select(select) = &*expanded_query.body {
            if let Some(ref into) = select.into {
                return self
                    .create_table_from_result(txn, db_id, search_path, &into.name, result)
                    .await;
            }
        }

        Ok(result)
    }

    /// Execute a subquery (no locks). Computes its own `QueryPlan` internally.
    ///
    /// Used for recursive execution: subqueries in FROM, correlated subqueries,
    /// set-operation branches, INSERT...SELECT, etc.
    pub(crate) async fn execute_subquery(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        analyzed: &AnalyzedQuery,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        let plan = plan_query(analyzed, &[]).map_err(SqlError::from)?;
        self.execute_analyzed_query(
            txn,
            db_id,
            sequence_values,
            search_path,
            analyzed,
            &[],
            ctes,
            &plan,
        )
        .await
    }

    /// Execute a fully analyzed query, driven by the pre-computed `QueryPlan`.
    ///
    /// The `plan` captures ALL routing decisions (path, WHERE strategy, ORDER BY
    /// strategy, projection strategy, etc.).  This function dispatches by
    /// `plan.path` instead of re-computing predicates inline.
    async fn execute_analyzed_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        analyzed: &AnalyzedQuery,
        _locks: &[sqlparser::ast::LockClause],
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        plan: &QueryPlan,
    ) -> Result<ExecuteResult> {
        // ── Dispatch by path (plan computed by caller) ──────────────
        match plan.path {
            ExecutionPath::SetOperation { op, all } => {
                let AnalyzedQueryBody::SetOperation {
                    ref left,
                    ref right,
                    ..
                } = analyzed.body
                else {
                    return Err(anyhow!("Expected SetOperation in analyzed query"));
                };
                return self
                    .execute_analyzed_set_op(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        analyzed,
                        op,
                        all,
                        left,
                        right,
                        ctes,
                    )
                    .await;
            }
            ExecutionPath::Values => {
                let AnalyzedQueryBody::Values(value_rows) = &analyzed.body else {
                    return Err(anyhow!("Expected VALUES in analyzed query"));
                };
                return self
                    .execute_analyzed_values(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        analyzed,
                        value_rows,
                        ctes,
                    )
                    .await;
            }
            ExecutionPath::Tableless => {
                let AnalyzedQueryBody::Select(select) = &analyzed.body else {
                    return Err(anyhow!("Expected SELECT in analyzed query"));
                };
                return self
                    .execute_analyzed_tableless(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        analyzed,
                        select,
                        ctes,
                    )
                    .await;
            }
            ExecutionPath::Join => {
                if plan.lock.has_lock {
                    return Err(anyhow!("FOR UPDATE/SHARE is not allowed with JOIN queries"));
                }
                return self
                    .execute_analyzed_join(txn, db_id, sequence_values, search_path, analyzed, ctes)
                    .await;
            }
            ExecutionPath::SingleTable => {
                // Fall through to single-table path below.
            }
        }

        let AnalyzedQueryBody::Select(select) = &analyzed.body else {
            return Err(anyhow!("Expected SELECT in analyzed query"));
        };

        // Single-table path: get the table schema from the analyzer's resolved info.
        let table_ref = &select.from[0];
        let AnalyzedTableRefKind::Table { ref name, .. } = table_ref.kind else {
            return Err(anyhow!("Expected table reference"));
        };

        // We need the full TableSchema (with indexes, table_id, etc.) for physical planning.
        // The analyzer only stores column info. Re-fetch from store.
        let alias = table_ref.alias.as_deref().unwrap_or(name);
        let cte_key = name.to_lowercase();
        let (schema, preloaded_rows) = if let Some((cte_schema, cte_rows)) = ctes.get(&cte_key) {
            (cte_schema.clone(), Some(cte_rows.clone()))
        } else {
            // The Analyzer already resolved the name to a fully-qualified store key
            // (e.g. "public.atm_users"). Use it directly instead of re-resolving.
            if let Some(table_schema) = self.store().get_schema(txn, db_id, name).await? {
                (table_schema, None)
            } else {
                // Try information_schema / virtual tables
                let (schema, rows) = self
                    .get_table_data(txn, db_id, sequence_values, search_path, name, ctes)
                    .await?;
                let is_virtual = schema.table_id == 0;
                if is_virtual {
                    (schema, Some(rows))
                } else {
                    (schema, None)
                }
            }
        };

        // Set up from_alias on schema if needed.
        let mut schema = schema;
        let schema_short_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
        if !schema_short_name.eq_ignore_ascii_case(alias) {
            schema.from_alias = Some(alias.to_string());
        }

        let rt = ExprRuntime::new(self, db_id, search_path, ctes);

        // Pre-materialize WHERE and split into sync/async parts.
        let prepared_where = rt
            .prepare_where(&select.where_clause, txn, sequence_values)
            .await?;
        let sync_where = prepared_where.sync_part;
        let async_where = prepared_where.async_part;

        // Pre-materialize ORDER BY and classify as inline or deferred.
        let (order_by, deferred_order_by) = rt
            .prepare_order_by(&analyzed.order_by, txn, sequence_values)
            .await?;

        let limit = analyzed
            .limit
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?;
        let offset = analyzed
            .offset
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?
            .unwrap_or(0);

        // FOR UPDATE / FOR SHARE: use pre-computed lock info from plan.
        let has_lock = plan.lock.has_lock;
        let has_skip_locked = plan.lock.has_skip_locked;
        let has_nowait = plan.lock.has_nowait;

        if has_lock {
            if schema.pk_indices.is_empty() {
                return Err(anyhow!("FOR UPDATE/SHARE requires primary key"));
            }
            if plan.lock.has_for_share {
                tracing::warn!(
                    "FOR SHARE acquires exclusive locks on TiKV (no shared row locks); \
                     this is stricter than PostgreSQL where FOR SHARE allows concurrent readers"
                );
            }
            // Aggregate/window + DISTINCT checks already validated by plan_query().
        }

        // Build projection info from the analyzed output.
        let is_wildcard_only = select
            .projection
            .iter()
            .all(|p| matches!(p.expr.kind, TypedExprKind::ColumnRef { .. }))
            && select.projection.len() == schema.columns.len()
            && select
                .projection
                .iter()
                .enumerate()
                .all(|(i, p)| match &p.expr.kind {
                    TypedExprKind::ColumnRef { column_index, .. } => *column_index == i,
                    _ => false,
                });

        let columns: Vec<String> = analyzed
            .output_schema
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let column_types: Vec<DataType> = analyzed
            .output_schema
            .iter()
            .map(|(_, dt)| dt.clone())
            .collect();
        // Pre-materialize async expressions (subqueries, sequences) in projection.
        let projection_exprs = rt
            .pre_materialize_projections(&select.projection, txn, sequence_values)
            .await?;

        let planner = PhysicalPlanner::new(search_path.to_vec());
        let estimated_rows = self
            .stats_cache()
            .get_estimate(db_id, schema.table_id)
            .unwrap_or(1000);

        // Handle DISTINCT variants (using plan).
        let is_distinct = matches!(plan.distinct, DistinctStrategy::Distinct);
        let is_distinct_on = matches!(plan.distinct, DistinctStrategy::DistinctOn);

        if is_distinct_on {
            return self
                .execute_analyzed_distinct_on(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &schema,
                    &planner,
                    estimated_rows,
                    select,
                    analyzed,
                    &columns,
                    &column_types,
                    &projection_exprs,
                    ctes,
                    preloaded_rows,
                )
                .await;
        }

        // Aggregate/window path: build scan+filter, then route to shared pipeline.
        if plan.features.has_aggregates || plan.features.has_windows {
            let base_op: BoxedOperator = if let Some(rows) = preloaded_rows {
                let mut op: BoxedOperator =
                    Box::new(TableScanOperator::new_with_rows(schema.clone(), rows));
                if let Some(ref f) = select.where_clause {
                    op = Box::new(FilterOperator::new(op, f.clone()));
                }
                op
            } else {
                // Use planner for scan+filter only (no sort/limit — those come post-aggregate/window).
                planner.plan_simple_select(
                    db_id,
                    schema.clone(),
                    select.where_clause.as_ref(),
                    Vec::new(),
                    None,
                    0,
                    estimated_rows,
                )?
            };
            return self
                .execute_analyzed_pipeline(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    base_op,
                    analyzed,
                    select,
                    ctes,
                )
                .await;
        }

        // Build operator tree.
        // For SKIP LOCKED, scan all matching rows without LIMIT/OFFSET so we can
        // try-lock each row and apply pagination to the locked subset.
        // Also defer LIMIT/OFFSET when ORDER BY is deferred (sorting must happen first).
        let (exec_limit, exec_offset) = if has_skip_locked || deferred_order_by.is_some() {
            (None, 0)
        } else {
            (limit, offset)
        };

        let preloaded_source: Option<BoxedOperator> = preloaded_rows.map(|rows| {
            Box::new(TableScanOperator::new_with_rows(schema.clone(), rows)) as BoxedOperator
        });

        let base_operator = if let Some(mut op) = preloaded_source {
            if let Some(ref f) = sync_where {
                op = Box::new(FilterOperator::new(op, f.clone()));
            }
            if !order_by.is_empty() {
                op = Box::new(SortOperator::new(op, order_by.clone()));
            }
            if exec_limit.is_some() || exec_offset > 0 {
                op = Box::new(LimitOperator::new(op, exec_limit, exec_offset));
            }
            op
        } else {
            planner.plan_simple_select(
                db_id,
                schema.clone(),
                sync_where.as_ref(),
                order_by.clone(),
                exec_limit,
                exec_offset,
                estimated_rows,
            )?
        };

        let mut operator: BoxedOperator = if is_distinct && !is_wildcard_only {
            // For DISTINCT with non-wildcard projection: project first, then deduplicate.
            let project_op = Box::new(ProjectOperator::new(
                base_operator,
                projection_exprs.clone(),
                columns.clone(),
                column_types.clone(),
            ));
            Box::new(DistinctOperator::new(project_op))
        } else if is_distinct {
            Box::new(DistinctOperator::new(base_operator))
        } else {
            base_operator
        };

        let rows = rt
            .run_operator_tree(&mut operator, txn, sequence_values)
            .await?;

        // Async post-filter: evaluate WHERE conjuncts with correlated subqueries per-row.
        let rows = rt
            .filter_async(rows, async_where.as_ref(), &schema, txn, sequence_values)
            .await?;

        // FOR UPDATE / FOR SHARE: lock rows before projection (need raw rows with PK).
        let rows = if has_lock {
            let table_name = &schema.name;
            if has_skip_locked {
                let max_locks = limit.map(|l| offset + l);
                let locked_indices = self
                    .store()
                    .lock_rows_skip_locked(txn, db_id, table_name, &rows, max_locks)
                    .await?;
                let locked_rows: Vec<Row> =
                    locked_indices.iter().map(|&i| rows[i].clone()).collect();
                // Apply offset + limit to the locked subset.
                let start = offset.min(locked_rows.len());
                let end = limit.map_or(locked_rows.len(), |l| (start + l).min(locked_rows.len()));
                locked_rows[start..end].to_vec()
            } else if has_nowait {
                self.store()
                    .lock_rows_nowait(txn, db_id, table_name, &rows)
                    .await?;
                rows
            } else {
                self.store()
                    .lock_rows(txn, db_id, table_name, &rows)
                    .await?;
                rows
            }
        } else {
            rows
        };

        // Apply projection if not wildcard and not already applied in DISTINCT path.
        let rows = if is_wildcard_only || (is_distinct && !is_wildcard_only) {
            rows
        } else {
            rt.project_rows(rows, &projection_exprs, &schema, txn, sequence_values)
                .await?
        };

        // Deferred ORDER BY: when ORDER BY contained subqueries (e.g. alias → ScalarSubquery),
        // sorting was deferred to after projection. Now sort the projected rows.
        // The ORDER BY expressions reference table-scope columns, but the Analyzer clones
        // projection expressions for alias matches. We find the matching output column index
        // by checking which output_schema column name matches the ORDER BY's column_name hint.
        let rows = if let Some(ref deferred_ob) = deferred_order_by {
            sort_projected_rows(rows, deferred_ob, &analyzed.output_schema, limit, offset)?
        } else {
            rows
        };

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    /// Execute DISTINCT ON through the analyzed path.
    #[allow(clippy::too_many_arguments)]
    async fn execute_analyzed_distinct_on(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        schema: &TableSchema,
        planner: &PhysicalPlanner,
        estimated_rows: usize,
        select: &AnalyzedSelect,
        analyzed: &AnalyzedQuery,
        columns: &[String],
        column_types: &[DataType],
        projection_exprs: &[TypedExpr],
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        preloaded_rows: Option<Vec<Row>>,
    ) -> Result<ExecuteResult> {
        let AnalyzedDistinct::DistinctOn(ref on_exprs) = select.distinct else {
            return Err(anyhow!("Expected DISTINCT ON"));
        };

        let where_clause = select.where_clause.clone();
        let order_by = analyzed.order_by.clone();
        let limit = analyzed
            .limit
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?;
        let offset = analyzed
            .offset
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?
            .unwrap_or(0);

        // Build scan with filter only (no sort/limit yet — DISTINCT ON needs full sort).
        let scan_operator: BoxedOperator = if let Some(rows) = preloaded_rows {
            let mut op: BoxedOperator =
                Box::new(TableScanOperator::new_with_rows(schema.clone(), rows));
            if let Some(ref f) = where_clause {
                op = Box::new(FilterOperator::new(op, f.clone()));
            }
            op
        } else {
            let typed_filter = where_clause.as_ref();
            planner.plan_simple_select(
                db_id,
                schema.clone(),
                typed_filter,
                Vec::new(),
                None,
                0,
                estimated_rows,
            )?
        };

        // Sort by ORDER BY expressions (pre-projection).
        let sorted: BoxedOperator = if !order_by.is_empty() {
            Box::new(SortOperator::new(scan_operator, order_by))
        } else {
            scan_operator
        };

        // DISTINCT ON deduplication.
        let distincted: BoxedOperator = Box::new(DistinctOnOperator::new(sorted, on_exprs.clone()));

        // Project.
        let projected: BoxedOperator = Box::new(ProjectOperator::new(
            distincted,
            projection_exprs.to_vec(),
            columns.to_vec(),
            column_types.to_vec(),
        ));

        // LIMIT/OFFSET.
        let mut operator: BoxedOperator = if limit.is_some() || offset > 0 {
            Box::new(LimitOperator::new(projected, limit, offset))
        } else {
            projected
        };

        let rt = ExprRuntime::new(self, db_id, search_path, ctes);
        let rows = rt
            .run_operator_tree(&mut operator, txn, sequence_values)
            .await?;

        Ok(ExecuteResult::Select {
            columns: columns.to_vec(),
            column_types: Some(column_types.to_vec()),
            rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    // ── VALUES query path ──────────────────────────────────────

    /// Execute an analyzed standalone VALUES query.
    ///
    /// Expressions are evaluated from typed IR with async materialization for
    /// subqueries/catalog-dependent functions.
    async fn execute_analyzed_values(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        analyzed: &AnalyzedQuery,
        value_rows: &[Vec<TypedExpr>],
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        let columns: Vec<String> = analyzed
            .output_schema
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let column_types: Vec<DataType> = analyzed
            .output_schema
            .iter()
            .map(|(_, t)| t.clone())
            .collect();

        let schema = build_set_op_schema(&columns, &column_types);
        let dummy_row = Row::new(vec![]);
        let rt = ExprRuntime::new(self, db_id, search_path, ctes);

        // Evaluate VALUES rows.
        let mut rows = Vec::with_capacity(value_rows.len());
        for expr_row in value_rows {
            let mut pre_materialized = Vec::with_capacity(expr_row.len());
            for expr in expr_row {
                pre_materialized.push(rt.pre_materialize(expr, txn, sequence_values).await?);
            }
            let materialized = rt
                .materialize_exprs_for_row(
                    pre_materialized,
                    &dummy_row,
                    &schema,
                    txn,
                    sequence_values,
                )
                .await?;
            let mut values = Vec::with_capacity(materialized.len());
            for expr in &materialized {
                values.push(rt.eval(expr, &dummy_row)?);
            }
            rows.push(Row::new(values));
        }

        // ORDER BY for VALUES: evaluate ORDER BY keys per row, then sort by keys.
        if !analyzed.order_by.is_empty() {
            let mut keyed_rows: Vec<(Row, Vec<Value>)> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut keys = Vec::with_capacity(analyzed.order_by.len());
                for ob in &analyzed.order_by {
                    keys.push(
                        rt.resolve_and_eval(&ob.expr, &row, &schema, txn, sequence_values)
                            .await?,
                    );
                }
                keyed_rows.push((row, keys));
            }

            crate::sql::expr::operators::sort_by_fallible(&mut keyed_rows, |a, b| {
                for (idx, ob) in analyzed.order_by.iter().enumerate() {
                    let ord = crate::sql::expr::compare_order_by_values(
                        &a.1[idx],
                        &b.1[idx],
                        ob.asc,
                        ob.nulls_first,
                    )?;
                    if ord != std::cmp::Ordering::Equal {
                        return Ok(ord);
                    }
                }
                Ok(std::cmp::Ordering::Equal)
            })?;

            rows = keyed_rows.into_iter().map(|(row, _)| row).collect();
        }

        // LIMIT/OFFSET for VALUES.
        let limit = analyzed
            .limit
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?;
        let offset = analyzed
            .offset
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?
            .unwrap_or(0);
        let start = offset.min(rows.len());
        let end = limit.map_or(rows.len(), |l| (start + l).min(rows.len()));
        let rows = rows[start..end].to_vec();

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    // ── Tableless query path ───────────────────────────────────

    /// Execute a tableless query (no FROM clause) through the analyzed path.
    ///
    /// Creates a zero-column dummy input with one row, then runs through the
    /// standard operator pipeline (ProjectOperator handles SRF expansion).
    #[allow(clippy::too_many_arguments)]
    async fn execute_analyzed_tableless(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        analyzed: &AnalyzedQuery,
        select: &AnalyzedSelect,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        // Build a zero-column schema with one empty row.
        let dummy_schema = TableSchema {
            name: "dual".to_string(),
            table_id: 0,
            columns: vec![],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let dummy_row = Row::new(vec![]);
        let base_op: BoxedOperator = Box::new(TableScanOperator::new_with_rows(
            dummy_schema.clone(),
            vec![dummy_row.clone()],
        ));

        // Aggregate/window path uses the shared pipeline.
        if has_aggregates(select) || has_windows(select) {
            return self
                .execute_analyzed_pipeline(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    base_op,
                    analyzed,
                    select,
                    ctes,
                )
                .await;
        }

        // Non-aggregate path: project, then ORDER BY / LIMIT / DISTINCT.
        let columns: Vec<String> = analyzed
            .output_schema
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let column_types: Vec<DataType> = analyzed
            .output_schema
            .iter()
            .map(|(_, t)| t.clone())
            .collect();
        let rt = ExprRuntime::new(self, db_id, search_path, ctes);

        // Pre-materialize async expressions in projection.
        let projection_exprs = rt
            .pre_materialize_projections(&select.projection, txn, sequence_values)
            .await?;

        // Materialize catalog-dependent functions (e.g. pg_get_indexdef) in tableless context
        // so the pure ProjectOperator can evaluate the remaining expression tree.
        let projection_exprs_materialized = rt
            .materialize_exprs_for_row(
                projection_exprs,
                &dummy_row,
                &dummy_schema,
                txn,
                sequence_values,
            )
            .await?;

        // Project first (handles SRF expansion).
        let mut op: BoxedOperator = Box::new(ProjectOperator::new(
            base_op,
            projection_exprs_materialized,
            columns.clone(),
            column_types.clone(),
        ));

        // ORDER BY.
        if !analyzed.order_by.is_empty() {
            op = Box::new(SortOperator::new(op, analyzed.order_by.clone()));
        }

        // LIMIT / OFFSET.
        let limit = analyzed
            .limit
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?;
        let offset = analyzed
            .offset
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?
            .unwrap_or(0);
        if limit.is_some() || offset > 0 {
            op = Box::new(LimitOperator::new(op, limit, offset));
        }

        // DISTINCT.
        if matches!(&select.distinct, AnalyzedDistinct::Distinct) {
            op = Box::new(DistinctOperator::new(op));
        }

        // Execute.
        let rows = rt.run_operator_tree(&mut op, txn, sequence_values).await?;

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    // ── Aggregate execution pipeline ────────────────────────────

    /// Shared pipeline for queries with aggregates and/or window functions.
    ///
    /// Builds: [HashAggregate → HAVING] → [Window] → ORDER BY → LIMIT →
    /// DISTINCT → Project → Execute.
    ///
    /// Called from both single-table and join paths when the query has
    /// aggregates or window functions (or both). `base_op` is the FROM+WHERE
    /// operator (pre-aggregate rows).
    #[allow(clippy::too_many_arguments)]
    async fn execute_analyzed_pipeline(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        base_op: BoxedOperator,
        analyzed: &AnalyzedQuery,
        select: &AnalyzedSelect,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        let has_agg = has_aggregates(select);
        let has_win = has_windows(select);

        let mut op: BoxedOperator = base_op;

        // ── Phase 1: Aggregate ──────────────────────────────────
        let agg_analysis = if has_agg {
            let analysis = build_aggregate_analysis(select, &analyzed.order_by);

            op = Box::new(HashAggregateOperator::new(
                op,
                analysis.group_by_exprs.clone(),
                analysis.aggregate_exprs.clone(),
                analysis.group_by_names.clone(),
                analysis.group_by_types.clone(),
                analysis.aggregate_names.clone(),
                analysis.aggregate_types.clone(),
            ));

            // HAVING filter (rewritten for post-aggregate positions).
            if let Some(ref having) = select.having {
                let rewritten = rewrite_for_post_aggregate(having, &analysis);
                op = Box::new(FilterOperator::new(op, rewritten));
            }

            Some(analysis)
        } else {
            None
        };

        // Rewrite projection and ORDER BY for post-aggregate positions.
        let mut rewritten_projection: Vec<TypedExpr> = if let Some(ref analysis) = agg_analysis {
            select
                .projection
                .iter()
                .map(|p| rewrite_for_post_aggregate(&p.expr, analysis))
                .collect()
        } else {
            select.projection.iter().map(|p| p.expr.clone()).collect()
        };

        let mut rewritten_order_by: Vec<TypedOrderByExpr> = if let Some(ref analysis) = agg_analysis
        {
            analyzed
                .order_by
                .iter()
                .map(|o| TypedOrderByExpr {
                    expr: rewrite_for_post_aggregate(&o.expr, analysis),
                    asc: o.asc,
                    nulls_first: o.nulls_first,
                })
                .collect()
        } else {
            analyzed.order_by.clone()
        };

        // ── Phase 2: Window functions ───────────────────────────
        if has_win {
            let input_col_count = op.schema().columns.len();

            // Extract WindowCall nodes from (possibly rewritten) projection.
            let window_functions = extract_window_functions(&rewritten_projection, select);
            if !window_functions.is_empty() {
                op = Box::new(WindowOperator::new(op, window_functions));

                // Rewrite projection: WindowCall → ColumnRef at window output position.
                let mut win_counter = 0usize;
                rewritten_projection = rewritten_projection
                    .iter()
                    .map(|e| rewrite_for_post_window(e, input_col_count, &mut win_counter))
                    .collect();

                // Rewrite ORDER BY for window positions too (rare but valid).
                let mut win_counter_ob = 0usize;
                rewritten_order_by = rewritten_order_by
                    .iter()
                    .map(|o| TypedOrderByExpr {
                        expr: rewrite_for_post_window(
                            &o.expr,
                            input_col_count,
                            &mut win_counter_ob,
                        ),
                        asc: o.asc,
                        nulls_first: o.nulls_first,
                    })
                    .collect();
            }
        }

        // ── Phase 3: ORDER BY, LIMIT, DISTINCT, Project, Execute ─
        if !rewritten_order_by.is_empty() {
            op = Box::new(SortOperator::new(op, rewritten_order_by));
        }

        let limit = analyzed
            .limit
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?;
        let offset = analyzed
            .offset
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?
            .unwrap_or(0);
        if limit.is_some() || offset > 0 {
            op = Box::new(LimitOperator::new(op, limit, offset));
        }

        let columns: Vec<String> = analyzed
            .output_schema
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let column_types: Vec<DataType> = analyzed
            .output_schema
            .iter()
            .map(|(_, t)| t.clone())
            .collect();

        // DISTINCT ON: apply after aggregate/window, before final projection.
        if let AnalyzedDistinct::DistinctOn(ref on_exprs) = select.distinct {
            // Rewrite ON exprs for post-aggregate/window positions if needed.
            let rewritten_on: Vec<TypedExpr> = if let Some(ref analysis) = agg_analysis {
                on_exprs
                    .iter()
                    .map(|e| rewrite_for_post_aggregate(e, analysis))
                    .collect()
            } else {
                on_exprs.clone()
            };
            op = Box::new(DistinctOnOperator::new(op, rewritten_on));
        }

        let rt = ExprRuntime::new(self, db_id, search_path, ctes);

        // DISTINCT: project first, then deduplicate.
        let is_distinct = matches!(&select.distinct, AnalyzedDistinct::Distinct);
        if is_distinct {
            let project_op = Box::new(ProjectOperator::new(
                op,
                rewritten_projection,
                columns.clone(),
                column_types.clone(),
            ));
            op = Box::new(DistinctOperator::new(project_op));

            let rows = rt.run_operator_tree(&mut op, txn, sequence_values).await?;

            return Ok(ExecuteResult::Select {
                columns,
                column_types: Some(column_types),
                rows,
                timezone: crate::session_context::current_timezone(),
            });
        }

        // Execute operator tree, then apply projection.
        let rows = rt.run_operator_tree(&mut op, txn, sequence_values).await?;

        // Pipeline projection is always sync (aggregates/windows are already resolved).
        let dummy_schema = TableSchema {
            name: "pipeline".to_string(),
            table_id: 0,
            columns: vec![],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };
        let projected = rt
            .project_rows(
                rows,
                &rewritten_projection,
                &dummy_schema,
                txn,
                sequence_values,
            )
            .await?;

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected,
            timezone: crate::session_context::current_timezone(),
        })
    }

    // ── JOIN execution path ─────────────────────────────────────

    /// Execute a query with joins through the analyzed path.
    async fn execute_analyzed_join(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        analyzed: &AnalyzedQuery,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        let AnalyzedQueryBody::Select(select) = &analyzed.body else {
            return Err(anyhow!("Expected SELECT in analyzed query"));
        };

        // Build the FROM operator tree (handles joins + implicit cross joins).
        let from_op = self
            .build_from_operators(txn, db_id, sequence_values, search_path, &select.from, ctes)
            .await?;

        let rt = ExprRuntime::new(self, db_id, search_path, ctes);

        // Pre-materialize WHERE and split into sync/async parts.
        let prepared_where = rt
            .prepare_where(&select.where_clause, txn, sequence_values)
            .await?;
        let sync_where = prepared_where.sync_part;
        let async_where = prepared_where.async_part;

        let mut op: BoxedOperator = from_op;
        if let Some(ref f) = sync_where {
            op = Box::new(FilterOperator::new(op, f.clone()));
        }

        // Aggregate/window path: route to shared pipeline after FROM+WHERE.
        if has_aggregates(select) || has_windows(select) {
            if async_where.is_some() {
                return Err(anyhow!(
                    "correlated subqueries or catalog-dependent functions in WHERE are not yet supported with aggregates/windows"
                ));
            }
            return self
                .execute_analyzed_pipeline(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    op,
                    analyzed,
                    select,
                    ctes,
                )
                .await;
        }

        // Pre-materialize ORDER BY and classify as inline or deferred.
        let (order_by, deferred_order_by) = rt
            .prepare_order_by(&analyzed.order_by, txn, sequence_values)
            .await?;

        // Add ORDER BY.
        if !order_by.is_empty() {
            op = Box::new(SortOperator::new(op, order_by));
        }

        // Add LIMIT/OFFSET.
        let limit = analyzed
            .limit
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?;
        let offset = analyzed
            .offset
            .as_ref()
            .map(|e| eval_const_usize(e))
            .transpose()?
            .unwrap_or(0);
        if deferred_order_by.is_none() && (limit.is_some() || offset > 0) {
            op = Box::new(LimitOperator::new(op, limit, offset));
        }

        // DISTINCT handling.
        let is_distinct = matches!(&select.distinct, AnalyzedDistinct::Distinct);
        let is_distinct_on = matches!(&select.distinct, AnalyzedDistinct::DistinctOn(_));

        // Build projection info from the analyzed output.
        let columns: Vec<String> = analyzed
            .output_schema
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let column_types: Vec<DataType> = analyzed
            .output_schema
            .iter()
            .map(|(_, t)| t.clone())
            .collect();
        // Pre-materialize async expressions in projection.
        let projection_exprs = rt
            .pre_materialize_projections(&select.projection, txn, sequence_values)
            .await?;

        let input_col_count = op.schema().columns.len();
        let is_wildcard_only = projection_exprs.len() == input_col_count
            && projection_exprs
                .iter()
                .enumerate()
                .all(|(i, p)| match &p.kind {
                    TypedExprKind::ColumnRef { column_index, .. } => *column_index == i,
                    _ => false,
                });

        // DISTINCT ON: sort + deduplicate + project + limit.
        if is_distinct_on {
            let AnalyzedDistinct::DistinctOn(ref on_exprs) = select.distinct else {
                return Err(anyhow!("Expected DISTINCT ON"));
            };

            // Re-build without ORDER BY/LIMIT (DISTINCT ON needs full sort first).
            let from_op = self
                .build_from_operators(txn, db_id, sequence_values, search_path, &select.from, ctes)
                .await?;
            let mut dop: BoxedOperator = from_op;
            if let Some(ref where_clause) = select.where_clause {
                dop = Box::new(FilterOperator::new(dop, where_clause.clone()));
            }
            if !analyzed.order_by.is_empty() {
                dop = Box::new(SortOperator::new(dop, analyzed.order_by.clone()));
            }
            dop = Box::new(DistinctOnOperator::new(dop, on_exprs.clone()));
            dop = Box::new(ProjectOperator::new(
                dop,
                projection_exprs,
                columns.clone(),
                column_types.clone(),
            ));
            if limit.is_some() || offset > 0 {
                dop = Box::new(LimitOperator::new(dop, limit, offset));
            }

            let rows = rt.run_operator_tree(&mut dop, txn, sequence_values).await?;

            return Ok(ExecuteResult::Select {
                columns,
                column_types: Some(column_types),
                rows,
                timezone: crate::session_context::current_timezone(),
            });
        }

        // DISTINCT with non-wildcard: project first, then deduplicate.
        let projection_needs_materialization = projection_exprs.iter().any(|e| needs_async(e));
        let mut post_distinct = false;
        if is_distinct && !is_wildcard_only {
            if projection_needs_materialization || async_where.is_some() {
                // Can't use ProjectOperator for catalog-dependent functions or correlated subqueries.
                // Apply DISTINCT after executor-level projection.
                post_distinct = true;
            } else {
                let project_op = Box::new(ProjectOperator::new(
                    op,
                    projection_exprs.clone(),
                    columns.clone(),
                    column_types.clone(),
                ));
                op = Box::new(DistinctOperator::new(project_op));
            }
        } else if is_distinct {
            op = Box::new(DistinctOperator::new(op));
        }

        // Execute operator tree.
        let input_schema = op.schema().clone();
        let rows = rt.run_operator_tree(&mut op, txn, sequence_values).await?;

        // Async post-filter for WHERE parts that require per-row materialization.
        let rows = rt
            .filter_async(
                rows,
                async_where.as_ref(),
                &input_schema,
                txn,
                sequence_values,
            )
            .await?;

        // Apply projection (executor-level materialization for subqueries/catalog funcs).
        let already_projected = is_distinct && !is_wildcard_only && !post_distinct;
        let rows = if is_wildcard_only || already_projected {
            rows
        } else {
            rt.project_rows(rows, &projection_exprs, &input_schema, txn, sequence_values)
                .await?
        };

        // Post-projection DISTINCT (only needed when we couldn't use ProjectOperator).
        let rows = if post_distinct {
            let schema = build_set_op_schema(&columns, &column_types);
            let mut dop: BoxedOperator =
                Box::new(TableScanOperator::new_with_rows(schema, rows)) as BoxedOperator;
            dop = Box::new(DistinctOperator::new(dop));
            rt.run_operator_tree(&mut dop, txn, sequence_values).await?
        } else {
            rows
        };

        // Deferred ORDER BY: sort projected rows after per-row materialization and apply LIMIT/OFFSET.
        let rows = if let Some(ref deferred_ob) = deferred_order_by {
            sort_projected_rows(rows, deferred_ob, &analyzed.output_schema, limit, offset)?
        } else {
            rows
        };

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    // ── FROM operator tree builders ─────────────────────────────

    /// Build operator tree from the FROM clause (handles joins + implicit cross joins).
    fn build_from_operators<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        from: &'a [AnalyzedTableRef],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<BoxedOperator>> + Send + 'a>> {
        Box::pin(async move {
            assert!(!from.is_empty(), "FROM clause must not be empty");

            // Build operator for first FROM item.
            let mut result = self
                .build_table_ref_operator(txn, db_id, sequence_values, search_path, &from[0], ctes)
                .await?;

            // Implicit cross joins for additional FROM items (FROM a, b).
            for table_ref in &from[1..] {
                let right_op = self
                    .build_table_ref_operator(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        table_ref,
                        ctes,
                    )
                    .await?;
                result = Box::new(NestedLoopJoinOperator::new(
                    result,
                    right_op,
                    OpJoinType::Cross,
                    None,
                ));
            }

            Ok(result)
        })
    }

    /// Recursively build an operator from a single AnalyzedTableRef (may be a join tree).
    fn build_table_ref_operator<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        table_ref: &'a AnalyzedTableRef,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<BoxedOperator>> + Send + 'a>> {
        Box::pin(async move {
            match &table_ref.kind {
                AnalyzedTableRefKind::Table { name, .. } => {
                    let alias = table_ref.alias.as_deref().unwrap_or(name);
                    self.build_scan_operator(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        name,
                        alias,
                        ctes,
                    )
                    .await
                }

                AnalyzedTableRefKind::Join {
                    left,
                    right,
                    join_type,
                    condition,
                    left_col_start,
                } => {
                    let mut left_op = self
                        .build_table_ref_operator(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            left,
                            ctes,
                        )
                        .await?;
                    let mut right_op = self
                        .build_table_ref_operator(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            right,
                            ctes,
                        )
                        .await?;

                    let left_schema = left_op.schema().clone();
                    let right_schema = right_op.schema().clone();
                    let left_col_count = left_schema.columns.len();

                    // Reindex ON condition: global column indices → local indices
                    // for this join pair. Needed when this join is nested inside a
                    // larger join tree (e.g. `A JOIN (B JOIN C ON ...)`), where the
                    // analyzer assigns global indices starting at left_col_start > 0.
                    let local_condition = if *left_col_start > 0 {
                        reindex_join_condition(condition, *left_col_start)
                    } else {
                        condition.clone()
                    };

                    // Pre-materialize async exprs in ON condition.
                    let materialized_condition = match &local_condition {
                        JoinCondition::On(expr) => {
                            let m = self
                                .pre_materialize_async_exprs(
                                    expr,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    ctes,
                                )
                                .await?;
                            JoinCondition::On(m)
                        }
                        other => other.clone(),
                    };

                    // If JOIN ON still contains correlated subqueries or catalog-dependent
                    // functions, force a nested-loop join with per-row materialization.
                    if let JoinCondition::On(ref on_expr) = materialized_condition {
                        if needs_async(on_expr) {
                            let rt = ExprRuntime::new(self, db_id, search_path, ctes);
                            let left_rows = rt
                                .run_operator_tree(&mut left_op, txn, sequence_values)
                                .await?;
                            let right_rows = rt
                                .run_operator_tree(&mut right_op, txn, sequence_values)
                                .await?;
                            let output_schema =
                                build_join_output_schema(&left_schema, &right_schema);
                            let result_rows = execute_async_nested_loop_join(
                                self,
                                *join_type,
                                &left_rows,
                                &right_rows,
                                on_expr,
                                &output_schema,
                                left_col_count,
                                right_schema.columns.len(),
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?;
                            return Ok(Box::new(TableScanOperator::new_with_rows(
                                output_schema,
                                result_rows,
                            )) as BoxedOperator);
                        }
                    }
                    build_join_operator(
                        left_op,
                        right_op,
                        *join_type,
                        &materialized_condition,
                        left_col_count,
                    )
                }

                AnalyzedTableRefKind::Subquery(inner_query) => {
                    // Recursively execute the subquery and materialize results.
                    let result = self
                        .execute_subquery(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner_query,
                            ctes,
                        )
                        .await?;
                    let (cols, types, rows) = match result {
                        ExecuteResult::Select {
                            columns,
                            column_types,
                            rows,
                            ..
                        } => (columns, column_types.unwrap_or_default(), rows),
                        _ => {
                            return Err(anyhow!(
                                "Expected SELECT result from derived table subquery"
                            ))
                        }
                    };

                    let alias = table_ref.alias.as_deref().unwrap_or("subquery");
                    let mut schema = build_set_op_schema(&cols, &types);
                    schema.name = alias.to_string();
                    schema.from_alias = Some(alias.to_string());
                    Ok(Box::new(TableScanOperator::new_with_rows(schema, rows)) as BoxedOperator)
                }

                AnalyzedTableRefKind::Function {
                    func,
                    args,
                    output_columns,
                } => {
                    // Build output schema from analyzer-resolved output columns.
                    let schema = TableSchema {
                        name: table_ref.alias.as_deref().unwrap_or(&func.name).to_string(),
                        table_id: 0,
                        columns: output_columns
                            .iter()
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
                        from_alias: table_ref.alias.clone(),
                    };

                    // Evaluate typed args to concrete Values, then bridge to FunctionArg.
                    let qc = crate::sql::query_context::QueryContext::from_task_locals();
                    let dummy_row = Row::new(vec![]);
                    let mut bridge_args: Vec<FunctionArg> = Vec::with_capacity(args.len());
                    for tfa in args {
                        let (name_opt, typed_expr) = match tfa {
                            TypedFunctionArg::Positional(e) => (None, e),
                            TypedFunctionArg::Named { name, expr } => (Some(name.clone()), expr),
                        };
                        let val = eval_typed_expr(typed_expr, &dummy_row, &qc)?;
                        let sql_expr = crate::sql::value_coercion::value_to_sql_expr(&val);
                        let fa = match name_opt {
                            None => FunctionArg::Unnamed(FunctionArgExpr::Expr(sql_expr)),
                            Some(n) => FunctionArg::Named {
                                name: sqlparser::ast::Ident::new(n),
                                arg: FunctionArgExpr::Expr(sql_expr),
                            },
                        };
                        bridge_args.push(fa);
                    }

                    let func_upper = func.name.to_uppercase();

                    let (_, rows) = if func_upper == "GENERATE_SERIES" {
                        self.execute_generate_series(
                            &bridge_args,
                            schema.name.as_str(),
                            None,
                            0,
                            None,
                        )
                        .await?
                    } else if func_upper == "_PGTIKV_SYS_RECORD_MIGRATION" {
                        self.execute_record_migration(txn, &bridge_args).await?
                    } else {
                        // Try extension table function.
                        let obj_name =
                            ObjectName(vec![sqlparser::ast::Ident::new(func.name.clone())]);
                        if let Some(result) = self
                            .try_execute_extension_table_function(
                                txn,
                                db_id,
                                search_path,
                                &obj_name,
                                &bridge_args,
                                None,
                            )
                            .await?
                        {
                            match result {
                                crate::sql::executor::extensions::ExtensionTableFunctionResult::Batch(
                                    _s,
                                    rows,
                                ) => (schema.clone(), rows),
                                crate::sql::executor::extensions::ExtensionTableFunctionResult::Streaming(
                                    _s,
                                    mut operator,
                                ) => {
                                    let rt = ExprRuntime::new(self, db_id, search_path, ctes);
                                    let rows = rt.run_operator_tree(&mut operator, txn, sequence_values).await?;
                                    (schema.clone(), rows)
                                }
                            }
                        } else if let Some((_, rows)) = self
                            .try_execute_user_table_function(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                &obj_name,
                                &bridge_args,
                                None,
                            )
                            .await?
                        {
                            (schema.clone(), rows)
                        } else {
                            // Scalar-in-FROM: construct FunctionCall and evaluate via typed_eval.
                            let typed_args: Vec<TypedExpr> = args
                                .iter()
                                .map(|a| match a {
                                    TypedFunctionArg::Positional(e) => e.clone(),
                                    TypedFunctionArg::Named { expr, .. } => expr.clone(),
                                })
                                .collect();

                            let mut scalar_arg_values = Vec::with_capacity(typed_args.len());
                            for arg in &typed_args {
                                scalar_arg_values.push(eval_typed_expr(arg, &dummy_row, &qc)?);
                            }
                            if let Some(result) =
                                crate::sql::executor::execute_cron_scalar_function(
                                    &self.store(),
                                    txn,
                                    db_id,
                                    qc.current_user.as_ref(),
                                    qc.database_name.as_ref(),
                                    crate::extensions::context::is_superuser(),
                                    &func.name,
                                    &scalar_arg_values,
                                )
                                .await
                            {
                                return Ok(Box::new(TableScanOperator::new_with_rows(
                                    schema.clone(),
                                    vec![Row::new(vec![result?])],
                                )) as BoxedOperator);
                            }

                            let typed_expr = TypedExpr {
                                kind: TypedExprKind::FunctionCall {
                                    func: func.clone(),
                                    args: typed_args,
                                    order_by: vec![],
                                    filter: None,
                                },
                                data_type: func.return_type.clone(),
                            };
                            let val = eval_typed_expr(&typed_expr, &dummy_row, &qc)?;
                            (schema.clone(), vec![Row::new(vec![val])])
                        }
                    };

                    Ok(Box::new(TableScanOperator::new_with_rows(schema, rows)) as BoxedOperator)
                }
            }
        })
    }

    // ── Set operation execution path ──────────────────────────────

    // ── Subquery pre-materialization ────────────────────────────

    /// Pre-materialize all uncorrelated subqueries in a TypedExpr tree.
    ///
    /// Walks the tree and replaces:
    /// - `InSubquery { expr, subquery }` → `InList { expr, list: [constants...] }`
    /// - `EXISTS { subquery }` → `Constant(Bool(...))`
    /// - `ScalarSubquery(q)` → `Constant(value)`
    /// - `AnyAll { expr, op, subquery }` → expanded to `expr op ANY (ARRAY[...])`
    ///
    /// Only processes uncorrelated subqueries (no outer column references).
    /// Correlated subqueries are left as-is for per-row materialization.
    fn pre_materialize_async_exprs<'a>(
        &'a self,
        expr: &'a TypedExpr,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>> {
        Box::pin(async move {
            match &expr.kind {
                TypedExprKind::InSubquery {
                    expr: inner_expr,
                    subquery,
                    negated,
                } => {
                    if is_correlated_query(subquery) {
                        // Leave correlated IN-subquery as-is for per-row evaluation.
                        return Ok(expr.clone());
                    }
                    // Recurse into the LHS expression first.
                    let rewritten_expr = self
                        .pre_materialize_async_exprs(
                            inner_expr,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;

                    // Execute the subquery to get result rows.
                    let result = self
                        .execute_subquery(txn, db_id, sequence_values, search_path, subquery, ctes)
                        .await?;
                    let rows = match result {
                        ExecuteResult::Select { rows, .. } => rows,
                        _ => return Err(anyhow!("Expected SELECT from IN subquery")),
                    };

                    // Extract first column of each row as constant values.
                    let list: Vec<TypedExpr> = rows
                        .into_iter()
                        .filter_map(|r| r.values.into_iter().next())
                        .map(|v| TypedExpr {
                            data_type: rewritten_expr.data_type.clone(),
                            kind: TypedExprKind::Constant(v),
                        })
                        .collect();

                    Ok(TypedExpr {
                        kind: TypedExprKind::InList {
                            expr: Box::new(rewritten_expr),
                            list,
                            negated: *negated,
                        },
                        data_type: DataType::Boolean,
                    })
                }

                TypedExprKind::Exists { subquery, negated } => {
                    if is_correlated_query(subquery) {
                        // Leave correlated EXISTS as-is for per-row evaluation.
                        return Ok(expr.clone());
                    }
                    // Execute the subquery — we only need to know if it returns any rows.
                    let result = self
                        .execute_subquery(txn, db_id, sequence_values, search_path, subquery, ctes)
                        .await?;
                    let has_rows = match result {
                        ExecuteResult::Select { rows, .. } => !rows.is_empty(),
                        _ => false,
                    };
                    let val = if *negated { !has_rows } else { has_rows };
                    Ok(TypedExpr {
                        kind: TypedExprKind::Constant(Value::Boolean(val)),
                        data_type: DataType::Boolean,
                    })
                }

                TypedExprKind::ScalarSubquery(subquery) => {
                    if is_correlated_query(subquery) {
                        // Leave correlated subqueries as-is for per-row evaluation.
                        return Ok(expr.clone());
                    }
                    // Execute the subquery — expect 0 or 1 rows, first column.
                    let result = self
                        .execute_subquery(txn, db_id, sequence_values, search_path, subquery, ctes)
                        .await?;
                    let value = match result {
                        ExecuteResult::Select { rows, .. } => {
                            if rows.is_empty() {
                                Value::Null
                            } else if rows.len() == 1 {
                                rows.into_iter()
                                    .next()
                                    .and_then(|r| r.values.into_iter().next())
                                    .unwrap_or(Value::Null)
                            } else {
                                return Err(anyhow!("Scalar subquery returned more than one row"));
                            }
                        }
                        _ => Value::Null,
                    };
                    Ok(TypedExpr {
                        kind: TypedExprKind::Constant(value),
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::AnyAll {
                    expr: inner_expr,
                    op,
                    subquery,
                    is_all,
                } => {
                    if is_correlated_query(subquery) {
                        // Leave correlated ANY/ALL as-is for per-row evaluation.
                        return Ok(expr.clone());
                    }
                    // Recurse into the LHS expression.
                    let rewritten_expr = self
                        .pre_materialize_async_exprs(
                            inner_expr,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;

                    // Execute subquery → get values → build array.
                    let result = self
                        .execute_subquery(txn, db_id, sequence_values, search_path, subquery, ctes)
                        .await?;
                    let rows = match result {
                        ExecuteResult::Select { rows, .. } => rows,
                        _ => return Err(anyhow!("Expected SELECT from ANY/ALL subquery")),
                    };

                    let values: Vec<TypedExpr> = rows
                        .into_iter()
                        .filter_map(|r| r.values.into_iter().next())
                        .map(|v| TypedExpr {
                            data_type: rewritten_expr.data_type.clone(),
                            kind: TypedExprKind::Constant(v),
                        })
                        .collect();

                    // ANY: true if `expr op value` for ANY value in the list.
                    // ALL: true if `expr op value` for ALL values in the list.
                    // For empty list: ANY → false, ALL → true.
                    if values.is_empty() {
                        return Ok(TypedExpr {
                            kind: TypedExprKind::Constant(Value::Boolean(*is_all)),
                            data_type: DataType::Boolean,
                        });
                    }

                    let comparisons: Vec<TypedExpr> = values
                        .into_iter()
                        .map(|v| TypedExpr {
                            kind: TypedExprKind::BinaryOp {
                                left: Box::new(rewritten_expr.clone()),
                                op: op.clone(),
                                right: Box::new(v),
                            },
                            data_type: DataType::Boolean,
                        })
                        .collect();

                    // ANY → OR chain, ALL → AND chain.
                    let chain_op = if *is_all {
                        TypedBinaryOp::And
                    } else {
                        TypedBinaryOp::Or
                    };

                    let combined = comparisons
                        .into_iter()
                        .reduce(|a, b| TypedExpr {
                            kind: TypedExprKind::BinaryOp {
                                left: Box::new(a),
                                op: chain_op.clone(),
                                right: Box::new(b),
                            },
                            data_type: DataType::Boolean,
                        })
                        .unwrap(); // values is non-empty, so this is safe.

                    Ok(combined)
                }

                TypedExprKind::ArraySubquery(subquery) => {
                    if is_correlated_query(subquery) {
                        // Leave correlated ArraySubquery as-is for per-row evaluation.
                        return Ok(expr.clone());
                    }
                    // Execute subquery → collect first column values into array.
                    let result = self
                        .execute_subquery(txn, db_id, sequence_values, search_path, subquery, ctes)
                        .await?;
                    let rows = match result {
                        ExecuteResult::Select { rows, .. } => rows,
                        _ => return Err(anyhow!("Expected SELECT from ARRAY(subquery)")),
                    };

                    let values: Vec<Value> = rows
                        .into_iter()
                        .filter_map(|r| r.values.into_iter().next())
                        .collect();

                    Ok(TypedExpr {
                        kind: TypedExprKind::Constant(Value::Array(values)),
                        data_type: expr.data_type.clone(),
                    })
                }

                // Recurse into children for non-subquery nodes.
                TypedExprKind::BinaryOp { left, right, op } => {
                    let l = self
                        .pre_materialize_async_exprs(
                            left,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let r = self
                        .pre_materialize_async_exprs(
                            right,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::BinaryOp {
                            left: Box::new(l),
                            op: op.clone(),
                            right: Box::new(r),
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::UnaryOp { op, operand } => {
                    let inner = self
                        .pre_materialize_async_exprs(
                            operand,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::UnaryOp {
                            op: *op,
                            operand: Box::new(inner),
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::Cast {
                    expr: inner,
                    target_type,
                    cast_context,
                } => {
                    let rewritten = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::Cast {
                            expr: Box::new(rewritten),
                            target_type: target_type.clone(),
                            cast_context: cast_context.clone(),
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::IsTest {
                    expr: inner,
                    test,
                    negated,
                } => {
                    let rewritten = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::IsTest {
                            expr: Box::new(rewritten),
                            test: *test,
                            negated: *negated,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::Between {
                    expr: inner,
                    low,
                    high,
                    negated,
                } => {
                    let e = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let l = self
                        .pre_materialize_async_exprs(
                            low,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let h = self
                        .pre_materialize_async_exprs(
                            high,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::Between {
                            expr: Box::new(e),
                            low: Box::new(l),
                            high: Box::new(h),
                            negated: *negated,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::InList {
                    expr: inner,
                    list,
                    negated,
                } => {
                    let e = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let mut new_list = Vec::with_capacity(list.len());
                    for item in list {
                        new_list.push(
                            self.pre_materialize_async_exprs(
                                item,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        );
                    }
                    Ok(TypedExpr {
                        kind: TypedExprKind::InList {
                            expr: Box::new(e),
                            list: new_list,
                            negated: *negated,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::Case {
                    operand,
                    when_clauses,
                    else_result,
                } => {
                    let new_operand = if let Some(ref op) = operand {
                        Some(Box::new(
                            self.pre_materialize_async_exprs(
                                op,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        ))
                    } else {
                        None
                    };
                    let mut new_whens = Vec::with_capacity(when_clauses.len());
                    for (w, t) in when_clauses {
                        let nw = self
                            .pre_materialize_async_exprs(
                                w,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?;
                        let nt = self
                            .pre_materialize_async_exprs(
                                t,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?;
                        new_whens.push((nw, nt));
                    }
                    let new_else = if let Some(ref e) = else_result {
                        Some(Box::new(
                            self.pre_materialize_async_exprs(
                                e,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        ))
                    } else {
                        None
                    };
                    Ok(TypedExpr {
                        kind: TypedExprKind::Case {
                            operand: new_operand,
                            when_clauses: new_whens,
                            else_result: new_else,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::Coalesce(args) => {
                    let mut new_args = Vec::with_capacity(args.len());
                    for a in args {
                        new_args.push(
                            self.pre_materialize_async_exprs(
                                a,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        );
                    }
                    Ok(TypedExpr {
                        kind: TypedExprKind::Coalesce(new_args),
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::NullIf(a, b) => {
                    let na = self
                        .pre_materialize_async_exprs(
                            a,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let nb = self
                        .pre_materialize_async_exprs(
                            b,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::NullIf(Box::new(na), Box::new(nb)),
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::FunctionCall {
                    func,
                    args,
                    order_by,
                    filter,
                } => {
                    let name_upper = func.name.to_ascii_uppercase();
                    let store = self.store();
                    let qctx = crate::sql::query_context::QueryContext::from_task_locals();

                    // Sequence functions: execute and replace with Constant.
                    match name_upper.as_str() {
                        "NEXTVAL" => {
                            let arg_val = eval_typed_expr(
                                args.first()
                                    .ok_or_else(|| anyhow!("nextval requires 1 argument"))?,
                                &Row::new(vec![]),
                                &qctx,
                            )?;
                            let full_name = resolve_sequence_full_name_from_value(
                                &store,
                                txn,
                                db_id,
                                search_path,
                                arg_val,
                            )
                            .await?;
                            let val = store.nextval_sequence(txn, db_id, &full_name).await?;
                            sequence_values.insert(full_name, val);
                            return Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(val)),
                                data_type: DataType::Int64,
                            });
                        }
                        "CURRVAL" => {
                            let arg_val = eval_typed_expr(
                                args.first()
                                    .ok_or_else(|| anyhow!("currval requires 1 argument"))?,
                                &Row::new(vec![]),
                                &qctx,
                            )?;
                            let full_name = resolve_sequence_full_name_from_value(
                                &store,
                                txn,
                                db_id,
                                search_path,
                                arg_val,
                            )
                            .await?;
                            if store.get_sequence(txn, db_id, &full_name).await?.is_none() {
                                return Err(anyhow!("Sequence '{}' does not exist", full_name));
                            }
                            let val = sequence_values.get(&full_name).copied().ok_or_else(|| {
                            anyhow!("currval of sequence \"{}\" is not yet defined in this session", full_name)
                        })?;
                            return Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(val)),
                                data_type: DataType::Int64,
                            });
                        }
                        "SETVAL" => {
                            let arg0 = eval_typed_expr(
                                args.first().ok_or_else(|| {
                                    anyhow!("setval requires at least 2 arguments")
                                })?,
                                &Row::new(vec![]),
                                &qctx,
                            )?;
                            let arg1 = eval_typed_expr(
                                args.get(1).ok_or_else(|| {
                                    anyhow!("setval requires at least 2 arguments")
                                })?,
                                &Row::new(vec![]),
                                &qctx,
                            )?;
                            let full_name = resolve_sequence_full_name_from_value(
                                &store,
                                txn,
                                db_id,
                                search_path,
                                arg0,
                            )
                            .await?;
                            let value_i64 = match arg1 {
                                Value::Int32(n) => n as i64,
                                Value::Int64(n) => n,
                                Value::Float64(n) => n as i64,
                                Value::Text(s) => s.trim().parse::<i64>().map_err(|_| {
                                    anyhow!("setval: value must be integer, got {}", s)
                                })?,
                                other => {
                                    return Err(anyhow!(
                                        "setval: value must be integer, got {}",
                                        other
                                    ))
                                }
                            };
                            let is_called = if let Some(arg2) = args.get(2) {
                                match eval_typed_expr(arg2, &Row::new(vec![]), &qctx)? {
                                    Value::Boolean(b) => b,
                                    Value::Text(s) => matches!(
                                        s.to_lowercase().as_str(),
                                        "true" | "t" | "1" | "yes" | "y"
                                    ),
                                    other => {
                                        return Err(anyhow!(
                                            "setval: is_called must be boolean, got {}",
                                            other
                                        ))
                                    }
                                }
                            } else {
                                true
                            };
                            let res = store
                                .setval_sequence(txn, db_id, &full_name, value_i64, is_called)
                                .await?;
                            return Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(res)),
                                data_type: DataType::Int64,
                            });
                        }
                        "LASTVAL" => {
                            // LASTVAL returns the value most recently obtained by nextval.
                            let val =
                                sequence_values.values().last().copied().ok_or_else(|| {
                                    anyhow!("lastval is not yet defined in this session")
                                })?;
                            return Ok(TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int64(val)),
                                data_type: DataType::Int64,
                            });
                        }
                        _ => {}
                    }

                    // Non-sequence function: recurse into args/filter.
                    let mut new_args = Vec::with_capacity(args.len());
                    for a in args {
                        new_args.push(
                            self.pre_materialize_async_exprs(
                                a,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        );
                    }
                    let new_filter = if let Some(ref f) = filter {
                        Some(Box::new(
                            self.pre_materialize_async_exprs(
                                f,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                ctes,
                            )
                            .await?,
                        ))
                    } else {
                        None
                    };
                    Ok(TypedExpr {
                        kind: TypedExprKind::FunctionCall {
                            func: func.clone(),
                            args: new_args,
                            order_by: order_by.clone(),
                            filter: new_filter,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::Like {
                    expr: inner,
                    pattern,
                    escape,
                    negated,
                    case_insensitive,
                } => {
                    let e = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let p = self
                        .pre_materialize_async_exprs(
                            pattern,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::Like {
                            expr: Box::new(e),
                            pattern: Box::new(p),
                            escape: escape.clone(),
                            negated: *negated,
                            case_insensitive: *case_insensitive,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                TypedExprKind::SimilarTo {
                    expr: inner,
                    pattern,
                    escape,
                    negated,
                } => {
                    let e = self
                        .pre_materialize_async_exprs(
                            inner,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    let p = self
                        .pre_materialize_async_exprs(
                            pattern,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    Ok(TypedExpr {
                        kind: TypedExprKind::SimilarTo {
                            expr: Box::new(e),
                            pattern: Box::new(p),
                            escape: escape.clone(),
                            negated: *negated,
                        },
                        data_type: expr.data_type.clone(),
                    })
                }

                // Leaf nodes (Constant, ColumnRef, etc.) — return as-is.
                _ => Ok(expr.clone()),
            }
        }) // end Box::pin
    }

    /// Execute a set operation (UNION/INTERSECT/EXCEPT) through the analyzed path.
    ///
    /// Recursively executes left and right branches, materializes their results,
    /// combines them via SetOperationOperator, then applies outer ORDER BY/LIMIT.
    ///
    /// Returns `Pin<Box<...>>` to break the recursive async future sizing
    /// (set operations can nest: `(A UNION B) INTERSECT C`).
    #[allow(clippy::too_many_arguments)]
    fn execute_analyzed_set_op<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        analyzed: &'a AnalyzedQuery,
        kind: SetOpKind,
        all: bool,
        left: &'a AnalyzedQuery,
        right: &'a AnalyzedQuery,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<ExecuteResult>> + Send + 'a>> {
        Box::pin(async move {
            // Execute left branch.
            let left_result = self
                .execute_subquery(txn, db_id, sequence_values, search_path, left, ctes)
                .await?;
            let (left_columns, left_types, left_rows) = match left_result {
                ExecuteResult::Select {
                    columns,
                    column_types,
                    rows,
                    ..
                } => (columns, column_types.unwrap_or_default(), rows),
                _ => {
                    return Err(anyhow!(
                        "Expected SELECT result from left branch of set operation"
                    ))
                }
            };

            // Execute right branch.
            let right_result = self
                .execute_subquery(txn, db_id, sequence_values, search_path, right, ctes)
                .await?;
            let right_rows = match right_result {
                ExecuteResult::Select { rows, .. } => rows,
                _ => {
                    return Err(anyhow!(
                        "Expected SELECT result from right branch of set operation"
                    ))
                }
            };

            // Build synthetic schemas for materializing both sides.
            let schema = build_set_op_schema(&left_columns, &left_types);
            let left_op: BoxedOperator =
                Box::new(TableScanOperator::new_with_rows(schema.clone(), left_rows));
            let right_op: BoxedOperator =
                Box::new(TableScanOperator::new_with_rows(schema, right_rows));

            // Map SetOpKind + ALL → SetOperationType.
            let set_op_type = match (kind, all) {
                (SetOpKind::Union, true) => SetOperationType::UnionAll,
                (SetOpKind::Union, false) => SetOperationType::Union,
                (SetOpKind::Intersect, true) => SetOperationType::IntersectAll,
                (SetOpKind::Intersect, false) => SetOperationType::Intersect,
                (SetOpKind::Except, true) => SetOperationType::ExceptAll,
                (SetOpKind::Except, false) => SetOperationType::Except,
            };
            let mut op: BoxedOperator =
                Box::new(SetOperationOperator::new(left_op, right_op, set_op_type));

            // Apply outer ORDER BY (operates on the combined result).
            if !analyzed.order_by.is_empty() {
                op = Box::new(SortOperator::new(op, analyzed.order_by.clone()));
            }

            // Apply outer LIMIT/OFFSET.
            let limit = analyzed
                .limit
                .as_ref()
                .map(|e| eval_const_usize(e))
                .transpose()?;
            let offset = analyzed
                .offset
                .as_ref()
                .map(|e| eval_const_usize(e))
                .transpose()?
                .unwrap_or(0);
            if limit.is_some() || offset > 0 {
                op = Box::new(LimitOperator::new(op, limit, offset));
            }

            // Execute operator tree.
            let rt = ExprRuntime::new(self, db_id, search_path, ctes);
            let rows = rt.run_operator_tree(&mut op, txn, sequence_values).await?;

            // Output using the analyzed query's output schema.
            let columns: Vec<String> = analyzed
                .output_schema
                .iter()
                .map(|(n, _)| n.clone())
                .collect();
            let column_types: Vec<DataType> = analyzed
                .output_schema
                .iter()
                .map(|(_, t)| t.clone())
                .collect();

            Ok(ExecuteResult::Select {
                columns,
                column_types: Some(column_types),
                rows,
                timezone: crate::session_context::current_timezone(),
            })
        }) // end Box::pin(async move { ... })
    }

    /// Build a TableScanOperator for a base table or CTE.
    async fn build_scan_operator(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        name: &str,
        alias: &str,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<BoxedOperator> {
        let cte_key = name.to_lowercase();
        if let Some((cte_schema, cte_rows)) = ctes.get(&cte_key) {
            let mut schema = cte_schema.clone();
            let short = schema.name.rsplit('.').next().unwrap_or(&schema.name);
            if !short.eq_ignore_ascii_case(alias) {
                schema.from_alias = Some(alias.to_string());
            }
            return Ok(
                Box::new(TableScanOperator::new_with_rows(schema, cte_rows.clone()))
                    as BoxedOperator,
            );
        }

        // The Analyzer already resolved the name to a fully-qualified store key.
        // Use it directly instead of re-resolving through ObjectName.
        if let Some(mut schema) = self.store().get_schema(txn, db_id, name).await? {
            let short = schema.name.rsplit('.').next().unwrap_or(&schema.name);
            if !short.eq_ignore_ascii_case(alias) {
                schema.from_alias = Some(alias.to_string());
            }
            Ok(Box::new(TableScanOperator::new(schema)) as BoxedOperator)
        } else {
            // Virtual table (information_schema, etc.)
            let (mut schema, rows) = self
                .get_table_data(txn, db_id, sequence_values, search_path, name, ctes)
                .await?;
            let short = schema.name.rsplit('.').next().unwrap_or(&schema.name);
            if !short.eq_ignore_ascii_case(alias) {
                schema.from_alias = Some(alias.to_string());
            }
            if schema.table_id == 0 {
                Ok(Box::new(TableScanOperator::new_with_rows(schema, rows)) as BoxedOperator)
            } else {
                Ok(Box::new(TableScanOperator::new(schema)) as BoxedOperator)
            }
        }
    }

    /// Try to execute a query through the CBO optimizer pipeline.
    ///
    /// `AnalyzedQuery → LogicalPlan → PhysicalPlan → BoxedOperator → execute`
    ///
    /// Precondition: the caller has verified `is_optimizer_eligible()`.
    /// Phase 3: handles single-table SELECTs and multi-table JOINs.
    async fn execute_via_optimizer(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        analyzed: &AnalyzedQuery,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        use crate::sql::operators::execute_operator_tree_with_ctes;
        use crate::sql::optimizer::{BuildContext, PlanningContext};

        tracing::debug!(target: "optimizer", "routing query through CBO pipeline");

        // Step 1: Build PlanningContext with table statistics and schemas.
        // Pre-load both stats and full TableSchema (including index metadata)
        // before optimize(), so the physical planner can do access-path selection.
        // The pre-loaded schemas are then shared with BuildContext (Step 3) to
        // eliminate redundant catalog reads.
        //
        // Uses collect_query_table_refs to recursively walk the query body,
        // handling Select, SetOperation, and Values bodies uniformly.
        let mut planning_ctx = PlanningContext::empty();
        let table_refs = crate::sql::optimizer::collect_query_table_refs(analyzed);
        {
            let mut stats_attempted = std::collections::HashSet::new();
            for (name, schema, _alias) in &table_refs {
                // Load statistics.
                let tid = schema.table_id;
                let stats = if stats_attempted.insert(tid) {
                    self.get_or_load_stats(txn, db_id, tid).await?
                } else {
                    self.stats_cache().get_full_stats(db_id, tid)
                };
                if let Some(stats) = stats {
                    planning_ctx.table_stats.insert(name.to_string(), stats);
                }
                // Load full table schema (with index metadata) for access-path selection.
                // CTE schemas are not loaded here — they have no indexes.
                let cte_key = name.to_lowercase();
                if ctes.get(&cte_key).is_none() {
                    if let Some(table_schema) = self.store().get_schema(txn, db_id, name).await? {
                        planning_ctx
                            .table_schemas
                            .insert(name.to_string(), table_schema);
                    }
                }
            }
        }

        // Step 2: AnalyzedQuery → PhysicalPlan (shared entrypoint).
        // Eligibility gate guarantees this always succeeds.
        let physical = crate::sql::optimizer::optimize(analyzed, &planning_ctx);

        // Step 3: Build BuildContext from pre-loaded schemas in PlanningContext.
        // Handles CTE schemas and alias assignment.
        let mut build_ctx = BuildContext::new();
        for (name, _schema, alias) in &table_refs {
            let display_alias = alias.unwrap_or(name);
            let cte_key = name.to_lowercase();
            if let Some((cte_schema, _)) = ctes.get(&cte_key) {
                build_ctx = build_ctx.with_schema(name.to_string(), cte_schema.clone());
            } else if let Some(table_schema) = planning_ctx.table_schemas.get(*name) {
                let mut schema = table_schema.clone();
                let short = schema.name.rsplit('.').next().unwrap_or(&schema.name);
                if !short.eq_ignore_ascii_case(display_alias) {
                    schema.from_alias = Some(display_alias.to_string());
                }
                build_ctx = build_ctx.with_schema(name.to_string(), schema);
            } else {
                // Eligibility gate rejects virtual catalog tables, so this
                // should be unreachable. Hard error to surface bugs early.
                return Err(anyhow!(
                    "Optimizer cannot resolve table schema for '{}' \
                     (should have been rejected by eligibility check)",
                    name
                ));
            }
        }

        // Step 4: PhysicalPlan → BoxedOperator
        let mut operator = physical.build_operators(&build_ctx)?;

        // Step 5: Execute the operator tree.
        let store = self.store().clone();
        let rows = execute_operator_tree_with_ctes(
            &mut operator,
            txn,
            store,
            db_id,
            search_path,
            sequence_values,
            ctes,
        )
        .await?;

        // Build result.
        let columns: Vec<String> = analyzed
            .output_schema
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let column_types: Vec<DataType> = analyzed
            .output_schema
            .iter()
            .map(|(_, dt)| dt.clone())
            .collect();

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows,
            timezone: crate::session_context::current_timezone(),
        })
    }
}

/// Sort projected rows by deferred ORDER BY expressions.
///
/// When ORDER BY references output aliases (Analyzer clones projection expressions),
/// the ORDER BY expressions match output columns by name. We find the output column
/// index for each ORDER BY key, then sort using those column values.
fn sort_projected_rows(
    mut rows: Vec<Row>,
    deferred_ob: &[TypedOrderByExpr],
    output_schema: &[(String, DataType)],
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<Row>> {
    // Map each ORDER BY expression to an output column index.
    // Strategy: for ColumnRef ORDER BY, use column_name to find the output column.
    // For complex expressions (ScalarSubquery cloned from alias), find by data_type match.
    let mut ob_col_indices: Vec<(usize, bool, bool)> = Vec::with_capacity(deferred_ob.len());
    for ob in deferred_ob {
        let idx = match &ob.expr.kind {
            TypedExprKind::ColumnRef { column_name, .. } => {
                // Match by column name against output schema.
                output_schema
                    .iter()
                    .position(|(name, _)| name.eq_ignore_ascii_case(column_name))
            }
            _ => {
                // For complex expressions (ScalarSubquery, etc.): the Analyzer cloned
                // this from some projection[i].expr where output_schema[i] has the alias.
                // Find by matching data_type + being the only expression of that type.
                let target_type = &ob.expr.data_type;
                let matches: Vec<usize> = output_schema
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, dt))| dt == target_type)
                    .map(|(i, _)| i)
                    .collect();
                if matches.len() == 1 {
                    Some(matches[0])
                } else {
                    // Fallback: first output column.
                    Some(0)
                }
            }
        };
        ob_col_indices.push((idx.unwrap_or(0), ob.asc, ob.nulls_first));
    }

    // Sort.
    use crate::sql::expr::operators::sort_by_fallible;
    sort_by_fallible(&mut rows, |a, b| {
        for &(col_idx, asc, nulls_first) in &ob_col_indices {
            let va = a.values.get(col_idx).unwrap_or(&Value::Null);
            let vb = b.values.get(col_idx).unwrap_or(&Value::Null);
            let ord = crate::sql::expr::compare_order_by_values(va, vb, asc, nulls_first)?;
            if ord != std::cmp::Ordering::Equal {
                return Ok(ord);
            }
        }
        Ok(std::cmp::Ordering::Equal)
    })?;

    // Apply deferred LIMIT/OFFSET.
    let start = offset.min(rows.len());
    let end = limit.map_or(rows.len(), |l| (start + l).min(rows.len()));
    Ok(rows[start..end].to_vec())
}
