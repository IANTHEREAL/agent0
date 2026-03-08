//! CTE materialization and optimizer context preparation for the analyzed SELECT path.
//!
//! Contains the pipeline steps that run *before* the optimizer:
//! - Pre-materializing non-correlated async expressions in the query body
//! - Pre-materializing JOIN ON conditions
//! - Loading table schemas and statistics into [`PlanningContext`]
//! - Loading virtual table data and table function results into [`BuildContext`]

use crate::model::{Row, TableSchema};
use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQueryBody, AnalyzedTableRef, AnalyzedTableRefKind, JoinCondition,
    TypedExpr, TypedFunctionArg, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use crate::sql::executor::core::Executor;
use crate::sql::expr::classify::{has_any_column_ref, needs_pre_materialization};
use crate::sql::optimizer::{BuildContext, PlanningContext};

use anyhow::Result;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tikv_client::Transaction;

use super::subquery::has_outer_ref;
use crate::sql::sequences::SequenceSession;

impl Executor {
    /// Pre-materialize all non-correlated async expressions in the query body.
    ///
    /// Walks all TypedExpr positions (projection, WHERE, HAVING, ORDER BY,
    /// GROUP BY, DISTINCT ON, JOIN ON) and replaces non-correlated subqueries
    /// with constants.
    pub(in crate::sql::executor::select::analyzed) fn pre_materialize_query_body<'a>(
        &'a self,
        analyzed: &'a mut AnalyzedQuery,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match &mut analyzed.body {
                AnalyzedQueryBody::Select(ref mut select) => {
                    // Projection.
                    for proj in &mut select.projection {
                        if needs_pre_materialization(&proj.expr) {
                            proj.expr = self
                                .pre_materialize_async_exprs(
                                    &proj.expr,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    ctes,
                                )
                                .await?;
                        }
                    }
                    // WHERE.
                    if let Some(ref w) = select.where_clause {
                        if needs_pre_materialization(w) {
                            let new_w = self
                                .pre_materialize_async_exprs(
                                    w,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    ctes,
                                )
                                .await?;
                            select.where_clause = Some(new_w);
                        }
                    }
                    // HAVING.
                    if let Some(ref h) = select.having {
                        if needs_pre_materialization(h) {
                            let new_h = self
                                .pre_materialize_async_exprs(
                                    h,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    ctes,
                                )
                                .await?;
                            select.having = Some(new_h);
                        }
                    }
                    // GROUP BY.
                    let group_exprs: Vec<TypedExpr> = select.group_by.clone();
                    for (i, expr) in group_exprs.iter().enumerate() {
                        if needs_pre_materialization(expr) {
                            select.group_by[i] = self
                                .pre_materialize_async_exprs(
                                    expr,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    ctes,
                                )
                                .await?;
                        }
                    }
                    // DISTINCT ON.
                    if let AnalyzedDistinct::DistinctOn(ref on_exprs) = select.distinct {
                        let cloned: Vec<TypedExpr> = on_exprs.clone();
                        let mut new_on = Vec::with_capacity(cloned.len());
                        for expr in &cloned {
                            if needs_pre_materialization(expr) {
                                new_on.push(
                                    self.pre_materialize_async_exprs(
                                        expr,
                                        txn,
                                        db_id,
                                        sequence_values,
                                        search_path,
                                        ctes,
                                    )
                                    .await?,
                                );
                            } else {
                                new_on.push(expr.clone());
                            }
                        }
                        select.distinct = AnalyzedDistinct::DistinctOn(new_on);
                    }
                    // JOIN ON conditions (recursive through table ref tree).
                    for tr in &mut select.from {
                        self.pre_materialize_table_ref_on(
                            tr,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                    }
                }
                AnalyzedQueryBody::SetOperation {
                    ref mut left,
                    ref mut right,
                    ..
                } => {
                    self.pre_materialize_query_body(
                        left,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                    )
                    .await?;
                    self.pre_materialize_query_body(
                        right,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                    )
                    .await?;
                }
                AnalyzedQueryBody::Values(ref mut rows) => {
                    for row in rows {
                        for expr in row {
                            if needs_pre_materialization(expr) {
                                *expr = self
                                    .pre_materialize_async_exprs(
                                        expr,
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
                }
            }

            // ORDER BY.
            let order_by_clone: Vec<TypedOrderByExpr> = analyzed.order_by.clone();
            for (i, ob) in order_by_clone.iter().enumerate() {
                if needs_pre_materialization(&ob.expr) {
                    analyzed.order_by[i].expr = self
                        .pre_materialize_async_exprs(
                            &ob.expr,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                        )
                        .await?;
                }
            }

            Ok(())
        })
    }

    /// Pre-materialize JOIN ON conditions in a table ref tree.
    pub(in crate::sql::executor::select::analyzed) fn pre_materialize_table_ref_on<'a>(
        &'a self,
        table_ref: &'a mut AnalyzedTableRef,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match &mut table_ref.kind {
                AnalyzedTableRefKind::Table { .. } | AnalyzedTableRefKind::Function { .. } => {
                    Ok(())
                }
                AnalyzedTableRefKind::Subquery(_) => Ok(()),
                AnalyzedTableRefKind::Join {
                    left,
                    right,
                    condition,
                    ..
                } => {
                    self.pre_materialize_table_ref_on(
                        left,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                    )
                    .await?;
                    self.pre_materialize_table_ref_on(
                        right,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                    )
                    .await?;
                    let new_cond = match condition {
                        JoinCondition::On(ref expr) => {
                            if needs_pre_materialization(expr) {
                                let new_expr = self
                                    .pre_materialize_async_exprs(
                                        expr,
                                        txn,
                                        db_id,
                                        sequence_values,
                                        search_path,
                                        ctes,
                                    )
                                    .await?;
                                Some(JoinCondition::On(new_expr))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    if let Some(c) = new_cond {
                        *condition = c;
                    }
                    Ok(())
                }
            }
        })
    }

