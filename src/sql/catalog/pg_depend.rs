use super::helpers::{int_col, int_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use crate::sql::catalog_oids;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

pub struct PgDepend;

#[async_trait]
impl VirtualTable for PgDepend {
    fn name(&self) -> &str {
        "pg_depend"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_depend".to_string(),
            columns: vec![
                int_col("classid"),
                int_col("objid"),
                int_col("objsubid"),
                int_col("refclassid"),
                int_col("refobjid"),
                int_col("refobjsubid"),
                text_col("deptype"),
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
        // pg_class has OID 1259 in PostgreSQL; use the standard constant so ORMs can join if needed.
        const PG_CLASS_OID: i64 = 1259;
        // pg_proc catalog relation OID in PostgreSQL.
        const PG_PROC_OID: i64 = 1255;
        // pg_trigger catalog relation OID in PostgreSQL.
        const PG_TRIGGER_OID: i64 = 2620;

        let seqs = ctx.store.list_sequences(ctx.txn, ctx.db_id).await?;
        let mut rows = Vec::new();

        for seq in seqs {
            let Some((owned_table, owned_col)) = seq.owned_by.as_ref() else {
                continue;
            };
            let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, owned_table)
                .await?
            else {
                continue;
            };
            let Some(col_idx) = schema
                .columns
                .iter()
                .position(|c| !c.is_dropped && c.name == *owned_col)
            else {
                continue;
            };

            let seq_oid = catalog_oids::pg_class_sequence_oid(seq.oid);
            let table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;
            let refobjsubid = (col_idx + 1) as i64;

            rows.push(Row::new(vec![
                int_val(PG_CLASS_OID),
                int_val(seq_oid),
                int_val(0),
                int_val(PG_CLASS_OID),
                int_val(table_oid),
                int_val(refobjsubid),
                text_val("a"),
            ]));
        }

        let mut func_oids = HashMap::new();
        for func in ctx.store.list_functions(ctx.txn, ctx.db_id).await? {
            func_oids.insert(
                format!("{}.{}", func.schema, func.name),
                catalog_oids::pg_proc_function_oid(func.oid),
            );
        }

        for trigger in ctx.store.list_triggers(ctx.txn, ctx.db_id).await? {
            // TriggerDef.function is expected to be schema-qualified. Keep a fallback
            // for legacy unqualified payloads.
            let func_key = if crate::sql::names::parse_full_name(&trigger.function).is_ok() {
                trigger.function.clone()
            } else {
                format!("{}.{}", trigger.schema, trigger.function)
            };
            let Some(&func_oid) = func_oids.get(&func_key) else {
                continue;
            };
            rows.push(Row::new(vec![
                int_val(PG_TRIGGER_OID),
                int_val(catalog_oids::pg_trigger_oid(trigger.oid)),
                int_val(0),
                int_val(PG_PROC_OID),
                int_val(func_oid),
                int_val(0),
                text_val("n"),
            ]));
        }

        Ok(rows)
    }
}
