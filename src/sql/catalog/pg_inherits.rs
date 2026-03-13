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
        TableSchema {
            table_id: 0,
            name: "pg_inherits".to_string(),
            columns: vec![
                int_col("inhrelid"),
                int_col("inhparent"),
                int_col("inhseqno"),
                bool_col("inhdetachpending"),
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
