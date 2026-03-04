use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

use crate::model::{Row, TableSchema};
use crate::sql::executor::Executor;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences::SequenceSession;
use crate::storage::TikvStore;

pub struct ExecutionContext<'a> {
    pub executor: &'a Executor,
    pub txn: &'a mut Transaction,
    pub store: Arc<TikvStore>,
    pub db_id: u64,
    #[allow(dead_code)] // forward-compat: threaded through for future operator use
    pub search_path: &'a [String],
    #[allow(dead_code)] // forward-compat: threaded through for future operator use
    pub sequence_values: &'a mut SequenceSession,
    pub cte_tables: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    pub query_ctx: &'a QueryContext,
    pub outer_row: Option<Row>,
}

static EMPTY_CTE_MAP: std::sync::LazyLock<HashMap<String, (TableSchema, Vec<Row>)>> =
    std::sync::LazyLock::new(HashMap::new);

impl<'a> ExecutionContext<'a> {
    pub fn new(
        executor: &'a Executor,
        txn: &'a mut Transaction,
        store: Arc<TikvStore>,
        db_id: u64,
        search_path: &'a [String],
        sequence_values: &'a mut SequenceSession,
        query_ctx: &'a QueryContext,
    ) -> Self {
        Self {
            executor,
            txn,
            store,
            db_id,
            search_path,
            sequence_values,
            cte_tables: &EMPTY_CTE_MAP,
            query_ctx,
            outer_row: None,
        }
    }

    pub fn with_ctes(
        executor: &'a Executor,
        txn: &'a mut Transaction,
        store: Arc<TikvStore>,
        db_id: u64,
        search_path: &'a [String],
        sequence_values: &'a mut SequenceSession,
        cte_tables: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        query_ctx: &'a QueryContext,
    ) -> Self {
        Self {
            executor,
            txn,
            store,
            db_id,
            search_path,
            sequence_values,
            cte_tables,
            query_ctx,
            outer_row: None,
        }
    }
}
