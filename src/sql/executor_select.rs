//! SELECT query execution

use super::helpers::{
    apply_offset_limit_fetch, collect_having_agg_funcs, dedup_rows, distinct_on_rows_with_indices,
    eval_having_expr, fill_row_defaults, get_select_item_name, infer_expr_type, AggExpr,
};
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
use std::collections::HashMap;
use tikv_client::Transaction;
use tracing::debug;

fn get_full_table_name(name: &ObjectName) -> String {
    name.0
        .iter()
        .map(|i| i.value.clone())
        .collect::<Vec<_>>()
        .join(".")
}

fn get_simple_table_name(name: &ObjectName) -> String {
    name.0.last().map(|i| i.value.clone()).unwrap_or_default()
}

impl Executor {
    pub(crate) async fn execute_query_with_ctes(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
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
            return self
                .execute_set_operation(txn, sequence_values, op, set_quantifier, left, right, ctes)
                .await;
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
            let result = self.execute_tableless_query(txn, sequence_values, select).await?;
            if let Some((target_name, _temp)) = select_into_target {
                return self
                    .create_table_from_result(txn, &target_name, result)
                    .await;
            }
            return Ok(result);
        }

        let has_joins = !select.from[0].joins.is_empty() || select.from.len() > 1;

        if has_joins {
            let result = self
                .execute_join_query_with_ctes(txn, sequence_values, query, select, ctes)
                .await?;
            if let Some((target_name, _temp)) = select_into_target {
                return self
                    .create_table_from_result(txn, &target_name, result)
                    .await;
            }
            return Ok(result);
        }

        let (t, outer_alias, schema, all_rows_base, is_virtual) = match &select.from[0].relation {
            TableFactor::Table { name, alias, .. } => {
                let full_name = get_full_table_name(name);
                let simple_name = get_simple_table_name(name);
                let lookup_name =
                    if super::information_schema::get_information_schema_schema(&full_name)
                        .is_some()
                    {
                        full_name.clone()
                    } else {
                        simple_name.clone()
                    };
                let t_lower = lookup_name.to_lowercase();
                let (schema, rows) = self
                    .get_table_data(txn, sequence_values, &lookup_name, ctes)
                    .await?;
                let is_virtual = ctes.contains_key(&t_lower)
                    || super::information_schema::get_information_schema_schema(&t_lower).is_some()
                    || self.store().get_view(txn, &t_lower).await?.is_some();
                let alias_str = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| simple_name.clone());
                (simple_name, alias_str, schema, rows, is_virtual)
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let alias_name = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "subquery".to_string());
                let (schema, rows) = self
                    .execute_derived_table(txn, sequence_values, subquery, &alias_name, ctes)
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
                Some(self.resolve_subqueries(txn, sequence_values, sel).await?)
            }
        } else {
            None
        };

        let resolved_projection = self
            .resolve_projection_subqueries_with_outer_context(
                txn,
                sequence_values,
                &select.projection,
                &outer_alias,
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
                _ => {}
            }
        }

        if let Some(having_expr) = &select.having {
            collect_having_agg_funcs(having_expr, &mut agg_funcs, extra_start);
        }

        let is_agg = !group_keys_exprs.is_empty() || !agg_funcs.is_empty();

        if is_agg {
            return self
                .execute_aggregate_query(
                    txn,
                    sequence_values,
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
                .create_table_from_result(txn, &target_name, result)
                .await;
        }
        Ok(result)
    }

    async fn execute_aggregate_query(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
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

        for row in filtered_rows {
            let mut key = Vec::new();
            for expr in group_keys_exprs {
                key.push(
                    self.eval_expr_maybe_sequence(txn, sequence_values, expr, Some(&row), Some(schema))
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
                    self.eval_expr_maybe_sequence(txn, sequence_values, e, Some(&row), Some(schema))
                        .await?
                } else {
                    Value::Int32(1)
                };
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
            for (i, _item) in resolved_projection.iter().enumerate() {
                if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                    row_values.push(default_aggs[agg_pos].result());
                } else {
                    row_values.push(Value::Null);
                }
            }
            final_rows.push(Row::new(row_values));
        }

        for (key_bytes, aggs) in groups {
            let representative = &group_rows[&key_bytes];

            if let Some(having_expr) = &select.having {
                let having_expr = if sequences::expr_uses_sequence_functions(having_expr) {
                    sequences::replace_sequence_functions(
                        &self.store(),
                        txn,
                        sequence_values,
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
                    let expr = if sequences::expr_uses_sequence_functions(expr) {
                        sequences::replace_sequence_functions(
                            &self.store(),
                            txn,
                            sequence_values,
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
                .create_table_from_result(txn, &target_name, result)
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
        indexed.sort_by(|(_, a), (_, b)| {
            for order_expr in order_by {
                let col_idx = if let Expr::Identifier(ref ident) = order_expr.expr {
                    col_names
                        .iter()
                        .position(|n| n.eq_ignore_ascii_case(&ident.value))
                } else {
                    None
                };

                let (val_a, val_b) = if let Some(idx) = col_idx {
                    (a.values.get(idx).cloned(), b.values.get(idx).cloned())
                } else {
                    (None, None)
                };

                let val_a = val_a.unwrap_or(Value::Null);
                let val_b = val_b.unwrap_or(Value::Null);
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
        indexed.into_iter().map(|(_, r)| r).collect()
    }

    async fn apply_order_by(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
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
            .any(sequences::expr_uses_sequence_functions);

        if order_by_uses_sequences {
            let mut rows_with_keys: Vec<(usize, Row, Vec<Value>)> =
                Vec::with_capacity(filtered_rows.len());
            for (orig_idx, row) in filtered_rows.into_iter().enumerate() {
                let mut keys = Vec::with_capacity(order_by.len());
                for actual_expr in &resolved_order_exprs {
                    let val = if sequences::expr_uses_sequence_functions(actual_expr) {
                        self.eval_expr_maybe_sequence(
                            txn,
                            sequence_values,
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
                    let val_a = eval_expr(actual_expr, Some(a), Some(schema)).unwrap_or(Value::Null);
                    let val_b = eval_expr(actual_expr, Some(b), Some(schema)).unwrap_or(Value::Null);
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

        let mut result_rows = Vec::new();
        for (row_idx, row) in rows_for_projection.iter().enumerate() {
            let mut row_values = Vec::new();
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
                    let value = if let Expr::Subquery(subquery) = expr {
                        self.eval_correlated_subquery(
                            txn,
                            sequence_values,
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
                            expr,
                            Some(row),
                            Some(schema),
                        )
                        .await?
                    };
                    row_values.push(value);
                }
            }
            result_rows.push(Row::new(row_values));
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
