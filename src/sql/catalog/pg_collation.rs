use super::helpers::{
    bool_col, int_col, int_val, null_val, schema_oid, text_col, text_val, BOOTSTRAP_SUPERUSER_OID,
};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgCollation;

#[async_trait]
impl VirtualTable for PgCollation {
    fn name(&self) -> &str {
        "pg_collation"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_collation".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("collname"),
                int_col("collnamespace"),
                int_col("collowner"),
                text_col("collprovider"),
                bool_col("collisdeterministic"),
                int_col("collencoding"),
                text_col("collcollate"),
                text_col("collctype"),
                text_col("colllocale"),
                text_col("collicurules"),
                text_col("collversion"),
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

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let pg_catalog_oid = schema_oid(ctx.schema_oids, "pg_catalog");
        Ok(vec![
            Row::new(vec![
                int_val(100),
                text_val("default"),
                int_val(pg_catalog_oid),
                int_val(BOOTSTRAP_SUPERUSER_OID),
                text_val("d"),
                Value::Boolean(true),
                int_val(-1),
                null_val(),
                null_val(),
                null_val(),
                null_val(),
                null_val(),
            ]),
            Row::new(vec![
                int_val(950),
                text_val("C"),
                int_val(pg_catalog_oid),
                int_val(BOOTSTRAP_SUPERUSER_OID),
                text_val("c"),
                Value::Boolean(true),
                int_val(-1),
                text_val("C"),
                text_val("C"),
                null_val(),
                null_val(),
                null_val(),
            ]),
            Row::new(vec![
                int_val(951),
                text_val("POSIX"),
                int_val(pg_catalog_oid),
                int_val(BOOTSTRAP_SUPERUSER_OID),
                text_val("c"),
                Value::Boolean(true),
                int_val(-1),
                text_val("POSIX"),
                text_val("POSIX"),
                null_val(),
                null_val(),
                null_val(),
            ]),
        ])
    }
}
