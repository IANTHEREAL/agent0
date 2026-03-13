use super::helpers::{int_col, int_val, split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use crate::sql::catalog_oids;
use anyhow::Result;
use async_trait::async_trait;

/// Minimal pg_stat_user_tables implementation:
/// one row per user table with zeroed counters.
pub struct PgStatUserTables;

#[async_trait]
impl VirtualTable for PgStatUserTables {
    fn name(&self) -> &str {
        "pg_stat_user_tables"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn relkind(&self) -> &str {
        super::helpers::RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_stat_user_tables".to_string(),
            columns: vec![
                int_col("relid"),
                text_col("schemaname"),
                text_col("relname"),
                int_col("seq_scan"),
                int_col("seq_tup_read"),
                int_col("idx_scan"),
                int_col("idx_tup_fetch"),
                int_col("n_tup_ins"),
                int_col("n_tup_upd"),
                int_col("n_tup_del"),
                int_col("n_live_tup"),
                int_col("n_dead_tup"),
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
        for full_table_name in ctx.user_tables {
            let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            else {
                continue;
            };
            let (schemaname, relname) = split_schema_and_name(full_table_name);
            rows.push(Row::new(vec![
                int_val(catalog_oids::pg_class_table_oid(schema.table_id)?),
                text_val(&schemaname),
                text_val(&relname),
                int_val(0),
                int_val(0),
                int_val(0),
                int_val(0),
                int_val(0),
                int_val(0),
                int_val(0),
                int_val(0),
                int_val(0),
            ]));
        }
        Ok(rows)
    }
}
