use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

use crate::storage::TikvStore;

pub struct ExecutionContext<'a> {
    pub txn: &'a mut Transaction,
    pub store: Arc<TikvStore>,
    pub search_path: &'a [String],
    pub sequence_values: &'a mut HashMap<String, i64>,
}

impl<'a> ExecutionContext<'a> {
    pub fn new(
        txn: &'a mut Transaction,
        store: Arc<TikvStore>,
        search_path: &'a [String],
        sequence_values: &'a mut HashMap<String, i64>,
    ) -> Self {
        Self {
            txn,
            store,
            search_path,
            sequence_values,
        }
    }
}
