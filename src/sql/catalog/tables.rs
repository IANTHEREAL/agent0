use super::helpers::{null_val, split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct Tables;

#[async_trait]
impl VirtualTable for Tables {
    fn name(&self) -> &str {
        "tables"
    }

    fn schema_name(&self) -> &str {
        "information_schema"
    }

    fn relkind(&self) -> &str {
        super::helpers::RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "tables".to_string(),
            columns: vec![
                text_col("table_catalog"),
                text_col("table_schema"),
                text_col("table_name"),
                text_col("table_type"),
                text_col("self_referencing_column_name"),
                text_col("reference_generation"),
                text_col("user_defined_type_catalog"),
                text_col("user_defined_type_schema"),
                text_col("user_defined_type_name"),
                text_col("is_insertable_into"),
                text_col("is_typed"),
                text_col("commit_action"),
                text_col("table_owner"),
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
            let schema = match ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            {
                Some(schema) => schema,
                None => continue,
            };
            let (table_schema, table_name) = split_schema_and_name(full_table_name);
            let owner = schema.owner;
            rows.push(Row::new(vec![
                text_val(ctx.database_name),
                text_val(&table_schema),
                text_val(&table_name),
                text_val("BASE TABLE"),
                null_val(),
                null_val(),
                null_val(),
                null_val(),
                null_val(),
                text_val("YES"),
                text_val("NO"),
                null_val(),
                Value::Text(owner),
            ]));
        }

        let views = ctx
            .store
            .list_views(ctx.txn, ctx.db_id)
            .await
            .unwrap_or_default();
        for view_def in views {
            rows.push(Row::new(vec![
                text_val(ctx.database_name),
                text_val(&view_def.schema),
                text_val(&view_def.name),
                text_val("VIEW"),
                null_val(),
                null_val(),
                null_val(),
                null_val(),
                null_val(),
                text_val("NO"),
                text_val("NO"),
                null_val(),
                text_val("postgres"),
            ]));
        }

        Ok(rows)
    }
}
