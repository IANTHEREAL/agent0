use super::helpers::{bool_col, int_col, name_col};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

/// Stub pg_publication — db9-server has no logical replication publications.
pub struct PgPublication;

#[async_trait]
impl VirtualTable for PgPublication {
    fn name(&self) -> &str {
        "pg_publication"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_publication".to_string(),
            columns: vec![
                int_col("oid"),
                name_col("pubname"),
                bool_col("puballtables"),
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
