use super::helpers::{bool_col, int_col};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

/// Stub pg_inherits — no declarative partition hierarchy in current db9 catalog.
pub struct PgInherits;

#[async_trait]
impl VirtualTable for PgInherits {
    fn name(&self) -> &str {
        "pg_inherits"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_inherits",
            vec![
                int_col("inhrelid"),
                int_col("inhparent"),
                int_col("inhseqno"),
                bool_col("inhdetachpending"),
            ],
        )
    }

    async fn scan(&self, _ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        Ok(Vec::new())
    }
}
