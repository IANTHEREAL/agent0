use super::helpers::{int_col, int_val, text_col};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use crate::sql::catalog_oids;
use crate::storage::CommentTarget;
use anyhow::Result;
use async_trait::async_trait;

pub struct PgDescription;

const PG_CLASS_OID: i64 = 1259;
const PG_PROC_OID: i64 = 1255;
const PG_EXTENSION_OID: i64 = 3079;

#[async_trait]
impl VirtualTable for PgDescription {
    fn name(&self) -> &str {
        "pg_description"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_description".to_string(),
            columns: vec![
                int_col("objoid"),
                int_col("classoid"),
                int_col("objsubid"),
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

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let comments = ctx.store.list_comments(ctx.txn, ctx.db_id).await?;
        let mut rows = Vec::new();

        for rec in comments {
            let target = rec.target;
            let description = Value::Text(rec.description);

            match target {
                CommentTarget::Extension { name } => {
                    if ctx
                        .store
                        .get_extension(ctx.txn, ctx.db_id, &name)
                        .await?
                        .is_none()
                    {
                        continue;
                    }
                    let objoid = crate::extensions::descriptor(&name)
                        .map(|d| d.oid)
                        .unwrap_or(0);
                    rows.push(Row::new(vec![
                        int_val(objoid),
                        int_val(PG_EXTENSION_OID),
                        int_val(0),
                        description,
                    ]));
                }
                CommentTarget::Function { full_name } => {
                    let Some(def) = ctx
                        .store
                        .get_function(ctx.txn, ctx.db_id, &full_name)
                        .await?
                    else {
                        continue;
                    };
                    let oid = catalog_oids::pg_proc_function_oid(def.oid);
                    rows.push(Row::new(vec![
                        int_val(oid),
                        int_val(PG_PROC_OID),
                        int_val(0),
                        description,
                    ]));
                }
                CommentTarget::Table { full_name } => {
                    let Some(schema) = ctx.store.get_schema(ctx.txn, ctx.db_id, &full_name).await?
                    else {
                        continue;
                    };
                    let oid = catalog_oids::pg_class_table_oid(schema.table_id)?;
                    rows.push(Row::new(vec![
                        int_val(oid),
                        int_val(PG_CLASS_OID),
                        int_val(0),
                        description,
                    ]));
                }
                CommentTarget::Column {
                    table_full_name,
                    column_name,
                } => {
                    let Some(schema) = ctx
                        .store
                        .get_schema(ctx.txn, ctx.db_id, &table_full_name)
                        .await?
                    else {
                        continue;
                    };
                    let Some(col_idx) = schema.column_index(&column_name) else {
                        continue;
                    };
                    let oid = catalog_oids::pg_class_table_oid(schema.table_id)?;
                    rows.push(Row::new(vec![
                        int_val(oid),
                        int_val(PG_CLASS_OID),
                        int_val((col_idx + 1) as i64),
                        description,
                    ]));
                }
            }
        }

        Ok(rows)
    }
}
