use super::super::*;

impl Executor {
    pub(in crate::sql::executor::select) async fn project_rows(
        &self,
        txn: &mut Transaction,
        db_id: u64,
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
                                                db_id,
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
                                            db_id,
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
                                            db_id,
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
                                                        db_id,
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
                                            let re =
                                                regex::Regex::new(&regex_pattern).map_err(|e| {
                                                    anyhow!("Invalid regex pattern: {}", e)
                                                })?;

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
                                            db_id,
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
                                            db_id,
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
                                                        db_id,
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
                                            let case_insensitive =
                                                flags.to_ascii_lowercase().contains('i');
                                            let regex_pattern = if case_insensitive {
                                                format!("(?i){}", pattern)
                                            } else {
                                                pattern
                                            };
                                            let re =
                                                regex::Regex::new(&regex_pattern).map_err(|e| {
                                                    anyhow!("Invalid regex pattern: {}", e)
                                                })?;

                                            let mut out = Vec::new();
                                            if global {
                                                for caps in re.captures_iter(&source) {
                                                    out.push(Value::Array(
                                                        regexp_captures_to_values(&caps),
                                                    ));
                                                }
                                            } else if let Some(caps) = re.captures(&source) {
                                                out.push(Value::Array(regexp_captures_to_values(
                                                    &caps,
                                                )));
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
                                            db_id,
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
                                db_id,
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
                                db_id,
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

}
