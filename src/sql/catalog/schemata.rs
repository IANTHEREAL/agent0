use super::helpers::{null_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct Schemata;

#[async_trait]
impl VirtualTable for Schemata {
    fn name(&self) -> &str {
        "schemata"
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
            name: "schemata".to_string(),
            columns: vec![
                text_col("catalog_name"),
                text_col("schema_name"),
                text_col("schema_owner"),
                text_col("default_character_set_catalog"),
                text_col("default_character_set_schema"),
                text_col("default_character_set_name"),
                text_col("sql_path"),
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
        Ok(ctx
            .schemas
            .iter()
            .map(|schema| {
                Row::new(vec![
                    text_val(ctx.database_name),
                    text_val(schema),
                    text_val("postgres"),
                    null_val(),
                    null_val(),
                    null_val(),
                    null_val(),
                ])
            })
            .collect())
    }
}
