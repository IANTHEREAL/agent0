use super::helpers::{text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgViews;

#[async_trait]
impl VirtualTable for PgViews {
    fn name(&self) -> &str {
        "pg_views"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn relkind(&self) -> &str {
        super::helpers::RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_views",
            vec![
                text_col("schemaname"),
                text_col("viewname"),
                text_col("viewowner"),
                text_col("definition"),
            ],
        )
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut views = ctx.store.list_views(ctx.txn, ctx.db_id).await?;
        views.sort_by_key(|v| v.full_name());

        let mut rows = Vec::new();
        for view_def in views {
            rows.push(Row::new(vec![
                text_val(&view_def.schema),
                text_val(&view_def.name),
                text_val(&view_def.owner),
                text_val(&view_def.query),
            ]));
        }

        Ok(rows)
    }
}
