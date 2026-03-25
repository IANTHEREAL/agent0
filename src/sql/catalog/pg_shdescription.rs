use super::helpers::{int_col, text_col};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

/// `pg_catalog.pg_shdescription` — shared object descriptions.
///
/// PostgreSQL stores comments for shared objects (databases, roles,
/// tablespaces) here. db9 does not persist shared descriptions, so this
/// table always returns empty results. Its presence prevents errors when
/// JetBrains DataGrip LEFT JOINs against it during introspection.
pub struct PgShdescription;

#[async_trait]
impl VirtualTable for PgShdescription {
    fn name(&self) -> &str {
        "pg_shdescription"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_shdescription",
            vec![
                int_col("objoid"),
                int_col("classoid"),
                text_col("description"),
            ],
        )
    }

    async fn scan(&self, _ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        Ok(Vec::new())
    }
}
