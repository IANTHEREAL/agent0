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
        TableSchema::virtual_table(
            "pg_publication",
            vec![
                int_col("oid"),
                name_col("pubname"),
                bool_col("puballtables"),
            ],
        )
    }

    async fn scan(&self, _ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        Ok(Vec::new())
    }
}
