use super::helpers::{
    int_col, int_val, owner_role_oid, schema_oid, text_array_col, text_col, text_val,
};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
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
        TableSchema::virtual_table(
            "pg_namespace",
            vec![
                int_col("oid"),
                text_col("nspname"),
                int_col("nspowner"),
                // nspacl — access privileges; NULL = no explicit ACL (PG default).
                text_array_col("nspacl"),
            ],
        )
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let db_owner = ctx
            .store
            .get_database_by_id(ctx.txn, ctx.db_id)
            .await?
            .map(|db| db.owner)
            .unwrap_or_else(|| "postgres".to_string());

        Ok(ctx
            .schemas
            .iter()
            .map(|s| {
                let owner_oid = if matches!(
                    s.as_str(),
                    "pg_catalog" | "information_schema" | "extensions"
                ) {
                    crate::sql::catalog_oids::pg_role_oid("postgres")
                } else {
                    owner_role_oid(Some(&db_owner), ctx.current_user)
                };
                Row::new(vec![
                    int_val(schema_oid(ctx.schema_oids, s)),
                    text_val(s),
                    int_val(owner_oid),
                    Value::Null, // nspacl — NULL means default privileges (PG parity)
                ])
            })
            .collect())
    }
}
