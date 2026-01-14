//! DML operations (INSERT, UPDATE, DELETE) for the SQL executor

use super::dml;
use super::executor::Executor;
use super::expr::{eval_expr, eval_expr_join, JoinContext};
use super::ExecuteResult;
use crate::types::{DataType, Row, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Assignment, Expr, Ident, ObjectName, OnInsert, Query, SelectItem, SetExpr, Values,
};
use std::collections::HashMap;
use tikv_client::Transaction;

impl Executor {
    pub(crate) async fn execute_insert(
        &self,
        txn: &mut Transaction,
        table_name: &ObjectName,
        columns: &[Ident],
        source: &Option<Box<Query>>,
        returning: &Option<Vec<SelectItem>>,
        on_conflict: &Option<OnInsert>,
    ) -> Result<ExecuteResult> {
        let t = table_name.0.last().unwrap().value.clone();
        let schema = self
            .store()
            .get_schema(txn, &t)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;
        let source = source
            .as_ref()
            .ok_or_else(|| anyhow!("INSERT requires VALUES"))?;
        let values = match &*source.body {
            SetExpr::Values(Values { rows, .. }) => rows,
            _ => return Err(anyhow!("Only VALUES supported")),
        };

        let mut affected = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = dml::build_returning_columns(returning, &schema)?;

        for exprs in values {
            let (mut row_vals, indices) = dml::prepare_insert_row(&schema, columns, exprs)?;
            dml::fill_missing_columns(&self.store(), txn, &schema, &mut row_vals, &indices).await?;
            dml::coerce_row_values(&schema, &mut row_vals)?;
            let row = Row::new(row_vals);
            dml::validate_check_constraints(&schema, &row)?;

            let result =
                dml::execute_insert_row(&self.store(), txn, &t, &schema, row, on_conflict).await?;
            if let Some(final_row) = result {
                affected += 1;
                if let Some(ret_row) = dml::eval_returning_row(returning, &final_row, &schema)? {
                    ret_rows.push(ret_row);
                }
            }
        }

        if returning.is_some() {
            let column_types = Some(
                ret_cols
                    .iter()
                    .map(|col_name| {
                        schema
                            .columns
                            .iter()
                            .find(|c| c.name.eq_ignore_ascii_case(col_name))
                            .map(|c| c.data_type.clone())
                            .unwrap_or(DataType::Text)
                    })
                    .collect(),
            );
            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
            })
        } else {
            Ok(ExecuteResult::Insert {
                affected_rows: affected,
            })
        }
    }

    pub(crate) async fn execute_delete(
        &self,
        txn: &mut Transaction,
        from: &[sqlparser::ast::TableWithJoins],
        selection: &Option<Expr>,
        returning: &Option<Vec<SelectItem>>,
    ) -> Result<ExecuteResult> {
        let t = match &from[0].relation {
            sqlparser::ast::TableFactor::Table { name, .. } => name.0.last().unwrap().value.clone(),
            _ => return Err(anyhow!("Unsupported")),
        };
        let schema = self
            .store()
            .get_schema(txn, &t)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!("No PK"));
        }
        let resolved_selection = if let Some(sel) = selection {
            Some(self.resolve_subqueries(txn, sel).await?)
        } else {
            None
        };
        let rows = self.scan_and_fill(txn, &t, &schema).await?;
        let mut cnt = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = dml::build_returning_columns(returning, &schema)?;

        for r in rows {
            if let Some(ref e) = resolved_selection {
                if !matches!(eval_expr(e, Some(&r), Some(&schema))?, Value::Boolean(true)) {
                    continue;
                }
            }
            if let Some(ret_row) = dml::eval_returning_row(returning, &r, &schema)? {
                ret_rows.push(ret_row);
            }
            dml::execute_delete_row(&self.store(), txn, &t, &schema, &r).await?;
            cnt += 1;
        }

        if returning.is_some() {
            let column_types = Some(
                ret_cols
                    .iter()
                    .map(|col_name| {
                        schema
                            .columns
                            .iter()
                            .find(|c| c.name.eq_ignore_ascii_case(col_name))
                            .map(|c| c.data_type.clone())
                            .unwrap_or(DataType::Text)
                    })
                    .collect(),
            );
            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
            })
        } else {
            Ok(ExecuteResult::Delete { affected_rows: cnt })
        }
    }

    pub(crate) async fn execute_update(
        &self,
        txn: &mut Transaction,
        table: &sqlparser::ast::TableWithJoins,
        assignments: &[Assignment],
        from: &Option<sqlparser::ast::TableWithJoins>,
        selection: &Option<Expr>,
        returning: &Option<Vec<SelectItem>>,
    ) -> Result<ExecuteResult> {
        let t = match &table.relation {
            sqlparser::ast::TableFactor::Table { name, .. } => name.0.last().unwrap().value.clone(),
            _ => return Err(anyhow!("Unsupported")),
        };
        let table_alias = match &table.relation {
            sqlparser::ast::TableFactor::Table { alias, .. } => alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_else(|| t.clone()),
            _ => t.clone(),
        };
        let schema = self
            .store()
            .get_schema(txn, &t)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!("No PK"));
        }
        let resolved_selection = if let Some(sel) = selection {
            Some(self.resolve_subqueries(txn, sel).await?)
        } else {
            None
        };
        let indices = dml::validate_update_columns(&schema, assignments)?;

        let (from_schema, from_rows, from_alias) = if let Some(from_table) = from {
            let from_name = match &from_table.relation {
                sqlparser::ast::TableFactor::Table { name, .. } => {
                    name.0.last().unwrap().value.clone()
                }
                _ => return Err(anyhow!("Unsupported FROM table")),
            };
            let from_alias_str = match &from_table.relation {
                sqlparser::ast::TableFactor::Table { alias, .. } => alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| from_name.clone()),
                _ => from_name.clone(),
            };
            let fs = self
                .store()
                .get_schema(txn, &from_name)
                .await?
                .ok_or_else(|| anyhow!("FROM table not found"))?;
            let fr = self.scan_and_fill(txn, &from_name, &fs).await?;
            (Some(fs), Some(fr), Some(from_alias_str))
        } else {
            (None, None, None)
        };

        let rows = self.scan_and_fill(txn, &t, &schema).await?;
        let mut cnt = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = dml::build_returning_columns(returning, &schema)?;

        for r in &rows {
            let matching_from_rows: Vec<&Row> = if let (Some(ref fs), Some(ref fr), Some(ref fa)) =
                (&from_schema, &from_rows, &from_alias)
            {
                let (combined_schema, _, column_offsets) =
                    dml::build_update_join_context(&schema, &table_alias, fs, fa, r, &fr[0]);

                fr.iter()
                    .filter(|from_row| {
                        if let Some(ref sel) = resolved_selection {
                            let mut combined_values = r.values.clone();
                            combined_values.extend(from_row.values.clone());
                            let combined_row = Row::new(combined_values);
                            let ctx = JoinContext {
                                tables: HashMap::new(),
                                column_offsets: column_offsets.clone(),
                                combined_row: &combined_row,
                                combined_schema: &combined_schema,
                            };
                            matches!(eval_expr_join(sel, &ctx), Ok(Value::Boolean(true)))
                        } else {
                            true
                        }
                    })
                    .collect()
            } else {
                if let Some(ref e) = resolved_selection {
                    if !matches!(
                        eval_expr(e, Some(r), Some(&schema)),
                        Ok(Value::Boolean(true))
                    ) {
                        continue;
                    }
                }
                vec![r]
            };

            if matching_from_rows.is_empty() && from.is_some() {
                continue;
            }

            let new_vals = if let (Some(ref fs), Some(ref fa)) = (&from_schema, &from_alias) {
                if let Some(first_from) = matching_from_rows.first() {
                    let (combined_schema, combined_row, _) = dml::build_update_join_context(
                        &schema,
                        &table_alias,
                        fs,
                        fa,
                        r,
                        first_from,
                    );
                    dml::compute_update_values(
                        &schema,
                        r,
                        assignments,
                        &indices,
                        Some((&combined_row, &combined_schema)),
                    )?
                } else {
                    dml::compute_update_values(&schema, r, assignments, &indices, None)?
                }
            } else {
                dml::compute_update_values(&schema, r, assignments, &indices, None)?
            };

            let new_row = Row::new(new_vals);
            dml::validate_check_constraints(&schema, &new_row)?;
            let updated_row =
                dml::execute_update_row(&self.store(), txn, &t, &schema, r, new_row).await?;

            if let Some(ret_row) = dml::eval_returning_row(returning, &updated_row, &schema)? {
                ret_rows.push(ret_row);
            }
            cnt += 1;
        }

        if returning.is_some() {
            let column_types = Some(
                ret_cols
                    .iter()
                    .map(|col_name| {
                        schema
                            .columns
                            .iter()
                            .find(|c| c.name.eq_ignore_ascii_case(col_name))
                            .map(|c| c.data_type.clone())
                            .unwrap_or(DataType::Text)
                    })
                    .collect(),
            );

            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
            })
        } else {
            Ok(ExecuteResult::Update { affected_rows: cnt })
        }
    }
}
