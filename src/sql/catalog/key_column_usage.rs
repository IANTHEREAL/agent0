use super::helpers::{int_col, int_val, null_val, split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::types::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct KeyColumnUsage;

#[async_trait]
impl VirtualTable for KeyColumnUsage {
    fn name(&self) -> &str {
        "key_column_usage"
    }

    fn schema_name(&self) -> &str {
        "information_schema"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "key_column_usage".to_string(),
            columns: vec![
                text_col("constraint_catalog"),
                text_col("constraint_schema"),
                text_col("constraint_name"),
                text_col("table_catalog"),
                text_col("table_schema"),
                text_col("table_name"),
                text_col("column_name"),
                int_col("ordinal_position"),
                int_col("position_in_unique_constraint"),
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
            if let Some(table_def) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            {
                if !table_def.pk_indices.is_empty() {
                    let pk_name = table_def
                        .pk_constraint_name
                        .clone()
                        .unwrap_or_else(|| format!("{}_pkey", table_name));
                    for (i, &col_idx) in table_def.pk_indices.iter().enumerate() {
                        let col_name = &table_def.columns[col_idx].name;
                        rows.push(Row::new(vec![
                            text_val(ctx.database_name),
                            text_val(&table_schema),
                            text_val(&pk_name),
                            text_val(ctx.database_name),
                            text_val(&table_schema),
                            text_val(&table_name),
                            text_val(col_name),
                            int_val((i + 1) as i64),
                            null_val(),
                        ]));
                    }
                }

                for idx in &table_def.indexes {
                    if idx.unique {
                        for (i, col_name) in idx.columns.iter().enumerate() {
                            rows.push(Row::new(vec![
                                text_val(ctx.database_name),
                                text_val(&table_schema),
                                text_val(&idx.name),
                                text_val(ctx.database_name),
                                text_val(&table_schema),
                                text_val(&table_name),
                                text_val(col_name),
                                int_val((i + 1) as i64),
                                null_val(),
                            ]));
                        }
                    }
                }

                for fk in &table_def.foreign_keys {
                    for (i, col_name) in fk.columns.iter().enumerate() {
                        rows.push(Row::new(vec![
                            text_val(ctx.database_name),
                            text_val(&table_schema),
                            text_val(&fk.name),
                            text_val(ctx.database_name),
                            text_val(&table_schema),
                            text_val(&table_name),
                            text_val(col_name),
                            int_val((i + 1) as i64),
                            int_val((i + 1) as i64),
                        ]));
                    }
                }
            }
        }

        Ok(rows)
    }
}
