//! GUC/SET handling: SET variable and SHOW ALL.

use super::super::*;

fn is_default_set_value(value: &[Expr]) -> bool {
    if value.len() != 1 {
        return false;
    }

    match &value[0] {
        Expr::Identifier(ident) => ident.value.eq_ignore_ascii_case("default"),
        Expr::CompoundIdentifier(idents) if idents.len() == 1 => {
            idents[0].value.eq_ignore_ascii_case("default")
        }
        Expr::Value(sqlparser::ast::Value::UnQuotedString(s)) => s.eq_ignore_ascii_case("default"),
        _ => false,
    }
}

/// Drain session pending notices into `ExecuteResult::Notice` items followed by
/// `CommandComplete(SET)`.
fn drain_notices_to_results(session: &mut Session) -> Vec<ExecuteResult> {
    let mut results: Vec<ExecuteResult> = session
        .drain_pending_notices()
        .into_iter()
        .map(|(severity, sqlstate, message)| ExecuteResult::Notice {
            severity,
            sqlstate,
            message,
        })
        .collect();
    results.push(ExecuteResult::CommandComplete { tag: "SET" });
    results
}

/// Execute a `SET <variable>` statement synchronously.
pub(super) fn execute_set_variable(
    session: &mut Session,
    local: bool,
    variable: &sqlparser::ast::ObjectName,
    value: &[Expr],
) -> Result<Vec<ExecuteResult>> {
    let var_name = variable
        .0
        .iter()
        .map(normalize_ident)
        .collect::<Vec<_>>()
        .join(".")
        .to_lowercase();

    // ① LOCAL-outside-transaction warning (PG ordering: warn before GUC check)
    let local_outside_txn = local && !session.is_in_transaction();
    if local_outside_txn {
        session.push_pending_notice(
            "WARNING".to_string(),
            "25P01".to_string(),
            "SET LOCAL can only be used in transaction blocks".to_string(),
        );
    }

    // ② Reserved GUC check (now runs after warning is queued)
    match check_reserved_guc_write(&var_name) {
        Ok(()) => {}
        Err(write_err) => {
            if is_default_set_value(value) {
                check_reserved_guc_reset(&var_name)?;
                if local_outside_txn {
                    // session_authorization TO DEFAULT outside txn: WARNING + SET
                    return Ok(drain_notices_to_results(session));
                }
                session.reset_setting(&var_name);
                return Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }]);
            }
            return Err(write_err);
        }
    }

    // session_authorization is intercepted in ast.rs via
    // execute_set_session_authorization; if it reaches here (e.g. a direct
    // call from tests), reject it as a reserved pseudo-GUC.
    if var_name == "session_authorization" && !is_default_set_value(value) {
        return Err(SqlError::CantChangeRuntimeParam {
            message: "parameter \"session_authorization\" cannot be changed".to_string(),
        }
        .into());
    }

    // ③ Non-reserved GUC handling
    if var_name == "search_path" {
        let mut new_search_path = Vec::new();
        for expr in value {
            match expr {
                Expr::Identifier(ident) => {
                    new_search_path.push(normalize_ident(ident));
                }
                Expr::CompoundIdentifier(idents) if idents.len() == 1 => {
                    new_search_path.push(normalize_ident(&idents[0]));
                }
                Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => {
                    new_search_path.extend(parse_search_path_guc_value(s));
                }
                _ => {
                    return Err(anyhow!("Unsupported search_path value: {}", expr));
                }
            }
        }
        let new_search_path = normalize_search_path_entries(new_search_path)?;
        if local_outside_txn {
            return Ok(drain_notices_to_results(session));
        }

        if local {
            session.set_local_search_path(new_search_path);
        } else {
            session.set_search_path(new_search_path);
        }
    } else {
        let value = set_variable_value_to_string(value)?;
        if local_outside_txn {
            crate::sql::session::SessionSettings::validate_and_normalize_value(&var_name, &value)?;
            return Ok(drain_notices_to_results(session));
        }

        if local {
            session.set_local_setting(&var_name, value)?;
        } else {
            session.set_known_setting(&var_name, value)?;
        }
    }
    Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
}

