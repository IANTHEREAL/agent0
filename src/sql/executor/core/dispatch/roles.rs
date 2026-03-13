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
                    let bypass_rls = user
                        .as_ref()
                        .map(|u| u.bypass_rls)
                        .or_else(|| role.as_ref().map(|r| r.bypass_rls))
                        .unwrap_or(false);

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
                    Ok::<(bool, bool), anyhow::Error>((is_superuser, bypass_rls))
                }
                .await;

                if is_autocommit {
                    if result.is_ok() {
                        session.commit().await?;
                        self.flush_trigger_activations();
                        self.flush_pending_hnsw_merges();
                    } else {
                        session.rollback().await?;
                        self.clear_trigger_activations();
                    }
                }

                let (is_superuser, bypass_rls) = result?;
                session.set_current_role(role_name, is_superuser, bypass_rls);
            }
        } else {
            // `SET ROLE NONE` (and our `RESET ROLE` rewrite) resets to the
            // session user.
            session.reset_role();
        }
        Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_executor() -> Executor {
        let store = crate::storage::TikvStore::new_stub();
        let keyspace = "dispatch_roles_tests".to_string();
        let observability = crate::observability::registry().tenant(&keyspace);
        let trigger_cache = std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new());
        let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
        Executor::new(
            store,
            keyspace,
            observability,
            crate::pool::TenantMemoryAccountant::unlimited("dispatch_roles_tests".to_string()),
            trigger_cache,
            stats_cache,
        )
    }

    #[tokio::test]
    async fn set_role_none_resets_to_session_user() {
        let executor = make_executor();
        let store = crate::storage::TikvStore::new_stub();
        let obs = crate::observability::registry().tenant("dispatch_roles_tests_none");
        let mut session = Session::new_with_user_and_database(
            store,
            obs,
            "app_user".to_string(),
            false,
            false,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        );
        session.set_current_role("other".to_string(), false, false);
        let out = executor
            .execute_set_role(&mut session, &None)
            .await
            .unwrap();
        assert!(matches!(
            out.as_slice(),
            [ExecuteResult::CommandComplete { tag: "SET" }]
        ));
        assert_eq!(session.current_user(), Some("app_user"));
    }

    #[tokio::test]
    async fn set_role_default_keyword_resets_role() {
        let executor = make_executor();
        let store = crate::storage::TikvStore::new_stub();
        let obs = crate::observability::registry().tenant("dispatch_roles_tests_default");
        let mut session = Session::new_with_user_and_database(
            store,
            obs,
            "app_user".to_string(),
            false,
            false,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        );
        session.set_current_role("other".to_string(), false, false);
        let role_ident = sqlparser::ast::Ident::new("default");
        let out = executor
            .execute_set_role(&mut session, &Some(role_ident))
            .await
            .unwrap();
        assert!(matches!(
            out.as_slice(),
            [ExecuteResult::CommandComplete { tag: "SET" }]
        ));
        assert_eq!(session.current_user(), Some("app_user"));
    }

    #[tokio::test]
    async fn set_role_errors_when_session_user_missing() {
        let executor = make_executor();
        let store = crate::storage::TikvStore::new_stub();
        let obs = crate::observability::registry().tenant("dispatch_roles_tests_missing");
        let mut session =
            Session::new_with_database(store, obs, 1, 1, "postgres".to_string(), 0, 0);

        let err = executor
            .execute_set_role(&mut session, &Some(sqlparser::ast::Ident::new("role_x")))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Missing session user"));
    }
}
