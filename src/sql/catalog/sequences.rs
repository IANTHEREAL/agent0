use super::helpers::{int_col, int_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct Sequences;

#[async_trait]
impl VirtualTable for Sequences {
    fn name(&self) -> &str {
        "sequences"
    }

    fn schema_name(&self) -> &str {
        "information_schema"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "sequences".to_string(),
            columns: vec![
                text_col("sequence_catalog"),
                text_col("sequence_schema"),
                text_col("sequence_name"),
                text_col("data_type"),
                int_col("start_value"),
                int_col("minimum_value"),
                int_col("maximum_value"),
                int_col("increment"),
                text_col("cycle_option"),
                text_col("sequence_owner"),
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut seqs = ctx.store.list_sequences(ctx.txn, ctx.db_id).await?;
        seqs.sort_by(|a, b| a.schema.cmp(&b.schema).then(a.name.cmp(&b.name)));

        let mut rows = Vec::with_capacity(seqs.len());
        for seq in seqs {
            rows.push(Row::new(vec![
                text_val(ctx.database_name),
                text_val(&seq.schema),
                text_val(&seq.name),
                text_val("bigint"),
                int_val(seq.start_value),
                int_val(seq.min_value),
                int_val(seq.max_value),
                int_val(seq.increment),
                text_val(if seq.is_cycled { "YES" } else { "NO" }),
                text_val(&seq.owner),
            ]));
        }

        Ok(rows)
    }
}
