use super::helpers::int_col;
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

/// Stub pg_publication_namespace — no logical replication namespace publications.
pub struct PgPublicationNamespace;

#[async_trait]
impl VirtualTable for PgPublicationNamespace {
    fn name(&self) -> &str {
        "pg_publication_namespace"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_publication_namespace",
            vec![int_col("pnpubid"), int_col("pnnspid")],
        )
    }

    async fn scan(&self, _ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        Ok(Vec::new())
    }
}
