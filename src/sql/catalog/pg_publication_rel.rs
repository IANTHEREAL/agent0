use super::helpers::{int_array_col, int_col, text_col};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

/// Stub pg_publication_rel — no logical replication relation publications.
pub struct PgPublicationRel;

#[async_trait]
impl VirtualTable for PgPublicationRel {
    fn name(&self) -> &str {
        "pg_publication_rel"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_publication_rel",
            vec![
                int_col("prpubid"),
                int_col("prrelid"),
                text_col("prqual"),
                int_array_col("prattrs"),
            ],
        )
    }

    async fn scan(&self, _ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        Ok(Vec::new())
    }
}
