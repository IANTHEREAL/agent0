use super::helpers::{bool_col, int_col, int_val, null_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::auth::AuthManager;
use crate::model::{Row, TableSchema, Value};
use crate::sql::catalog_oids;
use anyhow::Result;
use async_trait::async_trait;

pub struct PgRoles;

#[async_trait]
impl VirtualTable for PgRoles {
    fn name(&self) -> &str {
        "pg_roles"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn schema(&self) -> TableSchema {
        TableSchema {
            table_id: 0,
            name: "pg_roles".to_string(),
            columns: vec![
                text_col("rolname"),
                bool_col("rolsuper"),
                bool_col("rolinherit"),
                bool_col("rolcreaterole"),
                bool_col("rolcreatedb"),
                bool_col("rolcanlogin"),
                bool_col("rolreplication"),
                int_col("rolconnlimit"),
                text_col("rolpassword"),
                text_col("rolvaliduntil"),
                bool_col("rolbypassrls"),
                text_col("rolconfig"),
                int_col("oid"),
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
        let auth_manager = AuthManager::new();
        let mut users = auth_manager.list_users(ctx.txn).await?;
        users.sort_by(|a, b| a.name.cmp(&b.name));

        let mut rows = Vec::with_capacity(users.len() + 1);
        rows.push(Row::new(vec![
            text_val("postgres"),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(false),
            int_val(-1),
            null_val(),
            null_val(),
            Value::Boolean(false),
            null_val(),
            int_val(catalog_oids::pg_role_oid("postgres")),
        ]));

        for user in users {
            if user.name.eq_ignore_ascii_case("admin") || user.name.eq_ignore_ascii_case("postgres")
            {
                continue;
            }

            rows.push(Row::new(vec![
                text_val(&user.name),
                Value::Boolean(user.is_superuser),
                Value::Boolean(true),
                Value::Boolean(user.can_create_role),
                Value::Boolean(user.can_create_db),
                Value::Boolean(user.can_login),
                Value::Boolean(false),
                int_val(i64::from(user.connection_limit)),
                null_val(),
                null_val(),
                Value::Boolean(false),
                null_val(),
                int_val(catalog_oids::pg_role_oid(&user.name)),
            ]));
        }

        Ok(rows)
    }
}
