//! SELECT query execution — single-path through Analyzer.

use super::super::ExecuteResult;
use super::core::Executor;
use crate::model::{Row, TableSchema};
use crate::sql::sequences::SequenceSession;
use anyhow::Result;
use sqlparser::ast::Query;
use std::collections::HashMap;
use tikv_client::Transaction;

pub(crate) mod analyzed;

impl Executor {
    pub(crate) fn execute_query_with_outer_ctes<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut SequenceSession,
        search_path: &'a [String],
        query: &'a Query,
        outer_ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        current_role: Option<&'a str>,
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
                        current_role,
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
                    current_role,
                )
                .await?;
            self.execute_query_with_ctes(
                txn,
                db_id,
                sequence_values,
                search_path,
                query,
                &merged_ctes,
                current_role,
            )
            .await
        })
    }

    pub(crate) async fn execute_query_with_ctes(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        query: &Query,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        // All SELECT / VALUES / SET operations go through the Analyzer path.
        self.try_execute_analyzed(
            txn,
            db_id,
            sequence_values,
            search_path,
            query,
            ctes,
            current_role,
        )
        .await
    }
}
