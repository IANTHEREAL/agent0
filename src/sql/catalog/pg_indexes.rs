use super::helpers::{format_indexdef, null_val, split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::types::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgIndexes;

#[async_trait]
impl VirtualTable for PgIndexes {
    fn name(&self) -> &str {
        "pg_indexes"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_indexes".to_string(),
            columns: vec![
                text_col("schemaname"),
                text_col("tablename"),
                text_col("indexname"),
                text_col("tablespace"),
                text_col("indexdef"),
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
            let (table_schema, table_name) = split_schema_and_name(full_table_name);
            if let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            {
                if !schema.pk_indices.is_empty() {
                    let pk_name = schema
                        .pk_constraint_name
                        .clone()
                        .unwrap_or_else(|| format!("{}_pkey", table_name));
                    let pk_cols: Vec<String> = schema
                        .pk_indices
                        .iter()
                        .filter_map(|idx| schema.columns.get(*idx).map(|c| c.name.clone()))
                        .collect();
                    let indexdef = format!(
                        "CREATE UNIQUE INDEX {} ON {}.{} USING btree ({})",
                        pk_name,
                        table_schema,
                        table_name,
                        pk_cols.join(", ")
                    );
                    rows.push(Row::new(vec![
                        text_val(&table_schema),
                        text_val(&table_name),
                        text_val(&pk_name),
                        null_val(),
                        text_val(&indexdef),
                    ]));
                }

                for idx in &schema.indexes {
                    let indexdef = format_indexdef(&table_schema, &table_name, idx);
                    rows.push(Row::new(vec![
                        text_val(&table_schema),
                        text_val(&table_name),
                        text_val(&idx.name),
                        null_val(),
                        text_val(&indexdef),
                    ]));
                }
            }
        }

        Ok(rows)
    }
}
