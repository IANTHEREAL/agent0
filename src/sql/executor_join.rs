//! JOIN query execution for the SQL executor

use super::executor::Executor;
use super::helpers::{
    collect_having_agg_funcs, dedup_rows, distinct_on_rows_join, eval_having_expr_join,
    get_select_item_name, AggExpr,
};
use super::window::{compute_window_functions_join, extract_window_functions};
use super::{
    expr::{eval_expr, eval_expr_join, JoinContext},
    parse_sql, Aggregator, ExecuteResult,
};
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    BinaryOperator, Distinct, Expr, FunctionArg, FunctionArgExpr, GroupByExpr, Ident,
    JoinConstraint, JoinOperator, Query, SelectItem, Statement, TableFactor,
};
use std::collections::HashMap;
use tikv_client::Transaction;

impl Executor {
    #[allow(dead_code)]
    pub(crate) async fn execute_join_query(
        &self,
        txn: &mut Transaction,
        query: &Query,
        select: &sqlparser::ast::Select,
    ) -> Result<ExecuteResult> {
        self.execute_join_query_with_ctes(txn, query, select, &HashMap::new())
            .await
    }

    pub(crate) async fn get_table_data(
        &self,
        txn: &mut Transaction,
        table_name: &str,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<(TableSchema, Vec<Row>)> {
        let t_lower = table_name.to_lowercase();

        // Check if this is a known scalar function (with or without parentheses)
        let t_upper = table_name.trim_end_matches("()").to_uppercase();
        if matches!(
            t_upper.as_str(),
            "CURRENT_SCHEMA" | "CURRENT_DATABASE" | "CURRENT_USER" | "SESSION_USER" | "USER"
        ) {
            let result = match t_upper.as_str() {
                "CURRENT_SCHEMA" => Value::Text("public".to_string()),
                "CURRENT_DATABASE" => Value::Text("postgres".to_string()),
                "CURRENT_USER" | "SESSION_USER" | "USER" => Value::Text("postgres".to_string()),
                _ => unreachable!(),
            };

            // Create a single-column, single-row result
            let col_name = t_upper.to_lowercase();
            let schema = TableSchema {
                table_id: 0,
                name: table_name.to_string(),
                columns: vec![ColumnDef {
                    name: col_name.clone(),
                    data_type: result.data_type().unwrap_or(DataType::Text),
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                }],
                pk_indices: vec![],
                indexes: vec![],
                version: 1,
                check_constraints: vec![],
                foreign_keys: vec![],
            };
            let rows = vec![Row::new(vec![result])];
            return Ok((schema, rows));
        }

        if let Some((schema, rows)) = ctes.get(&t_lower) {
            return Ok((schema.clone(), rows.clone()));
        }

        if super::information_schema::get_information_schema_schema(&t_lower).is_some() {
            return super::information_schema::get_information_schema_data(
                &self.store(),
                txn,
                &t_lower,
            )
            .await;
        }

        if let Some(view_query) = self.store().get_view(txn, &t_lower).await? {
            let result = self.execute_view_query(txn, &view_query, ctes).await?;
            return match result {
                ExecuteResult::Select {
                    columns,
                    column_types: _,
                    rows,
                } => {
                    let schema = TableSchema {
                        table_id: 0,
                        name: t_lower,
                        columns: columns
                            .iter()
                            .map(|n| ColumnDef {
                                name: n.clone(),
                                data_type: DataType::Text,
                                nullable: true,
                                primary_key: false,
                                unique: false,
                                is_serial: false,
                                default_expr: None,
                            })
                            .collect(),
                        pk_indices: vec![],
                        indexes: vec![],
                        version: 1,
                        check_constraints: vec![],
                        foreign_keys: vec![],
                    };
                    Ok((schema, rows))
                }
                _ => Err(anyhow!("View must return SELECT result")),
            };
        }

        // Handle function calls in FROM clause (e.g., SELECT * FROM current_schema())
        if table_name.ends_with("()") || table_name.contains("(") && table_name.contains(")") {
            // Parse as a function call
            let func_name = table_name.trim_end_matches("()").to_uppercase();
            let result = match func_name.as_str() {
                "CURRENT_SCHEMA" => Value::Text("public".to_string()),
                "CURRENT_DATABASE" => Value::Text("postgres".to_string()),
                "CURRENT_USER" | "SESSION_USER" | "USER" => Value::Text("postgres".to_string()),
                _ => return Err(anyhow!("Function '{}' not found", func_name)),
            };

            // Create a single-column, single-row result
            let col_name = func_name.to_lowercase();
            let schema = TableSchema {
                table_id: 0,
                name: table_name.to_string(),
                columns: vec![ColumnDef {
                    name: col_name.clone(),
                    data_type: result.data_type().unwrap_or(DataType::Text),
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                }],
                pk_indices: vec![],
                indexes: vec![],
                version: 1,
                check_constraints: vec![],
                foreign_keys: vec![],
            };
            let rows = vec![Row::new(vec![result])];
            return Ok((schema, rows));
        }

        let schema = self
            .store()
            .get_schema(txn, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' not found", table_name))?;
        let rows = self.scan_and_fill(txn, table_name, &schema).await?;
        Ok((schema, rows))
    }

    pub(crate) fn execute_view_query<'a>(
        &'a self,
        txn: &'a mut Transaction,
        view_query: &'a str,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            let ast = parse_sql(view_query)?;
            if let Some(Statement::Query(q)) = ast.into_iter().next() {
                self.execute_query_with_ctes(txn, &q, ctes).await
            } else {
                Err(anyhow!("Invalid view query"))
            }
        })
    }

    pub(crate) fn execute_derived_table<'a>(
        &'a self,
        txn: &'a mut Transaction,
        subquery: &'a Query,
        alias: &'a str,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(TableSchema, Vec<Row>)>> + Send + 'a>,
    > {
        Box::pin(async move {
            let result = self.execute_query_with_ctes(txn, subquery, ctes).await?;
            match result {
                ExecuteResult::Select {
                    columns,
                    column_types: _,
                    rows,
                } => {
                    let schema = TableSchema {
                        table_id: 0,
                        name: alias.to_string(),
                        columns: columns
                            .iter()
                            .map(|n| ColumnDef {
                                name: n.clone(),
                                data_type: DataType::Text,
                                nullable: true,
                                primary_key: false,
                                unique: false,
                                is_serial: false,
                                default_expr: None,
                            })
                            .collect(),
                        pk_indices: vec![],
                        indexes: vec![],
                        version: 1,
                        check_constraints: vec![],
                        foreign_keys: vec![],
                    };
                    Ok((schema, rows))
                }
                _ => Err(anyhow!("Derived table must return SELECT result")),
            }
        })
    }

    pub(crate) fn resolve_table_factor<'a>(
        &'a self,
        txn: &'a mut Transaction,
        factor: &'a TableFactor,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(String, TableSchema, Vec<Row>)>> + Send + 'a>,
    > {
        Box::pin(async move {
            match factor {
                TableFactor::Table {
                    name, alias, args, ..
                } => {
                    let tbl = name.0.last().unwrap().value.clone();
                    let als = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| tbl.clone());
                    let tbl_upper = tbl.to_uppercase();
                    let is_scalar_function = matches!(
                        tbl_upper.as_str(),
                        "CURRENT_SCHEMA"
                            | "CURRENT_DATABASE"
                            | "CURRENT_USER"
                            | "SESSION_USER"
                            | "USER"
                    ) || args.is_some();
                    let table_name = if is_scalar_function {
                        format!("{}()", tbl)
                    } else {
                        tbl
                    };
                    let (schema, rows) = self.get_table_data(txn, &table_name, ctes).await?;
                    Ok((als, schema, rows))
                }
                TableFactor::Derived {
                    subquery, alias, ..
                } => {
                    let alias_name = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| "subquery".to_string());
                    let (schema, rows) = self
                        .execute_derived_table(txn, subquery, &alias_name, ctes)
                        .await?;
                    Ok((alias_name, schema, rows))
                }
                TableFactor::NestedJoin {
                    table_with_joins,
                    alias,
                } => {
                    let (mut base_alias, mut combined_schema, mut combined_rows) = self
                        .resolve_table_factor(txn, &table_with_joins.relation, ctes)
                        .await?;

                    for join in &table_with_joins.joins {
                        let (join_alias, join_schema, join_rows) =
                            self.resolve_table_factor(txn, &join.relation, ctes).await?;

                        let join_condition = match &join.join_operator {
                            JoinOperator::Inner(JoinConstraint::On(expr)) => Some(expr.clone()),
                            JoinOperator::LeftOuter(JoinConstraint::On(expr)) => Some(expr.clone()),
                            JoinOperator::RightOuter(JoinConstraint::On(expr)) => {
                                Some(expr.clone())
                            }
                            JoinOperator::FullOuter(JoinConstraint::On(expr)) => Some(expr.clone()),
                            JoinOperator::CrossJoin => None,
                            JoinOperator::Inner(JoinConstraint::None) => None,
                            _ => None,
                        };

                        let is_left_join = matches!(
                            join.join_operator,
                            JoinOperator::LeftOuter(_) | JoinOperator::FullOuter(_)
                        );
                        let is_right_join = matches!(
                            join.join_operator,
                            JoinOperator::RightOuter(_) | JoinOperator::FullOuter(_)
                        );

                        let mut column_offsets: HashMap<String, usize> = HashMap::new();
                        let mut offset = 0;
                        for col in &combined_schema.columns {
                            column_offsets.insert(format!("{}.{}", base_alias, col.name), offset);
                            if !column_offsets.contains_key(&col.name) {
                                column_offsets.insert(col.name.clone(), offset);
                            }
                            offset += 1;
                        }
                        for col in &join_schema.columns {
                            column_offsets.insert(format!("{}.{}", join_alias, col.name), offset);
                            if !column_offsets.contains_key(&col.name) {
                                column_offsets.insert(col.name.clone(), offset);
                            }
                            offset += 1;
                        }

                        let mut temp_columns = combined_schema.columns.clone();
                        temp_columns.extend(join_schema.columns.clone());
                        let temp_combined_schema = TableSchema {
                            table_id: 0,
                            name: "nested_join".to_string(),
                            columns: temp_columns,
                            pk_indices: vec![],
                            indexes: vec![],
                            version: 1,
                            check_constraints: vec![],
                            foreign_keys: vec![],
                        };

                        let mut new_rows = Vec::new();
                        let mut right_matched_flags = vec![false; join_rows.len()];
                        let right_cols = join_schema.columns.len();

                        for left_row in &combined_rows {
                            let mut matched_any = false;
                            for (right_idx, right_row) in join_rows.iter().enumerate() {
                                let mut combined_values = left_row.values.clone();
                                combined_values.extend(right_row.values.clone());
                                let combined_row = Row::new(combined_values);

                                let matches = if let Some(cond) = &join_condition {
                                    let ctx = JoinContext {
                                        tables: HashMap::new(),
                                        column_offsets: column_offsets.clone(),
                                        combined_row: &combined_row,
                                        combined_schema: &temp_combined_schema,
                                    };
                                    matches!(eval_expr_join(cond, &ctx), Ok(Value::Boolean(true)))
                                } else {
                                    true
                                };

                                if matches {
                                    new_rows.push(combined_row);
                                    matched_any = true;
                                    right_matched_flags[right_idx] = true;
                                }
                            }

                            if !matched_any && is_left_join {
                                let mut combined_values = left_row.values.clone();
                                combined_values.extend(vec![Value::Null; right_cols]);
                                new_rows.push(Row::new(combined_values));
                            }
                        }

                        if is_right_join {
                            let left_cols = combined_schema.columns.len();
                            for (right_idx, right_row) in join_rows.iter().enumerate() {
                                if !right_matched_flags[right_idx] {
                                    let mut combined_values = vec![Value::Null; left_cols];
                                    combined_values.extend(right_row.values.clone());
                                    new_rows.push(Row::new(combined_values));
                                }
                            }
                        }

                        let mut new_columns = combined_schema.columns.clone();
                        new_columns.extend(join_schema.columns.clone());
                        combined_schema = TableSchema {
                            table_id: 0,
                            name: format!("{}_{}", base_alias, join_alias),
                            columns: new_columns,
                            pk_indices: vec![],
                            indexes: vec![],
                            version: 1,
                            check_constraints: vec![],
                            foreign_keys: vec![],
                        };
                        combined_rows = new_rows;
                        base_alias = format!("{}_{}", base_alias, join_alias);
                    }

                    let final_alias = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or(base_alias);

                    Ok((final_alias, combined_schema, combined_rows))
                }
                _ => Err(anyhow!("Unsupported table factor")),
            }
        })
    }

    pub(crate) async fn execute_join_query_with_ctes(
        &self,
        txn: &mut Transaction,
        query: &Query,
        select: &sqlparser::ast::Select,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        let (base_alias, base_schema, base_rows) = match &select.from[0].relation {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let tbl = name.0.last().unwrap().value.clone();
                let als = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| tbl.clone());

                // Check if this is a known scalar function that can be used as a table
                let tbl_upper = tbl.to_uppercase();
                let is_scalar_function = matches!(
                    tbl_upper.as_str(),
                    "CURRENT_SCHEMA"
                        | "CURRENT_DATABASE"
                        | "CURRENT_USER"
                        | "SESSION_USER"
                        | "USER"
                ) || args.is_some();

                let table_name = if is_scalar_function {
                    format!("{}()", tbl) // Add parentheses for function detection in get_table_data
                } else {
                    tbl.clone()
                };

                let (schema, rows) = self.get_table_data(txn, &table_name, ctes).await?;
                (als, schema, rows)
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let alias_name = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "subquery".to_string());
                let (schema, rows) = self
                    .execute_derived_table(txn, subquery, &alias_name, ctes)
                    .await?;
                (alias_name, schema, rows)
            }
            TableFactor::NestedJoin { .. } => {
                self.resolve_table_factor(txn, &select.from[0].relation, ctes)
                    .await?
            }
            _ => return Err(anyhow!("Unsupported base table")),
        };

        let mut combined_schemas: Vec<(String, TableSchema)> =
            vec![(base_alias.clone(), base_schema.clone())];
        let mut combined_rows: Vec<Row> = base_rows;
        let mut has_natural_join = false;
        let mut natural_join_common_cols: Vec<String> = Vec::new();

        for from_item in select.from.iter().skip(1) {
            let (extra_alias, extra_schema, extra_rows) = match &from_item.relation {
                TableFactor::Table { name, alias, .. } => {
                    let tbl = name.0.last().unwrap().value.clone();
                    let als = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| tbl.clone());
                    let (schema, rows) = self.get_table_data(txn, &tbl, ctes).await?;
                    (als, schema, rows)
                }
                TableFactor::Derived {
                    subquery, alias, ..
                } => {
                    let alias_name = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| "subquery".to_string());
                    let (schema, rows) = self
                        .execute_derived_table(txn, subquery, &alias_name, ctes)
                        .await?;
                    (alias_name, schema, rows)
                }
                TableFactor::NestedJoin { .. } => {
                    self.resolve_table_factor(txn, &from_item.relation, ctes)
                        .await?
                }
                _ => return Err(anyhow!("Unsupported table factor in FROM")),
            };

            let mut new_combined_rows = Vec::new();
            for left_row in &combined_rows {
                for right_row in &extra_rows {
                    let mut combined_values = left_row.values.clone();
                    combined_values.extend(right_row.values.clone());
                    new_combined_rows.push(Row::new(combined_values));
                }
            }
            combined_schemas.push((extra_alias, extra_schema));
            combined_rows = new_combined_rows;

            for extra_join in &from_item.joins {
                let (join_alias, join_schema, join_rows) = match &extra_join.relation {
                    TableFactor::Table { name, alias, .. } => {
                        let tbl = name.0.last().unwrap().value.clone();
                        let als = alias
                            .as_ref()
                            .map(|a| a.name.value.clone())
                            .unwrap_or_else(|| tbl.clone());
                        let (schema, rows) = self.get_table_data(txn, &tbl, ctes).await?;
                        (als, schema, rows)
                    }
                    TableFactor::Derived {
                        subquery, alias, ..
                    } => {
                        let alias_name = alias
                            .as_ref()
                            .map(|a| a.name.value.clone())
                            .unwrap_or_else(|| "subquery".to_string());
                        let (schema, rows) = self
                            .execute_derived_table(txn, subquery, &alias_name, ctes)
                            .await?;
                        (alias_name, schema, rows)
                    }
                    TableFactor::NestedJoin { .. } => {
                        self.resolve_table_factor(txn, &extra_join.relation, ctes)
                            .await?
                    }
                    _ => return Err(anyhow!("Unsupported join table factor")),
                };

                let mut new_rows = Vec::new();
                for left_row in &combined_rows {
                    for right_row in &join_rows {
                        let mut combined_values = left_row.values.clone();
                        combined_values.extend(right_row.values.clone());
                        new_rows.push(Row::new(combined_values));
                    }
                }
                combined_schemas.push((join_alias, join_schema));
                combined_rows = new_rows;
            }
        }

        for join in &select.from[0].joins {
            let (join_alias, join_schema, join_rows) = match &join.relation {
                TableFactor::Table { name, alias, .. } => {
                    let tbl = name.0.last().unwrap().value.clone();
                    let als = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| tbl.clone());
                    let (schema, rows) = self.get_table_data(txn, &tbl, ctes).await?;
                    (als, schema, rows)
                }
                TableFactor::Derived {
                    subquery, alias, ..
                } => {
                    let alias_name = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| "subquery".to_string());
                    let (schema, rows) = self
                        .execute_derived_table(txn, subquery, &alias_name, ctes)
                        .await?;
                    (alias_name, schema, rows)
                }
                TableFactor::NestedJoin { .. } => {
                    self.resolve_table_factor(txn, &join.relation, ctes).await?
                }
                _ => return Err(anyhow!("Unsupported join table")),
            };

            let left_columns: Vec<String> = combined_schemas
                .iter()
                .flat_map(|(_, s)| s.columns.iter().map(|c| c.name.clone()))
                .collect();
            let right_columns: Vec<String> =
                join_schema.columns.iter().map(|c| c.name.clone()).collect();

            let (join_condition, is_natural) = match &join.join_operator {
                JoinOperator::Inner(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                JoinOperator::LeftOuter(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                JoinOperator::RightOuter(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                JoinOperator::FullOuter(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                JoinOperator::Inner(JoinConstraint::Natural)
                | JoinOperator::LeftOuter(JoinConstraint::Natural)
                | JoinOperator::RightOuter(JoinConstraint::Natural)
                | JoinOperator::FullOuter(JoinConstraint::Natural) => {
                    let common_cols: Vec<String> = left_columns
                        .iter()
                        .filter(|c| right_columns.contains(c))
                        .cloned()
                        .collect();
                    has_natural_join = true;
                    natural_join_common_cols = common_cols.clone();
                    if common_cols.is_empty() {
                        (None, true)
                    } else {
                        let cond = common_cols
                            .iter()
                            .map(|col| Expr::BinaryOp {
                                left: Box::new(Expr::CompoundIdentifier(vec![
                                    Ident::new(base_alias.clone()),
                                    Ident::new(col.clone()),
                                ])),
                                op: BinaryOperator::Eq,
                                right: Box::new(Expr::CompoundIdentifier(vec![
                                    Ident::new(join_alias.clone()),
                                    Ident::new(col.clone()),
                                ])),
                            })
                            .reduce(|a, b| Expr::BinaryOp {
                                left: Box::new(a),
                                op: BinaryOperator::And,
                                right: Box::new(b),
                            })
                            .unwrap();
                        (Some(cond), true)
                    }
                }
                JoinOperator::CrossJoin => (None, false),
                _ => return Err(anyhow!("Unsupported JOIN type")),
            };
            let _ = is_natural;

            let has_correlated_subquery = join_condition.as_ref().map_or(false, |cond| {
                combined_schemas.iter().any(|(alias, _)| {
                    super::helpers::query_has_outer_reference_in_expr(cond, alias)
                })
            });

            let join_condition = if let Some(cond) = join_condition {
                if has_correlated_subquery {
                    Some(cond)
                } else {
                    Some(self.resolve_subqueries(txn, &cond).await?)
                }
            } else {
                None
            };

            let is_left_join = matches!(&join.join_operator, JoinOperator::LeftOuter(_));
            let is_right_join = matches!(&join.join_operator, JoinOperator::RightOuter(_));
            let is_full_join = matches!(&join.join_operator, JoinOperator::FullOuter(_));

            let left_col_count: usize = combined_schemas.iter().map(|(_, s)| s.columns.len()).sum();

            let mut column_offsets: HashMap<String, usize> = HashMap::new();
            let mut offset = 0;
            for (alias, schema) in &combined_schemas {
                for col in &schema.columns {
                    column_offsets.insert(format!("{}.{}", alias, col.name), offset);
                    if !column_offsets.contains_key(&col.name) {
                        column_offsets.insert(col.name.clone(), offset);
                    }
                    offset += 1;
                }
            }
            let _join_start_offset = offset;
            for col in &join_schema.columns {
                column_offsets.insert(format!("{}.{}", join_alias, col.name), offset);
                if !column_offsets.contains_key(&col.name) {
                    column_offsets.insert(col.name.clone(), offset);
                }
                offset += 1;
            }

            let mut combined_col_defs: Vec<ColumnDef> = Vec::new();
            for (_, schema) in &combined_schemas {
                combined_col_defs.extend(schema.columns.clone());
            }
            combined_col_defs.extend(join_schema.columns.clone());
            let temp_combined_schema = TableSchema {
                name: "joined".to_string(),
                table_id: 0,
                columns: combined_col_defs,
                version: 1,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
            };

            let mut new_combined_rows = Vec::new();
            let mut right_matched: Vec<bool> = vec![false; join_rows.len()];

            for left_row in &combined_rows {
                let mut matched = false;

                let resolved_condition = if has_correlated_subquery {
                    if let Some(ref cond) = join_condition {
                        let mut substituted = cond.clone();
                        let mut value_offset = 0;
                        for (alias, schema) in &combined_schemas {
                            let row_values: Vec<Value> = left_row.values
                                [value_offset..value_offset + schema.columns.len()]
                                .to_vec();
                            let outer_row = Row::new(row_values);
                            substituted = super::helpers::substitute_outer_values(
                                &substituted,
                                alias,
                                schema,
                                &outer_row,
                            );
                            value_offset += schema.columns.len();
                        }
                        Some(self.resolve_subqueries(txn, &substituted).await?)
                    } else {
                        None
                    }
                } else {
                    join_condition.clone()
                };

                for (right_idx, right_row) in join_rows.iter().enumerate() {
                    let mut combined_values = left_row.values.clone();
                    combined_values.extend(right_row.values.clone());
                    let combined_row = Row::new(combined_values);

                    let matches = if let Some(ref cond) = resolved_condition {
                        let ctx = JoinContext {
                            tables: HashMap::new(),
                            column_offsets: column_offsets.clone(),
                            combined_row: &combined_row,
                            combined_schema: &temp_combined_schema,
                        };
                        matches!(eval_expr_join(cond, &ctx)?, Value::Boolean(true))
                    } else {
                        true
                    };

                    if matches {
                        new_combined_rows.push(combined_row);
                        matched = true;
                        right_matched[right_idx] = true;
                    }
                }
                if (is_left_join || is_full_join) && !matched {
                    let mut combined_values = left_row.values.clone();
                    for _ in 0..join_schema.columns.len() {
                        combined_values.push(Value::Null);
                    }
                    new_combined_rows.push(Row::new(combined_values));
                }
            }

            if is_right_join || is_full_join {
                for (right_idx, right_row) in join_rows.iter().enumerate() {
                    if !right_matched[right_idx] {
                        let mut combined_values: Vec<Value> = Vec::new();
                        for _ in 0..left_col_count {
                            combined_values.push(Value::Null);
                        }
                        combined_values.extend(right_row.values.clone());
                        new_combined_rows.push(Row::new(combined_values));
                    }
                }
            }

            combined_schemas.push((join_alias.clone(), join_schema));
            combined_rows = new_combined_rows;
        }

        let mut final_column_offsets: HashMap<String, usize> = HashMap::new();
        let mut final_columns: Vec<ColumnDef> = Vec::new();
        let mut offset = 0;
        for (alias, schema) in &combined_schemas {
            for col in &schema.columns {
                final_column_offsets.insert(format!("{}.{}", alias, col.name), offset);
                if !final_column_offsets.contains_key(&col.name) {
                    final_column_offsets.insert(col.name.clone(), offset);
                }
                final_columns.push(col.clone());
                offset += 1;
            }
        }
        let final_schema = TableSchema {
            name: "joined".to_string(),
            table_id: 0,
            columns: final_columns,
            version: 1,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
        };

        // Resolve subqueries (EXISTS, IN (SELECT ...), scalar subqueries) in WHERE clause
        let resolved_selection = if let Some(sel) = &select.selection {
            Some(self.resolve_subqueries(txn, sel).await?)
        } else {
            None
        };

        let filtered_rows = if let Some(ref sel) = resolved_selection {
            let mut v = Vec::new();
            for row in combined_rows {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets: final_column_offsets.clone(),
                    combined_row: &row,
                    combined_schema: &final_schema,
                };
                if matches!(eval_expr_join(sel, &ctx)?, Value::Boolean(true)) {
                    v.push(row);
                }
            }
            v
        } else {
            combined_rows
        };

        let resolved_projection = self
            .resolve_projection_subqueries(txn, &select.projection)
            .await?;

        let group_keys_exprs = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs,
            GroupByExpr::All => return Err(anyhow!("GROUP BY ALL not supported")),
        };

        let mut agg_funcs: Vec<(usize, AggExpr)> = Vec::new();
        for (i, item) in select.projection.iter().enumerate() {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(f))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(f),
                    ..
                } => {
                    if f.over.is_some() {
                        continue;
                    }
                    let func_name = f
                        .name
                        .0
                        .last()
                        .map(|i| i.value.to_uppercase())
                        .unwrap_or_default();
                    if matches!(
                        func_name.as_str(),
                        "COUNT" | "SUM" | "AVG" | "MAX" | "MIN" | "STRING_AGG" | "ARRAY_AGG"
                    ) {
                        agg_funcs.push((i, AggExpr::Function(f.clone())));
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
            let extra_start = select.projection.len();
            collect_having_agg_funcs(having_expr, &mut agg_funcs, extra_start);
        }

        let is_agg = !group_keys_exprs.is_empty() || !agg_funcs.is_empty();

        if is_agg {
            let mut groups: HashMap<Vec<u8>, Vec<Aggregator>> = HashMap::new();
            let mut group_rows: HashMap<Vec<u8>, Row> = HashMap::new();

            for row in filtered_rows {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets: final_column_offsets.clone(),
                    combined_row: &row,
                    combined_schema: &final_schema,
                };

                let mut key = Vec::new();
                for expr in group_keys_exprs {
                    key.push(eval_expr_join(expr, &ctx)?);
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
                                                match eval_expr_join(e, &ctx)? {
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
                        let filter_val = eval_expr_join(filter, &ctx)?;
                        if !matches!(filter_val, Value::Boolean(true)) {
                            continue;
                        }
                    }

                    let val = if let Some(e) = arg_expr {
                        eval_expr_join(e, &ctx)?
                    } else {
                        Value::Int32(1)
                    };
                    aggs[agg_idx].update(&val)?;
                }
            }

            let mut final_rows = Vec::new();
            let col_names: Vec<String> =
                select.projection.iter().map(get_select_item_name).collect();

            for (key_bytes, aggs) in groups {
                let representative = &group_rows[&key_bytes];
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets: final_column_offsets.clone(),
                    combined_row: representative,
                    combined_schema: &final_schema,
                };

                if let Some(having_expr) = &select.having {
                    let having_val = eval_having_expr_join(having_expr, &ctx, &agg_funcs, &aggs)?;
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
                            _ => return Err(anyhow!("Unsupported projection item")),
                        };
                        row_values.push(eval_expr_join(expr, &ctx)?);
                    }
                }
                final_rows.push(Row::new(row_values));
            }

            return Ok(ExecuteResult::Select {
                column_types: None,
                columns: col_names,
                rows: final_rows,
            });
        }

        let window_funcs = extract_window_functions(&select.projection);
        let window_results = if !window_funcs.is_empty() {
            Some(compute_window_functions_join(
                &filtered_rows,
                &final_column_offsets,
                &final_schema,
                &window_funcs,
            )?)
        } else {
            None
        };

        let (filtered_rows, window_results) = if !query.order_by.is_empty() {
            let mut indexed: Vec<(usize, Row)> = filtered_rows.into_iter().enumerate().collect();
            indexed.sort_by(|(_, a), (_, b)| {
                for order_expr in &query.order_by {
                    // Resolve ORDER BY expression: if it's an alias, use the SELECT list expression
                    let actual_expr = if let Expr::Identifier(ref ident) = order_expr.expr {
                        // Check if this identifier matches a SELECT list alias
                        let mut found_expr = None;
                        for item in &resolved_projection {
                            match item {
                                SelectItem::ExprWithAlias { expr, alias } => {
                                    if alias.value.eq_ignore_ascii_case(&ident.value) {
                                        found_expr = Some(expr);
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        found_expr.unwrap_or(&order_expr.expr)
                    } else {
                        &order_expr.expr
                    };

                    let ctx_a = JoinContext {
                        tables: HashMap::new(),
                        column_offsets: final_column_offsets.clone(),
                        combined_row: a,
                        combined_schema: &final_schema,
                    };
                    let ctx_b = JoinContext {
                        tables: HashMap::new(),
                        column_offsets: final_column_offsets.clone(),
                        combined_row: b,
                        combined_schema: &final_schema,
                    };
                    let val_a = eval_expr_join(actual_expr, &ctx_a).unwrap_or(Value::Null);
                    let val_b = eval_expr_join(actual_expr, &ctx_b).unwrap_or(Value::Null);
                    let cmp = super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                    if cmp != 0 {
                        let asc = order_expr.asc.unwrap_or(true);
                        return if asc {
                            if cmp > 0 {
                                std::cmp::Ordering::Greater
                            } else {
                                std::cmp::Ordering::Less
                            }
                        } else {
                            if cmp > 0 {
                                std::cmp::Ordering::Less
                            } else {
                                std::cmp::Ordering::Greater
                            }
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
            (reordered_rows, reordered_wr)
        } else {
            (filtered_rows, window_results)
        };

        let mut final_rows = filtered_rows;
        if let Some(offset) = &query.offset {
            if let Ok(v) = eval_expr(&offset.value, None, None) {
                let n = match v {
                    Value::Int64(n) => n as usize,
                    Value::Int32(n) => n as usize,
                    _ => 0,
                };
                final_rows = final_rows.into_iter().skip(n).collect();
            }
        }
        if let Some(limit) = &query.limit {
            if let Ok(v) = eval_expr(limit, None, None) {
                let n = match v {
                    Value::Int64(n) => n as usize,
                    Value::Int32(n) => n as usize,
                    _ => usize::MAX,
                };
                final_rows = final_rows.into_iter().take(n).collect();
            }
        }

        let mut cols = Vec::new();
        let mut result_rows = Vec::new();

        let wildcard = select
            .projection
            .iter()
            .any(|p| matches!(p, SelectItem::Wildcard(_)));
        if wildcard {
            if has_natural_join && !natural_join_common_cols.is_empty() {
                let mut col_indices_to_keep: Vec<usize> = Vec::new();
                let mut seen_common_cols: std::collections::HashSet<String> =
                    std::collections::HashSet::new();

                for common_col in &natural_join_common_cols {
                    cols.push(common_col.clone());
                }

                let mut offset = 0;
                for (_, schema) in &combined_schemas {
                    for col in &schema.columns {
                        if natural_join_common_cols.contains(&col.name) {
                            if !seen_common_cols.contains(&col.name) {
                                col_indices_to_keep.push(offset);
                                seen_common_cols.insert(col.name.clone());
                            }
                        } else {
                            cols.push(col.name.clone());
                            col_indices_to_keep.push(offset);
                        }
                        offset += 1;
                    }
                }

                result_rows = final_rows
                    .into_iter()
                    .map(|row| {
                        let vals: Vec<Value> = col_indices_to_keep
                            .iter()
                            .map(|&idx| row.values.get(idx).cloned().unwrap_or(Value::Null))
                            .collect();
                        Row::new(vals)
                    })
                    .collect();
            } else {
                for (alias, schema) in &combined_schemas {
                    for col in &schema.columns {
                        cols.push(format!("{}.{}", alias, col.name));
                    }
                }
                result_rows = final_rows;
            }
        } else {
            for item in &select.projection {
                match item {
                    SelectItem::UnnamedExpr(Expr::Identifier(id)) => cols.push(id.value.clone()),
                    SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
                        cols.push(
                            parts
                                .last()
                                .map(|p| p.value.clone())
                                .unwrap_or_else(|| "col".to_string()),
                        );
                    }
                    SelectItem::ExprWithAlias { alias, .. } => cols.push(alias.value.clone()),
                    SelectItem::UnnamedExpr(Expr::Function(f)) => {
                        cols.push(
                            f.name
                                .0
                                .last()
                                .map(|i| i.value.clone())
                                .unwrap_or("func".to_string()),
                        );
                    }
                    _ => cols.push("col".to_string()),
                }
            }

            let rows_to_project = match &select.distinct {
                Some(Distinct::On(on_exprs)) => distinct_on_rows_join(
                    final_rows,
                    on_exprs,
                    &final_column_offsets,
                    &final_schema,
                ),
                _ => final_rows,
            };

            let has_window_funcs = !window_funcs.is_empty();
            for (row_idx, row) in rows_to_project.iter().enumerate() {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets: final_column_offsets.clone(),
                    combined_row: row,
                    combined_schema: &final_schema,
                };
                let mut vals = Vec::new();
                for (proj_idx, item) in resolved_projection.iter().enumerate() {
                    if has_window_funcs {
                        if let Some(wf_idx) =
                            window_funcs.iter().position(|wf| wf.proj_idx == proj_idx)
                        {
                            if let Some(ref wr) = window_results {
                                vals.push(wr[row_idx][wf_idx].clone());
                                continue;
                            }
                        }
                    }
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        SelectItem::Wildcard(_) => continue,
                        _ => return Err(anyhow!("Unsupported select item")),
                    };
                    vals.push(eval_expr_join(expr, &ctx)?);
                }
                result_rows.push(Row::new(vals));
            }
        }

        if matches!(&select.distinct, Some(Distinct::Distinct)) {
            result_rows = dedup_rows(result_rows);
        }

        Ok(ExecuteResult::Select {
            column_types: None,
            columns: cols,
            rows: result_rows,
        })
    }
}
