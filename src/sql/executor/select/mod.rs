//! SELECT query execution — single-path through Analyzer.

use super::super::distinct::apply_offset_limit_fetch;
use super::super::sequences;
use super::super::ExecuteResult;
use super::core::Executor;
use crate::types::{Row, TableSchema};
use anyhow::{anyhow, Result};
use sqlparser::ast::{Query, SetExpr};
use std::collections::HashMap;
use tikv_client::Transaction;

mod analyzed;
pub(crate) mod order;

impl Executor {
    pub(crate) fn execute_query_with_outer_ctes<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        query: &'a Query,
        outer_ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            if query.with.is_none() {
                return self
                    .execute_query_with_ctes(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        query,
                        outer_ctes,
                    )
                    .await;
            }

            let merged_ctes = self
                .build_cte_context_with_base(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    outer_ctes,
                )
                .await?;
            self.execute_query_with_ctes(
                txn,
                db_id,
                sequence_values,
                search_path,
                query,
                &merged_ctes,
            )
            .await
        })
    }

    pub(crate) async fn execute_query_with_ctes(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        // Handle standalone VALUES queries (not handled by the Analyzer).
        if let SetExpr::Values(values) = &*query.body {
            return self
                .execute_values_query(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    values,
                    ctes,
                )
                .await;
        }

        // All SELECT / SET operations go through the Analyzer path.
        self.try_execute_analyzed(txn, db_id, sequence_values, search_path, query, ctes)
            .await
    }

    /// Execute a standalone VALUES (...) query.
    async fn execute_values_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        values: &sqlparser::ast::Values,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        let store = self.store();
        let mut column_count: Option<usize> = None;
        let mut rows = Vec::with_capacity(values.rows.len());
        for expr_row in &values.rows {
            let expr_len = expr_row.len();
            if let Some(expected) = column_count {
                if expr_len != expected {
                    return Err(anyhow!("VALUES lists must all be the same length"));
                }
            } else {
                column_count = Some(expr_len);
            }

            let mut row_values = Vec::with_capacity(expr_len);
            for expr in expr_row {
                let resolved = self
                    .resolve_subqueries(txn, db_id, sequence_values, search_path, expr, ctes, &[])
                    .await?;
                let value = sequences::eval_expr_with_sequences(
                    &store,
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &resolved,
                    None,
                    None,
                )
                .await?;
                row_values.push(value);
            }
            rows.push(Row::new(row_values));
        }

        let column_count = column_count.unwrap_or(0);
        let columns: Vec<String> = (1..=column_count)
            .map(|idx| format!("column{}", idx))
            .collect();

        if !query.order_by.is_empty() {
            rows = self.apply_order_by_for_aggregate(rows, &query.order_by, &columns)?;
        }
        rows = apply_offset_limit_fetch(rows, query);

        Ok(ExecuteResult::Select {
            column_types: None,
            columns,
            rows,
            timezone: crate::session_context::current_timezone(),
        })
    }
}
