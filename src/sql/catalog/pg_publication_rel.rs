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
        TableSchema {
            table_id: 0,
            name: "pg_publication_rel".to_string(),
            columns: vec![
                int_col("prpubid"),
                int_col("prrelid"),
                text_col("prqual"),
                int_array_col("prattrs"),
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    async fn scan(&self, _ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        Ok(Vec::new())
    }
}