    /// Pre-load KV-backed table schemas and statistics into [`PlanningContext`].
    ///
    /// This intentionally skips:
    /// - runtime CTE bindings (`ctes`)
    /// - WITH-local CTE names from the analyzed query body
    /// - virtual catalog tables (resolved at build/runtime only)
    pub(crate) async fn prepare_planning_context(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        analyzed: &AnalyzedQuery,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        planning_ctx: &mut PlanningContext,
    ) -> Result<()> {
        let table_refs = crate::sql::optimizer::collect_query_table_refs(analyzed);
        let analyzed_cte_names: std::collections::HashSet<String> = analyzed
            .ctes
            .iter()
            .map(|c| c.name.to_lowercase())
            .collect();

        let mut stats_attempted = std::collections::HashSet::new();
        for (name, _schema, alias) in &table_refs {
            let cte_key = name.to_lowercase();
            if ctes.contains_key(&cte_key) || analyzed_cte_names.contains(&cte_key) {
                continue;
            }

            if let Some(table_schema) = self.store().get_schema(txn, db_id, name).await? {
                let ctx_key = crate::sql::optimizer::schema_map_key(name, *alias);
                let tid = table_schema.table_id;
                let stats = if stats_attempted.insert(tid) {
                    self.get_or_load_stats(txn, db_id, tid).await?
                } else {
                    self.stats_cache().get_full_stats(db_id, tid)
                };
                if let Some(stats) = stats {
                    planning_ctx.table_stats.insert(ctx_key.clone(), stats);
                }
                planning_ctx.table_schemas.insert(ctx_key, table_schema);
            }
        }

        Ok(())
    }

