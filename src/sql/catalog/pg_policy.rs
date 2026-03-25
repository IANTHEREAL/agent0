use super::helpers::{bool_col, int_array_col, int_col, int_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use crate::sql::catalog_oids;
use anyhow::Result;
use async_trait::async_trait;

/// pg_policy — returns real RLS policy data from TiKV.
pub struct PgPolicy;

#[async_trait]
impl VirtualTable for PgPolicy {
    fn name(&self) -> &str {
        "pg_policy"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_policy",
            vec![
                int_col("oid"),
                text_col("polname"),
                int_col("polrelid"),
                text_col("polcmd"),
                bool_col("polpermissive"),
                int_array_col("polroles"),
                text_col("polqual"),
                text_col("polwithcheck"),
            ],
        )
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let policies = ctx.store.list_all_policies(ctx.txn, ctx.db_id).await?;

        let mut rows = Vec::new();
        for policy in policies {
            let polrelid = catalog_oids::pg_class_table_oid(policy.table_id).unwrap_or(0);
            let polcmd = policy.command.pg_polcmd();

            // polroles: oid[] — array of role OIDs. 0 means PUBLIC.
            let role_oids: Vec<Value> = if policy.roles.is_empty()
                || (policy.roles.len() == 1 && policy.roles[0] == "public")
            {
                vec![Value::Int64(0)]
            } else {
                policy
                    .roles
                    .iter()
                    .map(|r| {
                        if r == "public" {
                            Value::Int64(0)
                        } else {
                            Value::Int64(catalog_oids::pg_role_oid(r))
                        }
                    })
                    .collect()
            };

            rows.push(Row::new(vec![
                int_val(policy.oid as i64),
                text_val(&policy.name),
                int_val(polrelid),
                text_val(polcmd),
                Value::Boolean(policy.permissive),
                Value::Array(role_oids),
                policy
                    .using_expr
                    .as_deref()
                    .map(text_val)
                    .unwrap_or(Value::Null),
                policy
                    .with_check_expr
                    .as_deref()
                    .map(text_val)
                    .unwrap_or(Value::Null),
            ]));
        }

        Ok(rows)
    }
}
