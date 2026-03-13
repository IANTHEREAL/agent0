use super::helpers::{int_col, int_val, schema_oid, text_col, text_val, BOOTSTRAP_SUPERUSER_OID};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, UserTypeKind};
use crate::sql::pg_types;
use anyhow::Result;
use async_trait::async_trait;

pub struct PgType;

#[async_trait]
impl VirtualTable for PgType {
    fn name(&self) -> &str {
        "pg_type"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_type".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("typname"),
                int_col("typnamespace"),
                int_col("typowner"),
                int_col("typlen"),
                text_col("typbyval"),
                text_col("typtype"),
                text_col("typcategory"),
                text_col("typispreferred"),
                text_col("typisdefined"),
                text_col("typdelim"),
                int_col("typrelid"),
                int_col("typelem"),
                int_col("typarray"),
                int_col("typcollation"),
                int_col("typbasetype"),
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
        let mut rows = Vec::new();
        let pg_catalog_oid = schema_oid(ctx.schema_oids, "pg_catalog");

        for &pg_types::BuiltinPgType {
            oid,
            typname,
            typlen,
            typbyval,
            typtype,
            typcategory,
            typcollation,
        } in pg_types::BUILTIN_PG_TYPES
        {
            rows.push(Row::new(vec![
                int_val(oid),
                text_val(typname),
                int_val(pg_catalog_oid),
                int_val(BOOTSTRAP_SUPERUSER_OID),
                int_val(typlen as i64),
                text_val(typbyval),
                text_val(typtype),
                text_val(typcategory),
                text_val("f"),
                text_val("t"),
                text_val(","),
                int_val(0),
                int_val(0),
                int_val(0),
                int_val(typcollation),
                int_val(0), // typbasetype: 0 for non-domain types
            ]));
        }

        let public_oid = schema_oid(ctx.schema_oids, "public");
        let has_hstore = ctx
            .store
            .get_extension(ctx.txn, ctx.db_id, "hstore")
            .await?
            .map(|ext| ext.enabled)
            .unwrap_or(false);
        if has_hstore {
            rows.push(Row::new(vec![
                int_val(pg_types::OID_HSTORE_ARRAY),
                text_val("_hstore"),
                int_val(public_oid),
                int_val(BOOTSTRAP_SUPERUSER_OID),
                int_val(-1),
                text_val("f"),
                text_val("b"),
                text_val("A"),
                text_val("f"),
                text_val("t"),
                text_val(","),
                int_val(0),
                int_val(pg_types::OID_HSTORE),
                int_val(0),
                int_val(0),
                int_val(0), // typbasetype
            ]));

            rows.push(Row::new(vec![
                int_val(pg_types::OID_HSTORE),
                text_val("hstore"),
                int_val(public_oid),
                int_val(BOOTSTRAP_SUPERUSER_OID),
                int_val(-1),
                text_val("f"),
                text_val("b"),
                text_val("U"),
                text_val("f"),
                text_val("t"),
                text_val(","),
                int_val(0),
                int_val(0),
                int_val(pg_types::OID_HSTORE_ARRAY),
                int_val(0),
                int_val(0), // typbasetype
            ]));
        }

        let mut user_types = ctx.store.list_types(ctx.txn, ctx.db_id).await?;
        user_types.sort_by_key(|t| t.oid);
        for def in user_types {
            let (typlen, typbyval, typtype, typcategory) = match def.kind {
                UserTypeKind::Enum { .. } => (4, "t", "e", "E"),
                UserTypeKind::Composite { .. } => (-1, "f", "c", "C"),
            };

            rows.push(Row::new(vec![
                int_val(def.oid as i64),
                text_val(&def.name),
                int_val(schema_oid(ctx.schema_oids, &def.schema)),
                int_val(BOOTSTRAP_SUPERUSER_OID),
                int_val(typlen),
                text_val(typbyval),
                text_val(typtype),
                text_val(typcategory),
                text_val("f"),
                text_val("t"),
                text_val(","),
                int_val(0),
                int_val(0),
                int_val(0),
                int_val(0),
                int_val(0), // typbasetype
            ]));
        }

        Ok(rows)
    }
}
