use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

use super::{BoxedOperator, ExecutionContext};
use crate::model::Row;
use crate::pool::try_grow_statement_memory_scope;
use crate::sql::executor::Executor;
use crate::sql::memory::estimate_row_size;
use crate::sql::query_context::QueryContext;
use crate::storage::TikvStore;

use crate::model::TableSchema;

fn build_query_ctx_from_task_locals() -> QueryContext {
    QueryContext::from_task_locals()
}

pub async fn execute_operator_tree(
    executor: &Executor,
    operator: &mut BoxedOperator,
    txn: &mut Transaction,
    store: Arc<TikvStore>,
    db_id: u64,
    search_path: &[String],
    sequence_values: &mut HashMap<String, i64>,
) -> Result<Vec<Row>> {
    let qc = build_query_ctx_from_task_locals();
    let mut ctx = ExecutionContext::new(
        executor,
        txn,
        store,
        db_id,
        search_path,
        sequence_values,
        &qc,
    );

    operator.open(&mut ctx).await?;

    let mut rows = Vec::new();
    while let Some(row) = operator.next(&mut ctx).await? {
        try_grow_statement_memory_scope("operators.executor.root_rows", estimate_row_size(&row))?;
        rows.push(row);
    }

    operator.close(&mut ctx).await?;

    Ok(rows)
}

pub async fn execute_operator_tree_with_ctes(
    executor: &Executor,
    operator: &mut BoxedOperator,
    txn: &mut Transaction,
    store: Arc<TikvStore>,
    db_id: u64,
    search_path: &[String],
    sequence_values: &mut HashMap<String, i64>,
    cte_tables: &HashMap<String, (TableSchema, Vec<Row>)>,
) -> Result<Vec<Row>> {
    let qc = build_query_ctx_from_task_locals();
    let mut ctx = ExecutionContext::with_ctes(
        executor,
        txn,
        store,
        db_id,
        search_path,
        sequence_values,
        cte_tables,
        &qc,
    );

    operator.open(&mut ctx).await?;

    let mut rows = Vec::new();
    while let Some(row) = operator.next(&mut ctx).await? {
        try_grow_statement_memory_scope("operators.executor.root_rows", estimate_row_size(&row))?;
        rows.push(row);
    }

    operator.close(&mut ctx).await?;

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::build_query_ctx_from_task_locals;

    #[test]
    fn build_query_ctx_uses_test_fallback_when_task_locals_absent() {
        let ctx = build_query_ctx_from_task_locals();
        assert_eq!(ctx.connection_id, 0);
        assert_eq!(ctx.database_name.as_ref(), "postgres");
        assert_eq!(ctx.current_user.as_ref(), "postgres");
        assert_eq!(ctx.timezone.as_ref(), "UTC");
        assert!(ctx.params.is_empty());
    }
}
