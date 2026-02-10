use super::helpers::{split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::types::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct TableConstraints;

#[async_trait]
impl VirtualTable for TableConstraints {
    fn name(&self) -> &str {
        "table_constraints"
    }

    fn schema_name(&self) -> &str {
        "information_schema"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "table_constraints".to_string(),
            columns: vec![
                text_col("constraint_catalog"),
                text_col("constraint_schema"),
                text_col("constraint_name"),
                text_col("table_catalog"),
                text_col("table_schema"),
                text_col("table_name"),
                text_col("constraint_type"),
                text_col("is_deferrable"),
                text_col("initially_deferred"),
                text_col("enforced"),
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
                    rows.push(Row::new(vec![
                        text_val(ctx.database_name),
                        text_val(&table_schema),
                        text_val(&pk_name),
                        text_val(ctx.database_name),
                        text_val(&table_schema),
                        text_val(&table_name),
                        text_val("PRIMARY KEY"),
                        text_val("NO"),
                        text_val("NO"),
                        text_val("YES"),
                    ]));
                }

                let mut seen_unique_constraints: std::collections::HashSet<String> =
                    std::collections::HashSet::new();

                for idx in &table_def.indexes {
                    if idx.unique {
                        seen_unique_constraints.insert(idx.name.clone());
                        rows.push(Row::new(vec![
                            text_val(ctx.database_name),
                            text_val(&table_schema),
                            text_val(&idx.name),
                            text_val(ctx.database_name),
                            text_val(&table_schema),
                            text_val(&table_name),
                            text_val("UNIQUE"),
                            text_val("NO"),
                            text_val("NO"),
                            text_val("YES"),
                        ]));
                    }
                }

                for col in &table_def.columns {
                    if col.unique && !col.primary_key {
                        let constraint_name = format!("{}_{}_key", table_name, col.name);
                        if seen_unique_constraints.contains(&constraint_name) {
                            continue;
                        }
                        rows.push(Row::new(vec![
                            text_val(ctx.database_name),
                            text_val(&table_schema),
                            text_val(&constraint_name),
                            text_val(ctx.database_name),
                            text_val(&table_schema),
                            text_val(&table_name),
                            text_val("UNIQUE"),
                            text_val("NO"),
                            text_val("NO"),
                            text_val("YES"),
                        ]));
                    }
                }

                for fk in &table_def.foreign_keys {
                    rows.push(Row::new(vec![
                        text_val(ctx.database_name),
                        text_val(&table_schema),
                        text_val(&fk.name),
                        text_val(ctx.database_name),
                        text_val(&table_schema),
                        text_val(&table_name),
                        text_val("FOREIGN KEY"),
                        text_val("NO"),
                        text_val("NO"),
                        text_val("YES"),
                    ]));
                }

                for (i, check) in table_def.check_constraints.iter().enumerate() {
                    let name = check
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("{}_check{}", table_name, i + 1));
                    rows.push(Row::new(vec![
                        text_val(ctx.database_name),
                        text_val(&table_schema),
                        text_val(&name),
                        text_val(ctx.database_name),
                        text_val(&table_schema),
                        text_val(&table_name),
                        text_val("CHECK"),
                        text_val("NO"),
                        text_val("NO"),
                        text_val("YES"),
                    ]));
                }
            }
        }

        Ok(rows)
    }
}
