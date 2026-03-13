use super::helpers::{split_schema_and_name, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::model::{Row, TableSchema, Value};
use anyhow::Result;
use async_trait::async_trait;

/// pg_policies — human-readable view of RLS policies (PostgreSQL-compatible).
///
/// Unlike pg_policy (which uses OIDs), this view returns role names directly
/// and spells out command types as readable strings.
pub struct PgPolicies;

#[async_trait]
impl VirtualTable for PgPolicies {
    fn name(&self) -> &str {
        "pg_policies"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn relkind(&self) -> &str {
        super::helpers::RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_policies".to_string(),
            columns: vec![
                text_col("schemaname"),
                text_col("tablename"),
                text_col("policyname"),
                text_col("permissive"),
                text_col("roles"),
                text_col("cmd"),
                text_col("qual"),
                text_col("with_check"),
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

        // Build table_id → (schema, name) lookup from user tables.
        let mut table_id_to_name: std::collections::HashMap<u64, (String, String)> =
            std::collections::HashMap::new();
        for full_table_name in ctx.user_tables {
            if let Some(schema) = ctx
                .store
                .get_schema(ctx.txn, ctx.db_id, full_table_name)
                .await?
            {
                let (schema_name, table_name) = split_schema_and_name(full_table_name);
                table_id_to_name.insert(schema.table_id, (schema_name, table_name));
            }
        }

        let mut rows = Vec::new();
        for policy in policies {
            let (schema_name, table_name) = match table_id_to_name.get(&policy.table_id) {
                Some((s, t)) => (s.as_str(), t.as_str()),
                None => continue, // table not visible
            };

            let permissive = if policy.permissive {
                "PERMISSIVE"
            } else {
                "RESTRICTIVE"
            };

            let roles = if policy.roles.is_empty() {
                "{public}".to_string()
            } else {
                format!(
                    "{{{}}}",
                    policy
                        .roles
                        .iter()
                        .map(|r| r.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            };

            let cmd = policy.command.pg_cmd_display();

            rows.push(Row::new(vec![
                text_val(schema_name),
                text_val(table_name),
                text_val(&policy.name),
                text_val(permissive),
                text_val(&roles),
                text_val(cmd),
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
