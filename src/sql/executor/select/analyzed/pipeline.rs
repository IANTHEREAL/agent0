//! CTE materialization and optimizer context preparation for the analyzed SELECT path.
//!
//! Contains the pipeline steps that run *before* the optimizer:
//! - Pre-materializing non-correlated async expressions in the query body
//! - Pre-materializing JOIN ON conditions
//! - Loading table schemas and statistics into [`PlanningContext`]
//! - Loading virtual table data and table function results into [`BuildContext`]

use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQueryBody, AnalyzedTableRef, AnalyzedTableRefKind, JoinCondition,
    TypedExpr, TypedExprKind, TypedFunctionArg, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use crate::sql::executor::core::Executor;
use crate::sql::expr::classify::needs_pre_materialization;
use crate::sql::expr::traverse::visit_any;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::names;
use crate::sql::optimizer::{BuildContext, PlanningContext};
use crate::sql::table_functions::is_virtual_table_backed_system_function;
use crate::types::{Row, TableSchema, Value};

use anyhow::{anyhow, Result};
use sqlparser::ast::{FunctionArg, FunctionArgExpr};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use tikv_client::Transaction;

use super::expr_runtime::ExprRuntime;
use super::subquery::has_outer_ref;

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
        sequence_values: &'a mut HashMap<String, i64>,
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
        sequence_values: &'a mut HashMap<String, i64>,
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
        sequence_values: &mut HashMap<String, i64>,
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
        sequence_values: &'a mut HashMap<String, i64>,
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
        sequence_values: &'a mut HashMap<String, i64>,
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
                            .map(|(name, dt)| crate::types::ColumnDef {
                                name: name.clone(),
                                data_type: dt.clone(),
                                nullable: true,
                                primary_key: false,
                                unique: false,
                                is_serial: false,
                                default_expr: None,
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
                    let evaluated_args = evaluate_table_function_args(args, &dummy_row, &qc)?;
                    let func_upper = func.name.to_uppercase();

                    let rows = if func_upper == "UNNEST" {
                        let mut columns: Vec<Vec<Value>> = Vec::with_capacity(evaluated_args.len());
                        for arg in &evaluated_args {
                            let arr = match &arg.value {
                                Value::Array(a) => a.clone(),
                                Value::Null => vec![],
                                _ => return Err(anyhow!("UNNEST argument must be an array")),
                            };
                            columns.push(arr);
                        }

                        let max_len = columns.iter().map(|c| c.len()).max().unwrap_or(0);
                        let has_ordinality = output_columns.len() > columns.len();
                        let mut rows = Vec::with_capacity(max_len);
                        for i in 0..max_len {
                            let mut values: Vec<Value> = columns
                                .iter()
                                .map(|col| col.get(i).cloned().unwrap_or(Value::Null))
                                .collect();
                            if has_ordinality {
                                values.push(Value::Int64((i + 1) as i64));
                            }
                            rows.push(Row::new(values));
                        }
                        rows
                    } else {
                        let bridge_args = bridge_function_args(&evaluated_args);
                        if func_upper == "GENERATE_SERIES" {
                            let (_, rows) = self
                                .execute_generate_series(&bridge_args, &key, None, 0, None)
                                .await?;
                            rows
                        } else if func_upper == "_DB9_SYS_RECORD_MIGRATION" {
                            let (_, rows) =
                                self.execute_record_migration(txn, &bridge_args).await?;
                            rows
                        } else if bridge_args.is_empty()
                            && is_virtual_table_backed_system_function(&func.name)
                        {
                            let (_, rows) = self
                                .get_table_data(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &func.name,
                                    ctes,
                                )
                                .await?;
                            rows
                        } else {
                            // Try extension table function, user table function, or scalar-in-FROM.
                            let obj_name = names::object_name_from_str(&func.name)?;
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
                                        _,
                                        rows,
                                    ) => rows,
                                    crate::sql::executor::extensions::ExtensionTableFunctionResult::Streaming(
                                        _,
                                        mut op,
                                    ) => {
                                        let rt = ExprRuntime::new(self, db_id, search_path, ctes);
                                        rt.run_operator_tree(&mut op, txn, sequence_values).await?
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
                                rows
                            } else {
                                // Scalar-in-FROM: evaluate as function call.
                                let typed_args: Vec<TypedExpr> = args
                                    .iter()
                                    .map(|a| match a {
                                        TypedFunctionArg::Positional(e) => e.clone(),
                                        TypedFunctionArg::Named { expr, .. } => expr.clone(),
                                    })
                                    .collect();
                                let scalar_arg_values: Vec<Value> =
                                    evaluated_args.iter().map(|a| a.value.clone()).collect();
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
                                        self.tenant_keyspace(),
                                    )
                                    .await
                                {
                                    vec![Row::new(vec![result?])]
                                } else {
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
                                    vec![Row::new(vec![val])]
                                }
                            }
                        }
                    };

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

#[derive(Debug, Clone)]
struct EvaluatedTableFunctionArg {
    name: Option<String>,
    value: Value,
}

fn evaluate_table_function_args(
    args: &[TypedFunctionArg],
    dummy_row: &Row,
    qc: &crate::sql::query_context::QueryContext,
) -> Result<Vec<EvaluatedTableFunctionArg>> {
    let mut evaluated = Vec::with_capacity(args.len());
    for tfa in args {
        let (name_opt, typed_expr) = match tfa {
            TypedFunctionArg::Positional(e) => (None, e),
            TypedFunctionArg::Named { name, expr } => (Some(name.clone()), expr),
        };
        evaluated.push(EvaluatedTableFunctionArg {
            name: name_opt,
            value: eval_typed_expr(typed_expr, dummy_row, qc)?,
        });
    }
    Ok(evaluated)
}

fn bridge_function_args(args: &[EvaluatedTableFunctionArg]) -> Vec<FunctionArg> {
    args.iter()
        .map(|arg| {
            let sql_expr = crate::sql::value_coercion::value_to_sql_expr(&arg.value);
            match &arg.name {
                None => FunctionArg::Unnamed(FunctionArgExpr::Expr(sql_expr)),
                Some(name) => FunctionArg::Named {
                    name: sqlparser::ast::Ident::new(name),
                    arg: FunctionArgExpr::Expr(sql_expr),
                },
            }
        })
        .collect()
}

fn has_any_column_ref(expr: &TypedExpr) -> bool {
    visit_any(expr, |e| matches!(&e.kind, TypedExprKind::ColumnRef { .. }))
}
