//! Query execution helpers

use super::*;

impl Executor {
    pub(crate) async fn eval_expr_maybe_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        expr: &Expr,
        row: Option<&Row>,
        schema: Option<&TableSchema>,
    ) -> Result<Value> {
        if sequences::expr_needs_async_eval(expr) {
            sequences::eval_expr_with_sequences(
                &self.store,
                txn,
                db_id,
                sequence_values,
                search_path,
                expr,
                row,
                schema,
            )
            .await
        } else {
            crate::sql::expr::eval_expr(expr, row, schema)
        }
    }

    pub(crate) async fn eval_expr_join_maybe_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        expr: &Expr,
        join_ctx: &crate::sql::expr::JoinEvalContext<'_>,
    ) -> Result<Value> {
        if sequences::expr_needs_async_eval(expr) {
            sequences::eval_expr_join_with_sequences(
                &self.store,
                txn,
                db_id,
                sequence_values,
                search_path,
                expr,
                join_ctx,
            )
            .await
        } else {
            crate::sql::expr::eval_join_expr(join_ctx, expr)
        }
    }

    /// Evaluate a projection expression that may contain correlated subqueries or UDFs.
    /// Falls back to sync eval_expr for simple expressions.
    pub(crate) async fn eval_projection_expr(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        expr: &Expr,
        row: &Row,
        schema: &TableSchema,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<Value> {
        use super::super::subquery::{expr_contains_subquery, substitute_outer_values};

        if expr_contains_subquery(expr) {
            let outer_alias = schema
                .from_alias
                .as_deref()
                .unwrap_or(schema.name.rsplit('.').next().unwrap_or(&schema.name));
            let substituted = substitute_outer_values(expr, outer_alias, schema, row);
            let resolved = self
                .resolve_subqueries(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &substituted,
                    ctes,
                    &[],
                )
                .await?;
            self.eval_expr_maybe_sequence(
                txn,
                db_id,
                sequence_values,
                search_path,
                &resolved,
                Some(row),
                Some(schema),
            )
            .await
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
            .await
        }
    }

    pub(crate) async fn execute_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
    ) -> Result<ExecuteResult> {
        let ctes = self
            .build_cte_context(txn, db_id, sequence_values, search_path, query)
            .await?;
        self.execute_query_with_ctes(txn, db_id, sequence_values, search_path, query, &ctes)
            .await
    }

    pub(crate) async fn execute_tableless_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        select: &sqlparser::ast::Select,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
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

        async fn try_pg_sleep(
            store: &Arc<TikvStore>,
            txn: &mut Transaction,
            db_id: u64,
            sequence_values: &mut HashMap<String, i64>,
            search_path: &[String],
            expr: &Expr,
        ) -> Result<Option<Value>> {
            let Expr::Function(f) = expr else {
                return Ok(None);
            };
            let Some(name) = f.name.0.last() else {
                return Ok(None);
            };
            if !name.value.eq_ignore_ascii_case("pg_sleep") {
                return Ok(None);
            }

            let arg_expr = f.args.first().and_then(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                _ => None,
            });
            let seconds_val = if let Some(arg_expr) = arg_expr {
                sequences::eval_expr_with_sequences(
                    store,
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    arg_expr,
                    None,
                    None,
                )
                .await?
            } else {
                Value::Float64(0.0)
            };

            let seconds = match seconds_val {
                Value::Int32(n) => n as f64,
                Value::Int64(n) => n as f64,
                Value::Float64(f) => f,
                Value::Numeric(d) => d.to_f64().unwrap_or(0.0),
                Value::Text(s) => s.parse::<f64>().map_err(|_| {
                    anyhow::anyhow!(
                        "{}",
                        crate::sql::error::SqlError::InvalidInputSyntax {
                            type_name: "double precision".into(),
                            value: s.clone(),
                        }
                    )
                })?,
                _ => 0.0,
            }
            .max(0.0);

            if seconds > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            }

            // Match PostgreSQL's void-like output: an empty field.
            Ok(Some(Value::Text(String::new())))
        }

        let resolved_projection = self
            .resolve_projection_subqueries(
                txn,
                db_id,
                sequence_values,
                search_path,
                &select.projection,
                ctes,
            )
            .await?;

        let mut cols = Vec::new();
        let mut values = Vec::new();
        let mut srf_positions: Vec<(usize, Vec<Value>)> = Vec::new();

        for item in &resolved_projection {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    cols.push(get_expr_name(expr));
                    if let Some(val) =
                        try_pg_sleep(&self.store, txn, db_id, sequence_values, search_path, expr)
                            .await?
                    {
                        values.push(val);
                        continue;
                    }
                    if let Some(kind) = srf_kind(expr) {
                        let Expr::Function(f) = expr else {
                            values.push(Value::Null);
                            continue;
                        };
                        let output_values = match kind {
                            SrfKind::Unnest => {
                                let arg_expr = f.args.first().and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                if let Some(arg_expr) = arg_expr {
                                    match sequences::eval_expr_with_sequences(
                                        &self.store,
                                        txn,
                                        db_id,
                                        sequence_values,
                                        search_path,
                                        arg_expr,
                                        None,
                                        None,
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
                                let source_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg0,
                                    None,
                                    None,
                                )
                                .await?;
                                let source = match source_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                let pattern_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg1,
                                    None,
                                    None,
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
                                            match sequences::eval_expr_with_sequences(
                                                &self.store,
                                                txn,
                                                db_id,
                                                sequence_values,
                                                search_path,
                                                arg2,
                                                None,
                                                None,
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
                                let source_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg0,
                                    None,
                                    None,
                                )
                                .await?;
                                let source = match source_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                let pattern_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg1,
                                    None,
                                    None,
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
                                            match sequences::eval_expr_with_sequences(
                                                &self.store,
                                                txn,
                                                db_id,
                                                sequence_values,
                                                search_path,
                                                arg2,
                                                None,
                                                None,
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
                                        let re = regex::Regex::new(&regex_pattern)
                                            .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;
                                        let mut out = Vec::new();
                                        if global {
                                            for caps in re.captures_iter(&source) {
                                                out.push(Value::Array(regexp_captures_to_values(
                                                    &caps,
                                                )));
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
                                match sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    expr,
                                    None,
                                    None,
                                )
                                .await?
                                {
                                    Value::Array(arr) => arr,
                                    Value::Null => Vec::new(),
                                    other => vec![other],
                                }
                            }
                        };

                        srf_positions.push((values.len(), output_values));
                        values.push(Value::Null);
                    } else {
                        let val = sequences::eval_expr_with_sequences(
                            &self.store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            None,
                            None,
                        )
                        .await?;
                        values.push(val);
                    }
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    cols.push(alias.value.clone());
                    if let Some(val) =
                        try_pg_sleep(&self.store, txn, db_id, sequence_values, search_path, expr)
                            .await?
                    {
                        values.push(val);
                        continue;
                    }
                    if let Some(kind) = srf_kind(expr) {
                        let Expr::Function(f) = expr else {
                            values.push(Value::Null);
                            continue;
                        };
                        let output_values = match kind {
                            SrfKind::Unnest => {
                                let arg_expr = f.args.first().and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                if let Some(arg_expr) = arg_expr {
                                    match sequences::eval_expr_with_sequences(
                                        &self.store,
                                        txn,
                                        db_id,
                                        sequence_values,
                                        search_path,
                                        arg_expr,
                                        None,
                                        None,
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
                                let source_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg0,
                                    None,
                                    None,
                                )
                                .await?;
                                let source = match source_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                let pattern_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg1,
                                    None,
                                    None,
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
                                            match sequences::eval_expr_with_sequences(
                                                &self.store,
                                                txn,
                                                db_id,
                                                sequence_values,
                                                search_path,
                                                arg2,
                                                None,
                                                None,
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
                                let source_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg0,
                                    None,
                                    None,
                                )
                                .await?;
                                let source = match source_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                let pattern_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg1,
                                    None,
                                    None,
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
                                            match sequences::eval_expr_with_sequences(
                                                &self.store,
                                                txn,
                                                db_id,
                                                sequence_values,
                                                search_path,
                                                arg2,
                                                None,
                                                None,
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
                                        let re = regex::Regex::new(&regex_pattern)
                                            .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;
                                        let mut out = Vec::new();
                                        if global {
                                            for caps in re.captures_iter(&source) {
                                                out.push(Value::Array(regexp_captures_to_values(
                                                    &caps,
                                                )));
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
                                match sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    expr,
                                    None,
                                    None,
                                )
                                .await?
                                {
                                    Value::Array(arr) => arr,
                                    Value::Null => Vec::new(),
                                    other => vec![other],
                                }
                            }
                        };

                        srf_positions.push((values.len(), output_values));
                        values.push(Value::Null);
                    } else {
                        let val = sequences::eval_expr_with_sequences(
                            &self.store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            None,
                            None,
                        )
                        .await?;
                        values.push(val);
                    }
                }
                _ => {
                    return Err(SqlError::Unsupported(
                        "Unsupported select item in tableless query".into(),
                    )
                    .into())
                }
            }
        }

        let rows = if !srf_positions.is_empty() {
            let max_len = srf_positions
                .iter()
                .map(|(_, arr)| arr.len())
                .max()
                .unwrap_or(0);
            let mut result_rows = Vec::new();
            for i in 0..max_len {
                let mut row_values = values.clone();
                for (col_idx, arr) in &srf_positions {
                    row_values[*col_idx] = arr.get(i).cloned().unwrap_or(Value::Null);
                }
                result_rows.push(Row::new(row_values));
            }
            result_rows
        } else {
            vec![Row::new(values)]
        };

        let empty_schema = TableSchema::default();
        let mut column_types: Vec<DataType> =
            crate::types::infer_column_types_from_rows(&rows, cols.len());

        // Refine timestamp-typed values that are actually `timestamptz` per SQL semantics.
        for (idx, item) in select.projection.iter().enumerate() {
            if !matches!(column_types.get(idx), Some(DataType::Timestamp)) {
                continue;
            }
            let expr = match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
                _ => continue,
            };
            if matches!(
                infer_expr_type(expr, &empty_schema),
                Ok(DataType::TimestampTz)
            ) {
                column_types[idx] = DataType::TimestampTz;
            }
        }

        Ok(ExecuteResult::Select {
            column_types: Some(column_types),
            columns: cols,
            rows,
            timezone: session_context::current_timezone(),
        })
    }

    pub(crate) fn execute_set_operation<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        op: &'a SetOperator,
        quantifier: &'a SetQuantifier,
        left: &'a SetExpr,
        right: &'a SetExpr,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            let left_result = self
                .execute_set_expr(txn, db_id, sequence_values, search_path, left, ctes)
                .await?;
            let right_result = self
                .execute_set_expr(txn, db_id, sequence_values, search_path, right, ctes)
                .await?;

            let (left_cols, left_rows) = match left_result {
                ExecuteResult::Select {
                    columns,
                    column_types: _,
                    rows,
                    timezone: _,
                } => (columns, rows),
                _ => return Err(anyhow!("Left side of set operation must be SELECT")),
            };
            let (right_cols, right_rows) = match right_result {
                ExecuteResult::Select {
                    columns,
                    column_types: _,
                    rows,
                    timezone: _,
                } => (columns, rows),
                _ => return Err(anyhow!("Right side of set operation must be SELECT")),
            };

            if left_cols.len() != right_cols.len() {
                return Err(anyhow!("Column count mismatch in set operation"));
            }

            let op_type = match (op, query::is_set_quantifier_all(quantifier)) {
                (SetOperator::Union, true) => crate::sql::operators::SetOperationType::UnionAll,
                (SetOperator::Union, false) => crate::sql::operators::SetOperationType::Union,
                (SetOperator::Intersect, true) => {
                    crate::sql::operators::SetOperationType::IntersectAll
                }
                (SetOperator::Intersect, false) => {
                    crate::sql::operators::SetOperationType::Intersect
                }
                (SetOperator::Except, true) => crate::sql::operators::SetOperationType::ExceptAll,
                (SetOperator::Except, false) => crate::sql::operators::SetOperationType::Except,
            };

            let left_schema = TableSchema {
                name: "set_op_left".to_string(),
                table_id: 0,
                columns: left_cols
                    .iter()
                    .map(|name| crate::types::ColumnDef {
                        name: name.clone(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    })
                    .collect(),
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
                from_alias: None,
            };
            let right_schema = left_schema.clone();

            let left_op = Box::new(crate::sql::operators::TableScanOperator::new_with_rows(
                left_schema,
                left_rows,
            ));
            let right_op = Box::new(crate::sql::operators::TableScanOperator::new_with_rows(
                right_schema,
                right_rows,
            ));

            let mut set_op: crate::sql::operators::BoxedOperator = Box::new(
                crate::sql::operators::SetOperationOperator::new(left_op, right_op, op_type),
            );

            let rows = crate::sql::operators::execute_operator_tree(
                &mut set_op,
                txn,
                self.store(),
                db_id,
                search_path,
                sequence_values,
            )
            .await?;

            Ok(ExecuteResult::Select {
                column_types: None,
                columns: left_cols,
                rows,
                timezone: session_context::current_timezone(),
            })
        })
    }

    fn execute_set_expr<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        expr: &'a SetExpr,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            match expr {
                SetExpr::Select(s) => {
                    let query = Query {
                        with: None,
                        body: Box::new(SetExpr::Select(s.clone())),
                        order_by: vec![],
                        limit: None,
                        offset: None,
                        fetch: None,
                        locks: vec![],
                        limit_by: vec![],
                        for_clause: None,
                    };
                    self.execute_query_with_ctes(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &query,
                        ctes,
                    )
                    .await
                }
                SetExpr::SetOperation {
                    op,
                    set_quantifier,
                    left,
                    right,
                } => {
                    self.execute_set_operation(
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
                    .await
                }
                _ => Err(SqlError::Unsupported("Unsupported set expression".into()).into()),
            }
        })
    }
}
