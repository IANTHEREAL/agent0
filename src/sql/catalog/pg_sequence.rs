use super::helpers::{bool_col, int_col, int_val};
use super::{ScanContext, VirtualTable};
use crate::sql::catalog_oids;
use crate::types::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgSequence;

#[async_trait]
impl VirtualTable for PgSequence {
    fn name(&self) -> &str {
        "pg_sequence"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_sequence".to_string(),
            columns: vec![
                int_col("seqrelid"),
                int_col("seqtypid"),
                int_col("seqstart"),
                int_col("seqincrement"),
                int_col("seqmax"),
                int_col("seqmin"),
                int_col("seqcache"),
                bool_col("seqcycle"),
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let seqs = ctx.store.list_sequences(ctx.txn, ctx.db_id).await?;
        let mut rows = Vec::with_capacity(seqs.len());

        for seq in seqs {
            rows.push(Row::new(vec![
                int_val(catalog_oids::pg_class_sequence_oid(seq.oid)),
                int_val(20), // seqtypid: int8
                int_val(seq.start_value),
                int_val(seq.increment),
                int_val(seq.max_value),
                int_val(seq.min_value),
                int_val(seq.cache_size),
                Value::Boolean(seq.is_cycled),
            ]));
        }

        Ok(rows)
    }
}
