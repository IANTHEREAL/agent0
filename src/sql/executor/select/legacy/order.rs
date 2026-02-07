use super::super::*;

impl Executor {
    pub(in crate::sql::executor::select) async fn apply_order_by(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        filtered_rows: Vec<Row>,
        window_results: Option<Vec<Vec<Value>>>,
        order_by: &[sqlparser::ast::OrderByExpr],
        resolved_projection: &[SelectItem],
        schema: &TableSchema,
    ) -> Result<(Vec<Row>, Option<Vec<Vec<Value>>>)> {
        let resolved_order_exprs =
            resolve_order_by_exprs_for_non_agg(order_by, resolved_projection, schema)?;

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
                            db_id,
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
                    let asc = order_expr.asc.unwrap_or(true);
                    let nulls_first = order_expr.nulls_first.unwrap_or(!asc);
                    let ord = crate::sql::expr::compare_order_by_values(
                        &val_a,
                        &val_b,
                        asc,
                        nulls_first,
                    );
                    if !matches!(ord, std::cmp::Ordering::Equal) {
                        return ord;
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
                    let asc = order_expr.asc.unwrap_or(true);
                    let nulls_first = order_expr.nulls_first.unwrap_or(!asc);
                    let ord = crate::sql::expr::compare_order_by_values(
                        &val_a,
                        &val_b,
                        asc,
                        nulls_first,
                    );
                    if !matches!(ord, std::cmp::Ordering::Equal) {
                        return ord;
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
    pub(in crate::sql::executor::select) fn sort_by_correlated_subquery(
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
                    let cmp = crate::sql::expr::compare_values(&val_a, &val_b).unwrap_or(0);
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
