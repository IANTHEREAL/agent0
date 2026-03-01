use super::helpers::{int_col, text_col};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

/// Stub pg_stat_user_tables — psql `\d+` queries this for table size stats.
/// Returns empty rows (no persistent statistics in db9-server).
pub struct PgStatUserTables;

#[async_trait]
impl VirtualTable for PgStatUserTables {
    fn name(&self) -> &str {
        "pg_stat_user_tables"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_stat_user_tables".to_string(),
            columns: vec![
                int_col("relid"),
                text_col("schemaname"),
                text_col("relname"),
                int_col("seq_scan"),
                int_col("seq_tup_read"),
                int_col("idx_scan"),
                int_col("idx_tup_fetch"),
                int_col("n_tup_ins"),
                int_col("n_tup_upd"),
                int_col("n_tup_del"),
                int_col("n_live_tup"),
                int_col("n_dead_tup"),
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

    async fn scan(&self, _ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        Ok(Vec::new())
    }
}
