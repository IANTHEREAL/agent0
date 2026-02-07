use super::helpers::{
    access_method_oid, bool_col, int_col, int_val, schema_oid, text_col, text_val,
};
use super::{ScanContext, VirtualTable};
use crate::sql::catalog_oids;
use crate::types::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgClass;

#[async_trait]
impl VirtualTable for PgClass {
    fn name(&self) -> &str {
        "pg_class"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_class".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("relname"),
                int_col("relnamespace"),
                text_col("relkind"),
                int_col("relowner"),
                int_col("relam"),
                int_col("reltuples"),
                int_col("relpages"),
                bool_col("relhasindex"),
                bool_col("relispopulated"),
                text_col("relreplident"),
                bool_col("relispartition"),
            ],
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
        let mut rows = Vec::new();

        for full_table_name in ctx.user_tables {
            let (table_schema, table_name) = super::helpers::split_schema_and_name(full_table_name);
            let namespace_oid = schema_oid(ctx.schema_oids, &table_schema);
            if let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            {
                let table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;
                let relhasindex = !schema.indexes.is_empty() || !schema.pk_indices.is_empty();
                rows.push(Row::new(vec![
                    int_val(table_oid),
                    text_val(&table_name),
                    int_val(namespace_oid),
                    text_val("r"),
                    int_val(10),
                    int_val(0),
                    int_val(0),
                    int_val(0),
                    Value::Boolean(relhasindex),
                    Value::Boolean(true),
                    text_val("d"),
                    Value::Boolean(false),
                ]));

                for idx in &schema.indexes {
                    let index_oid = catalog_oids::pg_class_index_oid(schema.table_id, idx.id)?;
                    rows.push(Row::new(vec![
                        int_val(index_oid),
                        text_val(&idx.name),
                        int_val(namespace_oid),
                        text_val("i"),
                        int_val(10),
                        int_val(access_method_oid(idx.method.as_deref())),
                        int_val(0),
                        int_val(0),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        text_val("d"),
                        Value::Boolean(false),
                    ]));
                }

                if !schema.pk_indices.is_empty() {
                    let pk_name = schema
                        .pk_constraint_name
                        .clone()
                        .unwrap_or_else(|| format!("{}_pkey", table_name));
                    let pk_oid = catalog_oids::pg_class_pk_index_oid(schema.table_id)?;
                    rows.push(Row::new(vec![
                        int_val(pk_oid),
                        text_val(&pk_name),
                        int_val(namespace_oid),
                        text_val("i"),
                        int_val(10),
                        int_val(403),
                        int_val(0),
                        int_val(0),
                        Value::Boolean(false),
                        Value::Boolean(true),
                        text_val("d"),
                        Value::Boolean(false),
                    ]));
                }
            }
        }

        let sequences = ctx.store.list_sequences(ctx.txn, ctx.db_id).await?;
        for seq in sequences {
            let seq_oid = catalog_oids::pg_class_sequence_oid(seq.oid);
            let namespace_oid = schema_oid(ctx.schema_oids, &seq.schema);
            rows.push(Row::new(vec![
                int_val(seq_oid),
                text_val(&seq.name),
                int_val(namespace_oid),
                text_val("S"),
                int_val(10),
                int_val(0),
                int_val(0),
                int_val(0),
                Value::Boolean(false),
                Value::Boolean(true),
                text_val("d"),
                Value::Boolean(false),
            ]));
        }

        let views = ctx
            .store
            .list_views(ctx.txn, ctx.db_id)
            .await
            .unwrap_or_default();
        for view_def in views {
            let namespace_oid = schema_oid(ctx.schema_oids, &view_def.schema);
            let view_oid = catalog_oids::pg_class_view_oid(view_def.oid);
            rows.push(Row::new(vec![
                int_val(view_oid),
                text_val(&view_def.name),
                int_val(namespace_oid),
                text_val("v"),
                int_val(10),
                int_val(0),
                int_val(0),
                int_val(0),
                Value::Boolean(false),
                Value::Boolean(true),
                text_val("d"),
                Value::Boolean(false),
            ]));
        }

        Ok(rows)
    }
}
