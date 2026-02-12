use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

use crate::sql::query_context::QueryContext;
use crate::storage::TikvStore;
use crate::types::{Row, TableSchema};

pub struct ExecutionContext<'a> {
    pub txn: &'a mut Transaction,
    pub store: Arc<TikvStore>,
    pub db_id: u64,
    pub search_path: &'a [String],
    pub sequence_values: &'a mut HashMap<String, i64>,
    pub cte_tables: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    pub query_ctx: &'a QueryContext,
}

static EMPTY_CTE_MAP: std::sync::LazyLock<HashMap<String, (TableSchema, Vec<Row>)>> =
    std::sync::LazyLock::new(HashMap::new);

impl<'a> ExecutionContext<'a> {
    pub fn new(
        txn: &'a mut Transaction,
        store: Arc<TikvStore>,
        db_id: u64,
        search_path: &'a [String],
        sequence_values: &'a mut HashMap<String, i64>,
        query_ctx: &'a QueryContext,
    ) -> Self {
        Self {
            txn,
            store,
            db_id,
            search_path,
            sequence_values,
            cte_tables: &EMPTY_CTE_MAP,
            query_ctx,
        }
    }

    pub fn with_ctes(
        txn: &'a mut Transaction,
        store: Arc<TikvStore>,
        db_id: u64,
        search_path: &'a [String],
        sequence_values: &'a mut HashMap<String, i64>,
        cte_tables: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        query_ctx: &'a QueryContext,
    ) -> Self {
        Self {
            txn,
            store,
            db_id,
            search_path,
            sequence_values,
            cte_tables,
            query_ctx,
        }
    }
}
