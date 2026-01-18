//! SELECT query execution

use super::helpers::{
    apply_offset_limit_fetch, collect_having_agg_funcs, dedup_rows, distinct_on_rows_with_indices,
    eval_having_expr, fill_row_defaults, get_select_item_name, infer_expr_type, AggExpr,
};
use super::names;
use super::planner::{self, ScanType};
use super::sequences;
use super::window::{compute_window_functions, extract_window_functions, WindowFuncInfo};
use super::{expr::eval_expr, Aggregator, ExecuteResult, Executor};
use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Distinct, Expr, FunctionArg, FunctionArgExpr, GroupByExpr, LockType, ObjectName, Query,
    SelectItem, SetExpr, TableFactor, Value as SqlValue,
};
use std::collections::{HashMap, HashSet};
use tikv_client::Transaction;
use tracing::debug;

impl Executor {
    pub(crate) async fn execute_query_with_ctes(
        &self,
        txn: &mut Transaction,
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
                });
            }

            return Ok(result);
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
                .execute_tableless_query(txn, sequence_values, search_path, select, ctes)
                .await?;
            if let Some((target_name, _temp)) = select_into_target {
                return self
                    .create_table_from_result(txn, search_path, &target_name, result)
                    .await;
            }
            return Ok(result);
        }

        let has_joins = !select.from[0].joins.is_empty() || select.from.len() > 1;

        if has_joins {
            let result = self
                .execute_join_query_with_ctes(
                    txn,
                    sequence_values,
                    search_path,
                    query,
                    select,
                    ctes,
                )
                .await?;
            if let Some((target_name, _temp)) = select_into_target {
                return self
                    .create_table_from_result(txn, search_path, &target_name, result)
                    .await;
            }
            return Ok(result);
        }

        let (t, outer_alias, schema, all_rows_base, is_virtual) = match &select.from[0].relation {
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
                        let (schema, rows) = self
                            .execute_generate_series(func_args, &als, alias.as_ref())
                            .await?;
                        (schema.name.clone(), als, schema, rows, true)
                    } else {
                        return Err(anyhow!("generate_series requires at least 2 arguments"));
                    }
                } else {
                    let lookup_name = match schema_opt {
                        Some(schema) => format!("{}.{}", schema, obj_name),
                        None => obj_name.clone(),
                    };
                    let (schema, rows) = self
                        .get_table_data(txn, sequence_values, search_path, &lookup_name, ctes)
                        .await?;
                    let is_virtual = schema.table_id == 0;
                    let alias_str = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| obj_name.clone());
                    (schema.name.clone(), alias_str, schema, rows, is_virtual)
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
                        sequence_values,
                        search_path,
                        subquery,
                        &alias_name,
                        alias_columns,
                        ctes,
                    )
                    .await?;
                (alias_name.clone(), alias_name, schema, rows, true)
            }
            _ => return Err(anyhow!("Unsupported table")),
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
                    self.resolve_subqueries(txn, sequence_values, search_path, sel, ctes)
                        .await?,
                )
            }
        } else {
            None
        };

        let resolved_projection = self
            .resolve_projection_subqueries_with_outer_context(
                txn,
                sequence_values,
                search_path,
                &select.projection,
                &outer_alias,
                ctes,
            )
            .await?;

        let all_rows = if is_virtual {
            all_rows_base
        } else {
            let mut index_scan_rows = None;
            if let Some(ref sel) = resolved_selection {
                let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
                    vec![DataType::Uuid]
                } else {
                    schema
                        .pk_indices
                        .iter()
                        .map(|&idx| schema.columns[idx].data_type.clone())
                        .collect()
                };

                let predicates = planner::analyze_predicates(sel);
                let estimated_rows = all_rows_base.len().max(100);
                let access_path =
                    planner::choose_best_access_path(&schema, &predicates, estimated_rows);

                match access_path.scan_type {
                    ScanType::IndexScan {
                        index_id,
                        ref index_name,
                        ref values,
                        ..
                    } => {
                        let index = schema.indexes.iter().find(|i| i.id == index_id);
                        if let Some(idx) = index {
                            debug!(
                                "Using Index Scan on {} (cost: {:.2})",
                                index_name, access_path.cost
                            );
                            let pks = self
                                .store()
                                .scan_index(
                                    txn,
                                    schema.table_id,
                                    idx.id,
                                    values,
                                    idx.unique,
                                    &pk_types,
                                )
                                .await?;
                            if !pks.is_empty() {
                                let mut rows = self
                                    .store()
                                    .batch_get_rows(txn, schema.table_id, pks.clone(), &schema)
                                    .await?;
                                if !rows.is_empty() {
                                    for r in &mut rows {
                                        fill_row_defaults(r, &schema)?;
                                    }
                                    index_scan_rows = Some(rows);
                                }
                            }
                        }
                    }
                    ScanType::IndexRangeScan {
                        index_id,
                        ref index_name,
                        ref prefix_values,
                        ..
                    } => {
                        let index = schema.indexes.iter().find(|i| i.id == index_id);
                        if let Some(idx) = index {
                            debug!(
                                "Using Index Range Scan on {} with {} prefix columns (cost: {:.2})",
                                index_name,
                                prefix_values.len(),
                                access_path.cost
                            );
                            let pks = self
                                .store()
                                .scan_index(
                                    txn,
                                    schema.table_id,
                                    idx.id,
                                    prefix_values,
                                    idx.unique,
                                    &pk_types,
                                )
                                .await?;
                            if !pks.is_empty() {
                                let mut rows = self
                                    .store()
                                    .batch_get_rows(txn, schema.table_id, pks.clone(), &schema)
                                    .await?;
                                if !rows.is_empty() {
                                    for r in &mut rows {
                                        fill_row_defaults(r, &schema)?;
                                    }
                                    index_scan_rows = Some(rows);
                                }
                            }
                        }
                    }
                    ScanType::FullTableScan => {
                        debug!("Using Full Table Scan (cost: {:.2})", access_path.cost);
                    }
                }
            }
            index_scan_rows.unwrap_or(all_rows_base)
        };

        let filtered_rows = if let Some(ref sel) = resolved_selection {
            let mut v = Vec::new();
            if has_correlated_exists {
                for r in all_rows {
                    let result = self
                        .eval_selection_with_correlated_exists(
                            txn,
                            sequence_values,
                            sel,
                            search_path,
                            &outer_alias,
                            &schema,
                            &r,
                        )
                        .await?;
                    if matches!(result, Value::Boolean(true)) {
                        v.push(r);
                    }
                }
            } else {
                for r in all_rows {
                    if matches!(
                        self.eval_expr_maybe_sequence(
                            txn,
                            sequence_values,
                            search_path,
                            sel,
                            Some(&r),
                            Some(&schema),
                        )
                        .await?,
                        Value::Boolean(true)
                    ) {
                        v.push(r);
                    }
                }
            }
            v
        } else {
            all_rows
        };

        let has_for_update = query
            .locks
            .iter()
            .any(|l| matches!(l.lock_type, LockType::Update));
        if has_for_update && !filtered_rows.is_empty() {
            self.store().lock_rows(txn, &t, &filtered_rows).await?;
        }

        let group_keys_exprs = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs,
            GroupByExpr::All => return Err(anyhow!("GROUP BY ALL not supported")),
        };

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
            }
            false
        });

        let (filtered_rows, window_results) =
            if !query.order_by.is_empty() && !order_by_references_correlated_subquery {
                self.apply_order_by(
                    txn,
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

        let (rows_for_projection, window_results) = match &select.distinct {
            Some(Distinct::On(on_exprs)) => {
                let (rows, indices) =
                    distinct_on_rows_with_indices(filtered_rows, on_exprs, Some(&schema));
                let window_results =
                    window_results.map(|wr| super::query::reorder_by_indices(&wr, &indices));
                (rows, window_results)
            }
            _ => (filtered_rows, window_results),
        };

        let (cols, result_rows) = if pure_wildcard && !has_window_funcs {
            let cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
            (cols, rows_for_projection)
        } else {
            self.project_rows(
                txn,
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

        result_rows = apply_offset_limit_fetch(result_rows, query);

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
        };
        if let Some((target_name, _temp)) = select_into_target {
            return self
                .create_table_from_result(txn, search_path, &target_name, result)
                .await;
        }
        Ok(result)
    }

    async fn execute_aggregate_query(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
        schema: &TableSchema,
        filtered_rows: Vec<Row>,
        group_keys_exprs: &[Expr],
        agg_funcs: Vec<(usize, AggExpr)>,
        resolved_projection: &[SelectItem],
        select_into_target: Option<(ObjectName, bool)>,
    ) -> Result<ExecuteResult> {
        let mut groups: HashMap<Vec<u8>, Vec<Aggregator>> = HashMap::new();
        let mut group_rows: HashMap<Vec<u8>, Row> = HashMap::new();
        // Track seen values for DISTINCT aggregates: group_key -> (agg_idx -> seen_values)
        let mut seen_distinct: HashMap<Vec<u8>, Vec<HashSet<Vec<u8>>>> = HashMap::new();

        for row in filtered_rows {
            let mut key = Vec::new();
            for expr in group_keys_exprs {
                key.push(
                    self.eval_expr_maybe_sequence(
                        txn,
                        sequence_values,
                        search_path,
                        expr,
                        Some(&row),
                        Some(schema),
                    )
                    .await?,
                );
            }
            let key_bytes = bincode::serialize(&key).unwrap();

            if !groups.contains_key(&key_bytes) {
                let mut aggs = Vec::new();
                for (_, agg_expr) in &agg_funcs {
                    match agg_expr {
                        AggExpr::Function(f) => {
                            let name = f.name.0.last().unwrap().value.to_uppercase();
                            if name == "STRING_AGG" {
                                let delimiter = if f.args.len() >= 2 {
                                    match &f.args[1] {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                            match self
                                                .eval_expr_maybe_sequence(
                                                    txn,
                                                    sequence_values,
                                                    search_path,
                                                    e,
                                                    Some(&row),
                                                    Some(schema),
                                                )
                                                .await?
                                            {
                                                Value::Text(s) => s,
                                                _ => ",".to_string(),
                                            }
                                        }
                                        _ => ",".to_string(),
                                    }
                                } else {
                                    ",".to_string()
                                };
                                aggs.push(Aggregator::new_string_agg(delimiter));
                            } else {
                                aggs.push(Aggregator::new(&name)?);
                            }
                        }
                        AggExpr::ArrayAgg(_) => {
                            aggs.push(Aggregator::new_array_agg());
                        }
                    }
                }
                let distinct_sets: Vec<HashSet<Vec<u8>>> =
                    agg_funcs.iter().map(|_| HashSet::new()).collect();
                seen_distinct.insert(key_bytes.clone(), distinct_sets);
                groups.insert(key_bytes.clone(), aggs);
                group_rows.insert(key_bytes.clone(), row.clone());
            }

            let aggs = groups.get_mut(&key_bytes).unwrap();
            for (agg_idx, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                let (filter_expr, arg_expr) = match agg_expr {
                    AggExpr::Function(f) => {
                        let filter = f.filter.as_ref().map(|e| e.as_ref());
                        let arg = if f.args.is_empty() {
                            None
                        } else {
                            match &f.args[0] {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
                                _ => return Err(anyhow!("Unsupported arg")),
                            }
                        };
                        (filter, arg)
                    }
                    AggExpr::ArrayAgg(arr) => (None, Some(arr.expr.as_ref())),
                };

                if let Some(filter) = filter_expr {
                    let filter_val = self
                        .eval_expr_maybe_sequence(
                            txn,
                            sequence_values,
                            search_path,
                            filter,
                            Some(&row),
                            Some(schema),
                        )
                        .await?;
                    if !matches!(filter_val, Value::Boolean(true)) {
                        continue;
                    }
                }

                let val = if let Some(e) = arg_expr {
                    self.eval_expr_maybe_sequence(
                        txn,
                        sequence_values,
                        search_path,
                        e,
                        Some(&row),
                        Some(schema),
                    )
                    .await?
                } else {
                    Value::Int32(1)
                };

                let is_distinct = matches!(agg_expr, AggExpr::Function(f) if f.distinct);
                if is_distinct {
                    let val_bytes = bincode::serialize(&val).unwrap_or_default();
                    let distinct_sets = seen_distinct.get_mut(&key_bytes).unwrap();
                    if !distinct_sets[agg_idx].insert(val_bytes) {
                        continue;
                    }
                }

                aggs[agg_idx].update(&val)?;
            }
        }

        let mut final_rows = Vec::new();
        let col_names: Vec<String> = select.projection.iter().map(get_select_item_name).collect();

        if groups.is_empty() && group_keys_exprs.is_empty() && !agg_funcs.is_empty() {
            let mut default_aggs = Vec::new();
            for (_, agg_expr) in &agg_funcs {
                match agg_expr {
                    AggExpr::Function(f) => {
                        let name = f.name.0.last().unwrap().value.to_uppercase();
                        if name == "STRING_AGG" {
                            default_aggs.push(Aggregator::new_string_agg(",".to_string()));
                        } else {
                            default_aggs.push(Aggregator::new(&name)?);
                        }
                    }
                    AggExpr::ArrayAgg(_) => {
                        default_aggs.push(Aggregator::new_array_agg());
                    }
                }
            }
            let mut row_values = Vec::new();
            let empty_row = Row::new(vec![]);
            let empty_schema = TableSchema::default();
            for (i, item) in resolved_projection.iter().enumerate() {
                if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                    row_values.push(default_aggs[agg_pos].result());
                } else {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => {
                            row_values.push(Value::Null);
                            continue;
                        }
                    };
                    row_values.push(super::helpers::eval_having_expr(
                        expr,
                        &empty_row,
                        &empty_schema,
                        &agg_funcs,
                        &default_aggs,
                    )?);
                }
            }
            final_rows.push(Row::new(row_values));
        }

        for (key_bytes, aggs) in groups {
            let representative = &group_rows[&key_bytes];

            if let Some(having_expr) = &select.having {
                let having_expr = if sequences::expr_needs_async_eval(having_expr) {
                    sequences::replace_sequence_functions(
                        &self.store(),
                        txn,
                        sequence_values,
                        search_path,
                        having_expr,
                        Some(representative),
                        Some(schema),
                    )
                    .await?
                } else {
                    having_expr.clone()
                };
                let having_val =
                    eval_having_expr(&having_expr, representative, schema, &agg_funcs, &aggs)?;
                if !matches!(having_val, Value::Boolean(true)) {
                    continue;
                }
            }

            let mut row_values = Vec::new();

            for (i, item) in resolved_projection.iter().enumerate() {
                if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                    row_values.push(aggs[agg_pos].result());
                } else {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => return Err(anyhow!("Unsupported item")),
                    };
                    let expr = if sequences::expr_needs_async_eval(expr) {
                        sequences::replace_sequence_functions(
                            &self.store(),
                            txn,
                            sequence_values,
                            search_path,
                            expr,
                            Some(representative),
                            Some(schema),
                        )
                        .await?
                    } else {
                        expr.clone()
                    };
                    row_values.push(super::helpers::eval_having_expr(
                        &expr,
                        representative,
                        schema,
                        &agg_funcs,
                        &aggs,
                    )?);
                }
            }
            final_rows.push(Row::new(row_values));
        }

        let final_rows = if !query.order_by.is_empty() {
            self.apply_order_by_for_aggregate(final_rows, &query.order_by, &col_names)
        } else {
            final_rows
        };

        let final_rows = apply_offset_limit_fetch(final_rows, query);

        let result = ExecuteResult::Select {
            column_types: None,
            columns: col_names,
            rows: final_rows,
        };
        if let Some((target_name, _temp)) = select_into_target {
            return self
                .create_table_from_result(txn, search_path, &target_name, result)
                .await;
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_grouping_sets_query(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
        schema: &TableSchema,
        filtered_rows: Vec<Row>,
        grouping_sets: Vec<Vec<Expr>>,
        agg_funcs: Vec<(usize, AggExpr)>,
        resolved_projection: &[SelectItem],
        select_into_target: Option<(ObjectName, bool)>,
    ) -> Result<ExecuteResult> {
        let col_names: Vec<String> = resolved_projection
            .iter()
            .map(get_select_item_name)
            .collect();

        let all_group_cols: Vec<Expr> = grouping_sets
            .iter()
            .flatten()
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let mut all_final_rows = Vec::new();

        for grouping_set in &grouping_sets {
            let mut groups: HashMap<Vec<u8>, Vec<Aggregator>> = HashMap::new();
            let mut group_rows: HashMap<Vec<u8>, Row> = HashMap::new();

            for row in &filtered_rows {
                let mut key = Vec::new();
                for expr in grouping_set {
                    key.push(
                        self.eval_expr_maybe_sequence(
                            txn,
                            sequence_values,
                            search_path,
                            expr,
                            Some(row),
                            Some(schema),
                        )
                        .await?,
                    );
                }
                let key_bytes = bincode::serialize(&key).unwrap();

                if !groups.contains_key(&key_bytes) {
                    let mut aggs = Vec::new();
                    for (_, agg_expr) in &agg_funcs {
                        match agg_expr {
                            AggExpr::Function(f) => {
                                let name = f.name.0.last().unwrap().value.to_uppercase();
                                if name == "STRING_AGG" {
                                    let delimiter = if f.args.len() >= 2 {
                                        match &f.args[1] {
                                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                                match self
                                                    .eval_expr_maybe_sequence(
                                                        txn,
                                                        sequence_values,
                                                        search_path,
                                                        e,
                                                        Some(row),
                                                        Some(schema),
                                                    )
                                                    .await?
                                                {
                                                    Value::Text(s) => s,
                                                    _ => ",".to_string(),
                                                }
                                            }
                                            _ => ",".to_string(),
                                        }
                                    } else {
                                        ",".to_string()
                                    };
                                    aggs.push(Aggregator::new_string_agg(delimiter));
                                } else {
                                    aggs.push(Aggregator::new(&name)?);
                                }
                            }
                            AggExpr::ArrayAgg(_) => {
                                aggs.push(Aggregator::new_array_agg());
                            }
                        }
                    }
                    groups.insert(key_bytes.clone(), aggs);
                    group_rows.insert(key_bytes.clone(), row.clone());
                }

                let aggs = groups.get_mut(&key_bytes).unwrap();
                for (agg_idx, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                    let (filter_expr, arg_expr) = match agg_expr {
                        AggExpr::Function(f) => {
                            let filter = f.filter.as_ref().map(|e| e.as_ref());
                            let arg = if f.args.is_empty() {
                                None
                            } else {
                                match &f.args[0] {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
                                    _ => return Err(anyhow!("Unsupported arg")),
                                }
                            };
                            (filter, arg)
                        }
                        AggExpr::ArrayAgg(arr) => (None, Some(arr.expr.as_ref())),
                    };

                    if let Some(filter) = filter_expr {
                        let filter_val = self
                            .eval_expr_maybe_sequence(
                                txn,
                                sequence_values,
                                search_path,
                                filter,
                                Some(row),
                                Some(schema),
                            )
                            .await?;
                        if !matches!(filter_val, Value::Boolean(true)) {
                            continue;
                        }
                    }

                    let val = if let Some(e) = arg_expr {
                        self.eval_expr_maybe_sequence(
                            txn,
                            sequence_values,
                            search_path,
                            e,
                            Some(row),
                            Some(schema),
                        )
                        .await?
                    } else {
                        Value::Int32(1)
                    };
                    aggs[agg_idx].update(&val)?;
                }
            }

            for (key_bytes, aggs) in groups {
                let representative = &group_rows[&key_bytes];

                let mut group_eval_row = representative.clone();
                for group_expr in &all_group_cols {
                    let is_in_current_set = grouping_set
                        .iter()
                        .any(|gs_expr| expr_matches(gs_expr, group_expr));
                    if is_in_current_set {
                        continue;
                    }

                    let col_name = match group_expr {
                        Expr::Identifier(ident) => Some(ident.value.as_str()),
                        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.as_str()),
                        _ => None,
                    };
                    if let Some(col_name) = col_name {
                        if let Some(idx) = schema
                            .columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(col_name))
                        {
                            if idx < group_eval_row.values.len() {
                                group_eval_row.values[idx] = Value::Null;
                            }
                        }
                    }
                }

                if let Some(having_expr) = &select.having {
                    let having_expr = if sequences::expr_needs_async_eval(having_expr) {
                        sequences::replace_sequence_functions(
                            &self.store(),
                            txn,
                            sequence_values,
                            search_path,
                            having_expr,
                            Some(&group_eval_row),
                            Some(schema),
                        )
                        .await?
                    } else {
                        having_expr.clone()
                    };
                    let having_val =
                        eval_having_expr(&having_expr, &group_eval_row, schema, &agg_funcs, &aggs)?;
                    if !matches!(having_val, Value::Boolean(true)) {
                        continue;
                    }
                }

                let mut row_values = Vec::new();

                for (i, item) in resolved_projection.iter().enumerate() {
                    if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                        row_values.push(aggs[agg_pos].result());
                    } else {
                        let expr = match item {
                            SelectItem::UnnamedExpr(e)
                            | SelectItem::ExprWithAlias { expr: e, .. } => e,
                            _ => return Err(anyhow!("Unsupported item")),
                        };

                        if let Expr::Function(func) = expr {
                            let func_name = func
                                .name
                                .0
                                .last()
                                .map(|i| i.value.to_uppercase())
                                .unwrap_or_default();
                            if func_name == "GROUPING" && func.args.len() == 1 {
                                let arg_expr = match &func.args[0] {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                };
                                if let Some(arg_expr) = arg_expr {
                                    let is_in_current_set = grouping_set
                                        .iter()
                                        .any(|gs_expr| expr_matches(gs_expr, arg_expr));
                                    row_values.push(Value::Int32(if is_in_current_set { 0 } else { 1 }));
                                    continue;
                                }
                            }
                        }

                        let is_in_current_set =
                            grouping_set.iter().any(|gs_expr| expr_matches(gs_expr, expr));
                        if is_in_current_set {
                            let expr = if sequences::expr_needs_async_eval(expr) {
                                sequences::replace_sequence_functions(
                                    &self.store(),
                                    txn,
                                    sequence_values,
                                    search_path,
                                    expr,
                                    Some(&group_eval_row),
                                    Some(schema),
                                )
                                .await?
                            } else {
                                expr.clone()
                            };
                            row_values.push(super::helpers::eval_having_expr(
                                &expr,
                                &group_eval_row,
                                schema,
                                &agg_funcs,
                                &aggs,
                            )?);
                        } else {
                            row_values.push(Value::Null);
                        }
                    }
                }
                all_final_rows.push(Row::new(row_values));
            }
        }

        let final_rows = if !query.order_by.is_empty() {
            self.apply_order_by_for_aggregate(all_final_rows, &query.order_by, &col_names)
        } else {
            all_final_rows
        };

        let final_rows = apply_offset_limit_fetch(final_rows, query);

        let result = ExecuteResult::Select {
            column_types: None,
            columns: col_names,
            rows: final_rows,
        };
        if let Some((target_name, _temp)) = select_into_target {
            return self
                .create_table_from_result(txn, search_path, &target_name, result)
                .await;
        }
        Ok(result)
    }

    fn apply_order_by_for_aggregate(
        &self,
        rows: Vec<Row>,
        order_by: &[sqlparser::ast::OrderByExpr],
        col_names: &[String],
    ) -> Vec<Row> {
        let mut indexed: Vec<(usize, Row)> = rows.into_iter().enumerate().collect();
        indexed.sort_by(|(idx_a, a), (idx_b, b)| {
            for order_expr in order_by {
                let col_idx = match &order_expr.expr {
                    Expr::Identifier(ident) => col_names
                        .iter()
                        .position(|n| n.eq_ignore_ascii_case(&ident.value)),
                    Expr::CompoundIdentifier(parts) => parts
                        .last()
                        .and_then(|ident| col_names.iter().position(|n| n.eq_ignore_ascii_case(&ident.value))),
                    Expr::Value(SqlValue::Number(n, _)) => n.parse::<usize>().ok().map(|i| i.saturating_sub(1)),
                    _ => None,
                };

                let (val_a, val_b) = if let Some(idx) = col_idx {
                    (a.values.get(idx).cloned(), b.values.get(idx).cloned())
                } else {
                    (None, None)
                };

                let val_a = val_a.unwrap_or(Value::Null);
                let val_b = val_b.unwrap_or(Value::Null);

                let asc = order_expr.asc.unwrap_or(true);
                let nulls_first = order_expr.nulls_first.unwrap_or(!asc);

                match (&val_a, &val_b) {
                    (Value::Null, Value::Null) => continue,
                    (Value::Null, _) => {
                        return if nulls_first {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        }
                    }
                    (_, Value::Null) => {
                        return if nulls_first {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Less
                        }
                    }
                    _ => {}
                }

                let cmp = super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                if cmp != 0 {
                    return if asc {
                        if cmp > 0 {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Less
                        }
                    } else if cmp > 0 {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    };
                }
            }

            // Deterministic tie-breaker to match PostgreSQL's stable-looking output:
            // compare full output rows when ORDER BY keys are equal.
            let max_cols = a.values.len().max(b.values.len());
            for i in 0..max_cols {
                let va = a.values.get(i).unwrap_or(&Value::Null);
                let vb = b.values.get(i).unwrap_or(&Value::Null);
                let cmp = super::expr::compare_values(va, vb).unwrap_or(0);
                if cmp != 0 {
                    return if cmp > 0 {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Less
                    };
                }
            }

            idx_a.cmp(idx_b)
        });
        indexed.into_iter().map(|(_, r)| r).collect()
    }

    async fn apply_order_by(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        filtered_rows: Vec<Row>,
        window_results: Option<Vec<Vec<Value>>>,
        order_by: &[sqlparser::ast::OrderByExpr],
        resolved_projection: &[SelectItem],
        schema: &TableSchema,
    ) -> Result<(Vec<Row>, Option<Vec<Vec<Value>>>)> {
        let resolved_order_exprs: Vec<Expr> = order_by
            .iter()
            .map(|order_expr| {
                if let Expr::Identifier(ref ident) = order_expr.expr {
                    for item in resolved_projection {
                        if let SelectItem::ExprWithAlias { expr, alias } = item {
                            if alias.value.eq_ignore_ascii_case(&ident.value) {
                                return expr.clone();
                            }
                        }
                    }
                }
                order_expr.expr.clone()
            })
            .collect();

        let order_by_uses_sequences = resolved_order_exprs
            .iter()
            .any(|e| sequences::expr_needs_async_eval(e));

        if order_by_uses_sequences {
            let mut rows_with_keys: Vec<(usize, Row, Vec<Value>)> =
                Vec::with_capacity(filtered_rows.len());
            for (orig_idx, row) in filtered_rows.into_iter().enumerate() {
                let mut keys = Vec::with_capacity(order_by.len());
                for actual_expr in &resolved_order_exprs {
                    let val = if sequences::expr_needs_async_eval(actual_expr) {
                        self.eval_expr_maybe_sequence(
                            txn,
                            sequence_values,
                            search_path,
                            actual_expr,
                            Some(&row),
                            Some(schema),
                        )
                        .await?
                    } else {
                        eval_expr(actual_expr, Some(&row), Some(schema)).unwrap_or(Value::Null)
                    };
                    keys.push(val);
                }
                rows_with_keys.push((orig_idx, row, keys));
            }

            rows_with_keys.sort_by(|(_, _, a_keys), (_, _, b_keys)| {
                for (idx, order_expr) in order_by.iter().enumerate() {
                    let val_a = a_keys.get(idx).cloned().unwrap_or(Value::Null);
                    let val_b = b_keys.get(idx).cloned().unwrap_or(Value::Null);
                    let cmp = super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                    if cmp != 0 {
                        let asc = order_expr.asc.unwrap_or(true);
                        return if asc {
                            if cmp > 0 {
                                std::cmp::Ordering::Greater
                            } else {
                                std::cmp::Ordering::Less
                            }
                        } else if cmp > 0 {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        };
                    }
                }
                std::cmp::Ordering::Equal
            });

            let reordered_wr = window_results.map(|wr| {
                rows_with_keys
                    .iter()
                    .map(|(orig_idx, _, _)| wr[*orig_idx].clone())
                    .collect()
            });
            let reordered_rows: Vec<Row> = rows_with_keys.into_iter().map(|(_, r, _)| r).collect();
            Ok((reordered_rows, reordered_wr))
        } else {
            let mut indexed: Vec<(usize, Row)> = filtered_rows.into_iter().enumerate().collect();
            indexed.sort_by(|(_, a), (_, b)| {
                for (idx, order_expr) in order_by.iter().enumerate() {
                    let actual_expr = &resolved_order_exprs[idx];
                    let val_a =
                        eval_expr(actual_expr, Some(a), Some(schema)).unwrap_or(Value::Null);
                    let val_b =
                        eval_expr(actual_expr, Some(b), Some(schema)).unwrap_or(Value::Null);
                    let cmp = super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                    if cmp != 0 {
                        let asc = order_expr.asc.unwrap_or(true);
                        return if asc {
                            if cmp > 0 {
                                std::cmp::Ordering::Greater
                            } else {
                                std::cmp::Ordering::Less
                            }
                        } else if cmp > 0 {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        };
                    }
                }
                std::cmp::Ordering::Equal
            });
            let reordered_wr = window_results.map(|wr| {
                indexed
                    .iter()
                    .map(|(orig_idx, _)| wr[*orig_idx].clone())
                    .collect()
            });
            let reordered_rows: Vec<Row> = indexed.into_iter().map(|(_, r)| r).collect();
            Ok((reordered_rows, reordered_wr))
        }
    }

    async fn project_rows(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        select: &sqlparser::ast::Select,
        schema: &TableSchema,
        outer_alias: &str,
        rows_for_projection: Vec<Row>,
        resolved_projection: &[SelectItem],
        window_funcs: &[WindowFuncInfo],
        window_results: Option<&Vec<Vec<Value>>>,
    ) -> Result<(Vec<String>, Vec<Row>)> {
        let mut cols = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {
                    for c in &schema.columns {
                        cols.push(c.name.clone());
                    }
                }
                _ => {
                    let col_name = get_select_item_name(item);
                    cols.push(col_name);
                }
            }
        }

        fn validate_projection_expr(expr: &Expr, schema: &TableSchema) -> Result<()> {
            match expr {
                Expr::Identifier(ident) => {
                    if schema
                        .columns
                        .iter()
                        .all(|c| !c.name.eq_ignore_ascii_case(&ident.value))
                    {
                        return Err(anyhow!("Column '{}' not found", ident.value));
                    }
                    Ok(())
                }
                Expr::CompoundIdentifier(parts) => {
                    if let Some(last) = parts.last() {
                        if schema
                            .columns
                            .iter()
                            .all(|c| !c.name.eq_ignore_ascii_case(&last.value))
                        {
                            return Err(anyhow!("Column '{}' not found", last.value));
                        }
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }

        for item in resolved_projection {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    validate_projection_expr(expr, schema)?;
                }
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {}
            }
        }

        #[derive(Copy, Clone)]
        enum SrfKind {
            Unnest,
            RegexpSplitToTable,
            RegexpMatches,
            EvalFunctionArray,
        }

        fn srf_kind(expr: &Expr) -> Option<SrfKind> {
            let Expr::Function(f) = expr else {
                return None;
            };
            let Some(name) = f.name.0.last() else {
                return None;
            };
            match name.value.to_ascii_uppercase().as_str() {
                "UNNEST" => Some(SrfKind::Unnest),
                "REGEXP_SPLIT_TO_TABLE" => Some(SrfKind::RegexpSplitToTable),
                "REGEXP_MATCHES" => Some(SrfKind::RegexpMatches),
                "JSONB_OBJECT_KEYS"
                | "JSONB_ARRAY_ELEMENTS"
                | "JSONB_ARRAY_ELEMENTS_TEXT"
                | "JSONB_EACH"
                | "JSONB_EACH_TEXT" => Some(SrfKind::EvalFunctionArray),
                _ => None,
            }
        }

        fn regexp_captures_to_values(caps: &regex::Captures<'_>) -> Vec<Value> {
            if caps.len() > 1 {
                (1..caps.len())
                    .map(|idx| match caps.get(idx) {
                        Some(m) => Value::Text(m.as_str().to_string()),
                        None => Value::Null,
                    })
                    .collect()
            } else {
                caps.get(0)
                    .map(|m| vec![Value::Text(m.as_str().to_string())])
                    .unwrap_or_default()
            }
        }

        let has_srf = resolved_projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                srf_kind(e).is_some()
            }
            _ => false,
        });

        let mut result_rows = Vec::new();
        for (row_idx, row) in rows_for_projection.iter().enumerate() {
            let mut row_values = Vec::new();
            let mut srf_outputs: Vec<(usize, Vec<Value>)> = Vec::new();

            for (proj_idx, item) in resolved_projection.iter().enumerate() {
                if let Some(wf_pos) = window_funcs.iter().position(|wf| wf.proj_idx == proj_idx) {
                    if let Some(wr) = window_results {
                        row_values.push(wr[row_idx][wf_pos].clone());
                    } else {
                        row_values.push(Value::Null);
                    }
                } else {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) => e,
                        SelectItem::ExprWithAlias { expr: e, .. } => e,
                        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {
                            row_values.extend(row.values.clone());
                            continue;
                        }
                    };

                    if has_srf {
                        if let Some(kind) = srf_kind(expr) {
                            let Expr::Function(f) = expr else {
                                row_values.push(Value::Null);
                                continue;
                            };

                            let outputs = match kind {
                                SrfKind::Unnest => {
                                    let arg_expr = f.args.first().and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    if let Some(arg_expr) = arg_expr {
                                        match self
                                            .eval_expr_maybe_sequence(
                                                txn,
                                                sequence_values,
                                                search_path,
                                                arg_expr,
                                                Some(row),
                                                Some(schema),
                                            )
                                            .await?
                                        {
                                            Value::Array(arr) => arr,
                                            Value::Null => Vec::new(),
                                            other => vec![other],
                                        }
                                    } else {
                                        Vec::new()
                                    }
                                }
                                SrfKind::RegexpSplitToTable => {
                                    let arg0 = f.args.get(0).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let arg1 = f.args.get(1).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let arg2 = f.args.get(2).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let (Some(arg0), Some(arg1)) = (arg0, arg1) else {
                                        return Err(anyhow!(
                                            "regexp_split_to_table requires at least 2 arguments"
                                        ));
                                    };

                                    let source_val = self
                                        .eval_expr_maybe_sequence(
                                            txn,
                                            sequence_values,
                                            search_path,
                                            arg0,
                                            Some(row),
                                            Some(schema),
                                        )
                                        .await?;
                                    let source = match source_val {
                                        Value::Text(s) => Some(s),
                                        Value::Null => None,
                                        v => Some(v.to_string()),
                                    };

                                    let pattern_val = self
                                        .eval_expr_maybe_sequence(
                                            txn,
                                            sequence_values,
                                            search_path,
                                            arg1,
                                            Some(row),
                                            Some(schema),
                                        )
                                        .await?;
                                    let pattern = match pattern_val {
                                        Value::Text(s) => Some(s),
                                        Value::Null => None,
                                        v => Some(v.to_string()),
                                    };

                                    match (source, pattern) {
                                        (Some(source), Some(pattern)) => {
                                        let flags = if let Some(arg2) = arg2 {
                                            match self
                                                .eval_expr_maybe_sequence(
                                                    txn,
                                                    sequence_values,
                                                    search_path,
                                                    arg2,
                                                    Some(row),
                                                    Some(schema),
                                                )
                                                .await?
                                            {
                                                Value::Text(s) => s,
                                                Value::Null => String::new(),
                                                v => v.to_string(),
                                            }
                                        } else {
                                            String::new()
                                        };
                                        let case_insensitive =
                                            flags.to_ascii_lowercase().contains('i');
                                        let regex_pattern = if case_insensitive {
                                            format!("(?i){}", pattern)
                                        } else {
                                            pattern
                                        };
                                        let re = regex::Regex::new(&regex_pattern)
                                            .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;

                                        let mut parts = Vec::new();
                                        let mut last_end = 0usize;
                                        for m in re.find_iter(&source) {
                                            parts.push(Value::Text(
                                                source[last_end..m.start()].to_string(),
                                            ));
                                            last_end = m.end();
                                        }
                                        parts.push(Value::Text(source[last_end..].to_string()));
                                        parts
                                        }
                                        _ => Vec::new(),
                                    }
                                }
                                SrfKind::RegexpMatches => {
                                    let arg0 = f.args.get(0).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let arg1 = f.args.get(1).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let arg2 = f.args.get(2).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let (Some(arg0), Some(arg1)) = (arg0, arg1) else {
                                        return Err(anyhow!(
                                            "regexp_matches requires at least 2 arguments"
                                        ));
                                    };
                                    let source_val = self
                                        .eval_expr_maybe_sequence(
                                            txn,
                                            sequence_values,
                                            search_path,
                                            arg0,
                                            Some(row),
                                            Some(schema),
                                        )
                                        .await?;
                                    let source = match source_val {
                                        Value::Text(s) => Some(s),
                                        Value::Null => None,
                                        v => Some(v.to_string()),
                                    };

                                    let pattern_val = self
                                        .eval_expr_maybe_sequence(
                                            txn,
                                            sequence_values,
                                            search_path,
                                            arg1,
                                            Some(row),
                                            Some(schema),
                                        )
                                        .await?;
                                    let pattern = match pattern_val {
                                        Value::Text(s) => Some(s),
                                        Value::Null => None,
                                        v => Some(v.to_string()),
                                    };

                                    match (source, pattern) {
                                        (Some(source), Some(pattern)) => {
                                    let flags = if let Some(arg2) = arg2 {
                                        match self
                                            .eval_expr_maybe_sequence(
                                                txn,
                                                sequence_values,
                                                search_path,
                                                arg2,
                                                Some(row),
                                                Some(schema),
                                            )
                                            .await?
                                        {
                                            Value::Text(s) => s,
                                            Value::Null => String::new(),
                                            v => v.to_string(),
                                        }
                                    } else {
                                        String::new()
                                    };
                                    let global = flags.to_ascii_lowercase().contains('g');
                                    let case_insensitive = flags.to_ascii_lowercase().contains('i');
                                    let regex_pattern = if case_insensitive {
                                        format!("(?i){}", pattern)
                                    } else {
                                        pattern
                                    };
                                    let re = regex::Regex::new(&regex_pattern)
                                        .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;

                                    let mut out = Vec::new();
                                    if global {
                                        for caps in re.captures_iter(&source) {
                                            out.push(Value::Array(regexp_captures_to_values(&caps)));
                                        }
                                    } else if let Some(caps) = re.captures(&source) {
                                        out.push(Value::Array(regexp_captures_to_values(&caps)));
                                    }
                                    out
                                        }
                                        _ => Vec::new(),
                                    }
                                }
                                SrfKind::EvalFunctionArray => {
                                    match self
                                        .eval_expr_maybe_sequence(
                                            txn,
                                            sequence_values,
                                            search_path,
                                            expr,
                                            Some(row),
                                            Some(schema),
                                        )
                                        .await?
                                    {
                                        Value::Array(arr) => arr,
                                        Value::Null => Vec::new(),
                                        other => vec![other],
                                    }
                                }
                            };

                            srf_outputs.push((row_values.len(), outputs));
                            row_values.push(Value::Null);
                            continue;
                        }
                    } else {
                        let value = if let Expr::Subquery(subquery) = expr {
                            self.eval_correlated_subquery(
                                txn,
                                sequence_values,
                                search_path,
                                subquery,
                                outer_alias,
                                schema,
                                row,
                            )
                            .await?
                        } else {
                            self.eval_expr_maybe_sequence(
                                txn,
                                sequence_values,
                                search_path,
                                expr,
                                Some(row),
                                Some(schema),
                            )
                            .await?
                        };
                        row_values.push(value);
                    }
                }
            }

            if !srf_outputs.is_empty() {
                let max_len = srf_outputs
                    .iter()
                    .map(|(_, out)| out.len())
                    .max()
                    .unwrap_or(0);
                for i in 0..max_len {
                    let mut expanded_row = row_values.clone();
                    for (col_idx, out) in &srf_outputs {
                        expanded_row[*col_idx] = out.get(i).cloned().unwrap_or(Value::Null);
                    }
                    result_rows.push(Row::new(expanded_row));
                }
            } else {
                result_rows.push(Row::new(row_values));
            }
        }
        Ok((cols, result_rows))
    }

    fn sort_by_correlated_subquery(
        &self,
        result_rows: &mut [Row],
        cols: &[String],
        order_by: &[sqlparser::ast::OrderByExpr],
    ) {
        result_rows.sort_by(|a, b| {
            for order_expr in order_by {
                let col_idx = if let Expr::Identifier(ref ident) = order_expr.expr {
                    cols.iter()
                        .position(|c| c.eq_ignore_ascii_case(&ident.value))
                } else if let Expr::Value(SqlValue::Number(n, _)) = &order_expr.expr {
                    n.parse::<usize>().ok().map(|i| i.saturating_sub(1))
                } else {
                    None
                };

                if let Some(idx) = col_idx {
                    let val_a = a.values.get(idx).cloned().unwrap_or(Value::Null);
                    let val_b = b.values.get(idx).cloned().unwrap_or(Value::Null);
                    let cmp = super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                    if cmp != 0 {
                        let asc = order_expr.asc.unwrap_or(true);
                        return if asc {
                            if cmp > 0 {
                                std::cmp::Ordering::Greater
                            } else {
                                std::cmp::Ordering::Less
                            }
                        } else if cmp > 0 {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        };
                    }
                }
            }
            std::cmp::Ordering::Equal
        });
    }
}

