//! Role management: SET ROLE handling.

use super::super::*;

impl Executor {
    /// Handle `SET ROLE` statement: validate the target role and update
    /// the session's current role accordingly.
    pub(super) async fn execute_set_role(
        &self,
        session: &mut Session,
        role_name: &Option<sqlparser::ast::Ident>,
    ) -> Result<Vec<ExecuteResult>> {
        if let Some(role_ident) = role_name.as_ref() {
            if role_ident.quote_style.is_none() && role_ident.value.eq_ignore_ascii_case("default")
            {
                session.reset_role();
            } else {
                let role_name = role_ident.value.clone();
                let session_user = session
                    .session_user()
                    .map(|u| u.to_string())
                    .ok_or_else(|| anyhow!("Missing session user"))?;

                let is_autocommit = !session.is_in_transaction();
                if is_autocommit {
                    session.begin().await?;
                }

                let result = async {
                    let (txn, _sequence_values, _search_path) = session
                        .get_mut_txn_sequence_values_and_search_path()
                        .expect("Transaction must be active");

                    let user = self.auth_manager.get_user(txn, &role_name).await?;
                    let role = self.auth_manager.get_role(txn, &role_name).await?;
                    let is_superuser = user
                        .as_ref()
                        .map(|u| u.is_superuser)
                        .or_else(|| role.as_ref().map(|r| r.is_superuser))
                        .ok_or_else(|| anyhow!("role \"{}\" does not exist", role_name))?;

                    let session_user_def = self.auth_manager.get_user(txn, &session_user).await?;
                    let can_set_role = match session_user_def.as_ref() {
                        Some(user) if user.is_superuser => true,
                        Some(user) => role_name == session_user || user.roles.contains(&role_name),
                        None => role_name == session_user,
                    };
                    if !can_set_role {
                        return Err(SqlError::PermissionDenied {
                            object_type: "role".into(),
                            object_name: role_name.clone(),
                        }
                        .into());
                    }
                    Ok::<bool, anyhow::Error>(is_superuser)
                }
                .await;

                if is_autocommit {
                    if result.is_ok() {
                        session.commit().await?;
                        self.flush_trigger_activations();
                    } else {
                        session.rollback().await?;
                        self.clear_trigger_activations();
                    }
                }

                let is_superuser = result?;
                session.set_current_role(role_name, is_superuser);
            }
        } else {
            // `SET ROLE NONE` (and our `RESET ROLE` rewrite) resets to the
            // session user.
            session.reset_role();
        }
        Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
    }
}
