use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterRoleOperation, Expr, GrantObjects, Ident, ObjectName, Password as SqlPassword, Privileges,
    ResetConfig, SetConfigValue, Value as SqlValue,
};
use std::collections::HashSet;
use std::sync::Arc;
use tikv_client::Transaction;

use crate::auth::{AuthManager, GrantedPrivilege, Privilege, PrivilegeObject, User};
use crate::storage::TikvStore;

use super::ExecuteResult;

pub async fn execute_create_role(
    auth_manager: &AuthManager,
    txn: &mut Transaction,
    names: &[ObjectName],
    if_not_exists: bool,
    login: &Option<bool>,
    password: &Option<SqlPassword>,
    superuser: &Option<bool>,
    create_db: &Option<bool>,
    create_role: &Option<bool>,
) -> Result<ExecuteResult> {
    for name in names {
        let role_name = name
            .0
            .last()
            .ok_or_else(|| anyhow!("Invalid role name"))?
            .value
            .clone();

        if if_not_exists && auth_manager.get_user(txn, &role_name).await?.is_some() {
            continue;
        }

        let pwd = match password {
            Some(SqlPassword::Password(expr)) => {
                if let Expr::Value(SqlValue::SingleQuotedString(s)) = expr {
                    s.clone()
                } else {
                    String::new()
                }
            }
            Some(SqlPassword::NullPassword) => String::new(),
            None => String::new(),
        };

        let is_superuser = superuser.unwrap_or(false);
        let can_login = login.unwrap_or(false);

        let mut user = if is_superuser {
            User::new_superuser(&role_name, &pwd)
        } else {
            User::new(&role_name, &pwd)
        };

        user.can_login = can_login;
        user.can_create_db = create_db.unwrap_or(false);
        user.can_create_role = create_role.unwrap_or(false);

        auth_manager.create_user(txn, user).await?;
    }

    Ok(ExecuteResult::CreateRole)
}

pub async fn execute_alter_role(
    store: &Arc<TikvStore>,
    auth_manager: &AuthManager,
    txn: &mut Transaction,
    name: &Ident,
    operation: &AlterRoleOperation,
) -> Result<ExecuteResult> {
    let role_name = name.value.clone();
    let mut user = auth_manager
        .get_user(txn, &role_name)
        .await?
        .ok_or_else(|| anyhow!("Role '{}' does not exist", role_name))?;

    match operation {
        AlterRoleOperation::RenameRole {
            role_name: new_name,
        } => {
            auth_manager.drop_user(txn, &role_name).await?;
            super::role_settings::rename_role_settings(txn, &role_name, &new_name.value).await?;
            user.name = new_name.value.clone();
            auth_manager.create_user(txn, user).await?;
        }
        AlterRoleOperation::WithOptions { options } => {
            for opt in options {
                match opt {
                    sqlparser::ast::RoleOption::SuperUser(v) => user.is_superuser = *v,
                    sqlparser::ast::RoleOption::CreateDB(v) => user.can_create_db = *v,
                    sqlparser::ast::RoleOption::CreateRole(v) => user.can_create_role = *v,
                    sqlparser::ast::RoleOption::Login(v) => user.can_login = *v,
                    sqlparser::ast::RoleOption::Password(p) => {
                        if let SqlPassword::Password(expr) = p {
                            if let Expr::Value(SqlValue::SingleQuotedString(s)) = expr {
                                user.set_password(s);
                            }
                        }
                    }
                    sqlparser::ast::RoleOption::ConnectionLimit(expr) => {
                        if let Expr::Value(SqlValue::Number(n, _)) = expr {
                            user.connection_limit = n.parse().unwrap_or(-1);
                        }
                    }
                    _ => {}
                }
            }
            auth_manager.update_user(txn, user).await?;
        }
        AlterRoleOperation::Set {
            config_name,
            config_value,
            in_database,
        } => {
            let database_oid = if let Some(db_name) = in_database {
                let (schema_opt, db_name) = super::names::split_object_name(db_name)?;
                if schema_opt.is_some() {
                    return Err(anyhow!("invalid database name '{}'", db_name));
                }

                let db_id = store
                    .get_database_id(txn, &db_name)
                    .await?
                    .ok_or_else(|| anyhow!("database \"{}\" does not exist", db_name))?;
                let def = store
                    .get_database_by_id(txn, db_id)
                    .await?
                    .ok_or_else(|| anyhow!("database \"{}\" does not exist", db_name))?;
                def.oid
            } else {
                0
            };

            let setting_name = config_name
                .0
                .iter()
                .map(super::names::normalize_ident)
                .collect::<Vec<_>>()
                .join(".");

            match config_value {
                SetConfigValue::Default => {
                    super::role_settings::reset_role_setting(
                        txn,
                        &role_name,
                        database_oid,
                        &setting_name,
                    )
                    .await?;
                }
                SetConfigValue::FromCurrent => {
                    // KISS: keep compatibility with the previous behavior (no-op).
                }
                SetConfigValue::Value(expr) => {
                    let setting_value = match expr {
                        Expr::Value(SqlValue::Number(n, _)) => n.clone(),
                        Expr::Value(SqlValue::SingleQuotedString(s))
                        | Expr::Value(SqlValue::DoubleQuotedString(s)) => s.clone(),
                        Expr::Value(SqlValue::Boolean(b)) => b.to_string(),
                        Expr::Value(SqlValue::Null) => {
                            return Err(anyhow!("role setting value must not be NULL"));
                        }
                        _ => expr.to_string(),
                    };

                    super::role_settings::set_role_setting(
                        txn,
                        &role_name,
                        database_oid,
                        &setting_name,
                        &setting_value,
                    )
                    .await?;
                }
            }
        }
        AlterRoleOperation::Reset {
            config_name,
            in_database,
        } => {
            let database_oid = if let Some(db_name) = in_database {
                let (schema_opt, db_name) = super::names::split_object_name(db_name)?;
                if schema_opt.is_some() {
                    return Err(anyhow!("invalid database name '{}'", db_name));
                }

                let db_id = store
                    .get_database_id(txn, &db_name)
                    .await?
                    .ok_or_else(|| anyhow!("database \"{}\" does not exist", db_name))?;
                let def = store
                    .get_database_by_id(txn, db_id)
                    .await?
                    .ok_or_else(|| anyhow!("database \"{}\" does not exist", db_name))?;
                def.oid
            } else {
                0
            };

            match config_name {
                ResetConfig::ALL => {
                    super::role_settings::reset_role_settings_all(txn, &role_name, database_oid)
                        .await?;
                }
                ResetConfig::ConfigName(name) => {
                    let setting_name = name
                        .0
                        .iter()
                        .map(super::names::normalize_ident)
                        .collect::<Vec<_>>()
                        .join(".");
                    super::role_settings::reset_role_setting(
                        txn,
                        &role_name,
                        database_oid,
                        &setting_name,
                    )
                    .await?;
                }
            }
        }
        AlterRoleOperation::AddMember { member_name } => {
            auth_manager
                .grant_role_to_user(txn, &member_name.value, &role_name)
                .await?;
        }
        AlterRoleOperation::DropMember { member_name } => {
            auth_manager
                .revoke_role_from_user(txn, &member_name.value, &role_name)
                .await?;
        }
    }

    Ok(ExecuteResult::AlterRole)
}

