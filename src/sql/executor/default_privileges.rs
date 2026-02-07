use anyhow::{anyhow, Result};

use super::super::default_privileges::{
    apply_default_privileges_grant, apply_default_privileges_revoke,
    parse_alter_default_privileges_sql, AlterDefaultPrivilegesCommand, AlterDefaultPrivilegesOp,
};
use super::super::{ExecuteResult, Session};
use super::core::Executor;
use crate::sql::error::SqlError;

impl Executor {
    pub(crate) async fn execute_alter_default_privileges_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let AlterDefaultPrivilegesCommand {
            target_role,
            schemas,
            op,
        } = parse_alter_default_privileges_sql(sql)?;

        let target_role = match target_role {
            Some(role) => role,
            None => session
                .current_user()
                .map(|u| u.to_string())
                .ok_or_else(|| anyhow!("Missing target role"))?,
        };

        let current_user = session
            .current_user()
            .map(|u| u.to_string())
            .ok_or_else(|| anyhow!("Missing current user"))?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let role_exists = self
                .auth_manager()
                .get_user(txn, &target_role)
                .await?
                .is_some()
                || self
                    .auth_manager()
                    .get_role(txn, &target_role)
                    .await?
                    .is_some();
            if !role_exists {
                return Err(anyhow!("role \"{}\" does not exist", target_role));
            }

            if current_user != target_role {
                let caller_user = self.auth_manager().get_user(txn, &current_user).await?;
                let caller_role = self.auth_manager().get_role(txn, &current_user).await?;

                let is_superuser = caller_user
                    .as_ref()
                    .map(|u| u.is_superuser)
                    .or_else(|| caller_role.as_ref().map(|r| r.is_superuser))
                    .unwrap_or(false);

                let is_member = caller_user
                    .as_ref()
                    .is_some_and(|u| u.roles.contains(&target_role))
                    || caller_role
                        .as_ref()
                        .is_some_and(|r| r.member_of.contains(&target_role));

                if !is_superuser && !is_member {
                    return Err(SqlError::PermissionDenied {
                        object_type: "role".into(),
                        object_name: target_role.clone(),
                    }
                    .into());
                }
            }

            let schema_scopes: Vec<Option<String>> = match schemas.as_ref() {
                Some(schemas) => {
                    for schema in schemas {
                        if !self.store().schema_exists(txn, db_id, schema).await? {
                            return Err(anyhow!("schema '{}' does not exist", schema));
                        }
                    }
                    schemas.iter().cloned().map(Some).collect()
                }
                None => vec![None],
            };

            match &op {
                AlterDefaultPrivilegesOp::Grant {
                    privileges,
                    grantees,
                    with_grant_option,
                } => {
                    for grantee in grantees {
                        let exists = self.auth_manager().get_user(txn, grantee).await?.is_some()
                            || self.auth_manager().get_role(txn, grantee).await?.is_some();
                        if !exists {
                            return Err(anyhow!("role \"{}\" does not exist", grantee));
                        }
                    }

                    for schema in schema_scopes {
                        apply_default_privileges_grant(
                            txn,
                            &target_role,
                            db_id,
                            schema.as_deref(),
                            privileges,
                            grantees,
                            *with_grant_option,
                        )
                        .await?;
                    }
                }
                AlterDefaultPrivilegesOp::Revoke {
                    privileges,
                    grantees,
                } => {
                    for schema in schema_scopes {
                        apply_default_privileges_revoke(
                            txn,
                            &target_role,
                            db_id,
                            schema.as_deref(),
                            privileges,
                            grantees,
                        )
                        .await?;
                    }
                }
            }

            Ok(ExecuteResult::CommandComplete {
                tag: "ALTER DEFAULT PRIVILEGES",
            })
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }
}