/// Build a `SHOW ALL` result set: three columns (name, setting, description),
/// sorted alphabetically by name.
pub(super) fn build_show_all_result(session: &Session, timezone: Arc<str>) -> ExecuteResult {
    let all_settings = session.show_all_settings();
    let rows = all_settings
        .into_iter()
        .map(|(name, setting, description)| {
            Row::new(vec![
                Value::Text(name),
                Value::Text(setting),
                Value::Text(description),
            ])
        })
        .collect();
    ExecuteResult::Select {
        columns: vec![
            "name".to_string(),
            "setting".to_string(),
            "description".to_string(),
        ],
        column_types: Some(vec![DataType::Text, DataType::Text, DataType::Text]),
        rows,
        timezone,
    }
}

use super::super::Executor;

impl Executor {
    /// Handle `SET [LOCAL] session_authorization = <value>` with PostgreSQL
    /// role-existence and permission branching:
    ///   - target == session_user → success (SET)
    ///   - target role does not exist → SQLSTATE 22023
    ///   - target role exists but unauthorized → SQLSTATE 42501
    ///   - target role exists and authorized (superuser) → success (SET)
    pub(super) async fn execute_set_session_authorization(
        &self,
        session: &mut Session,
        local: bool,
        value: &[Expr],
    ) -> Result<Vec<ExecuteResult>> {
        // ① LOCAL-outside-transaction warning (PG ordering: warn before error)
        let local_outside_txn = local && !session.is_in_transaction();
        if local_outside_txn {
            session.push_pending_notice(
                "WARNING".to_string(),
                "25P01".to_string(),
                "SET LOCAL can only be used in transaction blocks".to_string(),
            );
        }

        // ② DEFAULT → reset session_authorization to original authenticated user
        if is_default_set_value(value) {
            if local_outside_txn {
                return Ok(drain_notices_to_results(session));
            }
            if local {
                session.save_session_auth_for_local();
            } else {
                session.clear_local_session_auth_save();
            }
            session.reset_session_authorization();
            return Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }]);
        }

        // ③ Non-DEFAULT: role-existence + permission branching
        let role_name = set_variable_value_to_string(value)?;
        let session_user = session.session_user().unwrap_or("postgres").to_string();

        // Fast path: target is the current session user → always allowed
        if role_name == session_user {
            if local_outside_txn {
                return Ok(drain_notices_to_results(session));
            }
            // Must apply state transition even for same-role: resets current_user
            // after a prior SET ROLE (PG parity).
            if local {
                session.save_session_auth_for_local();
            } else {
                session.clear_local_session_auth_save();
            }
            let is_su = session.session_user_is_superuser();
            let brls = session.bypass_rls();
            session.set_session_authorization(role_name, is_su, brls);
            return Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }]);
        }

        // Need async role lookup: start an auto-commit txn if necessary
        // PG bases SET SESSION AUTHORIZATION permission on the initial
        // authenticated (login) user, not the currently effective session user.
        let session_user_is_su = session.authenticated_user_is_superuser();
        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let (txn, _seq, _sp) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            // Check role existence (PG checks existence before permission)
            let user = self.auth_manager.get_user(txn, &role_name).await?;
            let role = self.auth_manager.get_role(txn, &role_name).await?;
            let target_is_superuser = user
                .as_ref()
                .map(|u| u.is_superuser)
                .or_else(|| role.as_ref().map(|r| r.is_superuser));

            if target_is_superuser.is_none() {
                // Role does not exist → SQLSTATE 22023
                return Err(SqlError::InvalidParameterValue {
                    message: format!("role \"{}\" does not exist", role_name),
                }
                .into());
            }

            let target_bypass_rls = user
                .as_ref()
                .map(|u| u.bypass_rls)
                .or_else(|| role.as_ref().map(|r| r.bypass_rls))
                .unwrap_or(false);

            // Permission check: only superusers can SET session_authorization
            // to a different role (PG parity: SQLSTATE 42501)
            if !session_user_is_su {
                return Err(SqlError::InsufficientPrivilege {
                    message: format!(
                        "permission denied to set session authorization \"{}\"",
                        role_name
                    ),
                }
                .into());
            }

            Ok::<(bool, bool), anyhow::Error>((target_is_superuser.unwrap(), target_bypass_rls))
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

        let (target_is_superuser, target_bypass_rls) = result?;

        // Authorized: apply the session authorization change
        if local_outside_txn {
            return Ok(drain_notices_to_results(session));
        }
        if local {
            session.save_session_auth_for_local();
        } else {
            session.clear_local_session_auth_save();
        }
        session.set_session_authorization(role_name, target_is_superuser, target_bypass_rls);
        Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
    }
}

