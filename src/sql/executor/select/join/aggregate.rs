use super::super::*;
use super::using_merge::{rewrite_for_using_join, UsingMergeColumn};
use sqlparser::ast::OrderByExpr;

impl Executor {
    pub(super) async fn execute_join_aggregate_path(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        mut running_op: BoxedOperator,
        query: &Query,
        select: &sqlparser::ast::Select,
        resolved_projection: &[SelectItem],
        table_aliases: &[(String, TableSchema)],
        merge_columns: &[UsingMergeColumn],
    ) -> Result<Option<ExecuteResult>> {
        let join_schema = running_op.schema().clone();

        let rewritten_projection: Vec<SelectItem> = resolved_projection
            .iter()
            .map(|item| match item {
                SelectItem::UnnamedExpr(expr) => {
                    let original_name = get_select_item_name(item);
                    let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                    let rewritten_name =
                        get_select_item_name(&SelectItem::UnnamedExpr(rewritten.clone()));
                    if rewritten_name != original_name {
                        Ok(SelectItem::ExprWithAlias {
                            expr: rewritten,
                            alias: Ident::new(original_name),
                        })
                    } else {
                        Ok(SelectItem::UnnamedExpr(rewritten))
                    }
                }
                SelectItem::ExprWithAlias { expr, alias } => Ok(SelectItem::ExprWithAlias {
                    expr: rewrite_for_using_join(expr, table_aliases, merge_columns)?,
                    alias: alias.clone(),
                }),
                other => Ok(other.clone()),
            })
            .collect::<Result<Vec<_>>>()?;

        let (mut group_by_exprs, mut group_by_names, mut group_by_types) = {
            let raw_exprs = match &select.group_by {
                GroupByExpr::Expressions(exprs) => exprs.clone(),
                GroupByExpr::All => Vec::new(),
            };
            let mut exprs = Vec::new();
            let mut names = Vec::new();
            let mut types = Vec::new();
            for expr in &raw_exprs {
                let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                let name = match &rewritten {
                    Expr::Identifier(id) => id.value.clone(),
                    Expr::CompoundIdentifier(parts) => parts
                        .iter()
                        .map(|p| p.value.as_str())
                        .collect::<Vec<_>>()
                        .join("."),
                    _ => format!("{}", rewritten),
                };
                let data_type = infer_expr_type(&rewritten, &join_schema);
                exprs.push(rewritten);
                names.push(name);
                types.push(data_type);
            }
            (exprs, names, types)
        };

        Executor::add_pg_get_indexdef_support_to_group_by(
            &rewritten_projection,
            &mut group_by_exprs,
            &mut group_by_names,
            &mut group_by_types,
            &join_schema,
        );

        let (mut agg_exprs, mut agg_names, mut agg_types) =
            Executor::extract_aggregate_info(&rewritten_projection, &join_schema);

        if let Some(having_expr) = &select.having {
            let rewritten_having_for_agg =
                rewrite_for_using_join(having_expr, table_aliases, merge_columns)?;
            let mut seen_sigs: std::collections::HashSet<String> = agg_exprs
                .iter()
                .map(|a| {
                    if a.func_name.eq_ignore_ascii_case("ARRAY_AGG") {
                        if let Some(arg) = a.arg.as_ref() {
                            return crate::sql::executor::operators::array_agg_signature_for_map(
                                a.distinct,
                                arg,
                                &a.order_by,
                            );
                        }
                    }
                    let distinct_prefix = if a.distinct { "DISTINCT " } else { "" };
                    let arg_str = a.arg.as_ref().map_or("*".to_string(), |e| format!("{}", e));
                    let filter_suffix = a
                        .filter
                        .as_ref()
                        .map_or(String::new(), |flt| format!(" filter(where {})", flt));
                    let mut s = format!(
                        "{}({}{}){}",
                        a.func_name, distinct_prefix, arg_str, filter_suffix
                    )
                    .to_lowercase();
                    if let Some(ref delim) = a.delimiter {
                        s = format!(
                            "{}({}{}, '{}'){}",
                            a.func_name, distinct_prefix, arg_str, delim, filter_suffix
                        )
                        .to_lowercase();
                    }
                    s
                })
                .collect();
            let having_aggs = crate::sql::executor::operators::collect_nested_aggregates(
                &rewritten_having_for_agg,
            );
            for agg_ref in having_aggs {
                use crate::sql::executor::operators::NestedAggregateRef;
                match agg_ref {
                    NestedAggregateRef::Function(f) => {
                        Executor::add_aggregate_from_function(
                            f,
                            None,
                            &join_schema,
                            &mut agg_exprs,
                            &mut agg_names,
                            &mut agg_types,
                            &mut seen_sigs,
                        );
                    }
                    NestedAggregateRef::ArrayAgg(arr) => {
                        let sig = format!("{}", arr).to_lowercase();
                        if !seen_sigs.contains(&sig) {
                            seen_sigs.insert(sig);
                            let arg = Some((*arr.expr).clone());
                            agg_exprs.push(crate::sql::operators::AggregateExpr {
                                func_name: "ARRAY_AGG".to_string(),
                                arg: arg.clone(),
                                distinct: arr.distinct,
                                delimiter: None,
                                filter: None,
                                order_by: arr.order_by.clone().unwrap_or_default(),
                            });
                            agg_names.push("array_agg".to_string());
                            agg_types.push(DataType::Array(Box::new(
                                crate::sql::projection::infer_expr_type(
                                    arr.expr.as_ref(),
                                    &join_schema,
                                ),
                            )));
                        }
                    }
                }
            }
        }

        let group_by_count = group_by_names.len();

        let group_by_exprs_clone = group_by_exprs.clone();

        let ordered_array_agg_order_by: Option<Vec<OrderByExpr>> = {
            let mut ordered: Vec<Vec<OrderByExpr>> = Vec::new();
            for item in &rewritten_projection {
                let expr = match item {
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                    _ => continue,
                };
                if let Expr::ArrayAgg(arr) = expr {
                    if let Some(order_by) = arr.order_by.as_ref() {
                        if !order_by.is_empty() {
                            ordered.push(order_by.clone());
                        }
                    }
                }
            }

            if ordered.is_empty() {
                None
            } else {
                let canonical = |obs: &[OrderByExpr]| -> String {
                    obs.iter()
                        .map(|o| format!("{}|{:?}|{:?}", o.expr, o.asc, o.nulls_first))
                        .collect::<Vec<_>>()
                        .join(",")
                };
                let first_key = canonical(&ordered[0]);
                if ordered.iter().any(|o| canonical(o) != first_key) {
                    return Err(SqlError::Unsupported(
                        "Multiple ordered aggregates with different ORDER BY are not supported"
                            .into(),
                    )
                    .into());
                }
                Some(ordered.remove(0))
            }
        };

        if let Some(order_by) = ordered_array_agg_order_by {
            let mut sort_keys: Vec<OrderByExpr> = group_by_exprs_clone
                .iter()
                .map(|e| OrderByExpr {
                    expr: e.clone(),
                    asc: Some(true),
                    nulls_first: None,
                })
                .collect();
            sort_keys.extend(order_by);
            running_op = Box::new(SortOperator::new(running_op, sort_keys));
        }

        running_op = Box::new(HashAggregateOperator::new(
            running_op,
            group_by_exprs,
            agg_exprs.clone(),
            group_by_names.clone(),
            group_by_types.clone(),
            agg_names.clone(),
            agg_types.clone(),
        ));

        let rows = execute_operator_tree(
            &mut running_op,
            txn,
            self.store(),
            db_id,
            search_path,
            sequence_values,
        )
        .await?;

        let agg_output_schema = running_op.schema().clone();

        let rows = if let Some(having_expr) = &select.having {
            let rewritten_having =
                rewrite_for_using_join(having_expr, table_aliases, merge_columns)?;
            let mut filtered_rows = Vec::with_capacity(rows.len());
            for row in rows {
                let having_val = eval_having_expr_for_operators(
                    &rewritten_having,
                    &row,
                    &agg_output_schema,
                    &agg_exprs,
                    group_by_count,
                )?;
                let having_val = coerce_text_literal_to_bool(&rewritten_having, having_val)?;
                match having_val {
                    Value::Boolean(true) => filtered_rows.push(row),
                    Value::Boolean(false) | Value::Null => {}
                    other => {
                        return Err(anyhow!(
                            "HAVING clause must evaluate to boolean, got {:?}",
                            other
                        ));
                    }
                }
            }
            filtered_rows
        } else {
            rows
        };

        let agg_column_map: HashMap<String, String> = {
            let mut map = HashMap::new();
            for (i, agg) in agg_exprs.iter().enumerate() {
                if agg.func_name.eq_ignore_ascii_case("ARRAY_AGG") {
                    if let Some(arg) = agg.arg.as_ref() {
                        let sig = crate::sql::executor::operators::array_agg_signature_for_map(
                            agg.distinct,
                            arg,
                            &agg.order_by,
                        );
                        map.insert(sig, agg_names[i].clone());
                    }
                    continue;
                }
                let distinct_prefix = if agg.distinct { "DISTINCT " } else { "" };
                let arg_str = agg
                    .arg
                    .as_ref()
                    .map_or("*".to_string(), |e| format!("{}", e));
                let filter_suffix = agg
                    .filter
                    .as_ref()
                    .map_or(String::new(), |flt| format!(" filter(where {})", flt));
                let mut sig = format!(
                    "{}({}{}){}",
                    agg.func_name, distinct_prefix, arg_str, filter_suffix
                )
                .to_lowercase();
                if let Some(ref delim) = agg.delimiter {
                    sig = format!(
                        "{}({}{}, '{}'){}",
                        agg.func_name, distinct_prefix, arg_str, delim, filter_suffix
                    )
                    .to_lowercase();
                }
                map.insert(sig, agg_names[i].clone());
            }
            for item in &rewritten_projection {
                let expr = match item {
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                    _ => continue,
                };
                if let Expr::Function(f) = expr {
                    if is_aggregate_func(f) {
                        let sig = crate::sql::executor::operators::agg_func_signature(f);
                        let name = get_select_item_name(item);
                        if !map.contains_key(&sig) {
                            map.insert(sig, name);
                        }
                    }
                }
                if let Expr::ArrayAgg(_) = expr {
                    let sig = format!("{}", expr).to_lowercase();
                    let name = get_select_item_name(item);
                    if !map.contains_key(&sig) {
                        map.insert(sig, name);
                    }
                }
            }
            map
        };

        let group_by_expr_map: HashMap<String, String> = group_by_exprs_clone
            .iter()
            .zip(group_by_names.iter())
            .map(|(expr, name)| (format!("{}", expr).to_lowercase(), name.clone()))
            .collect();

        let mut columns: Vec<String> = Vec::new();
        let mut column_types: Vec<DataType> = Vec::new();
        let mut projection_exprs: Vec<Expr> = Vec::new();

        for item in &rewritten_projection {
            match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    for col in &agg_output_schema.columns {
                        columns.push(col.name.clone());
                        column_types.push(col.data_type.clone());
                        projection_exprs.push(Expr::Identifier(Ident::new(col.name.clone())));
                    }
                }
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    columns.push(get_select_item_name(item));
                    let expr_str = format!("{}", expr).to_lowercase();
                    if let Some(gb_col) = group_by_expr_map.get(&expr_str) {
                        let rewritten = Expr::Identifier(Ident::new(gb_col.clone()));
                        column_types.push(infer_expr_type(&rewritten, &agg_output_schema));
                        projection_exprs.push(rewritten);
                    } else {
                        let rewritten =
                            rewrite_agg_refs_to_columns(expr, &agg_column_map, &group_by_names);
                        column_types.push(infer_expr_type(&rewritten, &agg_output_schema));
                        projection_exprs.push(rewritten);
                    }
                }
            }
        }

        let mut projected_rows: Vec<Row> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut values: Vec<Value> = Vec::with_capacity(projection_exprs.len());
            for expr in &projection_exprs {
                let val = eval_expr(expr, Some(row), Some(&agg_output_schema))?;
                values.push(val);
            }
            projected_rows.push(Row::new(values));
        }

        if !query.order_by.is_empty() {
            projected_rows =
                self.apply_order_by_for_aggregate(projected_rows, &query.order_by, &columns);
        }

        let offset = extract_offset(query);
        if offset > 0 {
            projected_rows = projected_rows.into_iter().skip(offset).collect();
        }

        if let Some(limit) = extract_limit(query) {
            projected_rows.truncate(limit);
        }

        Ok(Some(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        }))
    }
}
