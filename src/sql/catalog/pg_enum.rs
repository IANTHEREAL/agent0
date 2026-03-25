use super::helpers::{float_col, float_val, int_col, int_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, UserTypeKind};
use anyhow::{anyhow, Result};
use async_trait::async_trait;

pub struct PgEnum;

#[async_trait]
impl VirtualTable for PgEnum {
    fn name(&self) -> &str {
        "pg_enum"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_enum",
            vec![
                int_col("oid"),
                int_col("enumtypid"),
                float_col("enumsortorder"),
                text_col("enumlabel"),
            ],
        )
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut rows = Vec::new();

        let mut user_types = ctx.store.list_types(ctx.txn, ctx.db_id).await?;
        user_types.sort_by_key(|t| t.oid);

        for def in user_types {
            let UserTypeKind::Enum { labels } = def.kind else {
                continue;
            };

            for (i, label) in labels.iter().enumerate() {
                let enum_oid = (def.oid as i64)
                    .checked_mul(1_000_000)
                    .and_then(|v| v.checked_add(i as i64 + 1))
                    .ok_or_else(|| anyhow!("pg_enum oid overflow"))?;

                rows.push(Row::new(vec![
                    int_val(enum_oid),
                    int_val(def.oid as i64),
                    float_val((i + 1) as f64),
                    text_val(label),
                ]));
            }
        }

        Ok(rows)
    }
}
