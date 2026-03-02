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
            ]));
        }

        const HSTORE_OID: i64 = 16386;
        const HSTORE_ARRAY_OID: i64 = 16387;
        let public_oid = schema_oid(ctx.schema_oids, "public");

        rows.push(Row::new(vec![
            int_val(HSTORE_ARRAY_OID),
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
            int_val(HSTORE_OID),
            int_val(0),
            int_val(0),
        ]));

        rows.push(Row::new(vec![
            int_val(HSTORE_OID),
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
            int_val(HSTORE_ARRAY_OID),
            int_val(0),
        ]));

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
            ]));
        }

        Ok(rows)
    }
}
