//! SELECT query execution

use super::super::aggregate::{collect_having_agg_funcs, AggExpr};
use super::super::distinct::{apply_offset_limit_fetch, dedup_rows};
use super::super::names;
use super::super::operators::{
    execute_operator_tree, execute_operator_tree_with_ctes, BoxedOperator, FilterOperator,
    HashAggregateOperator, HashJoinConfig, HashJoinOperator, HashJoinType, JoinType, LimitOperator,
    NestedLoopJoinOperator, PhysicalPlanner, SortOperator, TableScanOperator, WindowOperator,
};
use super::super::planner::{choose_join_algorithm, JoinAlgorithmChoice};
use super::super::projection::{get_select_item_name, infer_expr_type};
use super::super::sequences;
use super::super::wildcard::build_join_wildcard_plan;
use super::super::{
    expr::{coerce_text_literal_to_bool, eval_expr, validate_bool_expr_in_boolean_context},
    ExecuteResult,
};
use super::core::Executor;
use super::extensions::ExtensionTableFunctionResult;
use super::operators::rewrite_expr_for_multi_join;
use super::operators::{
    eval_having_expr_for_operators, extract_limit, extract_offset, is_aggregate_func,
    rewrite_agg_refs_to_columns,
};
use super::subquery::{expr_contains_subquery, substitute_outer_values};
use crate::sql::error::SqlError;
use crate::sql::information_schema::VirtualTableFilter;
#[allow(unused_imports)] // Re-exported for join sub-modules via glob import
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
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
pub(crate) mod order;
mod pushdown;