pub async fn execute_drop_role(
    auth_manager: &AuthManager,
    txn: &mut Transaction,
    names: &[ObjectName],
    if_exists: bool,
) -> Result<ExecuteResult> {
    for name in names {
        let role_name = name
            .0
            .last()
            .ok_or_else(|| anyhow!("Invalid role name"))?
            .value
            .clone();

        let dropped = auth_manager.drop_user(txn, &role_name).await?;
        if !dropped && !if_exists {
            return Err(anyhow!("Role '{}' does not exist", role_name));
        }

        if !dropped {
            let role_dropped = auth_manager.drop_role(txn, &role_name).await?;
            if !role_dropped && !if_exists {
                return Err(anyhow!("Role '{}' does not exist", role_name));
            }
        }

        super::role_settings::delete_role_settings_for_role(txn, &role_name).await?;
        super::default_privileges::cleanup_default_table_privileges_for_role(txn, &role_name)
            .await?;
    }

    Ok(ExecuteResult::DropRole)
}

fn parse_privileges(privileges: &Privileges) -> Vec<Privilege> {
    match privileges {
        Privileges::All { .. } => vec![Privilege::All],
        Privileges::Actions(actions) => actions
            .iter()
            .filter_map(|a| match a {
                sqlparser::ast::Action::Select { .. } => Some(Privilege::Select),
                sqlparser::ast::Action::Insert { .. } => Some(Privilege::Insert),
                sqlparser::ast::Action::Update { .. } => Some(Privilege::Update),
                sqlparser::ast::Action::Delete { .. } => Some(Privilege::Delete),
                sqlparser::ast::Action::Truncate => Some(Privilege::Truncate),
                sqlparser::ast::Action::References { .. } => Some(Privilege::References),
                sqlparser::ast::Action::Trigger => Some(Privilege::Trigger),
                sqlparser::ast::Action::Connect => Some(Privilege::Connect),
                sqlparser::ast::Action::Create => Some(Privilege::CreateTable),
                sqlparser::ast::Action::Execute => Some(Privilege::Execute),
                sqlparser::ast::Action::Usage => Some(Privilege::Usage),
                _ => None,
            })
            .collect(),
    }
}

