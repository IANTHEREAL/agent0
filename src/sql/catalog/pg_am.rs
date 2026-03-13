use super::helpers::{int_col, int_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgAm;

#[async_trait]
impl VirtualTable for PgAm {
    fn name(&self) -> &str {
        "pg_am"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_am".to_string(),
            columns: vec![int_col("oid"), text_col("amname")],
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
        Ok(vec![
            Row::new(vec![int_val(2), text_val("heap")]),
            Row::new(vec![int_val(403), text_val("btree")]),
            Row::new(vec![int_val(405), text_val("hash")]),
            Row::new(vec![int_val(783), text_val("gist")]),
            Row::new(vec![int_val(2742), text_val("gin")]),
            Row::new(vec![int_val(4000), text_val("spgist")]),
            Row::new(vec![int_val(3580), text_val("brin")]),
        ])
    }
}
