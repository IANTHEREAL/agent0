use super::helpers::{text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::types::{Row, TableSchema};
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

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_views".to_string(),
            columns: vec![
                text_col("schemaname"),
                text_col("viewname"),
                text_col("viewowner"),
                text_col("definition"),
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
        let mut views = ctx.store.list_views(ctx.txn, ctx.db_id).await?;
        views.sort_by(|a, b| a.full_name().cmp(&b.full_name()));

        let mut rows = Vec::new();
        for view_def in views {
            rows.push(Row::new(vec![
                text_val(&view_def.schema),
                text_val(&view_def.name),
                text_val("postgres"),
                text_val(&view_def.query),
            ]));
        }

        Ok(rows)
    }
}