use analysis::projection_has_non_window_aggregate;
use analysis::projection_has_window_function;
use join::ensure_no_locking_clauses_for_join;
use order::{extract_grouping_sets, resolve_group_by_exprs};
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
                        .resolve_subqueries(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            ctes,
                            &[],
                        )
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

        let mut streaming_scan_operator: Option<BoxedOperator> = None;
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
                                &normalize_query_offset_limit_fetch_expressions(query)
                            } else {
                                query
                            };
                        let pushdown =
                            plan_generate_series_offset_limit_pushdown(query_for_pushdown, select);
                        let offset = pushdown.offset;
                        let limit = pushdown.limit;

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

        // Propagate FROM alias into the schema so that expression evaluation
        // can resolve whole-row references like `SELECT bar FROM foo_tbl AS bar`
        // and qualified column references like `SELECT bar.col`.
        let mut schema = schema;
        let schema_short_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
        if !schema_short_name.eq_ignore_ascii_case(&outer_alias) {
            schema.from_alias = Some(outer_alias.clone());
        }

        let is_from_cte = ctes.contains_key(&t.to_lowercase());
        let (is_virtual, rows_loaded) = if is_from_cte {
            (false, false)
        } else {
            (is_virtual, rows_loaded)
        };

        let from_aliases = vec![outer_alias.clone()];
        let resolved_selection = if let Some(sel) = &select.selection {
            Some(
                self.resolve_subqueries(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    sel,
                    ctes,
                    &from_aliases,
                )
                .await?,
            )
        } else {
            None
        };

        if let Some(sel) = resolved_selection.as_ref() {
            // Skip validation if the selection still contains correlated subqueries
            // (they'll be resolved per-row later).
            if !expr_contains_subquery(sel) {
                validate_bool_expr_in_boolean_context(
                    sel,
                    &schema,
                    "Filter predicate must evaluate to boolean",
                )?;
            }
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

        // Preloaded rows: virtual tables, materialized views, CTEs, derived tables,
        // generate_series results already have all rows in memory.
        // Also check for streaming scan operators from extension table functions.
        let preloaded_rows: Option<Vec<Row>> = if streaming_scan_operator.is_some() {
            // Streaming scan operator will be used directly; no preloaded rows.
            None
        } else if is_virtual || rows_loaded {
            Some(all_rows_base)
        } else {
            None
        };

        // If the resolved selection contains correlated subqueries or UDF/sequence
        // calls, the sync FilterOperator can't handle them.  Split into
        // operator-safe (pushdown) and async (per-row) parts.
        let needs_async_filter = resolved_selection.as_ref().is_some_and(|sel| {
            expr_contains_subquery(sel) || sequences::expr_needs_async_eval(sel)
        });
        let (resolved_selection, mut preloaded_rows) = if needs_async_filter {
            let sel = resolved_selection.unwrap();
            let (safe_parts, async_parts) = split_async_conjuncts(&sel);

            // Load all rows using only the operator-safe filter.
            let base_rows = if let Some(rows) = preloaded_rows {
                // Already have rows; apply the safe filter manually.
                if let Some(ref safe) = safe_parts {
                    let mut filtered = Vec::new();
                    for row in rows {
                        let val = eval_expr(safe, Some(&row), Some(&schema))?;
                        let val = coerce_text_literal_to_bool(safe, val)?;
                        if matches!(val, Value::Boolean(true)) {
                            filtered.push(row);
                        }
                    }
                    filtered
                } else {
                    rows
                }
            } else {
                let mut scan_op: BoxedOperator = if let Some(op) = streaming_scan_operator.take() {
                    op
                } else {
                    Box::new(TableScanOperator::new(schema.clone()))
                };
                if let Some(ref safe) = safe_parts {
                    scan_op = Box::new(FilterOperator::new(scan_op, safe.clone()));
                }
                if ctes.is_empty() {
                    execute_operator_tree(
                        &mut scan_op,
                        txn,
                        self.store(),
                        db_id,
                        search_path,
                        sequence_values,
                    )
                    .await?
                } else {
                    execute_operator_tree_with_ctes(
                        &mut scan_op,
                        txn,
                        self.store(),
                        db_id,
                        search_path,
                        sequence_values,
                        ctes,
                    )
                    .await?
                }
            };

            // Apply async filter per-row (handles correlated subqueries, UDFs, sequences).
            let filtered_rows = if let Some(ref async_filter) = async_parts {
                let has_subqueries = expr_contains_subquery(async_filter);
                let mut out = Vec::new();
                for row in base_rows {
                    let eval_expr_input = if has_subqueries {
                        // Substitute outer values and resolve any remaining subqueries.
                        let substituted =
                            substitute_outer_values(async_filter, &outer_alias, &schema, &row);
                        self.resolve_subqueries(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &substituted,
                            ctes,
                            &[],
                        )
                        .await?
                    } else {
                        async_filter.clone()
                    };
                    let val = self
                        .eval_expr_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &eval_expr_input,
                            Some(&row),
                            Some(&schema),
                        )
                        .await?;
                    let val = coerce_text_literal_to_bool(&eval_expr_input, val)?;
                    if matches!(val, Value::Boolean(true)) {
                        out.push(row);
                    }
                }
                out
            } else {
                base_rows
            };

            // Pass filtered rows as preloaded; no further WHERE filtering needed.
            (None, Some(filtered_rows))
        } else {
            (resolved_selection, preloaded_rows)
        };

        // FOR UPDATE / FOR SHARE: lock matching rows.
        let has_for_update = query
            .locks
            .iter()
            .any(|l| matches!(l.lock_type, LockType::Update));
        let has_for_share = query
            .locks
            .iter()
            .any(|l| matches!(l.lock_type, LockType::Share));
        let has_skip_locked = query
            .locks
            .iter()
            .any(|l| matches!(l.nonblock, Some(NonBlock::SkipLocked)));
        let has_nowait = query
            .locks
            .iter()
            .any(|l| matches!(l.nonblock, Some(NonBlock::Nowait)));

        if (has_for_update || has_for_share) && schema.pk_indices.is_empty() {
            return Err(anyhow!("FOR UPDATE/SHARE requires primary key"));
        }

        if has_for_update || has_for_share {
            let planner = PhysicalPlanner::new(search_path.to_vec());
            let estimated_rows = 1000;

            if has_skip_locked {
                // SKIP LOCKED: scan all matching rows in ORDER BY order (no
                // LIMIT/OFFSET) so we can try-lock each row with NOWAIT and
                // skip rows held by other transactions.
                let mut lock_operator = planner.plan_simple_select(
                    db_id,
                    schema.clone(),
                    resolved_selection.as_ref(),
                    query.order_by.clone(),
                    None,
                    0,
                    estimated_rows,
                )?;
                let candidate_rows = execute_operator_tree(
                    &mut lock_operator,
                    txn,
                    self.store(),
                    db_id,
                    search_path,
                    sequence_values,
                )
                .await?;

                // Try-lock rows one-by-one (NOWAIT); collect at most
                // offset + limit successfully-locked rows.
                let offset = extract_offset(query);
                let limit = extract_limit(query);
                let max_locks = limit.map(|l| offset + l);

                let locked_indices = self
                    .store()
                    .lock_rows_skip_locked(txn, db_id, &t, &candidate_rows, max_locks)
                    .await?;

                // Feed all locked rows into preloaded_rows (preserving order).
                // The main query will apply ORDER BY + OFFSET + LIMIT on top.
                let locked_rows: Vec<Row> = locked_indices
                    .iter()
                    .map(|&i| candidate_rows[i].clone())
                    .collect();
                preloaded_rows = Some(locked_rows);
            } else {
                // Regular FOR UPDATE/SHARE (including NOWAIT):
                // Lock only the rows that appear in the final result by
                // respecting ORDER BY + LIMIT + OFFSET in the lock scan.
                let mut lock_operator = planner.plan_simple_select(
                    db_id,
                    schema.clone(),
                    resolved_selection.as_ref(),
                    query.order_by.clone(),
                    extract_limit(query),
                    extract_offset(query),
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
                // Do not set preloaded_rows: the main query independently scans
                // and applies WHERE/ORDER BY/LIMIT, preserving correct behavior
                // when async WHERE conjuncts have already pre-filtered rows.
            }
        }
        let _ = has_nowait; // TODO: implement NOWAIT semantics

        // Detect grouping sets (CUBE/ROLLUP/GROUPING SETS).
        let group_by_exprs_for_grouping_sets_check = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs.as_slice(),
            GroupByExpr::All => &[][..],
        };
        let grouping_sets = extract_grouping_sets(group_by_exprs_for_grouping_sets_check);

        let has_window = projection_has_window_function(&resolved_projection);
        let has_agg_or_group_by = projection_has_non_window_aggregate(&resolved_projection)
            || !matches!(
                &select.group_by,
                GroupByExpr::Expressions(exprs) if exprs.is_empty()
            )
            || select.having.is_some();

        // Convert preloaded_rows (Option<Vec<Row>>) to preloaded_source (Option<BoxedOperator>)
        // for operator functions that expect BoxedOperator. Also consider streaming_scan_operator.
        let preloaded_source: Option<BoxedOperator> =
            if let Some(op) = streaming_scan_operator.take() {
                Some(op)
            } else if let Some(rows) = &preloaded_rows {
                Some(Box::new(TableScanOperator::new_with_rows(
                    schema.clone(),
                    rows.clone(),
                )))
            } else {
                None
            };

        let result = if let Some(grouping_sets) = grouping_sets {
            // GROUPING SETS / CUBE / ROLLUP
            self.execute_grouping_sets_with_operators(
                txn,
                db_id,
                sequence_values,
                search_path,
                schema.clone(),
                resolved_selection.as_ref(),
                &grouping_sets,
                select.having.as_ref(),
                &query.order_by,
                extract_limit(query),
                extract_offset(query),
                &resolved_projection,
                ctes,
                preloaded_rows,
            )
            .await?
        } else if has_window && !has_agg_or_group_by {
            self.execute_window_with_operators(
                txn,
                db_id,
                sequence_values,
                search_path,
                schema.clone(),
                resolved_selection.as_ref(),
                &query.order_by,
                extract_limit(query),
                extract_offset(query),
                &resolved_projection,
                ctes,
                preloaded_source,
                &select.named_window,
            )
            .await?
        } else if has_agg_or_group_by {
            let resolved_group_by = match &select.group_by {
                GroupByExpr::Expressions(exprs) => GroupByExpr::Expressions(
                    resolve_group_by_exprs(exprs, &resolved_projection, &schema)?,
                ),
                GroupByExpr::All => GroupByExpr::All,
            };
            self.execute_aggregate_with_operators(
                txn,
                db_id,
                sequence_values,
                search_path,
                schema.clone(),
                resolved_selection.as_ref(),
                &resolved_group_by,
                select.having.as_ref(),
                &query.order_by,
                extract_limit(query),
                extract_offset(query),
                &resolved_projection,
                ctes,
                preloaded_source,
            )
            .await?
        } else {
            self.execute_with_operators(
                txn,
                db_id,
                sequence_values,
                search_path,
                schema.clone(),
                resolved_selection.as_ref(),
                &query.order_by,
                extract_limit(query),
                extract_offset(query),
                &resolved_projection,
                select.distinct.as_ref(),
                ctes,
                preloaded_source,
            )
            .await?
        };

        // SELECT INTO post-processing: create table from result.
        if let Some((target_name, _temp)) = select_into_target {
            return self
                .create_table_from_result(txn, db_id, search_path, &target_name, result)
                .await;
        }
        Ok(result)
    }
}

