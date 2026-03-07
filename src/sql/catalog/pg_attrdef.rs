use super::helpers::{int_col, int_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema};
use crate::sql::sequences::SerialDefaultBehavior;
use crate::sql::{catalog_oids, sequences};
use anyhow::Result;
use async_trait::async_trait;

pub struct PgAttrdef;

#[async_trait]
impl VirtualTable for PgAttrdef {
    fn name(&self) -> &str {
        "pg_attrdef"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_attrdef".to_string(),
            columns: vec![
                int_col("oid"),
                int_col("adrelid"),
                int_col("adnum"),
                text_col("adbin"),
                text_col("adsrc"),
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        let sequence_defs = ctx.store.list_sequences(ctx.txn, ctx.db_id).await?;

        for full_table_name in ctx.user_tables {
            let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            else {
                continue;
            };
            let table_oid = catalog_oids::pg_class_table_oid(schema.table_id)?;

            for (i, col) in schema.columns.iter().enumerate() {
                let expr = if col.is_serial {
                    match sequences::classify_serial_default(col.default_expr.as_deref()) {
                        SerialDefaultBehavior::ExplicitExpr(expr) => Some(expr.to_string()),
                        SerialDefaultBehavior::ExplicitNull => None,
                        SerialDefaultBehavior::ImplicitSequence => {
                            let seq_full_name = sequences::serial_column_sequence_full_name(
                                &sequence_defs,
                                full_table_name,
                                &col.name,
                            )?;
                            Some(format!("nextval('{}'::regclass)", seq_full_name))
                        }
                    }
                } else {
                    col.default_expr.clone()
                };
                let Some(expr) = expr else {
                    continue;
                };
                let attnum = (i + 1) as i64;
                let oid = catalog_oids::pg_attrdef_oid(schema.table_id, (i + 1) as u32)?;

                rows.push(Row::new(vec![
                    int_val(oid),
                    int_val(table_oid),
                    int_val(attnum),
                    text_val(&expr),
                    text_val(&expr),
                ]));
            }
        }

        Ok(rows)
    }
}