async fn expand_privilege_objects(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    objects: &GrantObjects,
) -> Result<Vec<PrivilegeObject>> {
    match objects {
        GrantObjects::Tables(tables) => {
            if tables.is_empty() {
                return Ok(vec![PrivilegeObject::Global]);
            }

            let mut out = Vec::with_capacity(tables.len());
            for name in tables {
                let (schema_opt, table) = super::names::split_object_name(name)?;
                let schema = schema_opt.unwrap_or_else(|| "public".to_string());
                out.push(PrivilegeObject::Table {
                    schema,
                    name: table,
                });
            }
            Ok(out)
        }
        GrantObjects::AllTablesInSchema { schemas } => {
            let all_tables = store.list_tables(txn, db_id).await?;
            let mut objects = HashSet::new();

            for schema_name in schemas {
                let (_schema_prefix, schema) = super::names::split_object_name(schema_name)?;
                for table in &all_tables {
                    if let Ok((tbl_schema, tbl_name)) = super::names::parse_full_name(table) {
                        if tbl_schema == schema {
                            objects.insert(PrivilegeObject::Table {
                                schema: tbl_schema,
                                name: tbl_name,
                            });
                        }
                    }
                }
            }

            Ok(objects.into_iter().collect())
        }
        GrantObjects::Schemas(schemas) => {
            let mut out = Vec::with_capacity(schemas.len());
            for schema_name in schemas {
                let (_schema_prefix, schema) = super::names::split_object_name(schema_name)?;
                out.push(PrivilegeObject::Schema(schema));
            }
            Ok(out)
        }
        _ => Ok(vec![PrivilegeObject::Global]),
    }
}

pub async fn execute_grant(
    store: &Arc<TikvStore>,
    auth_manager: &AuthManager,
    txn: &mut Transaction,
    db_id: u64,
    privileges: &Privileges,
    objects: &GrantObjects,
    grantees: &[Ident],
    with_grant_option: bool,
) -> Result<ExecuteResult> {
    let privs = parse_privileges(privileges);
    let objects = expand_privilege_objects(store, txn, db_id, objects).await?;

    for grantee in grantees {
        let username = grantee.value.clone();
        if let Some(mut user) = auth_manager.get_user(txn, &username).await? {
            for priv_type in &privs {
                for obj in &objects {
                    user.grant_privilege(priv_type.clone(), obj.clone(), with_grant_option);
                }
            }
            auth_manager.update_user(txn, user).await?;
        } else if let Some(mut role) = auth_manager.get_role(txn, &username).await? {
            for priv_type in &privs {
                for obj in &objects {
                    role.privileges
                        .retain(|p| !(p.privilege == *priv_type && p.object == *obj));
                    role.privileges.push(GrantedPrivilege {
                        privilege: priv_type.clone(),
                        object: obj.clone(),
                        with_grant_option,
                    });
                }
            }
            auth_manager.update_role(txn, role).await?;
        } else {
            return Err(anyhow!("Role or user '{}' does not exist", username));
        }
    }

    Ok(ExecuteResult::Grant)
}

pub async fn execute_revoke(
    store: &Arc<TikvStore>,
    auth_manager: &AuthManager,
    txn: &mut Transaction,
    db_id: u64,
    privileges: &Privileges,
    objects: &GrantObjects,
    grantees: &[Ident],
) -> Result<ExecuteResult> {
    let privs = parse_privileges(privileges);
    let mut expanded_objects = expand_privilege_objects(store, txn, db_id, objects).await?;
    if let GrantObjects::AllTablesInSchema { schemas } = objects {
        for schema_name in schemas {
            let (_schema_prefix, schema) = super::names::split_object_name(schema_name)?;
            expanded_objects.push(PrivilegeObject::AllTablesInSchema(schema));
        }
    }

    for grantee in grantees {
        let username = grantee.value.clone();
        if let Some(mut user) = auth_manager.get_user(txn, &username).await? {
            for priv_type in &privs {
                for obj in &expanded_objects {
                    user.revoke_privilege(priv_type, obj);
                }
            }
            auth_manager.update_user(txn, user).await?;
        } else if let Some(mut role) = auth_manager.get_role(txn, &username).await? {
            role.privileges.retain(|p| {
                !(privs.contains(&p.privilege)
                    && expanded_objects.iter().any(|obj| p.object == *obj))
            });
            auth_manager.update_role(txn, role).await?;
        } else {
            return Err(anyhow!("Role or user '{}' does not exist", username));
        }
    }

    Ok(ExecuteResult::Revoke)
}
