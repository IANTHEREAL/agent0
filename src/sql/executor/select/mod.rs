//! SELECT query execution

use super::super::aggregate::{collect_having_agg_funcs, eval_having_expr, AggExpr};
use super::super::distinct::{apply_offset_limit_fetch, dedup_rows, distinct_on_rows_with_indices};
use super::super::gin;
use super::super::names;
use super::super::operators::{
    execute_operator_tree, BoxedOperator, FilterOperator, HashAggregateOperator, HashJoinConfig,
    HashJoinOperator, HashJoinType, JoinType, LimitOperator, NestedLoopJoinOperator,
    PhysicalPlanner, SortOperator, TableScanOperator, WindowOperator,
};
use super::super::planner::{self, choose_join_algorithm, JoinAlgorithmChoice, ScanType};
use super::super::projection::{fill_row_defaults, get_select_item_name, infer_expr_type};
use super::super::sequences;
use super::super::value_key::{serialize_value_for_key, serialize_values_for_key};
use super::super::wildcard::build_join_wildcard_plan;
use super::super::window::{compute_window_functions, extract_window_functions, WindowFuncInfo};
use super::super::{
    expr::{coerce_text_literal_to_bool, eval_expr, validate_bool_expr_in_boolean_context},
    Aggregator, ExecuteResult,
};
use super::core::Executor;
use super::extensions::ExtensionTableFunctionResult;
use super::operators::rewrite_expr_for_multi_join;
use super::operators::{
    eval_having_expr_for_operators, extract_limit, extract_offset, is_aggregate_func,
    projection_may_have_udf, rewrite_agg_refs_to_columns, use_operator_execution,
};
use crate::sql::error::SqlError;
use crate::sql::information_schema::VirtualTableFilter;
use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    BinaryOperator, Distinct, Expr, Function, FunctionArg, FunctionArgExpr, GroupByExpr, Ident,
    JoinConstraint, LockType, NonBlock, ObjectName, Query, SelectItem, SetExpr, TableFactor,
    Value as SqlValue,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use tikv_client::Transaction;
use tracing::debug;

mod analysis;
mod join;
mod legacy;
mod order;
mod pushdown;

use analysis::projection_has_window_function;
use analysis::{expr_has_subquery, projection_has_non_window_aggregate};
use join::ensure_no_locking_clauses_for_join;
use order::{
    expand_projection_exprs_for_positional_order_by, expr_matches, extract_grouping_sets,
    resolve_group_by_exprs, resolve_order_by_exprs_for_non_agg,
};
use pushdown::{
    generate_series_offset_limit_pushdown_eligible, normalize_query_offset_limit_fetch_expressions,
    plan_generate_series_offset_limit_pushdown,
};

impl Executor {
    pub(crate) fn execute_query_with_outer_ctes<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        query: &'a Query,
        outer_ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            if query.with.is_none() {
                return self
                    .execute_query_with_ctes(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        query,
                        outer_ctes,
                    )
                    .await;
            }

