use super::helpers::{bool_col, int_col, int_val, text_col, text_val};
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
        TableSchema {
            table_id: 0,
            name: "pg_policy".to_string(),
            columns: vec![
                int_col("oid"),
                text_col("polname"),
                int_col("polrelid"),
                text_col("polcmd"),
                bool_col("polpermissive"),
                text_col("polroles"),
                text_col("polqual"),
                text_col("polwithcheck"),
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
        let policies = ctx.store.list_all_policies(ctx.txn, ctx.db_id).await?;

        let mut rows = Vec::new();
        for policy in policies {
            let polrelid = catalog_oids::pg_class_table_oid(policy.table_id).unwrap_or(0);
            let polcmd = policy.command.pg_polcmd();

            // polroles: array of role OIDs. We use 0 for PUBLIC.
            let role_oids_str = if policy.roles.is_empty()
                || (policy.roles.len() == 1 && policy.roles[0] == "public")
            {
                "{0}".to_string()
            } else {
                // For named roles, use a hash-based OID (consistent with pg_authid stub)
                let oids: Vec<String> = policy
                    .roles
                    .iter()
                    .map(|r| {
                        if r == "public" {
                            "0".to_string()
                        } else {
                            // Simple hash to generate a stable OID
                            let hash = r
                                .bytes()
                                .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
                            (hash as i64 + 10000).to_string()
                        }
                    })
                    .collect();
                format!("{{{}}}", oids.join(","))
            };

            rows.push(Row::new(vec![
                int_val(policy.oid as i64),
                text_val(&policy.name),
                int_val(polrelid),
                text_val(polcmd),
                Value::Boolean(policy.permissive),
                text_val(&role_oids_str), // polroles as text representation of int[]
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
