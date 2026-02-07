use super::super::*;
use super::table_factor::extract_virtual_table_filter;
use super::using_merge::{rewrite_for_using_join, UsingMergeColumn};

impl Executor {
    pub(super) async fn execute_lateral_join(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
        resolved_projection: &[SelectItem],
        resolved_selection: Option<&Expr>,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        use crate::sql::executor::subquery::substitute_outer_values_in_query;
        use crate::types::ColumnDef;

        let virtual_filter = resolved_selection
            .map(extract_virtual_table_filter)
            .unwrap_or_default();

        let from_item = &select.from[0];
        let resolved_outer = self
            .resolve_join_table_factor(
                txn,
                db_id,
                sequence_values,
                search_path,
                &from_item.relation,
                ctes,
                &virtual_filter,
            )
            .await?;
        let (outer_alias, outer_schema, outer_preloaded) = match resolved_outer {
            Some(t) => t,
            None => return Err(anyhow!("LATERAL JOIN: could not resolve outer table")),
        };

        let outer_rows = if let Some(rows) = outer_preloaded {
            rows
        } else {
            let (_, rows) = self
                .get_table_data(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &outer_schema.name,
                    ctes,
                )
                .await?;
            rows
        };

        let mut combined_rows: Vec<Row>;
        let mut table_aliases: Vec<(String, TableSchema)> =
            vec![(outer_alias.clone(), outer_schema.clone())];
        let mut combined_columns: Vec<ColumnDef> = Vec::new();
        for col in &outer_schema.columns {
            combined_columns.push(ColumnDef {
                name: format!("{}.{}", outer_alias, col.name),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        let mut current_rows: Vec<Row> = outer_rows.clone();

        for join in &from_item.joins {
            let jt = JoinType::from(&join.join_operator);

            if let TableFactor::Derived {
                lateral: true,
                subquery,
                alias,
                ..
            } = &join.relation
            {
                let lateral_alias_name = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "lateral".to_string());
                let alias_cols = alias
                    .as_ref()
                    .map(|a| a.columns.clone())
                    .unwrap_or_default();

                let mut lateral_schema: Option<TableSchema> = None;
                let mut new_rows: Vec<Row> = Vec::new();

                for left_row in &current_rows {
                    let outer_row =
                        Row::new(left_row.values[..outer_schema.columns.len()].to_vec());
                    let substituted = substitute_outer_values_in_query(
                        subquery,
                        &outer_alias,
                        &outer_schema,
                        &outer_row,
                    );

                    let result = self
                        .execute_query_with_outer_ctes(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &substituted,
                            ctes,
                        )
                        .await?;

                    let (cols, _col_types, sub_rows) = match result {
                        ExecuteResult::Select {
                            columns,
                            rows,
                            column_types,
                            ..
                        } => (columns, column_types, rows),
                        _ => continue,
                    };

                    if lateral_schema.is_none() {
                        let col_names: Vec<String> = if alias_cols.is_empty() {
                            cols
                        } else {
                            alias_cols.iter().map(|c| c.value.clone()).collect()
                        };
                        let inferred_types: Vec<DataType> = if let Some(first) = sub_rows.first() {
                            first
                                .values
                                .iter()
                                .map(|v| v.data_type().unwrap_or(DataType::Text))
                                .collect()
                        } else {
                            vec![DataType::Text; col_names.len()]
                        };
                        lateral_schema = Some(TableSchema {
                            table_id: 0,
                            name: lateral_alias_name.clone(),
                            columns: col_names
                                .iter()
                                .zip(inferred_types.iter())
                                .map(|(n, dt)| ColumnDef {
                                    name: n.clone(),
                                    data_type: dt.clone(),
                                    nullable: true,
                                    primary_key: false,
                                    unique: false,
                                    is_serial: false,
                                    default_expr: None,
                                })
                                .collect(),
                            pk_constraint_name: None,
                            pk_indices: vec![],
                            indexes: vec![],
                            version: 1,
                            check_constraints: vec![],
                            foreign_keys: vec![],
                            owner: String::new(),
                        });
                    }

                    if sub_rows.is_empty() {
                        if matches!(jt, JoinType::Left) {
                            let lat_cols = lateral_schema
                                .as_ref()
                                .map(|s| s.columns.len())
                                .unwrap_or(0);
                            let mut values = left_row.values.clone();
                            values.extend(std::iter::repeat(Value::Null).take(lat_cols));
                            new_rows.push(Row::new(values));
                        }
                    } else {
                        for sub_row in &sub_rows {
                            let mut values = left_row.values.clone();
                            values.extend(sub_row.values.iter().cloned());
                            new_rows.push(Row::new(values));
                        }
                    }
                }

                let lat_schema = lateral_schema.unwrap_or_else(|| TableSchema {
                    table_id: 0,
                    name: lateral_alias_name.clone(),
                    columns: vec![],
                    pk_constraint_name: None,
                    pk_indices: vec![],
                    indexes: vec![],
                    version: 1,
                    check_constraints: vec![],
                    foreign_keys: vec![],
                    owner: String::new(),
                });

                for col in &lat_schema.columns {
                    combined_columns.push(ColumnDef {
                        name: format!("{}.{}", lat_schema.name, col.name),
                        data_type: col.data_type.clone(),
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    });
                }
                table_aliases.push((lat_schema.name.clone(), lat_schema));
                current_rows = new_rows;
            } else {
                let resolved = self
                    .resolve_join_table_factor(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &join.relation,
                        ctes,
                        &virtual_filter,
                    )
                    .await?;
                let (right_alias, right_schema, right_preloaded) = match resolved {
                    Some(t) => t,
                    None => {
                        return Err(anyhow!(
                            "LATERAL JOIN: could not resolve table in join chain"
                        ))
                    }
                };

                let right_rows = if let Some(rows) = right_preloaded {
                    rows
                } else {
                    let (_, rows) = self
                        .get_table_data(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &right_schema.name,
                            ctes,
                        )
                        .await?;
                    rows
                };

                let condition = match &join.join_operator {
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::On(expr)) => {
                        Some(expr.clone())
                    }
                    sqlparser::ast::JoinOperator::CrossJoin => None,
                    _ => None,
                };

                for col in &right_schema.columns {
                    combined_columns.push(ColumnDef {
                        name: format!("{}.{}", right_alias, col.name),
                        data_type: col.data_type.clone(),
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    });
                }

                let temp_combined_schema = TableSchema {
                    name: "join_result".to_string(),
                    table_id: 0,
                    columns: combined_columns.clone(),
                    version: 1,
                    pk_constraint_name: None,
                    pk_indices: vec![],
                    indexes: vec![],
                    check_constraints: vec![],
                    foreign_keys: vec![],
                    owner: String::new(),
                };

                let mut temp_table_aliases = table_aliases.clone();
                temp_table_aliases.push((right_alias.clone(), right_schema.clone()));

                let rewritten_condition = condition.as_ref().map(|cond| {
                    rewrite_expr_for_multi_join(cond, &temp_table_aliases)
                        .unwrap_or_else(|_| cond.clone())
                });

                let mut new_rows: Vec<Row> = Vec::new();
                for left_row in &current_rows {
                    let mut matched = false;
                    for right_row in &right_rows {
                        let mut combined = left_row.values.clone();
                        combined.extend(right_row.values.iter().cloned());
                        let combined_row = Row::new(combined);

                        let passes = match &rewritten_condition {
                            Some(cond) => matches!(
                                eval_expr(cond, Some(&combined_row), Some(&temp_combined_schema)),
                                Ok(Value::Boolean(true))
                            ),
                            None => true,
                        };

                        if passes {
                            matched = true;
                            new_rows.push(combined_row);
                        }
                    }
                    if !matched && matches!(jt, JoinType::Left) {
                        let mut values = left_row.values.clone();
                        values.extend(
                            std::iter::repeat(Value::Null).take(right_schema.columns.len()),
                        );
                        new_rows.push(Row::new(values));
                    }
                }

                table_aliases.push((right_alias, right_schema));
                current_rows = new_rows;
            }
        }

        combined_rows = current_rows;

        let combined_schema = TableSchema {
            name: "join_result".to_string(),
            table_id: 0,
            columns: combined_columns,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let merge_columns: Vec<UsingMergeColumn> = Vec::new();

        if let Some(filter) = resolved_selection {
            let rewritten = rewrite_for_using_join(filter, &table_aliases, &merge_columns)?;
            combined_rows.retain(|row| {
                matches!(
                    eval_expr(&rewritten, Some(row), Some(&combined_schema)),
                    Ok(Value::Boolean(true))
                )
            });
        }

        let rewritten_projection: Vec<SelectItem> = resolved_projection
            .iter()
            .map(|item| match item {
                SelectItem::UnnamedExpr(e) => {
                    let rewritten = rewrite_for_using_join(e, &table_aliases, &merge_columns)
                        .unwrap_or_else(|_| e.clone());
                    SelectItem::UnnamedExpr(rewritten)
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let rewritten = rewrite_for_using_join(expr, &table_aliases, &merge_columns)
                        .unwrap_or_else(|_| expr.clone());
                    SelectItem::ExprWithAlias {
                        expr: rewritten,
                        alias: alias.clone(),
                    }
                }
                SelectItem::Wildcard(_) => {
                    SelectItem::Wildcard(sqlparser::ast::WildcardAdditionalOptions::default())
                }
                other => other.clone(),
            })
            .collect();

        if !query.order_by.is_empty() {
            let order_exprs: Vec<_> = query
                .order_by
                .iter()
                .map(|o| {
                    let rewritten = rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)
                        .unwrap_or_else(|_| o.expr.clone());
                    (rewritten, o.asc.unwrap_or(true))
                })
                .collect();

            combined_rows.sort_by(|a, b| {
                for (expr, asc) in &order_exprs {
                    let va =
                        eval_expr(expr, Some(a), Some(&combined_schema)).unwrap_or(Value::Null);
                    let vb =
                        eval_expr(expr, Some(b), Some(&combined_schema)).unwrap_or(Value::Null);
                    let cmp_val = crate::sql::expr::compare_values(&va, &vb).unwrap_or(0);
                    let cmp = match cmp_val {
                        n if n < 0 => std::cmp::Ordering::Less,
                        0 => std::cmp::Ordering::Equal,
                        _ => std::cmp::Ordering::Greater,
                    };
                    let cmp = if *asc { cmp } else { cmp.reverse() };
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }

        let offset = extract_offset(query);
        let limit = extract_limit(query);
        if offset > 0 {
            combined_rows = combined_rows.into_iter().skip(offset).collect();
        }
        if let Some(lim) = limit {
            combined_rows.truncate(lim);
        }

        let is_wildcard = rewritten_projection
            .iter()
            .any(|item| matches!(item, SelectItem::Wildcard(_)));

        let (columns, column_types, final_rows) = if is_wildcard {
            let cols: Vec<String> = combined_schema
                .columns
                .iter()
                .map(|c| c.name.split('.').last().unwrap_or(&c.name).to_string())
                .collect();
            let types: Vec<DataType> = combined_schema
                .columns
                .iter()
                .map(|c| c.data_type.clone())
                .collect();
            (cols, types, combined_rows)
        } else {
            let cols: Vec<String> = rewritten_projection
                .iter()
                .map(|item| get_select_item_name(item))
                .collect();
            let types: Vec<DataType> = rewritten_projection
                .iter()
                .map(|item| match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        infer_expr_type(expr, &combined_schema)
                    }
                    _ => DataType::Text,
                })
                .collect();
            let mut projected = Vec::with_capacity(combined_rows.len());
            for row in &combined_rows {
                let mut values = Vec::with_capacity(rewritten_projection.len());
                for item in &rewritten_projection {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => continue,
                    };
                    let val = eval_expr(expr, Some(row), Some(&combined_schema))?;
                    values.push(val);
                }
                projected.push(Row::new(values));
            }
            (cols, types, projected)
        };

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: final_rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

}
