use super::helpers::{int_col, name_col, text_array_col};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

/// Stub pg_statistic_ext — db9-server does not maintain extended statistics.
/// Needed so psql `\d` introspection queries can run without relation-not-found.
pub struct PgStatisticExt;

#[async_trait]
impl VirtualTable for PgStatisticExt {
    fn name(&self) -> &str {
        "pg_statistic_ext"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_statistic_ext".to_string(),
            columns: vec![
                int_col("oid"),
                int_col("stxrelid"),
                int_col("stxnamespace"),
                name_col("stxname"),
                text_array_col("stxkind"),
                int_col("stxstattarget"),
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
