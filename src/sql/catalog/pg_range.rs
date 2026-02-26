use super::helpers::{int_col, text_col};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgRange;

#[async_trait]
impl VirtualTable for PgRange {
    fn name(&self) -> &str {
        "pg_range"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_range".to_string(),
            columns: vec![
                int_col("rngtypid"),
                int_col("rngsubtype"),
                int_col("rngcollation"),
                int_col("rngsubopc"),
                text_col("rngcanonical"),
                text_col("rngsubdiff"),
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
        Ok(vec![])
    }
}