            let merged_ctes = self
                .build_cte_context_with_base(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    outer_ctes,
                )
                .await?;
            self.execute_query_with_ctes(
                txn,
                db_id,
                sequence_values,
                search_path,
                query,
                &merged_ctes,
            )
            .await
        })
    }

    pub(crate) async fn execute_query_with_ctes(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        if let SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } = &*query.body
        {
            let result = self
                .execute_set_operation(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    op,
                    set_quantifier,
                    left,
                    right,
                    ctes,
                )
                .await?;

            // ORDER BY / LIMIT / OFFSET apply to the full set-operation result.
            if let ExecuteResult::Select {
                columns,
                column_types,
                rows,
                timezone,
            } = result
            {
                let mut rows = rows;
                if !query.order_by.is_empty() {
                    rows = self.apply_order_by_for_aggregate(rows, &query.order_by, &columns);
                }
                rows = apply_offset_limit_fetch(rows, query);
                return Ok(ExecuteResult::Select {
                    columns,
                    column_types,
                    rows,
                    timezone,
                });
            }

            return Ok(result);
        }

        if let SetExpr::Values(values) = &*query.body {
            let store = self.store();
            let mut column_count: Option<usize> = None;
            let mut rows = Vec::with_capacity(values.rows.len());
            for expr_row in &values.rows {
                let expr_len = expr_row.len();
                if let Some(expected) = column_count {
                    if expr_len != expected {
                        return Err(anyhow!("VALUES lists must all be the same length"));
                    }
                } else {
                    column_count = Some(expr_len);
                }

                let mut row_values = Vec::with_capacity(expr_len);
                for expr in expr_row {
                    let resolved = self
                        .resolve_subqueries(txn, db_id, sequence_values, search_path, expr, ctes)
                        .await?;
                    let value = sequences::eval_expr_with_sequences(
                        &store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &resolved,
                        None,
                        None,
                    )
                    .await?;
                    row_values.push(value);
                }
                rows.push(Row::new(row_values));
            }

            let column_count = column_count.unwrap_or(0);
            let columns: Vec<String> = (1..=column_count)
                .map(|idx| format!("column{}", idx))
                .collect();
            let mut rows = rows;

            if !query.order_by.is_empty() {
                rows = self.apply_order_by_for_aggregate(rows, &query.order_by, &columns);
            }
            rows = apply_offset_limit_fetch(rows, query);

            return Ok(ExecuteResult::Select {
                column_types: None,
                columns,
                rows,
                timezone: crate::session_context::current_timezone(),
            });
        }

        let select = match &*query.body {
            SetExpr::Select(s) => s,
            _ => return Err(anyhow!("Only SELECT supported")),
        };

        let select_into_target = select
            .into
            .as_ref()
            .map(|into| (into.name.clone(), into.temporary));

        if select.from.is_empty() {
            let result = self
                .execute_tableless_query(txn, db_id, sequence_values, search_path, select, ctes)
                .await?;
            if let Some((target_name, _temp)) = select_into_target {
                return self
                    .create_table_from_result(txn, db_id, search_path, &target_name, result)
                    .await;
            }
            return Ok(result);
        }

        let has_joins = !select.from[0].joins.is_empty() || select.from.len() > 1;

        if has_joins {
            ensure_no_locking_clauses_for_join(query)?;
            if let Some(result) = self
                .try_execute_simple_join_with_operators(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    select,
                    ctes,
                )
                .await?
            {
                if let Some((target_name, _temp)) = select_into_target {
                    return self
                        .create_table_from_result(txn, db_id, search_path, &target_name, result)
                        .await;
                }
                return Ok(result);
            }

            return Err(anyhow!(
                "JOIN query could not be executed: unsupported table factor or schema not found"
            ));
        }

        let mut generate_series_offset_limit_pushed_down = false;
        let mut streaming_scan_operator: Option<BoxedOperator> = None;
        let mut query_with_evaluated_offset_limit_fetch: Option<Query> = None;
        let (t, outer_alias, schema, all_rows_base, is_virtual, rows_loaded) = match &select.from[0]
            .relation
        {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let (schema_opt, obj_name) = names::split_object_name(name)?;
                let tbl_upper = obj_name.to_uppercase();

                // Handle GENERATE_SERIES as a table-valued function
                if tbl_upper == "GENERATE_SERIES" {
                    if let Some(func_args) = args {
                        let als = alias
                            .as_ref()
                            .map(|a| a.name.value.clone())
                            .unwrap_or_else(|| obj_name.clone());
                        let query_for_pushdown =
                            if generate_series_offset_limit_pushdown_eligible(query, select) {
                                query_with_evaluated_offset_limit_fetch =
                                    Some(normalize_query_offset_limit_fetch_expressions(query));
                                query_with_evaluated_offset_limit_fetch.as_ref().unwrap()
                            } else {
                                query
                            };
                        let pushdown =
                            plan_generate_series_offset_limit_pushdown(query_for_pushdown, select);
                        let offset = pushdown.offset;
                        let limit = pushdown.limit;
                        generate_series_offset_limit_pushed_down =
                            pushdown.clear_query_offset_limit_fetch;

                        let (schema, rows) = self
                            .execute_generate_series(func_args, &als, alias.as_ref(), offset, limit)
                            .await?;
                        (schema.name.clone(), als, schema, rows, true, true)
                    } else {
                        return Err(anyhow!("generate_series requires at least 2 arguments"));
                    }
                } else {
                    if let Some(func_args) = args {
                        if let Some(result) = self
                            .try_execute_extension_table_function(
                                txn,
                                db_id,
                                search_path,
                                name,
                                func_args,
                                alias.as_ref(),
                            )
                            .await?
                        {
                            let alias_str = alias
                                .as_ref()
                                .map(|a| a.name.value.clone())
                                .unwrap_or_else(|| obj_name.clone());
                            match result {
                                ExtensionTableFunctionResult::Batch(schema, rows) => {
                                    (schema.name.clone(), alias_str, schema, rows, true, true)
                                }
                                ExtensionTableFunctionResult::Streaming(schema, operator) => {
                                    streaming_scan_operator = Some(operator);
                                    (
                                        schema.name.clone(),
                                        alias_str,
                                        schema,
                                        Vec::new(),
                                        true,
                                        true,
                                    )
                                }
                            }
                        } else if let Some((schema, rows)) = self
                            .try_execute_user_table_function(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                name,
                                func_args,
                                alias.as_ref(),
                            )
                            .await?
                        {
                            let alias_str = alias
                                .as_ref()
                                .map(|a| a.name.value.clone())
                                .unwrap_or_else(|| obj_name.clone());
                            (schema.name.clone(), alias_str, schema, rows, true, true)
                        } else {
                            let lookup_name = match schema_opt {
                                Some(schema) => format!("{}.{}", schema, obj_name),
                                None => obj_name.clone(),
                            };
                            let (schema, rows) = self
                                .get_table_data(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &lookup_name,
                                    ctes,
                                )
                                .await?;
                            let is_virtual = schema.table_id == 0;
                            let alias_str = alias
                                .as_ref()
                                .map(|a| a.name.value.clone())
                                .unwrap_or_else(|| obj_name.clone());
                            (
                                schema.name.clone(),
                                alias_str,
                                schema,
                                rows,
                                is_virtual,
                                true,
                            )
                        }
                    } else {
                        let alias_str = alias
                            .as_ref()
                            .map(|a| a.name.value.clone())
                            .unwrap_or_else(|| obj_name.clone());

                        // Prefer loading schema only for base tables, so we can use indexes
                        // without first scanning the full table.
                        let cte_key = obj_name.to_lowercase();
                        if let Some((cte_schema, cte_rows)) = ctes.get(&cte_key) {
                            (
                                cte_schema.name.clone(),
                                alias_str,
                                cte_schema.clone(),
                                cte_rows.clone(),
                                true,
                                true,
                            )
                        } else if let Some(resolved) = names::resolve_existing_table_name(
                            self.store().as_ref(),
                            txn,
                            db_id,
                            name,
                            search_path,
                        )
                        .await?
                        {
                            let schema = self
                                .store()
                                .get_schema(txn, db_id, &resolved.full)
                                .await?
                                .ok_or_else(|| SqlError::RelationNotFound(resolved.full.clone()))?;
                            (
                                schema.name.clone(),
                                alias_str,
                                schema,
                                Vec::new(),
                                false,
                                false,
                            )
                        } else {
                            let lookup_name = match schema_opt {
                                Some(schema) => format!("{}.{}", schema, obj_name),
                                None => obj_name.clone(),
                            };
                            let (schema, rows) = self
                                .get_table_data(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &lookup_name,
                                    ctes,
                                )
                                .await?;
                            let is_virtual = schema.table_id == 0;
                            (
                                schema.name.clone(),
                                alias_str,
                                schema,
                                rows,
                                is_virtual,
                                true,
                            )
                        }
                    }
                }
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let alias_name = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "subquery".to_string());
                let alias_columns = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);
                let (schema, rows) = self
                    .execute_derived_table(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        subquery,
                        &alias_name,
                        alias_columns,
                        ctes,
                    )
                    .await?;
                (alias_name.clone(), alias_name, schema, rows, true, true)
            }
            _ => return Err(SqlError::Unsupported("Unsupported table".into()).into()),
        };

        let is_from_cte = ctes.contains_key(&t.to_lowercase());
        let (is_virtual, rows_loaded) = if is_from_cte && use_operator_execution() {
            (false, false)
        } else {
            (is_virtual, rows_loaded)
        };

        let has_correlated_exists = select
            .selection
            .as_ref()
            .map(|sel| self.expr_has_correlated_exists(sel, &outer_alias))
            .unwrap_or(false);

        let resolved_selection = if let Some(sel) = &select.selection {
            if has_correlated_exists {
                Some(sel.clone())
            } else {
                Some(
                    self.resolve_subqueries(txn, db_id, sequence_values, search_path, sel, ctes)
                        .await?,
                )
            }
        } else {
            None
        };

        if let Some(sel) = resolved_selection.as_ref() {
            validate_bool_expr_in_boolean_context(
                sel,
                &schema,
                "Filter predicate must evaluate to boolean",
            )?;
        }

        let resolved_projection = self
            .resolve_projection_subqueries_with_outer_context(
                txn,
                db_id,
                sequence_values,
                search_path,
                &select.projection,
                &outer_alias,
                ctes,
            )
            .await?;

        let has_for_update = query
            .locks
            .iter()
            .any(|l| matches!(l.lock_type, LockType::Update));

        // Detect grouping sets (CUBE/ROLLUP/GROUPING SETS) early — not yet supported by operators.
        let group_by_exprs_for_grouping_sets_check = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs.as_slice(),
            GroupByExpr::All => &[][..],
        };
        let has_grouping_sets =
            extract_grouping_sets(group_by_exprs_for_grouping_sets_check).is_some();

        let has_udf = projection_may_have_udf(&resolved_projection)
            || resolved_selection
                .as_ref()
                .map_or(false, |sel| super::operators::expr_may_have_udf_pub(sel));

        let has_scalar_subquery = resolved_projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                expr_has_subquery(e)
            }
            _ => false,
        }) || resolved_selection
            .as_ref()
            .map_or(false, |sel| expr_has_subquery(sel));

        if use_operator_execution()
            && !has_correlated_exists
            && !has_scalar_subquery
            && !has_udf
            && query.locks.is_empty()
            && !(has_for_update && schema.pk_indices.is_empty())
            && select_into_target.is_none()
            && !has_grouping_sets
            && matches!(&*query.body, SetExpr::Select(_))
        {
            // Preloaded rows: virtual tables, materialized views, CTEs, derived tables,
            // generate_series results already have all rows in memory.
            let preloaded_source: Option<BoxedOperator> =
                if let Some(op) = streaming_scan_operator.take() {
                    Some(op)
                } else if is_virtual || rows_loaded {
                    Some(Box::new(TableScanOperator::new_with_rows(
                        schema.clone(),
                        all_rows_base,
                    )))
                } else {
                    None
                };

            if has_for_update {
                let planner = PhysicalPlanner::new(self.store(), search_path.to_vec());
                let estimated_rows = 1000;
                let mut lock_operator = planner.plan_simple_select(
                    db_id,
                    schema.clone(),
                    resolved_selection.as_ref(),
                    Vec::new(),
                    None,
                    0,
                    estimated_rows,
                )?;
                let lock_rows = execute_operator_tree(
                    &mut lock_operator,
                    txn,
                    self.store(),
                    db_id,
                    search_path,
                    sequence_values,
                )
                .await?;
                if !lock_rows.is_empty() {
                    self.store().lock_rows(txn, db_id, &t, &lock_rows).await?;
                }
            }

            let has_window = projection_has_window_function(&resolved_projection);
            let has_agg_or_group_by = projection_has_non_window_aggregate(&resolved_projection)
                || !matches!(
                    &select.group_by,
                    GroupByExpr::Expressions(exprs) if exprs.is_empty()
                )
                || select.having.is_some();

            if has_window && !has_agg_or_group_by {
                return self
                    .execute_window_with_operators(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        schema,
                        resolved_selection.as_ref(),
                        &query.order_by,
                        super::operators::extract_limit(query),
                        super::operators::extract_offset(query),
                        &resolved_projection,
                        ctes,
                        preloaded_source,
                    )
                    .await;
            } else if has_agg_or_group_by {
                let resolved_group_by = match &select.group_by {
                    GroupByExpr::Expressions(exprs) => GroupByExpr::Expressions(
                        resolve_group_by_exprs(exprs, &resolved_projection, &schema)?,
                    ),
                    GroupByExpr::All => GroupByExpr::All,
                };
                return self
                    .execute_aggregate_with_operators(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        schema,
                        resolved_selection.as_ref(),
                        &resolved_group_by,
                        select.having.as_ref(),
                        &query.order_by,
                        super::operators::extract_limit(query),
                        super::operators::extract_offset(query),
                        &resolved_projection,
                        ctes,
                        preloaded_source,
                    )
                    .await;
            } else {
                return self
                    .execute_with_operators(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        schema,
                        resolved_selection.as_ref(),
                        &query.order_by,
                        super::operators::extract_limit(query),
                        super::operators::extract_offset(query),
                        &resolved_projection,
                        select.distinct.as_ref(),
                        ctes,
                        preloaded_source,
                    )
                    .await;
            }
        }

        let pkless_for_update = has_for_update && schema.pk_indices.is_empty();
        if pkless_for_update && (is_virtual || rows_loaded) {
            return Err(anyhow!(
                "FOR UPDATE not supported for '{}': table has no primary key",
                t
            ));
        }

        let mut pkless_row_keys: Option<Vec<Vec<u8>>> = None;

        let all_rows = if is_virtual {
            all_rows_base
        } else if rows_loaded {
            // Rows are already materialized (CTE, derived table, view, etc). Index scans
            // are only applicable to base tables.
            all_rows_base
        } else if pkless_for_update {
            let mut rows_with_keys = self.store().scan_with_keys(txn, db_id, &t).await?;
            for (_, row) in &mut rows_with_keys {
                fill_row_defaults(row, &schema)?;
            }
            let (keys, rows) = rows_with_keys.into_iter().unzip();
            pkless_row_keys = Some(keys);
            rows
        } else {
            let estimated_rows = 1000;

            match &resolved_selection {
                None => {
                    let scan_upper_bound = {
                        let limit = super::operators::extract_limit(query);
                        let offset = super::operators::extract_offset(query);
                        match limit {
                            Some(0) => Some(0),
                            Some(n) => Some(offset.saturating_add(n)),
                            None => None,
                        }
                    };

                    let has_for_update = query
                        .locks
                        .iter()
                        .any(|l| matches!(l.lock_type, LockType::Update));

                    let can_pushdown_scan_limit = scan_upper_bound.is_some()
                        && !has_for_update
                        && select.distinct.is_none()
                        && query.order_by.is_empty()
                        && matches!(
                            &select.group_by,
                            GroupByExpr::Expressions(exprs) if exprs.is_empty()
                        )
                        && select.having.is_none()
                        && select.from.len() == 1
                        && select.from[0].joins.is_empty()
                        && !projection_has_window_function(&select.projection);

                    let scan_upper_bound = if can_pushdown_scan_limit {
                        scan_upper_bound
                    } else {
                        None
                    };

                    self.scan_and_fill_with_limit(txn, db_id, &t, &schema, scan_upper_bound)
                        .await?
                }
                Some(sel) => {
                    let access_path = planner::choose_best_access_path_for_filter(
                        db_id,
                        &schema,
                        Some(sel),
                        estimated_rows,
                    );

                    match access_path.scan_type {
                        ScanType::GinIndexScan {
                            index_id,
                            ref index_name,
                            ref column,
                            ref pattern,
                            ..
                        } => {
                            let index = schema.indexes.iter().find(|i| i.id == index_id);
                            let Some(idx) = index else {
                                return Err(anyhow!("GIN index not found"));
                            };

                            let gin_col_type = schema
                                .columns
                                .iter()
                                .find(|c| c.name.eq_ignore_ascii_case(column))
                                .map(|c| &c.data_type);

                            let token_hashes = match &pattern {
                                Value::Null => Vec::new(),
                                Value::Array(arr) => gin::extract_array_gin_tokens(arr),
                                Value::Tsquery(s) => gin::extract_tsquery_gin_tokens(s),
                                Value::Tsvector(s) => gin::extract_tsvector_gin_tokens(s),
                                Value::Json(s) | Value::Jsonb(s) => {
                                    let pattern_json: serde_json::Value = serde_json::from_str(s)
                                        .map_err(|e| {
                                        anyhow!("Invalid JSONB pattern for @>: {}", e)
                                    })?;
                                    gin::extract_gin_tokens(&pattern_json).into_scan_hashes()
                                }
                                Value::Text(s) => match gin_col_type {
                                    Some(DataType::Tsvector) => gin::extract_tsquery_gin_tokens(s),
                                    Some(DataType::Array(_)) => {
                                        let parsed: Vec<Value> =
                                            serde_json::from_str(s).unwrap_or_default();
                                        gin::extract_array_gin_tokens(&parsed)
                                    }
                                    _ => {
                                        let pattern_json: serde_json::Value =
                                            serde_json::from_str(s).map_err(|e| {
                                                anyhow!("Invalid JSONB pattern for @>: {}", e)
                                            })?;
                                        gin::extract_gin_tokens(&pattern_json).into_scan_hashes()
                                    }
                                },
                                other => {
                                    return Err(anyhow!(
                                        "GIN pattern must be array, json/jsonb, or tsquery, got {}",
                                        other.data_type().unwrap_or(DataType::Text)
                                    ));
                                }
                            };

                            if token_hashes.is_empty() {
                                debug!(
                                    "GIN predicate yields no tokens; falling back to full scan (index: {})",
                                    index_name
                                );
                                self.scan_and_fill(txn, db_id, &t, &schema).await?
                            } else {
                                debug!(
                                    "Using GIN Index Scan on {} (cost: {:.2})",
                                    index_name, access_path.cost
                                );
                                let pk_keys = self
                                    .store()
                                    .scan_gin_index_intersection(
                                        txn,
                                        db_id,
                                        schema.table_id,
                                        idx.id,
                                        &token_hashes,
                                    )
                                    .await?;
                                let mut rows = self
                                    .store()
                                    .batch_get_rows_by_pk_keys(txn, db_id, schema.table_id, pk_keys)
                                    .await?;
                                for r in &mut rows {
                                    fill_row_defaults(r, &schema)?;
                                }
                                rows
                            }
                        }
                        ScanType::IndexScan {
                            index_id,
                            ref index_name,
                            ref values,
                            ..
                        } => {
                            let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
                                vec![DataType::Uuid]
                            } else {
                                schema
                                    .pk_indices
                                    .iter()
                                    .map(|&idx| schema.columns[idx].data_type.clone())
                                    .collect()
                            };

                            let index = schema.indexes.iter().find(|i| i.id == index_id);
                            let Some(idx) = index else {
                                return Err(anyhow!("Index not found"));
                            };

                            debug!(
                                "Using Index Scan on {} (cost: {:.2})",
                                index_name, access_path.cost
                            );

                            let pks = self
                                .store()
                                .scan_index(
                                    txn,
                                    db_id,
                                    schema.table_id,
                                    idx.id,
                                    values,
                                    idx.unique,
                                    &pk_types,
                                    None,
                                )
                                .await?;
                            let mut rows = self
                                .store()
                                .batch_get_rows(txn, db_id, schema.table_id, pks, &schema)
                                .await?;
                            for r in &mut rows {
                                fill_row_defaults(r, &schema)?;
                            }
                            rows
                        }
                        ScanType::IndexRangeScan {
                            index_id,
                            ref index_name,
                            ref prefix_values,
                            ..
                        } => {
                            let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
                                vec![DataType::Uuid]
                            } else {
                                schema
                                    .pk_indices
                                    .iter()
                                    .map(|&idx| schema.columns[idx].data_type.clone())
                                    .collect()
                            };

                            let index = schema.indexes.iter().find(|i| i.id == index_id);
                            let Some(idx) = index else {
                                return Err(anyhow!("Index not found"));
                            };

                            let index_column_types: Vec<_> = idx
                                .columns
                                .iter()
                                .map(|col| {
                                    schema
                                        .columns
                                        .iter()
                                        .find(|c| c.name.eq_ignore_ascii_case(col))
                                        .map(|c| c.data_type.clone())
                                        .ok_or_else(|| anyhow!("Index column '{}' not found", col))
                                })
                                .collect::<Result<Vec<_>>>()?;

                            debug!(
                                "Using Index Range Scan on {} (cost: {:.2})",
                                index_name, access_path.cost
                            );

                            let pks = self
                                .store()
                                .scan_index_prefix(
                                    txn,
                                    db_id,
                                    schema.table_id,
                                    idx.id,
                                    prefix_values,
                                    idx.unique,
                                    &index_column_types,
                                    &pk_types,
                                    None,
                                )
                                .await?;
                            let mut rows = self
                                .store()
                                .batch_get_rows(txn, db_id, schema.table_id, pks, &schema)
                                .await?;
                            for r in &mut rows {
                                fill_row_defaults(r, &schema)?;
                            }
                            rows
                        }
                        ScanType::IndexBoundedRangeScan {
                            index_id,
                            ref index_name,
                            ref prefix_values,
                            ref range_start,
                            start_inclusive,
                            ref range_end,
                            end_inclusive,
                            ..
                        } => {
                            let index = schema.indexes.iter().find(|i| i.id == index_id);
                            let Some(idx) = index else {
                                return Err(anyhow!("Index not found"));
                            };

                            let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
                                vec![DataType::Uuid]
                            } else {
                                schema
                                    .pk_indices
                                    .iter()
                                    .map(|&i| schema.columns[i].data_type.clone())
                                    .collect()
                            };

                            let index_column_types: Vec<_> = idx
                                .columns
                                .iter()
                                .map(|col| {
                                    schema
                                        .columns
                                        .iter()
                                        .find(|c| c.name.eq_ignore_ascii_case(col))
                                        .map(|c| c.data_type.clone())
                                        .ok_or_else(|| anyhow!("Index column '{}' not found", col))
                                })
                                .collect::<Result<Vec<_>>>()?;

                            debug!(
                                "Using Index Bounded Range Scan on {} (cost: {:.2})",
                                index_name, access_path.cost
                            );

                            let pks = self
                                .store()
                                .scan_index_range(
                                    txn,
                                    db_id,
                                    schema.table_id,
                                    idx.id,
                                    prefix_values,
                                    range_start.as_ref(),
                                    start_inclusive,
                                    range_end.as_ref(),
                                    end_inclusive,
                                    idx.unique,
                                    &index_column_types,
                                    &pk_types,
                                    None,
                                )
                                .await?;
                            let mut rows = self
                                .store()
                                .batch_get_rows(txn, db_id, schema.table_id, pks, &schema)
                                .await?;
                            for r in &mut rows {
                                fill_row_defaults(r, &schema)?;
                            }
                            rows
                        }
                        ScanType::InListScan {
                            index_id,
                            ref index_name,
                            ref column_values,
                            ..
                        } => {
                            let index = schema.indexes.iter().find(|i| i.id == index_id);
                            let Some(idx) = index else {
                                return Err(anyhow!("Index not found"));
                            };

                            let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
                                vec![DataType::Uuid]
                            } else {
                                schema
                                    .pk_indices
                                    .iter()
                                    .map(|&i| schema.columns[i].data_type.clone())
                                    .collect()
                            };

                            debug!(
                                "Using In-List Scan on {} ({} values, cost: {:.2})",
                                index_name,
                                column_values.len(),
                                access_path.cost
                            );

                            let mut all_pks = Vec::new();
                            for values in column_values {
                                let pks = self
                                    .store()
                                    .scan_index(
                                        txn,
                                        db_id,
                                        schema.table_id,
                                        idx.id,
                                        values,
                                        idx.unique,
                                        &pk_types,
                                        None,
                                    )
                                    .await?;
                                all_pks.extend(pks);
                            }

                            let mut deduped_pks = Vec::with_capacity(all_pks.len());
                            for pk in all_pks {
                                if !deduped_pks.contains(&pk) {
                                    deduped_pks.push(pk);
                                }
                            }

                            let mut rows = self
                                .store()
                                .batch_get_rows(txn, db_id, schema.table_id, deduped_pks, &schema)
                                .await?;
                            for r in &mut rows {
                                fill_row_defaults(r, &schema)?;
                            }
                            rows
                        }
                        ScanType::FullTableScan => {
                            debug!("Using Full Table Scan (cost: {:.2})", access_path.cost);
                            self.scan_and_fill(txn, db_id, &t, &schema).await?
                        }
                    }
                }
            }
        };

        let (filtered_rows, lock_keys) = if pkless_for_update {
            let all_keys = pkless_row_keys
                .take()
                .ok_or_else(|| anyhow!("missing row keys for FOR UPDATE"))?;

            if let Some(ref sel) = resolved_selection {
                let mut rows = Vec::new();
                let mut keys = Vec::new();
                if has_correlated_exists {
                    for (key, r) in all_keys.into_iter().zip(all_rows.into_iter()) {
                        let result = self
                            .eval_selection_with_correlated_exists(
                                txn,
                                db_id,
                                sequence_values,
                                sel,
                                search_path,
                                &outer_alias,
                                &schema,
                                &r,
                            )
                            .await?;
                        let result = coerce_text_literal_to_bool(sel, result)?;
                        match result {
                            Value::Boolean(true) => {
                                rows.push(r);
                                keys.push(key);
                            }
                            Value::Boolean(false) | Value::Null => {}
                            other => {
                                return Err(anyhow!(
                                    "Filter predicate must evaluate to boolean, got {:?}",
                                    other
                                ));
                            }
                        }
                    }
                } else {
                    for (key, r) in all_keys.into_iter().zip(all_rows.into_iter()) {
                        let result = self
                            .eval_expr_maybe_sequence(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                sel,
                                Some(&r),
                                Some(&schema),
                            )
                            .await?;
                        let result = coerce_text_literal_to_bool(sel, result)?;
                        match result {
                            Value::Boolean(true) => {
                                rows.push(r);
                                keys.push(key);
                            }
                            Value::Boolean(false) | Value::Null => {}
                            other => {
                                return Err(anyhow!(
                                    "Filter predicate must evaluate to boolean, got {:?}",
                                    other
                                ));
                            }
                        }
                    }
                }
                (rows, keys)
            } else {
                (all_rows, all_keys)
            }
        } else if let Some(ref sel) = resolved_selection {
            let mut v = Vec::new();
            if has_correlated_exists {
                for r in all_rows {
                    let result = self
                        .eval_selection_with_correlated_exists(
                            txn,
                            db_id,
                            sequence_values,
                            sel,
                            search_path,
                            &outer_alias,
                            &schema,
                            &r,
                        )
                        .await?;
                    let result = coerce_text_literal_to_bool(sel, result)?;
                    match result {
                        Value::Boolean(true) => v.push(r),
                        Value::Boolean(false) | Value::Null => {}
                        other => {
                            return Err(anyhow!(
                                "Filter predicate must evaluate to boolean, got {:?}",
                                other
                            ));
                        }
                    }
                }
            } else {
                for r in all_rows {
                    let result = self
                        .eval_expr_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            sel,
                            Some(&r),
                            Some(&schema),
                        )
                        .await?;
                    let result = coerce_text_literal_to_bool(sel, result)?;
                    match result {
                        Value::Boolean(true) => v.push(r),
                        Value::Boolean(false) | Value::Null => {}
                        other => {
                            return Err(anyhow!(
                                "Filter predicate must evaluate to boolean, got {:?}",
                                other
                            ));
                        }
                    }
                }
            }
            (v, Vec::new())
        } else {
            (all_rows, Vec::new())
        };

        if has_for_update && pkless_for_update && !filtered_rows.is_empty() {
            txn.lock_keys(lock_keys).await.map_err(|e| anyhow!(e))?;
        }

        let group_keys_exprs = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs,
            GroupByExpr::All => {
                return Err(SqlError::Unsupported("GROUP BY ALL not supported".into()).into())
            }
        };

        let resolved_group_keys_exprs =
            resolve_group_by_exprs(group_keys_exprs, &resolved_projection, &schema)?;
        let group_keys_exprs = resolved_group_keys_exprs.as_slice();

        let grouping_sets = extract_grouping_sets(group_keys_exprs);
        let has_grouping_sets = grouping_sets.is_some();

        let window_funcs = extract_window_functions(&select.projection);

        let mut agg_funcs: Vec<(usize, AggExpr)> = Vec::new();
        let extra_start = select.projection.len();
        for (i, item) in select.projection.iter().enumerate() {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(f))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(f),
                    ..
                } => {
                    if f.over.is_none() {
                        let func_name = f
                            .name
                            .0
                            .last()
                            .map(|n| n.value.to_uppercase())
                            .unwrap_or_default();
                        if matches!(
                            func_name.as_str(),
                            "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "STRING_AGG" | "ARRAY_AGG"
                        ) {
                            agg_funcs.push((i, AggExpr::Function(f.clone())));
                        } else {
                            collect_having_agg_funcs(
                                &Expr::Function(f.clone()),
                                &mut agg_funcs,
                                extra_start,
                            );
                        }
                    }
                }
                SelectItem::UnnamedExpr(Expr::ArrayAgg(arr))
                | SelectItem::ExprWithAlias {
                    expr: Expr::ArrayAgg(arr),
                    ..
                } => {
                    agg_funcs.push((i, AggExpr::ArrayAgg(arr.clone())));
                }
                // Handle expressions containing nested aggregates (e.g., 'X=' || count(*))
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    collect_having_agg_funcs(expr, &mut agg_funcs, extra_start);
                }
                _ => {}
            }
        }

        if let Some(having_expr) = &select.having {
            collect_having_agg_funcs(having_expr, &mut agg_funcs, extra_start);
        }

        let is_agg = !group_keys_exprs.is_empty() || !agg_funcs.is_empty();

        if is_agg {
            if has_grouping_sets {
                return self
                    .execute_grouping_sets_query(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        query,
                        select,
                        &schema,
                        filtered_rows,
                        grouping_sets.unwrap(),
                        agg_funcs,
                        &resolved_projection,
                        select_into_target,
                    )
                    .await;
            }
            return self
                .execute_aggregate_query(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    select,
                    &schema,
                    filtered_rows,
                    group_keys_exprs,
                    agg_funcs,
                    &resolved_projection,
                    select_into_target,
                )
                .await;
        }

        let window_results = if !window_funcs.is_empty() {
            Some(compute_window_functions(
                &filtered_rows,
                &schema,
                &window_funcs,
            )?)
        } else {
            None
        };

        let output_exprs_for_order_by =
            expand_projection_exprs_for_positional_order_by(&resolved_projection, &schema);

        let order_by_references_correlated_subquery = query.order_by.iter().any(|order_expr| {
            if let Expr::Identifier(ref ident) = order_expr.expr {
                for item in &resolved_projection {
                    if let SelectItem::ExprWithAlias { expr, alias } = item {
                        if alias.value.eq_ignore_ascii_case(&ident.value) {
                            if let Expr::Subquery(_) = expr {
                                return true;
                            }
                        }
                    }
                }
                return false;
            }

            if let Expr::Value(SqlValue::Number(n, _)) = &order_expr.expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos > 0 {
                        if let Some(expr) = output_exprs_for_order_by.get(pos - 1) {
                            return matches!(expr, Expr::Subquery(_));
                        }
                    }
                }
                return false;
            }

            false
        });

        let (filtered_rows, window_results) =
            if !query.order_by.is_empty() && !order_by_references_correlated_subquery {
                self.apply_order_by(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    filtered_rows,
                    window_results,
                    &query.order_by,
                    &resolved_projection,
                    &schema,
                )
                .await?
            } else {
                (filtered_rows, window_results)
            };

        let has_window_funcs = !window_funcs.is_empty();
        let wildcard = select
            .projection
            .iter()
            .any(|p| matches!(p, SelectItem::Wildcard(_)));
        let pure_wildcard = wildcard && select.projection.len() == 1;

        let (mut rows_for_projection, mut window_results) = match &select.distinct {
            Some(Distinct::On(on_exprs)) => {
                let (rows, indices) =
                    distinct_on_rows_with_indices(filtered_rows, on_exprs, Some(&schema))?;
                let window_results =
                    window_results.map(|wr| super::super::query::reorder_by_indices(&wr, &indices));
                (rows, window_results)
            }
            _ => (filtered_rows, window_results),
        };

        if has_for_update && !pkless_for_update && !rows_for_projection.is_empty() {
            let for_update_nonblock = query
                .locks
                .iter()
                .find(|l| matches!(l.lock_type, LockType::Update))
                .and_then(|l| l.nonblock);

            let query_base_for_offset_limit_fetch = query_with_evaluated_offset_limit_fetch
                .as_ref()
                .unwrap_or(query);
            let query_for_offset_limit_fetch = if generate_series_offset_limit_pushed_down {
                let mut q = query_base_for_offset_limit_fetch.clone();
                q.offset = None;
                q.limit = None;
                q.fetch = None;
                q
            } else {
                query_base_for_offset_limit_fetch.clone()
            };

            let offset = extract_offset(&query_for_offset_limit_fetch);
            let max_lock = match extract_limit(&query_for_offset_limit_fetch) {
                Some(0) => Some(0),
                Some(n) => Some(offset.saturating_add(n)),
                None => None,
            };

            match for_update_nonblock {
                Some(NonBlock::SkipLocked) => {
                    let locked_indices = self
                        .store()
                        .lock_rows_skip_locked(txn, db_id, &t, &rows_for_projection, max_lock)
                        .await?;
                    rows_for_projection = locked_indices
                        .iter()
                        .map(|&idx| rows_for_projection[idx].clone())
                        .collect();
                    window_results = window_results
                        .map(|wr| super::super::query::reorder_by_indices(&wr, &locked_indices));
                }
                _ => {
                    let lock_count = max_lock.unwrap_or(rows_for_projection.len());
                    let lock_count = lock_count.min(rows_for_projection.len());
                    if lock_count > 0 {
                        self.store()
                            .lock_rows(txn, db_id, &t, &rows_for_projection[..lock_count])
                            .await?;
                    }
                }
            }
        }

        let (cols, result_rows) = if pure_wildcard && !has_window_funcs {
            let cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
            (cols, rows_for_projection)
        } else {
            self.project_rows(
                txn,
                db_id,
                sequence_values,
                search_path,
                select,
                &schema,
                &outer_alias,
                rows_for_projection,
                &resolved_projection,
                &window_funcs,
                window_results.as_ref(),
            )
            .await?
        };

        let mut result_rows = result_rows;
        if order_by_references_correlated_subquery && !query.order_by.is_empty() {
            self.sort_by_correlated_subquery(&mut result_rows, &cols, &query.order_by);
        }

        if matches!(&select.distinct, Some(Distinct::Distinct)) {
            result_rows = dedup_rows(result_rows);
        }

        let query_base_for_offset_limit_fetch = query_with_evaluated_offset_limit_fetch
            .as_ref()
            .unwrap_or(query);
        let query_no_offset_limit = if generate_series_offset_limit_pushed_down {
            let mut q = query_base_for_offset_limit_fetch.clone();
            q.offset = None;
            q.limit = None;
            q.fetch = None;
            Some(q)
        } else {
            None
        };
        let query_for_offset_limit_fetch = query_no_offset_limit
            .as_ref()
            .unwrap_or(query_base_for_offset_limit_fetch);
        result_rows = apply_offset_limit_fetch(result_rows, query_for_offset_limit_fetch);

        let column_types = Some(
            select
                .projection
                .iter()
                .flat_map(|item| match item {
                    SelectItem::Wildcard(_) => schema
                        .columns
                        .iter()
                        .map(|c| c.data_type.clone())
                        .collect::<Vec<_>>(),
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        vec![infer_expr_type(expr, &schema)]
                    }
                    _ => vec![DataType::Text],
                })
                .collect(),
        );

        let result = ExecuteResult::Select {
            column_types,
            columns: cols,
            rows: result_rows,
            timezone: crate::session_context::current_timezone(),
        };
        if let Some((target_name, _temp)) = select_into_target {
            return self
                .create_table_from_result(txn, db_id, search_path, &target_name, result)
                .await;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
