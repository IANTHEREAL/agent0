use super::helpers::{
    bool_col, int_array_col, int_col, int_val, null_val, schema_oid, text_array_col, text_col,
    text_val, BOOTSTRAP_SUPERUSER_OID,
};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgExtension;

#[async_trait]
impl VirtualTable for PgExtension {
    fn name(&self) -> &str {
        "pg_extension"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_extension".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("extname"),
                int_col("extowner"),
                int_col("extnamespace"),
                bool_col("extrelocatable"),
                text_col("extversion"),
                int_array_col("extconfig"),
                text_array_col("extcondition"),
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

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut exts = ctx.store.list_extensions(ctx.txn, ctx.db_id).await?;
        exts.sort_by(|a, b| a.name.cmp(&b.name));

        let mut rows = Vec::new();
        for ext in exts {
            let oid = crate::extensions::descriptor(&ext.name)
                .map(|d| d.oid)
                .unwrap_or(0);
            let namespace_oid = schema_oid(ctx.schema_oids, &ext.schema);

            rows.push(Row::new(vec![
                int_val(oid),
                text_val(&ext.name),
                int_val(BOOTSTRAP_SUPERUSER_OID),
                int_val(namespace_oid),
                Value::Boolean(false),
                text_val(&ext.version),
                null_val(),
                null_val(),
            ]));
        }

        Ok(rows)
    }
}
