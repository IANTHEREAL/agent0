use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

use super::{BoxedOperator, ExecutionContext};
use crate::sql::query_context::QueryContext;
use crate::storage::TikvStore;
use crate::types::Row;

use crate::types::TableSchema;

fn build_query_ctx_from_task_locals() -> QueryContext {
    use crate::sql::expr::{get_connection_id_value, get_current_database_name};
    use crate::sql::statement_time::statement_timestamp_millis_or_now;

    QueryContext::new(
        get_connection_id_value(),
        get_current_database_name().unwrap_or_else(|| Arc::from("postgres")),
        statement_timestamp_millis_or_now(),
        crate::session_context::current_timezone(),
    )
}

pub async fn execute_operator_tree(
    operator: &mut BoxedOperator,
    txn: &mut Transaction,
    store: Arc<TikvStore>,
    db_id: u64,
    search_path: &[String],
    sequence_values: &mut HashMap<String, i64>,
) -> Result<Vec<Row>> {
    let qc = build_query_ctx_from_task_locals();
    let mut ctx =
        ExecutionContext::new(txn, store, db_id, search_path, sequence_values).with_query_ctx(&qc);

    operator.open(&mut ctx).await?;

    let mut rows = Vec::new();
    while let Some(row) = operator.next(&mut ctx).await? {
        rows.push(row);
    }

    operator.close(&mut ctx).await?;

    Ok(rows)
}

pub async fn execute_operator_tree_with_ctes(
    operator: &mut BoxedOperator,
    txn: &mut Transaction,
    store: Arc<TikvStore>,
    db_id: u64,
    search_path: &[String],
    sequence_values: &mut HashMap<String, i64>,
    cte_tables: &HashMap<String, (TableSchema, Vec<Row>)>,
) -> Result<Vec<Row>> {
    let qc = build_query_ctx_from_task_locals();
    let mut ctx =
        ExecutionContext::with_ctes(txn, store, db_id, search_path, sequence_values, cte_tables)
            .with_query_ctx(&qc);

    operator.open(&mut ctx).await?;

    let mut rows = Vec::new();
    while let Some(row) = operator.next(&mut ctx).await? {
        rows.push(row);
    }

    operator.close(&mut ctx).await?;

    Ok(rows)
}
