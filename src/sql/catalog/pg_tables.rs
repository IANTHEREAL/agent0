use super::helpers::{bool_col, null_val, split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

pub struct PgTables;

#[async_trait]
impl VirtualTable for PgTables {
    fn name(&self) -> &str {
        "pg_tables"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_tables".to_string(),
            columns: vec![
                text_col("schemaname"),
                text_col("tablename"),
                text_col("tableowner"),
                text_col("tablespace"),
                bool_col("hasindexes"),
                bool_col("hasrules"),
                bool_col("hastriggers"),
                bool_col("rowsecurity"),
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
        let triggers = ctx
            .store
            .list_triggers(ctx.txn, ctx.db_id)
            .await
            .unwrap_or_default();
        let mut tables_with_triggers: HashMap<String, bool> = HashMap::new();
        for t in triggers {
            tables_with_triggers.insert(t.table, true);
        }

        let mut rows = Vec::new();
        for full_table_name in ctx.user_tables {
            let (table_schema, table_name) = split_schema_and_name(full_table_name);
            let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            else {
                continue;
            };
            let hasindexes = !schema.pk_indices.is_empty() || !schema.indexes.is_empty();
            let hastriggers = tables_with_triggers
                .get(full_table_name.as_str())
                .copied()
                .unwrap_or(false);

            rows.push(Row::new(vec![
                text_val(&table_schema),
                text_val(&table_name),
                text_val(&schema.owner),
                null_val(),
                Value::Boolean(hasindexes),
                Value::Boolean(false),
                Value::Boolean(hastriggers),
                Value::Boolean(false),
            ]));
        }

        Ok(rows)
    }
}
