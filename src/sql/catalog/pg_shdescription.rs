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
        TableSchema {
            table_id: 0,
            name: "pg_shdescription".to_string(),
            columns: vec![
                int_col("objoid"),
                int_col("classoid"),
                text_col("description"),
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
        Ok(Vec::new())
    }
}
