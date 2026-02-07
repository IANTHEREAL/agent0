use super::helpers::{int_col, int_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::sql::catalog_oids;
use crate::types::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

pub struct PgTrigger;

#[async_trait]
impl VirtualTable for PgTrigger {
    fn name(&self) -> &str {
        "pg_trigger"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_trigger".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("tgname"),
                int_col("tgrelid"),
                int_col("tgfoid"),
                text_col("tgenabled"),
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
        let mut table_oids: HashMap<String, i64> = HashMap::new();
        for table_name in ctx.user_tables {
            if let Some(schema) = ctx.store.get_schema(ctx.txn, ctx.db_id, table_name).await? {
                table_oids.insert(
                    table_name.to_string(),
                    catalog_oids::pg_class_table_oid(schema.table_id)?,
                );
            }
        }

        let mut func_oids: HashMap<String, i64> = HashMap::new();
        let funcs = ctx.store.list_functions(ctx.txn, ctx.db_id).await?;
        for f in funcs {
            func_oids.insert(
                format!("{}.{}", f.schema, f.name),
                catalog_oids::pg_proc_function_oid(f.oid),
            );
        }

        let mut triggers = ctx.store.list_triggers(ctx.txn, ctx.db_id).await?;
        triggers.sort_by_key(|t| t.oid);

        let mut rows = Vec::new();
        for t in triggers {
            let tgrelid = table_oids.get(&t.table).copied().unwrap_or(0);
            let tgfoid = func_oids.get(&t.function).copied().unwrap_or(0);

            rows.push(Row::new(vec![
                int_val(catalog_oids::pg_trigger_oid(t.oid)),
                text_val(&t.name),
                int_val(tgrelid),
                int_val(tgfoid),
                text_val("O"),
            ]));
        }

        Ok(rows)
    }
}
