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
        TableSchema {
            table_id: 0,
            name: "pg_publication_namespace".to_string(),
            columns: vec![int_col("pnpubid"), int_col("pnnspid")],
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
