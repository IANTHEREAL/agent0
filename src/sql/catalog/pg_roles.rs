use super::helpers::{bool_col, int_col, int_val, null_val, text_col, text_val};
use super::{ScanContext, VirtualTable};
use crate::auth::AuthManager;
use crate::model::{Row, TableSchema, Value};
use crate::sql::catalog_oids;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashSet;

pub struct PgRoles;

#[async_trait]
impl VirtualTable for PgRoles {
    fn name(&self) -> &str {
        "pg_roles"
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
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let auth_manager = AuthManager::new();
        let mut users = auth_manager.list_users(ctx.txn).await?;
        users.sort_by(|a, b| a.name.cmp(&b.name));
        let mut roles = auth_manager.list_roles(ctx.txn).await?;
        roles.sort_by(|a, b| a.name.cmp(&b.name));

        fn role_row(
            name: &str,
            is_superuser: bool,
            can_create_role: bool,
            can_create_db: bool,
            can_login: bool,
            bypass_rls: bool,
        ) -> Row {
            Row::new(vec![
                text_val(name),
                Value::Boolean(is_superuser),
                Value::Boolean(true),
                Value::Boolean(can_create_role),
                Value::Boolean(can_create_db),
                Value::Boolean(can_login),
                Value::Boolean(false),
                int_val(-1),
                null_val(),
                null_val(),
                Value::Boolean(bypass_rls),
                null_val(),
                int_val(catalog_oids::pg_role_oid(name)),
            ])
        }

        let mut seen = HashSet::new();
        let mut rows = Vec::with_capacity(users.len() + roles.len() + 2);
        rows.push(role_row("postgres", true, true, true, true, true));
        seen.insert("postgres".to_string());

        for user in users {
            let lower = user.name.to_ascii_lowercase();
            if seen.contains(&lower) {
                continue;
            }
            rows.push(role_row(
                &user.name,
                user.is_superuser,
                user.can_create_role,
                user.can_create_db,
                user.can_login,
                user.bypass_rls,
            ));
            seen.insert(lower);
        }

        for role in roles {
            let lower = role.name.to_ascii_lowercase();
            if seen.contains(&lower) {
                continue;
            }
            rows.push(role_row(
                &role.name,
                role.is_superuser,
                role.can_create_role,
                role.can_create_db,
                false,
                role.bypass_rls,
            ));
            seen.insert(lower);
        }

        let current_user_lower = ctx.current_user.to_ascii_lowercase();
        if !seen.contains(&current_user_lower) {
            rows.push(role_row(
                ctx.current_user,
                ctx.is_superuser,
                false,
                false,
                true,
                false,
            ));
        }

        Ok(rows)
    }
}