/// Split an AND-connected expression into parts that are safe for the sync operator
/// path vs parts that need async per-row evaluation (subqueries, UDFs, sequences).
/// Returns (operator_safe, async_parts). Either may be None.
fn split_async_conjuncts(expr: &Expr) -> (Option<Expr>, Option<Expr>) {
    let conjuncts = flatten_and_conjuncts(expr);
    let mut safe = Vec::new();
    let mut needs_async = Vec::new();
    for c in conjuncts {
        if expr_contains_subquery(c) || sequences::expr_needs_async_eval(c) {
            needs_async.push(c.clone());
        } else {
            safe.push(c.clone());
        }
    }
    (
        if safe.is_empty() {
            None
        } else {
            Some(and_conjuncts(safe))
        },
        if needs_async.is_empty() {
            None
        } else {
            Some(and_conjuncts(needs_async))
        },
    )
}

fn flatten_and_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut parts = flatten_and_conjuncts(left);
            parts.extend(flatten_and_conjuncts(right));
            parts
        }
        Expr::Nested(inner) => flatten_and_conjuncts(inner),
        _ => vec![expr],
    }
}

fn and_conjuncts(mut exprs: Vec<Expr>) -> Expr {
    let first = exprs.remove(0);
    exprs.into_iter().fold(first, |acc, e| Expr::BinaryOp {
        left: Box::new(acc),
        op: BinaryOperator::And,
        right: Box::new(e),
    })
}

#[cfg(test)]
mod tests;
