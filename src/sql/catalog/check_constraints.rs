use super::helpers::{split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct CheckConstraints;

#[async_trait]
impl VirtualTable for CheckConstraints {
    fn name(&self) -> &str {
        "check_constraints"
    }

    fn schema_name(&self) -> &str {
        "information_schema"
    }

    fn relkind(&self) -> &str {
        super::helpers::RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "check_constraints",
            vec![
                text_col("constraint_catalog"),
                text_col("constraint_schema"),
                text_col("constraint_name"),
                text_col("check_clause"),
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
                for (i, check) in table_def.check_constraints.iter().enumerate() {
                    let name = check
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("{}_check{}", table_name, i + 1));
                    rows.push(Row::new(vec![
                        text_val(ctx.database_name),
                        text_val(&table_schema),
                        text_val(&name),
                        text_val(&check.expr),
                    ]));
                }
            }
        }

        Ok(rows)
    }
}
