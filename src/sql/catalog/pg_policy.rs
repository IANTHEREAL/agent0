use super::helpers::{bool_col, int_array_col, int_col, text_col};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

/// Stub pg_policy — db9-server does not support row-level security policies,
/// but psql `\d` queries this table unconditionally. Returns empty rows.
pub struct PgPolicy;

#[async_trait]
impl VirtualTable for PgPolicy {
    fn name(&self) -> &str {
        "pg_policy"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_policy".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("polname"),
                int_col("polrelid"),
                text_col("polcmd"),
                bool_col("polpermissive"),
                int_array_col("polroles"),
                text_col("polqual"),
                text_col("polwithcheck"),
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
        // No RLS policies — always empty.
        Ok(Vec::new())
    }
}
