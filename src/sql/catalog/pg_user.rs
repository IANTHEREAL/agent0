use super::helpers::{
    bool_col, int_col, int_val, null_val, text_array_col, text_col, text_val, timestamptz_col,
};
use super::{ScanContext, VirtualTable};
use crate::auth::AuthManager;
use crate::model::{Row, TableSchema, Value};
use crate::sql::{catalog_oids, role_settings};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

pub struct PgUser;

#[async_trait]
impl VirtualTable for PgUser {
    fn name(&self) -> &str {
        "pg_user"
    }

    fn schema_name(&self) -> &str {
        "pg_catalog"
    }

    fn relkind(&self) -> &str {
        super::helpers::RELKIND_VIEW
    }

    fn schema(&self) -> TableSchema {
        TableSchema::virtual_table(
            "pg_user",
            vec![
                text_col("usename"),
                int_col("usesysid"),
                bool_col("usecreatedb"),
                bool_col("usesuper"),
                bool_col("userepl"),
                bool_col("usebypassrls"),
                text_col("passwd"),
                timestamptz_col("valuntil"),
                text_array_col("useconfig"),
            ],
        )
    }

    async fn scan(&self, ctx: &mut ScanContext<'_>) -> Result<Vec<Row>> {
        let auth_manager = AuthManager::new();

        let mut global_configs: HashMap<String, Vec<String>> = HashMap::new();
        for setting in role_settings::list_db_role_settings(ctx.txn).await? {
            if setting.database_oid != 0 || setting.setconfig.is_empty() {
                continue;
            }
            global_configs.insert(setting.role_name.to_ascii_lowercase(), setting.setconfig);
        }

        fn user_row(
            name: &str,
            is_superuser: bool,
            can_create_db: bool,
            config: Option<&Vec<String>>,
        ) -> Row {
            let useconfig = match config {
                Some(cfg) if !cfg.is_empty() => {
                    Value::Array(cfg.iter().cloned().map(Value::Text).collect())
                }
                _ => null_val(),
            };
            Row::new(vec![
                text_val(name),
                int_val(catalog_oids::pg_role_oid(name)),
                Value::Boolean(can_create_db),
                Value::Boolean(is_superuser),
                Value::Boolean(false),
                Value::Boolean(false),
                text_val("********"),
                null_val(),
                useconfig,
            ])
        }

        let mut users = auth_manager.list_users(ctx.txn).await?;
        users.sort_by(|a, b| a.name.cmp(&b.name));

        let mut seen = HashSet::new();
        let mut rows = Vec::with_capacity(users.len() + 2);

        rows.push(user_row(
            "postgres",
            true,
            true,
            global_configs.get("postgres"),
        ));
        seen.insert("postgres".to_string());

        for user in users {
            if !user.can_login {
                continue;
            }
            let lower = user.name.to_ascii_lowercase();
            if seen.contains(&lower) {
                continue;
            }
            rows.push(user_row(
                &user.name,
                user.is_superuser,
                user.can_create_db,
                global_configs.get(&lower),
            ));
            seen.insert(lower);
        }

        let current_user_lower = ctx.current_user.to_ascii_lowercase();
        if !seen.contains(&current_user_lower) {
            if let Some(user) = auth_manager.get_user(ctx.txn, ctx.current_user).await? {
                if user.can_login {
                    rows.push(user_row(
                        &user.name,
                        user.is_superuser,
                        user.can_create_db,
                        global_configs.get(&current_user_lower),
                    ));
                    return Ok(rows);
                }
            }

            rows.push(user_row(
                ctx.current_user,
                ctx.is_superuser,
                false,
                global_configs.get(&current_user_lower),
            ));
        }

        Ok(rows)
    }
}
