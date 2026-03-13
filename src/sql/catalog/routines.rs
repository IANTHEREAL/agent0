use super::helpers::{text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct Routines;

#[async_trait]
impl VirtualTable for Routines {
    fn name(&self) -> &str {
        "routines"
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
            name: "routines".to_string(),
            columns: vec![
                text_col("routine_catalog"),
                text_col("routine_schema"),
                text_col("routine_name"),
                text_col("routine_type"),
                text_col("data_type"),
                text_col("routine_owner"),
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
        let mut funcs = ctx.store.list_functions(ctx.txn, ctx.db_id).await?;
        funcs.sort_by(|a, b| a.schema.cmp(&b.schema).then(a.name.cmp(&b.name)));

        let mut rows = Vec::with_capacity(funcs.len());
        for func in funcs {
            rows.push(Row::new(vec![
                text_val(ctx.database_name),
                text_val(&func.schema),
                text_val(&func.name),
                text_val("FUNCTION"),
                text_val(&func.return_type),
                text_val(&func.owner),
            ]));
        }

        Ok(rows)
    }
}
