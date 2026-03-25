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
        TableSchema::virtual_table(
            "pg_range",
            vec![
                int_col("rngtypid"),
                int_col("rngsubtype"),
                int_col("rngcollation"),
                int_col("rngsubopc"),
                text_col("rngcanonical"),
                text_col("rngsubdiff"),
            ],
        )
    }

    async fn scan(&self, _ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        Ok(vec![])
    }
}
