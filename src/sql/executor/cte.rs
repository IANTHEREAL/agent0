//! CTE (Common Table Expression) execution for the SQL executor

use super::core::Executor;
use super::super::helpers::{cte_is_recursive, normalize_ident, set_expr_references_table};
use super::super::ExecuteResult;
use crate::types::{ColumnDef, DataType, Row, TableSchema};
use anyhow::{anyhow, Result};
use sqlparser::ast::{Ident, Query, SetExpr, SetOperator, SetQuantifier};
use std::collections::HashMap;
use tikv_client::Transaction;

impl Executor {
    pub(crate) async fn build_cte_context_with_base(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        base_ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<HashMap<String, (TableSchema, Vec<Row>)>> {
        let mut ctes: HashMap<String, (TableSchema, Vec<Row>)> = base_ctes.clone();
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                let cte_name = cte.alias.name.value.to_lowercase();

                if with.recursive && cte_is_recursive(&cte.query, &cte_name) {
                    let (schema, rows) = self
                        .execute_recursive_cte(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &cte_name,
                            &cte.query,
                            &cte.alias.columns,
                            &ctes,
                        )
                        .await?;
                    ctes.insert(cte_name, (schema, rows));
                } else {
                    let cte_result = self
                        .execute_query_with_outer_ctes(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &cte.query,
                            &ctes,
                        )
                        .await?;
                    match cte_result {
                        ExecuteResult::Select {
                            columns,
                            column_types,
                            rows,
                        } => {
                            let col_names: Vec<String> = if cte.alias.columns.is_empty() {
                                columns
                            } else {
                                cte.alias.columns.iter().map(normalize_ident).collect()
                            };
                            let inferred_types: Vec<DataType> = if let Some(types) = column_types {
                                types
                            } else if let Some(first_row) = rows.first() {
                                first_row
                                    .values
                                    .iter()
                                    .map(|v| v.data_type().unwrap_or(DataType::Text))
                                    .collect()
                            } else {
                                vec![DataType::Text; col_names.len()]
                            };
	                            let schema = TableSchema {
	                                table_id: 0,
	                                name: cte_name.clone(),
                                columns: col_names
                                    .iter()
                                    .enumerate()
                                    .map(|(idx, n)| ColumnDef {
                                        name: n.clone(),
                                        data_type: inferred_types
                                            .get(idx)
                                            .cloned()
                                            .unwrap_or(DataType::Text),
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
	                            };
                            ctes.insert(cte_name, (schema, rows));
                        }
                        _ => return Err(anyhow!("CTE must be a SELECT query")),
                    }
                }
            }
        }
        Ok(ctes)
    }

    pub(crate) async fn build_cte_context(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
    ) -> Result<HashMap<String, (TableSchema, Vec<Row>)>> {
        let base = HashMap::new();
        self.build_cte_context_with_base(txn, db_id, sequence_values, search_path, query, &base)
            .await
    }

    pub(crate) async fn execute_recursive_cte(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        cte_name: &str,
        query: &Query,
        alias_columns: &[Ident],
        existing_ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<(TableSchema, Vec<Row>)> {
        let (base_expr, recursive_expr, is_union_all) = match &*query.body {
            SetExpr::SetOperation {
                op: SetOperator::Union,
                set_quantifier,
                left,
                right,
            } => {
                let is_all = matches!(set_quantifier, SetQuantifier::All);
                if set_expr_references_table(left, cte_name) {
                    (right.clone(), left.clone(), is_all)
                } else {
                    (left.clone(), right.clone(), is_all)
                }
            }
            _ => return Err(anyhow!("Recursive CTE must use UNION or UNION ALL")),
        };

        let base_query = Query {
            with: None,
            body: base_expr,
            order_by: vec![],
            limit: None,
            offset: None,
            fetch: None,
            locks: vec![],
            limit_by: vec![],
            for_clause: None,
        };
        let base_result = self
            .execute_query_with_ctes(
                txn,
                db_id,
                sequence_values,
                search_path,
                &base_query,
                existing_ctes,
            )
            .await?;
        let (columns, base_types, mut all_rows) = match base_result {
            ExecuteResult::Select {
                columns,
                column_types,
                rows,
            } => (columns, column_types, rows),
            _ => return Err(anyhow!("Recursive CTE base must be SELECT")),
        };

        let col_names: Vec<String> = if alias_columns.is_empty() {
            columns
        } else {
            alias_columns.iter().map(normalize_ident).collect()
        };
        let inferred_types: Vec<DataType> = if let Some(types) = base_types {
            types
        } else if let Some(first_row) = all_rows.first() {
            first_row
                .values
                .iter()
                .map(|v| v.data_type().unwrap_or(DataType::Text))
                .collect()
        } else {
            vec![DataType::Text; col_names.len()]
        };
	        let schema = TableSchema {
	            table_id: 0,
	            name: cte_name.to_string(),
            columns: col_names
                .iter()
                .enumerate()
                .map(|(idx, n)| ColumnDef {
                    name: n.clone(),
                    data_type: inferred_types.get(idx).cloned().unwrap_or(DataType::Text),
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
	        };

        let mut working_table = all_rows.clone();
        let max_iterations = 1000;
        let mut iteration = 0;

        while !working_table.is_empty() && iteration < max_iterations {
            iteration += 1;

            let mut temp_ctes = existing_ctes.clone();
            temp_ctes.insert(
                cte_name.to_string(),
                (schema.clone(), working_table.clone()),
            );

            let recursive_query = Query {
                with: None,
                body: recursive_expr.clone(),
                order_by: vec![],
                limit: None,
                offset: None,
                fetch: None,
                locks: vec![],
                limit_by: vec![],
                for_clause: None,
            };
        let recursive_result = self
            .execute_query_with_ctes(
                txn,
                db_id,
                sequence_values,
                search_path,
                &recursive_query,
                &temp_ctes,
            )
            .await?;

            let new_rows = match recursive_result {
                ExecuteResult::Select { rows, .. } => rows,
                _ => return Err(anyhow!("Recursive CTE iteration must be SELECT")),
            };

            if new_rows.is_empty() {
                break;
            }

            if is_union_all {
                all_rows.extend(new_rows.clone());
                working_table = new_rows;
            } else {
                let mut unique_new_rows = Vec::new();
                for row in new_rows {
                    if !all_rows.contains(&row) {
                        unique_new_rows.push(row.clone());
                        all_rows.push(row);
                    }
                }
                working_table = unique_new_rows;
            }
        }

        Ok((schema, all_rows))
    }
}