fn extract_grouping_sets(exprs: &[Expr]) -> Option<Vec<Vec<Expr>>> {
    for expr in exprs {
        match expr {
            Expr::GroupingSets(sets) => {
                return Some(sets.iter().map(|s| s.clone()).collect());
            }
            Expr::Rollup(cols) => {
                let mut sets = Vec::new();
                for i in 0..=cols.len() {
                    let mut subset = Vec::new();
                    for group in cols.iter().take(cols.len() - i) {
                        subset.extend(group.iter().cloned());
                    }
                    sets.push(subset);
                }
                return Some(sets);
            }
            Expr::Cube(cols) => {
                let n = cols.len();
                let mut sets = Vec::new();
                for mask in 0..(1usize << n) {
                    let mut subset = Vec::new();
                    for (i, group) in cols.iter().enumerate() {
                        if (mask & (1usize << i)) != 0 {
                            subset.extend(group.iter().cloned());
                        }
                    }
                    sets.push(subset);
                }
                return Some(sets);
            }
            _ => {}
        }
    }
    None
}

fn expr_matches(pattern: &Expr, target: &Expr) -> bool {
    match (pattern, target) {
        (Expr::Identifier(a), Expr::Identifier(b)) => a.value.eq_ignore_ascii_case(&b.value),
        (Expr::CompoundIdentifier(a), Expr::CompoundIdentifier(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(x, y)| x.value.eq_ignore_ascii_case(&y.value))
        }
        (Expr::Identifier(a), Expr::CompoundIdentifier(b)) => b
            .last()
            .map(|i| i.value.eq_ignore_ascii_case(&a.value))
            .unwrap_or(false),
        (Expr::CompoundIdentifier(a), Expr::Identifier(b)) => a
            .last()
            .map(|i| i.value.eq_ignore_ascii_case(&b.value))
            .unwrap_or(false),
        _ => format!("{}", pattern) == format!("{}", target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(name: &str) -> Expr {
        Expr::Identifier(sqlparser::ast::Ident::new(name))
    }

    #[test]
    fn cube_treats_grouped_items_as_units() {
        let a = ident("a");
        let b = ident("b");
        let c = ident("c");
        let cube = Expr::Cube(vec![vec![a.clone(), b.clone()], vec![c.clone()]]);

        let sets = extract_grouping_sets(&[cube]).unwrap();
        assert_eq!(sets.len(), 4);

        for set in &sets {
            let has_a = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "a"));
            let has_b = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "b"));
            assert_eq!(has_a, has_b, "unexpected set: {:?}", set);
        }

        assert!(sets.iter().any(|s| s.is_empty()));
        assert!(sets.iter().any(|s| s.len() == 1 && matches!(&s[0], Expr::Identifier(id) if id.value == "c")));
        assert!(sets.iter().any(|s| s.len() == 2));
        assert!(sets.iter().any(|s| s.len() == 3));
    }

    #[test]
    fn rollup_treats_grouped_items_as_units() {
        let a = ident("a");
        let b = ident("b");
        let c = ident("c");
        let rollup = Expr::Rollup(vec![vec![a.clone(), b.clone()], vec![c.clone()]]);

        let sets = extract_grouping_sets(&[rollup]).unwrap();
        assert_eq!(sets.len(), 3);

        for set in &sets {
            let has_a = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "a"));
            let has_b = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "b"));
            assert_eq!(has_a, has_b, "unexpected set: {:?}", set);
        }

        assert!(sets.iter().any(|s| s.is_empty()));
        assert!(sets.iter().any(|s| s.len() == 2));
        assert!(sets.iter().any(|s| s.len() == 3));
    }
}