    /// Pre-load table schemas, statistics, virtual table data, and table function
    /// results into the PlanningContext and BuildContext.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::sql::executor::select::analyzed) async fn prepare_optimizer_contexts(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        analyzed: &AnalyzedQuery,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        planning_ctx: &mut PlanningContext,
        build_ctx: &mut BuildContext,
    ) -> Result<()> {
        self.prepare_planning_context(txn, db_id, analyzed, ctes, planning_ctx)
            .await?;

        // Walk the query body to collect all table references.
        let table_refs = crate::sql::optimizer::collect_query_table_refs(analyzed);

        for (name, _schema, alias) in &table_refs {
            // Use scope-safe composite key: "table_name\0alias" to prevent
            // collisions when the same alias appears in different scopes
            // (e.g., outer FROM users AS t vs inner subquery FROM orders AS t).
            let ctx_key = crate::sql::optimizer::schema_map_key(name, *alias);
            let alias_display = alias.unwrap_or(name);
            let cte_key = name.to_lowercase();

            // CTE: use CTE schema + rows.
            if let Some((cte_schema, cte_rows)) = ctes.get(&cte_key) {
                build_ctx
                    .table_schemas
                    .insert(ctx_key.clone(), cte_schema.clone());
                build_ctx.preloaded_rows.insert(ctx_key, cte_rows.clone());
                continue;
            }

            // Try KV table (regular user table).
            if let Some(table_schema) = planning_ctx.table_schemas.get(&ctx_key).cloned() {
                let mut s = table_schema;
                let short = s.name.rsplit('.').next().unwrap_or(&s.name);
                if !short.eq_ignore_ascii_case(alias_display) {
                    s.from_alias = Some(alias_display.to_string());
                }
                build_ctx.table_schemas.insert(ctx_key, s);
                continue;
            }
            if let Some(table_schema) = self.store().get_schema(txn, db_id, name).await? {
                let mut s = table_schema;
                let short = s.name.rsplit('.').next().unwrap_or(&s.name);
                if !short.eq_ignore_ascii_case(alias_display) {
                    s.from_alias = Some(alias_display.to_string());
                }
                build_ctx.table_schemas.insert(ctx_key, s);
                continue;
            }

            // Virtual catalog table (information_schema, pg_catalog, etc.).
            // Pre-load data — these tables don't live in KV storage.
            let (mut virt_schema, virt_rows) = self
                .get_table_data(txn, db_id, sequence_values, search_path, name, ctes)
                .await?;
            let short = virt_schema
                .name
                .rsplit('.')
                .next()
                .unwrap_or(&virt_schema.name);
            if !short.eq_ignore_ascii_case(alias_display) {
                virt_schema.from_alias = Some(alias_display.to_string());
            }
            build_ctx.table_schemas.insert(ctx_key.clone(), virt_schema);
            build_ctx.preloaded_rows.insert(ctx_key, virt_rows);
        }

        // Also walk table functions in FROM and pre-execute them.
        self.preload_table_functions(
            txn,
            db_id,
            sequence_values,
            search_path,
            analyzed,
            ctes,
            build_ctx,
        )
        .await?;

        Ok(())
    }

    /// Pre-execute table functions referenced in FROM and store results in BuildContext.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::sql::executor::select::analyzed) fn preload_table_functions<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        analyzed: &'a AnalyzedQuery,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        build_ctx: &'a mut BuildContext,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let body = &analyzed.body;
            match body {
                AnalyzedQueryBody::Select(select) => {
                    for tr in &select.from {
                        self.preload_table_function_refs(
                            tr,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                            build_ctx,
                        )
                        .await?;
                    }
                }
                AnalyzedQueryBody::SetOperation { left, right, .. } => {
                    self.preload_table_functions(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        left,
                        ctes,
                        build_ctx,
                    )
                    .await?;
                    self.preload_table_functions(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        right,
                        ctes,
                        build_ctx,
                    )
                    .await?;
                }
                AnalyzedQueryBody::Values(_) => {}
            }
            Ok(())
        }) // end Box::pin
    }

    /// Pre-execute table function references in a table ref tree.
    #[allow(clippy::too_many_arguments)]
    fn preload_table_function_refs<'a>(
        &'a self,
        table_ref: &'a AnalyzedTableRef,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        build_ctx: &'a mut BuildContext,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            match &table_ref.kind {
                AnalyzedTableRefKind::Function {
                    func,
                    args,
                    output_columns,
                } => {
                    let key = table_ref.alias.as_deref().unwrap_or(&func.name).to_string();

                    let has_correlated_args = args.iter().any(|arg| {
                        let expr = match arg {
                            TypedFunctionArg::Positional(expr) => expr,
                            TypedFunctionArg::Named { expr, .. } => expr,
                        };
                        // Table functions are preloaded once with a dummy row; any column
                        // reference requires per-row LATERAL semantics.
                        has_outer_ref(expr) || has_any_column_ref(expr)
                    });

                    // Build schema from analyzer-resolved output columns.
                    let schema = TableSchema {
                        name: key.clone(),
                        table_id: 0,
                        columns: output_columns
                            .iter()
                            .map(|(name, dt)| crate::model::ColumnDef {
                                name: name.clone(),
                                data_type: dt.clone(),
                                nullable: true,
                                primary_key: false,
                                unique: false,
                                is_serial: false,
                                default_expr: None,
                                generation_expr: None,
                                generation_expr_authorized_by: None,
                                collation: None,
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

                    build_ctx.table_schemas.insert(key.clone(), schema);
                    if has_correlated_args {
                        build_ctx.correlated_table_functions.insert(key.clone());
                        return Ok(());
                    }

                    let qc = crate::sql::query_context::QueryContext::from_task_locals();
                    let dummy_row = Row::new(vec![]);
                    let runtime_schema = build_ctx
                        .table_schemas
                        .get(&key)
                        .cloned()
                        .expect("table function schema inserted before execution");
                    let rows = self
                        .execute_table_function_rows(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            ctes,
                            &func.name,
                            args,
                            &dummy_row,
                            &runtime_schema,
                            &qc,
                            &key,
                        )
                        .await?;

                    build_ctx.preloaded_rows.insert(key, rows);
                }
                AnalyzedTableRefKind::Join { left, right, .. } => {
                    self.preload_table_function_refs(
                        left,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                        build_ctx,
                    )
                    .await?;
                    self.preload_table_function_refs(
                        right,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        ctes,
                        build_ctx,
                    )
                    .await?;
                }
                AnalyzedTableRefKind::Subquery(subquery) => {
                    self.preload_table_functions(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        subquery,
                        ctes,
                        build_ctx,
                    )
                    .await?;
                }
                _ => {}
            }
            Ok(())
        }) // end Box::pin
    }
}
