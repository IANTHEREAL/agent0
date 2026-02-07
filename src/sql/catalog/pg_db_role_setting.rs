use super::helpers::{int_col, int_val, text_array_col};
use super::{ScanContext, VirtualTable};
use crate::sql::{catalog_oids, role_settings};
use crate::types::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgDbRoleSetting;

#[async_trait]
impl VirtualTable for PgDbRoleSetting {
    fn name(&self) -> &str {
        "pg_db_role_setting"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_db_role_setting".to_string(),
            columns: vec![
                int_col("setdatabase"),
                int_col("setrole"),
                text_array_col("setconfig"),
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
        let settings = role_settings::list_db_role_settings(ctx.txn).await?;
        let mut rows = Vec::with_capacity(settings.len());

        for setting in settings {
            let setconfig = Value::Array(
                setting
                    .setconfig
                    .into_iter()
                    .map(Value::Text)
                    .collect::<Vec<_>>(),
            );
            rows.push(Row::new(vec![
                int_val(i64::from(setting.database_oid)),
                int_val(catalog_oids::pg_role_oid(&setting.role_name)),
                setconfig,
            ]));
        }

        Ok(rows)
    }
}

