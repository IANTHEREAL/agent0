use super::helpers::{int_col, int_val, schema_oid, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::types::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgNamespace;

#[async_trait]
impl VirtualTable for PgNamespace {
    fn name(&self) -> &str {
        "pg_namespace"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_namespace".to_string(),
            columns: vec![int_col("oid"), text_col("nspname"), int_col("nspowner")],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        Ok(ctx
            .schemas
            .iter()
            .map(|s| {
                Row::new(vec![
                    int_val(schema_oid(ctx.schema_oids, s)),
                    text_val(s),
                    int_val(10),
                ])
            })
            .collect())
    }
}
