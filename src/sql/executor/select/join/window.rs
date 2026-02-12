use super::super::*;
use super::using_merge::{rewrite_for_using_join, UsingMergeColumn};

impl Executor {
    pub(super) async fn execute_join_window_path(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        running_op: BoxedOperator,
        query: &Query,
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

        let (mut window_funcs, window_sig_to_column) =
            Executor::extract_window_function_exprs(&rewritten_projection, &join_schema)?;

        for wf in &mut window_funcs {
            wf.partition_by = wf
                .partition_by
                .iter()
                .map(|e| rewrite_for_using_join(e, table_aliases, merge_columns))
                .collect::<Result<Vec<_>>>()?;
            wf.order_by = wf
                .order_by
                .iter()
                .map(|o| {
                    Ok(sqlparser::ast::OrderByExpr {
                        expr: rewrite_for_using_join(&o.expr, table_aliases, merge_columns)?,
                        asc: o.asc,
                        nulls_first: o.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if let Some(ref arg) = wf.arg_expr {
                wf.arg_expr = Some(rewrite_for_using_join(arg, table_aliases, merge_columns)?);
            }
            if let Some(ref offset) = wf.offset_expr {
                wf.offset_expr = Some(rewrite_for_using_join(
                    offset,
                    table_aliases,
                    merge_columns,
                )?);
            }
            if let Some(ref default_val) = wf.default_value_expr {
                wf.default_value_expr = Some(rewrite_for_using_join(
                    default_val,
                    table_aliases,
                    merge_columns,
                )?);
            }
        }

        let window_operator = Box::new(WindowOperator::new(running_op, window_funcs.clone()));

        let mut rewritten_order_by: Vec<sqlparser::ast::OrderByExpr> = Vec::new();
        let mut projection_exprs: Vec<Expr> = Vec::new();
        let mut alias_exprs: HashMap<String, Expr> = HashMap::new();
        for item in &rewritten_projection {
            match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    for col in &join_schema.columns {
                        projection_exprs.push(Expr::Identifier(Ident::new(col.name.clone())));
                    }
                }
                SelectItem::UnnamedExpr(expr) => {
                    projection_exprs.push(expr.clone());
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    alias_exprs.insert(alias.value.to_lowercase(), expr.clone());
                    projection_exprs.push(expr.clone());
                }
            }
        }

        // Register implicit window aliases (window_0, window_1, ...) for ORDER BY resolution
        for (_sig, internal_name) in &window_sig_to_column {
            if let Some(public_alias) = internal_name.strip_prefix("__") {
                alias_exprs
                    .entry(public_alias.to_string())
                    .or_insert_with(|| Expr::Identifier(Ident::new(internal_name.clone())));
            }
        }

        for o in &query.order_by {
            let expr = if let Expr::Identifier(ident) = &o.expr {
                if let Some(e) = alias_exprs.get(&ident.value.to_lowercase()) {
                    e.clone()
                } else {
                    rewrite_for_using_join(&o.expr, table_aliases, merge_columns)?
                }
            } else if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = &o.expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos == 0 || pos > projection_exprs.len() {
                        return Err(anyhow!("ORDER BY position {} is not in select list", pos));
                    }
                    projection_exprs[pos - 1].clone()
                } else {
                    rewrite_for_using_join(&o.expr, table_aliases, merge_columns)?
                }
            } else {
                rewrite_for_using_join(&o.expr, table_aliases, merge_columns)?
            };
            rewritten_order_by.push(sqlparser::ast::OrderByExpr {
                expr,
                asc: o.asc,
                nulls_first: o.nulls_first,
            });
        }

        let mut operator: BoxedOperator = if !rewritten_order_by.is_empty() {
            let sort_op = Box::new(SortOperator::new(window_operator, rewritten_order_by));
            let limit = extract_limit(query);
            let offset = extract_offset(query);
            if limit.is_some() || offset > 0 {
                Box::new(LimitOperator::new(sort_op, limit, offset))
            } else {
                sort_op
            }
        } else {
            let limit = extract_limit(query);
            let offset = extract_offset(query);
            if limit.is_some() || offset > 0 {
                Box::new(LimitOperator::new(window_operator, limit, offset))
            } else {
                window_operator
            }
        };

        let raw_rows = execute_operator_tree(
            &mut operator,
            txn,
            self.store(),
            db_id,
            search_path,
            sequence_values,
        )
        .await?;

        let (columns, column_types, projected_rows) = Executor::project_window_results(
            &rewritten_projection,
            &join_schema,
            &window_funcs,
            &window_sig_to_column,
            raw_rows,
        )?;

        Ok(Some(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        }))
    }
}
