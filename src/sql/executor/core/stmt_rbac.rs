//! RBAC statement sub-dispatcher

use super::*;
use crate::auth::{Privilege, PrivilegeObject};
use crate::sql::sequences::SequenceSession;

impl Executor {
    pub(super) async fn execute_rbac_statement(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        _sequence_values: &mut SequenceSession,
        _search_path: &[String],
        stmt: &Statement,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        match stmt {
            Statement::CreateRole {
                names,
                if_not_exists,
                login,
                password,
                superuser,
                create_db,
                create_role,
                ..
            } => {
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::CreateRole,
                    PrivilegeObject::Global,
                    "role",
                    "role".to_string(),
                )
                .await?;
                rbac::execute_create_role(
                    &self.auth_manager,
                    txn,
                    names,
                    *if_not_exists,
                    login,
                    password,
                    superuser,
                    create_db,
                    create_role,
                )
                .await
            }
            Statement::AlterRole { name, operation } => {
                self.require_privilege(
                    txn,
                    current_role,
                    Privilege::CreateRole,
                    PrivilegeObject::Global,
                    "role",
                    name.value.clone(),
                )
                .await?;
                rbac::execute_alter_role(&self.store, &self.auth_manager, txn, name, operation)
                    .await
            }
            Statement::Grant {
                privileges,
                objects,
                grantees,
                with_grant_option,
                ..
            } => {
                rbac::execute_grant(
                    &self.store,
                    &self.auth_manager,
                    txn,
                    db_id,
                    current_role,
                    privileges,
                    objects,
                    grantees,
                    *with_grant_option,
                )
                .await
            }
            Statement::Revoke {
                privileges,
                objects,
                grantees,
                ..
            } => {
                rbac::execute_revoke(
                    &self.store,
                    &self.auth_manager,
                    txn,
                    db_id,
                    current_role,
                    privileges,
                    objects,
                    grantees,
                )
                .await
            }
            _ => unreachable!("RBAC dispatcher received non-RBAC statement"),
        }
    }
}
