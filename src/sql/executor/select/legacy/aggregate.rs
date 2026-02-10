use super::super::*;

impl Executor {
    pub(in crate::sql::executor::select) async fn execute_aggregate_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
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
        // Buffers for ARRAY_AGG with ORDER BY: group_key -> (agg_idx -> Option<buffer>)
        // Each buffer entry holds (sort_keys, value) pairs to be sorted before producing result.
        let mut ordered_agg_buffers: HashMap<Vec<u8>, Vec<Option<Vec<(Vec<Value>, Value)>>>> =
            HashMap::new();

        for row in filtered_rows {
            let mut key = Vec::new();
            for expr in group_keys_exprs {
                key.push(
                    self.eval_expr_maybe_sequence(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        expr,
                        Some(&row),
                        Some(schema),
                    )
                    .await?,
                );
            }
            let key_bytes = serialize_values_for_key(&key).unwrap();

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
                                                    db_id,
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
                let ord_buffers: Vec<Option<Vec<(Vec<Value>, Value)>>> = agg_funcs
                    .iter()
                    .map(|(_, agg_expr)| {
                        if let AggExpr::ArrayAgg(arr) = agg_expr {
                            if arr.order_by.as_ref().map_or(false, |ob| !ob.is_empty()) {
                                return Some(Vec::new());
                            }
                        }
                        None
                    })
                    .collect();
                ordered_agg_buffers.insert(key_bytes.clone(), ord_buffers);
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
                                _ => {
                                    return Err(
                                        SqlError::Unsupported("Unsupported arg".into()).into()
                                    )
                                }
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
                            db_id,
                            sequence_values,
                            search_path,
                            filter,
                            Some(&row),
                            Some(schema),
                        )
                        .await?;
                    let filter_val = coerce_text_literal_to_bool(filter, filter_val)?;
                    match filter_val {
                        Value::Boolean(true) => {}
                        Value::Boolean(false) | Value::Null => continue,
                        other => {
                            return Err(anyhow!(
                                "FILTER clause must evaluate to boolean, got {:?}",
                                other
                            ));
                        }
                    }
                }

                let val = if let Some(e) = arg_expr {
                    self.eval_expr_maybe_sequence(
                        txn,
                        db_id,
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
                    let val_bytes = serialize_value_for_key(&val).unwrap_or_default();
                    let distinct_sets = seen_distinct.get_mut(&key_bytes).unwrap();
                    if !distinct_sets[agg_idx].insert(val_bytes) {
                        continue;
                    }
                }

                // Buffer ordered ARRAY_AGG values instead of updating directly
                let bufs = ordered_agg_buffers.get_mut(&key_bytes).unwrap();
                if let Some(buf) = bufs[agg_idx].as_mut() {
                    if let AggExpr::ArrayAgg(arr) = &agg_funcs[agg_idx].1 {
                        let order_by = arr.order_by.as_ref().unwrap();
                        let mut keys = Vec::with_capacity(order_by.len());
                        for o in order_by {
                            let key = self
                                .eval_expr_maybe_sequence(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &o.expr,
                                    Some(&row),
                                    Some(schema),
                                )
                                .await?;
                            keys.push(key);
                        }
                        buf.push((keys, val));
                    }
                } else {
                    aggs[agg_idx].update(&val)?;
                }
            }
        }

        // Flush ordered ARRAY_AGG buffers: sort and feed into aggregators
        for (key_bytes, aggs) in groups.iter_mut() {
            if let Some(bufs) = ordered_agg_buffers.get_mut(key_bytes) {
                for (agg_idx, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                    if let Some(mut buf) = bufs[agg_idx].take() {
                        if let AggExpr::ArrayAgg(arr) = agg_expr {
                            let order_by = arr.order_by.as_ref().unwrap();
                            buf.sort_by(|(keys_a, _), (keys_b, _)| {
                                for (key_idx, order_expr) in order_by.iter().enumerate() {
                                    let asc = order_expr.asc.unwrap_or(true);
                                    let nulls_first = order_expr.nulls_first.unwrap_or(!asc);
                                    let ord = crate::sql::expr::compare_order_by_values(
                                        &keys_a[key_idx],
                                        &keys_b[key_idx],
                                        asc,
                                        nulls_first,
                                    );
                                    if ord != std::cmp::Ordering::Equal {
                                        return ord;
                                    }
                                }
                                std::cmp::Ordering::Equal
                            });
                            for (_, val) in buf {
                                aggs[agg_idx].update(&val)?;
                            }
                        }
                    }
                }
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
                    row_values.push(eval_having_expr(
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
                        db_id,
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
                let having_val = coerce_text_literal_to_bool(&having_expr, having_val)?;
                match having_val {
                    Value::Boolean(true) => {}
                    Value::Boolean(false) | Value::Null => continue,
                    other => {
                        return Err(anyhow!(
                            "HAVING clause must evaluate to boolean, got {:?}",
                            other
                        ));
                    }
                }
            }

            let mut row_values = Vec::new();

            for (i, item) in resolved_projection.iter().enumerate() {
                if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                    row_values.push(aggs[agg_pos].result());
                } else {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => return Err(SqlError::Unsupported("Unsupported item".into()).into()),
                    };
                    let expr = if sequences::expr_needs_async_eval(expr) {
                        sequences::replace_sequence_functions(
                            &self.store(),
                            txn,
                            db_id,
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
                    row_values.push(eval_having_expr(
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
            column_types: Some(
                resolved_projection
                    .iter()
                    .map(|item| match item {
                        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                            infer_expr_type(expr, schema)
                        }
                        _ => DataType::Text,
                    })
                    .collect(),
            ),
            columns: col_names,
            rows: final_rows,
            timezone: crate::session_context::current_timezone(),
        };
        if let Some((target_name, _temp)) = select_into_target {
            return self
                .create_table_from_result(txn, db_id, search_path, &target_name, result)
                .await;
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::sql::executor::select) async fn execute_grouping_sets_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
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
            let mut ordered_agg_buffers: HashMap<Vec<u8>, Vec<Option<Vec<(Vec<Value>, Value)>>>> =
                HashMap::new();

            for row in &filtered_rows {
                let mut key = Vec::new();
                for expr in grouping_set {
                    key.push(
                        self.eval_expr_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            Some(row),
                            Some(schema),
                        )
                        .await?,
                    );
                }
                let key_bytes = serialize_values_for_key(&key).unwrap();

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
                                                        db_id,
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
                    let ord_buffers: Vec<Option<Vec<(Vec<Value>, Value)>>> = agg_funcs
                        .iter()
                        .map(|(_, agg_expr)| {
                            if let AggExpr::ArrayAgg(arr) = agg_expr {
                                if arr.order_by.as_ref().map_or(false, |ob| !ob.is_empty()) {
                                    return Some(Vec::new());
                                }
                            }
                            None
                        })
                        .collect();
                    ordered_agg_buffers.insert(key_bytes.clone(), ord_buffers);
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
                                    _ => {
                                        return Err(
                                            SqlError::Unsupported("Unsupported arg".into()).into()
                                        )
                                    }
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
                                db_id,
                                sequence_values,
                                search_path,
                                filter,
                                Some(row),
                                Some(schema),
                            )
                            .await?;
                        let filter_val = coerce_text_literal_to_bool(filter, filter_val)?;
                        match filter_val {
                            Value::Boolean(true) => {}
                            Value::Boolean(false) | Value::Null => continue,
                            other => {
                                return Err(anyhow!(
                                    "FILTER clause must evaluate to boolean, got {:?}",
                                    other
                                ));
                            }
                        }
                    }

                    let val = if let Some(e) = arg_expr {
                        self.eval_expr_maybe_sequence(
                            txn,
                            db_id,
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
                    // Buffer ordered ARRAY_AGG values instead of updating directly
                    let bufs = ordered_agg_buffers.get_mut(&key_bytes).unwrap();
                    if let Some(buf) = bufs[agg_idx].as_mut() {
                        if let AggExpr::ArrayAgg(arr) = &agg_funcs[agg_idx].1 {
                            let order_by = arr.order_by.as_ref().unwrap();
                            let mut keys = Vec::with_capacity(order_by.len());
                            for o in order_by {
                                let key = self
                                    .eval_expr_maybe_sequence(
                                        txn,
                                        db_id,
                                        sequence_values,
                                        search_path,
                                        &o.expr,
                                        Some(row),
                                        Some(schema),
                                    )
                                    .await?;
                                keys.push(key);
                            }
                            buf.push((keys, val));
                        }
                    } else {
                        aggs[agg_idx].update(&val)?;
                    }
                }
            }

            // Flush ordered ARRAY_AGG buffers: sort and feed into aggregators
            for (key_bytes, aggs) in groups.iter_mut() {
                if let Some(bufs) = ordered_agg_buffers.get_mut(key_bytes) {
                    for (agg_idx, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                        if let Some(mut buf) = bufs[agg_idx].take() {
                            if let AggExpr::ArrayAgg(arr) = agg_expr {
                                let order_by = arr.order_by.as_ref().unwrap();
                                buf.sort_by(|(keys_a, _), (keys_b, _)| {
                                    for (key_idx, order_expr) in order_by.iter().enumerate() {
                                        let asc = order_expr.asc.unwrap_or(true);
                                        let nulls_first = order_expr.nulls_first.unwrap_or(!asc);
                                        let ord = crate::sql::expr::compare_order_by_values(
                                            &keys_a[key_idx],
                                            &keys_b[key_idx],
                                            asc,
                                            nulls_first,
                                        );
                                        if ord != std::cmp::Ordering::Equal {
                                            return ord;
                                        }
                                    }
                                    std::cmp::Ordering::Equal
                                });
                                for (_, val) in buf {
                                    aggs[agg_idx].update(&val)?;
                                }
                            }
                        }
                    }
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
                            db_id,
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
                    let having_val = coerce_text_literal_to_bool(&having_expr, having_val)?;
                    match having_val {
                        Value::Boolean(true) => {}
                        Value::Boolean(false) | Value::Null => continue,
                        other => {
                            return Err(anyhow!(
                                "HAVING clause must evaluate to boolean, got {:?}",
                                other
                            ));
                        }
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
                            _ => {
                                return Err(SqlError::Unsupported("Unsupported item".into()).into())
                            }
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
                                    row_values.push(Value::Int32(if is_in_current_set {
                                        0
                                    } else {
                                        1
                                    }));
                                    continue;
                                }
                            }
                        }

                        let is_in_current_set = grouping_set
                            .iter()
                            .any(|gs_expr| expr_matches(gs_expr, expr));
                        if is_in_current_set {
                            let expr = if sequences::expr_needs_async_eval(expr) {
                                sequences::replace_sequence_functions(
                                    &self.store(),
                                    txn,
                                    db_id,
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
                            row_values.push(eval_having_expr(
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
            column_types: Some(
                resolved_projection
                    .iter()
                    .map(|item| match item {
                        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                            infer_expr_type(expr, schema)
                        }
                        _ => DataType::Text,
                    })
                    .collect(),
            ),
            columns: col_names,
            rows: final_rows,
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
