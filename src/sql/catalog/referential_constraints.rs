use super::helpers::{split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{ForeignKeyAction, Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;

pub struct ReferentialConstraints;

#[async_trait]
impl VirtualTable for ReferentialConstraints {
    fn name(&self) -> &str {
        "referential_constraints"
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
            name: "referential_constraints".to_string(),
            columns: vec![
                text_col("constraint_catalog"),
                text_col("constraint_schema"),
                text_col("constraint_name"),
                text_col("unique_constraint_catalog"),
                text_col("unique_constraint_schema"),
                text_col("unique_constraint_name"),
                text_col("match_option"),
                text_col("update_rule"),
                text_col("delete_rule"),
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
            let (table_schema, _) = split_schema_and_name(full_table_name);
            if let Some(table_def) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            {
                for fk in &table_def.foreign_keys {
                    let (ref_schema, ref_table_name) = split_schema_and_name(&fk.ref_table);
                    let ref_pk_name = ctx
                        .store
                        .get_schema(ctx.txn, ctx.db_id, &fk.ref_table)
                        .await?
                        .and_then(|s| s.pk_constraint_name)
                        .unwrap_or_else(|| format!("{}_pkey", ref_table_name));
                    let update_rule = fk_action_str(&fk.on_update);
                    let delete_rule = fk_action_str(&fk.on_delete);
                    rows.push(Row::new(vec![
                        text_val(ctx.database_name),
                        text_val(&table_schema),
                        text_val(&fk.name),
                        text_val(ctx.database_name),
                        text_val(&ref_schema),
                        text_val(&ref_pk_name),
                        text_val("NONE"),
                        text_val(update_rule),
                        text_val(delete_rule),
                    ]));
                }
            }
        }

        Ok(rows)
    }
}

fn fk_action_str(action: &ForeignKeyAction) -> &'static str {
    match action {
        ForeignKeyAction::Cascade => "CASCADE",
        ForeignKeyAction::SetNull => "SET NULL",
        ForeignKeyAction::SetDefault => "SET DEFAULT",
        ForeignKeyAction::Restrict => "RESTRICT",
        ForeignKeyAction::NoAction => "NO ACTION",
    }
}