#[cfg(test)]
mod tests {
    use super::{build_show_all_result, execute_set_variable};
    use crate::model::DataType;
    use crate::sql::parse_sql;
    use crate::sql::{ExecuteResult, Session};
    use sqlparser::ast::{Expr, ObjectName, SetExpr, Statement, Value as SqlValue};

    fn make_session() -> Session {
        let store = crate::storage::TikvStore::new_stub();
        let obs = crate::observability::registry().tenant("dispatch_guc_tests");
        Session::new_with_database(store, obs, 1, 1, "postgres".to_string(), 0, 0).unwrap()
    }

    fn parse_set(sql: &str) -> (bool, ObjectName, Vec<Expr>) {
        let mut stmts = parse_sql(sql).expect("parse");
        let stmt = stmts.remove(0);
        let Statement::SetVariable {
            local,
            variable,
            value,
            ..
        } = stmt
        else {
            panic!("expected set variable");
        };
        (local, variable, value)
    }

    #[test]
    fn execute_set_variable_updates_known_setting() {
        let mut session = make_session();
        let (local, variable, value) = parse_set("SET statement_timeout = 1500");
        let out = execute_set_variable(&mut session, local, &variable, &value).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            session.show_setting_value("statement_timeout").as_deref(),
            Some("1500ms")
        );
    }

    #[test]
    fn execute_set_variable_search_path_local_outside_txn_returns_warning_notice() {
        let mut session = make_session();
        let variable = ObjectName(vec![sqlparser::ast::Ident::new("search_path")]);
        let value = vec![Expr::Value(SqlValue::SingleQuotedString(
            "public, pg_catalog".to_string(),
        ))];

        let out = execute_set_variable(&mut session, true, &variable, &value).unwrap();
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], ExecuteResult::Notice { .. }));
        assert!(matches!(
            out[1],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
    }

    #[test]
    fn execute_set_variable_rejects_unsupported_search_path_expr() {
        let mut session = make_session();
        let variable = ObjectName(vec![sqlparser::ast::Ident::new("search_path")]);
        let value = vec![Expr::BinaryOp {
            left: Box::new(Expr::Value(SqlValue::Number("1".to_string(), false))),
            op: sqlparser::ast::BinaryOperator::Plus,
            right: Box::new(Expr::Value(SqlValue::Number("2".to_string(), false))),
        }];

        let err = execute_set_variable(&mut session, false, &variable, &value)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unsupported search_path value"));
    }

    /// When execute_set_variable is called directly for session_authorization
    /// (bypassing the ast.rs interception), it falls through to the generic
    /// reserved-pseudo-GUC error.
    #[test]
    fn execute_set_variable_rejects_session_auth_as_reserved_guc() {
        let mut session = make_session();
        let (local, variable, value) = parse_set("SET session_authorization = 'evil_user'");
        let err = execute_set_variable(&mut session, local, &variable, &value)
            .unwrap_err()
            .to_string();
        assert!(err.contains("parameter \"session_authorization\" cannot be changed"));
    }

    #[test]
    fn execute_set_variable_allows_session_authorization_default_reset() {
        let mut session = make_session();
        let (local, variable, value) = parse_set("SET session_authorization TO DEFAULT");
        let out = execute_set_variable(&mut session, local, &variable, &value).unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
        assert_eq!(
            session
                .show_setting_value("session_authorization")
                .as_deref(),
            Some("postgres")
        );
    }

    #[test]
    fn build_show_all_result_emits_select_shape() {
        let mut session = make_session();
        let (local, variable, value) = parse_set("SET statement_timeout = 1000");
        let _ = execute_set_variable(&mut session, local, &variable, &value).unwrap();

        let out = build_show_all_result(&session, std::sync::Arc::from("UTC"));
        let ExecuteResult::Select {
            columns,
            column_types,
            rows,
            ..
        } = out
        else {
            panic!("expected select");
        };
        assert_eq!(columns, vec!["name", "setting", "description"]);
        assert_eq!(
            column_types,
            Some(vec![DataType::Text, DataType::Text, DataType::Text])
        );
        assert!(!rows.is_empty());
    }

    #[test]
    fn set_tableless_helpers_do_not_trigger_on_non_select_statement() {
        let mut stmts = parse_sql("VALUES (1)").expect("parse");
        let stmt = stmts.remove(0);
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert!(!matches!(q.body.as_ref(), SetExpr::Select(_)));
    }

    /// Bug B fix: SET LOCAL is_superuser = 'on' outside txn → Err + pending notice.
    /// Regression test for #1622: WARNING SQLSTATE must be 25P01 (PG parity).
    #[test]
    fn set_local_is_superuser_outside_txn_queues_notice_and_errors() {
        let mut session = make_session();
        let (_, variable, value) = parse_set("SET LOCAL is_superuser = 'on'");
        let err = execute_set_variable(&mut session, true, &variable, &value).unwrap_err();
        assert!(err
            .to_string()
            .contains("parameter \"is_superuser\" cannot be changed"));
        // Error SQLSTATE must be 55P02 (cant_change_runtime_param)
        let sql_err = err.downcast_ref::<crate::sql::error::SqlError>().unwrap();
        assert_eq!(sql_err.sqlstate(), "55P02");
        let notices = session.drain_pending_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "WARNING");
        assert_eq!(notices[0].1, "25P01"); // SQLSTATE: no_active_sql_transaction
        assert_eq!(
            notices[0].2,
            "SET LOCAL can only be used in transaction blocks"
        );
    }

    /// Bug A fix: SET LOCAL is_superuser TO DEFAULT outside txn → Err + pending notice.
    /// Regression test for #1622: WARNING SQLSTATE must be 25P01 (PG parity).
    #[test]
    fn set_local_is_superuser_default_outside_txn_queues_notice_and_errors() {
        let mut session = make_session();
        let (_, variable, value) = parse_set("SET LOCAL is_superuser TO DEFAULT");
        let err = execute_set_variable(&mut session, true, &variable, &value).unwrap_err();
        assert!(err
            .to_string()
            .contains("parameter \"is_superuser\" cannot be changed"));
        let sql_err = err.downcast_ref::<crate::sql::error::SqlError>().unwrap();
        assert_eq!(sql_err.sqlstate(), "55P02");
        let notices = session.drain_pending_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "WARNING");
        assert_eq!(notices[0].1, "25P01");
        assert_eq!(
            notices[0].2,
            "SET LOCAL can only be used in transaction blocks"
        );
    }

    /// SET LOCAL session_authorization = 'evil' outside txn via execute_set_variable
    /// (bypass) → Err "parameter cannot be changed" + pending notice.
    /// Regression test for #1622: WARNING SQLSTATE must be 25P01 (PG parity).
    #[test]
    fn set_local_session_auth_outside_txn_via_set_variable_returns_reserved_error() {
        let mut session = make_session();
        let (_, variable, value) = parse_set("SET LOCAL session_authorization = 'evil_user'");
        let err = execute_set_variable(&mut session, true, &variable, &value).unwrap_err();
        assert!(err
            .to_string()
            .contains("parameter \"session_authorization\" cannot be changed"));
        let sql_err = err.downcast_ref::<crate::sql::error::SqlError>().unwrap();
        assert_eq!(sql_err.sqlstate(), "55P02");
        let notices = session.drain_pending_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "WARNING");
        assert_eq!(notices[0].1, "25P01");
        assert_eq!(
            notices[0].2,
            "SET LOCAL can only be used in transaction blocks"
        );
    }

    /// SET LOCAL session_authorization TO DEFAULT outside txn → Ok with WARNING + SET.
    /// Regression test for #1622: WARNING SQLSTATE must be 25P01 (PG parity).
    #[test]
    fn set_local_session_auth_default_outside_txn_warns_and_succeeds() {
        let mut session = make_session();
        let (_, variable, value) = parse_set("SET LOCAL session_authorization TO DEFAULT");
        let out = execute_set_variable(&mut session, true, &variable, &value).unwrap();
        assert_eq!(out.len(), 2);
        assert!(matches!(
            &out[0],
            ExecuteResult::Notice { severity, sqlstate, message }
            if severity == "WARNING"
                && sqlstate == "25P01"
                && message == "SET LOCAL can only be used in transaction blocks"
        ));
        assert!(matches!(
            out[1],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
    }

    // --- Async tests for execute_set_session_authorization (Executor method) ---

    use super::Executor;

    fn make_executor() -> Executor {
        let store = crate::storage::TikvStore::new_stub();
        let keyspace = "dispatch_guc_session_auth_tests".to_string();
        let observability = crate::observability::registry().tenant(&keyspace);
        let trigger_cache = std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new());
        let rls_policy_cache = std::sync::Arc::new(crate::sql::rls::cache::RlsPolicyCache::new());
        let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
        Executor::new(
            store,
            keyspace,
            observability,
            crate::pool::TenantMemoryAccountant::unlimited(
                "dispatch_guc_session_auth_tests".to_string(),
            ),
            trigger_cache,
            rls_policy_cache,
            stats_cache,
        )
    }

    fn make_session_with_user(name: &str, is_superuser: bool) -> Session {
        let store = crate::storage::TikvStore::new_stub();
        let obs = crate::observability::registry().tenant("guc_session_auth_test");
        Session::new_with_user_and_database(
            store,
            obs,
            name.to_string(),
            is_superuser,
            false,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap()
    }

    fn parse_set_value(sql: &str) -> Vec<Expr> {
        let (_, _, value) = parse_set(sql);
        value
    }

    /// SET session_authorization = '<session_user>' → success (fast path)
    #[tokio::test]
    async fn set_session_auth_same_role_succeeds() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);
        let value = parse_set_value("SET session_authorization = 'app_user'");
        let out = executor
            .execute_set_session_authorization(&mut session, false, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
    }

    /// SET session_authorization TO DEFAULT → success
    #[tokio::test]
    async fn set_session_auth_default_succeeds() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);
        let value = parse_set_value("SET session_authorization TO DEFAULT");
        let out = executor
            .execute_set_session_authorization(&mut session, false, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
    }

    /// SET LOCAL session_authorization = '<session_user>' outside txn →
    /// WARNING + SET (fast path, same role)
    /// Regression test for #1622: WARNING SQLSTATE must be 25P01 (PG parity).
    #[tokio::test]
    async fn set_local_session_auth_same_role_outside_txn_warns_and_succeeds() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);
        let value = parse_set_value("SET session_authorization = 'app_user'");
        let out = executor
            .execute_set_session_authorization(&mut session, true, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert!(matches!(
            &out[0],
            ExecuteResult::Notice { severity, sqlstate, message }
            if severity == "WARNING"
                && sqlstate == "25P01"
                && message == "SET LOCAL can only be used in transaction blocks"
        ));
        assert!(matches!(
            out[1],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
    }

    /// SET LOCAL session_authorization TO DEFAULT outside txn → WARNING + SET
    /// Regression test for #1622: WARNING SQLSTATE must be 25P01 (PG parity).
    #[tokio::test]
    async fn set_local_session_auth_default_outside_txn_via_executor() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);
        let value = parse_set_value("SET session_authorization TO DEFAULT");
        let out = executor
            .execute_set_session_authorization(&mut session, true, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert!(matches!(
            &out[0],
            ExecuteResult::Notice { severity, sqlstate, message }
            if severity == "WARNING"
                && sqlstate == "25P01"
                && message == "SET LOCAL can only be used in transaction blocks"
        ));
        assert!(matches!(
            out[1],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
    }

    /// P1 regression: same-role fast path must reset current_user after SET ROLE.
    /// Repro: SET ROLE other; SET SESSION AUTHORIZATION <session_user>; SELECT current_user;
    /// Expected: current_user == session_user (not "other")
    #[tokio::test]
    async fn set_session_auth_same_role_resets_current_user_after_set_role() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);

        // Simulate SET ROLE other
        session.set_current_role("other".to_string(), false, false);
        assert_eq!(session.current_user(), Some("other"));

        // SET SESSION AUTHORIZATION 'app_user' (same as session_user)
        let value = parse_set_value("SET session_authorization = 'app_user'");
        let out = executor
            .execute_set_session_authorization(&mut session, false, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
        // current_user must be reset to session_user, not remain as "other"
        assert_eq!(session.current_user(), Some("app_user"));
    }

    /// P1 regression: DEFAULT must restore original authenticated user after
    /// a prior SET SESSION AUTHORIZATION change.
    /// Repro: SET SESSION AUTHORIZATION alice; SET SESSION AUTHORIZATION DEFAULT;
    ///        SELECT current_user;
    /// Expected: current_user == original authenticated user
    #[tokio::test]
    async fn set_session_auth_default_restores_authenticated_user() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);

        // Simulate a prior SET SESSION AUTHORIZATION 'alice'
        session.set_session_authorization("alice".to_string(), false, false);
        assert_eq!(session.session_user(), Some("alice"));
        assert_eq!(session.current_user(), Some("alice"));

        // SET SESSION AUTHORIZATION DEFAULT
        let value = parse_set_value("SET session_authorization TO DEFAULT");
        let out = executor
            .execute_set_session_authorization(&mut session, false, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
        // Must restore to original authenticated user "app_user"
        assert_eq!(session.session_user(), Some("app_user"));
        assert_eq!(session.current_user(), Some("app_user"));
    }

    /// QG block #4, fix 1: SET LOCAL session_authorization same-role inside txn
    /// reverts on commit.
    #[tokio::test]
    async fn set_local_session_auth_same_role_reverts_on_commit() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);

        // Simulate SET ROLE other
        session.set_current_role("other".to_string(), false, false);
        assert_eq!(session.current_user(), Some("other"));

        // Enter explicit transaction
        session.force_test_transaction_state(true, false);

        // SET LOCAL SESSION AUTHORIZATION 'app_user' (same as session_user)
        let value = parse_set_value("SET session_authorization = 'app_user'");
        let out = executor
            .execute_set_session_authorization(&mut session, true, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        // current_user changed to session_user within txn
        assert_eq!(session.current_user(), Some("app_user"));

        // Simulate COMMIT: clear local overrides
        session.clear_local_overrides();

        // Must revert to pre-SET LOCAL state: current_user was "other"
        assert_eq!(session.current_user(), Some("other"));
    }

    /// QG block #4, fix 1: SET LOCAL session_authorization DEFAULT inside txn
    /// reverts on rollback.
    #[tokio::test]
    async fn set_local_session_auth_default_reverts_on_rollback() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);

        // Simulate a prior SET SESSION AUTHORIZATION to change session_user
        session.set_session_authorization("alice".to_string(), false, false);
        assert_eq!(session.session_user(), Some("alice"));

        // Enter explicit transaction
        session.force_test_transaction_state(true, false);

        // SET LOCAL SESSION AUTHORIZATION DEFAULT → resets to authenticated_user
        let value = parse_set_value("SET session_authorization TO DEFAULT");
        let out = executor
            .execute_set_session_authorization(&mut session, true, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        // During txn, reset to authenticated user
        assert_eq!(session.session_user(), Some("app_user"));

        // Simulate ROLLBACK: clear local overrides
        session.clear_local_overrides();

        // Must revert to pre-SET LOCAL state: session_user was "alice"
        assert_eq!(session.session_user(), Some("alice"));
        assert_eq!(session.current_user(), Some("alice"));
    }

    /// QG block #4, fix 2: non-LOCAL SET session_authorization is NOT reverted
    /// by clear_local_overrides (session-wide mutation persists).
    #[tokio::test]
    async fn set_session_auth_non_local_persists_after_commit() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);

        // Enter explicit transaction
        session.force_test_transaction_state(true, false);

        // Non-LOCAL SET SESSION AUTHORIZATION (same-role fast path)
        let value = parse_set_value("SET session_authorization = 'app_user'");
        let out = executor
            .execute_set_session_authorization(&mut session, false, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);

        // Simulate COMMIT
        session.clear_local_overrides();

        // Non-LOCAL change persists
        assert_eq!(session.session_user(), Some("app_user"));
        assert_eq!(session.current_user(), Some("app_user"));
    }

    /// P1: SET LOCAL then SET (non-LOCAL) same-role in txn — COMMIT must NOT
    /// restore stale snapshot from SET LOCAL.
    #[tokio::test]
    async fn set_local_then_non_local_session_auth_commit_preserves_non_local() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);

        // Simulate SET ROLE other
        session.set_current_role("other".to_string(), false, false);
        assert_eq!(session.current_user(), Some("other"));

        // Enter explicit transaction
        session.force_test_transaction_state(true, false);

        // SET LOCAL session_authorization = 'app_user' (saves snapshot)
        let value = parse_set_value("SET session_authorization = 'app_user'");
        let out = executor
            .execute_set_session_authorization(&mut session, true, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(session.current_user(), Some("app_user"));

        // SET session_authorization = 'app_user' (non-LOCAL, clears snapshot)
        let out = executor
            .execute_set_session_authorization(&mut session, false, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(session.current_user(), Some("app_user"));

        // Simulate COMMIT
        session.clear_local_overrides();

        // Non-LOCAL change must persist — no stale restore
        assert_eq!(session.current_user(), Some("app_user"));
        assert_eq!(session.session_user(), Some("app_user"));
    }

    /// P1: SET LOCAL then SET DEFAULT (non-LOCAL) in txn — COMMIT must NOT
    /// restore stale snapshot from SET LOCAL.
    #[tokio::test]
    async fn set_local_then_non_local_default_session_auth_commit_preserves_non_local() {
        let executor = make_executor();
        let mut session = make_session_with_user("app_user", false);

        // Simulate a prior SET SESSION AUTHORIZATION to change session_user
        session.set_session_authorization("alice".to_string(), false, false);
        assert_eq!(session.session_user(), Some("alice"));

        // Enter explicit transaction
        session.force_test_transaction_state(true, false);

        // SET LOCAL session_authorization TO DEFAULT (saves snapshot, resets to authenticated)
        let value = parse_set_value("SET session_authorization TO DEFAULT");
        let out = executor
            .execute_set_session_authorization(&mut session, true, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(session.session_user(), Some("app_user"));

        // SET session_authorization TO DEFAULT (non-LOCAL, clears snapshot)
        let out = executor
            .execute_set_session_authorization(&mut session, false, &value)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(session.session_user(), Some("app_user"));

        // Simulate COMMIT
        session.clear_local_overrides();

        // Non-LOCAL change must persist — no stale restore to "alice"
        assert_eq!(session.session_user(), Some("app_user"));
        assert_eq!(session.current_user(), Some("app_user"));
    }
}
