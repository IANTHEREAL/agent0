use super::helpers::{is_unique_constraint_index, split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

/// Render a deferrability flag as the SQL standard `'YES'` / `'NO'` literal
/// used by `information_schema.table_constraints`.
fn yes_no(flag: bool) -> &'static str {
    if flag {
        "YES"
    } else {
        "NO"
    }
}

pub struct TableConstraints;

#[async_trait]
impl VirtualTable for TableConstraints {
    fn name(&self) -> &str {
        "table_constraints"
    }

    fn schema_name(&self) -> &str {
        "information_schema"
    }

    fn relkind(&self) -> &str {
        super::helpers::RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "table_constraints",
            vec![
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
        )
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
                        text_val(yes_no(table_def.pk_deferrable)),
                        text_val(yes_no(table_def.pk_initially_deferred)),
                        text_val("YES"),
                    ]));
                }

                for idx in &table_def.indexes {
                    if is_unique_constraint_index(idx) {
                        rows.push(Row::new(vec![
                            text_val(ctx.database_name),
                            text_val(&table_schema),
                            text_val(&idx.name),
                            text_val(ctx.database_name),
                            text_val(&table_schema),
                            text_val(&table_name),
                            text_val("UNIQUE"),
                            text_val(yes_no(idx.deferrable)),
                            text_val(yes_no(idx.initially_deferred)),
                            text_val("YES"),
                        ]));
                    }
                }

                for col in &table_def.columns {
                    if col.unique && !col.primary_key {
                        // A column-level UNIQUE backed by an explicit (possibly
                        // named) unique-constraint index is already emitted by the
                        // loop above. Synthesizing a default `{table}_{col}_key`
                        // row here would duplicate it under the wrong name — PG
                        // surfaces only the real constraint. (#2683)
                        let backed_by_index = table_def.indexes.iter().any(|idx| {
                            is_unique_constraint_index(idx)
                                && idx.columns.len() == 1
                                && idx.columns[0] == col.name
                        });
                        if backed_by_index {
                            continue;
                        }
                        let constraint_name = format!("{}_{}_key", table_name, col.name);
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
                        text_val(yes_no(fk.deferrable)),
                        text_val(yes_no(fk.initially_deferred)),
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
