//! Centralized reserved pseudo-GUC classification and write guards.

use crate::auth::AuthManager;
use crate::sql::error::SqlError;
use crate::sql::session::settings::KNOWN_GUCS;
use crate::sql::session::SessionSettings;
use crate::storage::TikvStore;
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GucKind {
    ReadOnlyPseudo,
    SessionAuthPseudo,
    SearchPath,
    Known,
    UnknownCompat,
}

pub(super) fn classify_guc(name: &str) -> GucKind {
    let lowered = name.to_ascii_lowercase();
    match lowered.as_str() {
        "is_superuser" => return GucKind::ReadOnlyPseudo,
        "session_authorization" => return GucKind::SessionAuthPseudo,
        _ => {}
    }

    let canonical = SessionSettings::canonical_setting_name(&lowered);
    if canonical == "search_path" {
        return GucKind::SearchPath;
    }
    if KNOWN_GUCS.iter().any(|g| g.name == canonical) {
        return GucKind::Known;
    }
    GucKind::UnknownCompat
}

pub(crate) fn check_reserved_guc_write(name: &str) -> Result<()> {
    match classify_guc(name) {
        GucKind::ReadOnlyPseudo => Err(SqlError::CantChangeRuntimeParam {
            message: "parameter \"is_superuser\" cannot be changed".to_string(),
        }
        .into()),
        // session_authorization is handled per-callsite via check_session_auth_write
        GucKind::SessionAuthPseudo
        | GucKind::SearchPath
        | GucKind::Known
        | GucKind::UnknownCompat => Ok(()),
    }
}

/// PostgreSQL allows `SET session_authorization` / `set_config('session_authorization', ...)`
/// when the target value matches the current session user.  Setting to a different user
/// requires superuser privilege (which db9 does not yet support), so we return a
/// permission-denied error for any other value.
///
/// Note: production callers should prefer [`session_auth_different_user_error`] (async)
/// or [`session_auth_different_user_error_sync`] (sync) for the different-user case,
/// which distinguish nonexistent roles from permission denied.
#[cfg(test)]
fn check_session_auth_write(name: &str, value: &str, current_user: &str) -> Result<()> {
    if classify_guc(name) != GucKind::SessionAuthPseudo {
        return Ok(());
    }
    if value == current_user {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "permission denied to set session authorization"
        ))
    }
}

/// Produce the PG-parity error for setting session_authorization to a different user.
///
/// PostgreSQL distinguishes two cases:
/// - nonexistent target role → `role "..." does not exist` (SQLSTATE 42704)
/// - existing non-current role → `permission denied to set session authorization`
///
/// When the store has no TiKV client (stub/unit-test), falls back to the
/// generic permission-denied message.
///
/// Infrastructure/catalog failures (txn creation, role lookup) are propagated
/// as-is rather than masked as auth semantic errors.
pub(crate) async fn session_auth_different_user_error(
    target: &str,
    store: &TikvStore,
) -> anyhow::Error {
    if store.transaction_client().is_some() {
        let mut txn = match store.begin_optimistic().await {
            Ok(txn) => txn,
            Err(e) => return e,
        };
        let auth = AuthManager::new();
        let user_result = auth.get_user(&mut txn, target).await;
        let role_result = auth.get_role(&mut txn, target).await;
        let exists = match (&user_result, &role_result) {
            // At least one lookup confirmed existence.
            (Ok(Some(_)), _) | (_, Ok(Some(_))) => true,
            // Both succeeded with None — role genuinely doesn't exist.
            (Ok(None), Ok(None)) => false,
            // One or both lookups failed with no positive result —
            // propagate the infrastructure error rather than masking
            // it as a semantic auth error.
            (Err(e), _) => {
                let _ = txn.rollback().await;
                return anyhow::anyhow!("{}", e);
            }
            (_, Err(e)) => {
                let _ = txn.rollback().await;
                return anyhow::anyhow!("{}", e);
            }
        };
        let _ = txn.rollback().await;
        if !exists {
            return SqlError::InvalidParameterValue {
                message: format!("role \"{}\" does not exist", target),
            }
            .into();
        }
    }
    SqlError::InsufficientPrivilege {
        message: format!(
            "permission denied to set session authorization \"{}\"",
            target
        ),
    }
    .into()
}

/// Synchronous wrapper around [`session_auth_different_user_error`] for use in
/// the typed expression evaluator (sync `eval_function_call` context).
///
/// When a real TiKV client is available (production), uses `block_in_place` +
/// `block_on` to perform the async role-existence lookup.  Falls back to the
/// generic permission-denied message when no client is available (unit-test
/// stubs).
pub(crate) fn session_auth_different_user_error_sync(
    target: &str,
    store: Option<&TikvStore>,
) -> anyhow::Error {
    let Some(store) = store else {
        return SqlError::InsufficientPrivilege {
            message: format!(
                "permission denied to set session authorization \"{}\"",
                target
            ),
        }
        .into();
    };
    if store.transaction_client().is_none() {
        return SqlError::InsufficientPrivilege {
            message: format!(
                "permission denied to set session authorization \"{}\"",
                target
            ),
        }
        .into();
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(session_auth_different_user_error(target, store))
    })
}

pub(crate) fn check_reserved_guc_reset(name: &str) -> Result<()> {
    match classify_guc(name) {
        GucKind::ReadOnlyPseudo => Err(SqlError::CantChangeRuntimeParam {
            message: "parameter \"is_superuser\" cannot be changed".to_string(),
        }
        .into()),
        GucKind::SessionAuthPseudo => Ok(()),
        GucKind::SearchPath | GucKind::Known | GucKind::UnknownCompat => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        check_reserved_guc_reset, check_reserved_guc_write, check_session_auth_write, classify_guc,
        GucKind,
    };

    #[test]
    fn classify_reserved_and_compat_names() {
        assert_eq!(classify_guc("is_superuser"), GucKind::ReadOnlyPseudo);
        assert_eq!(
            classify_guc("session_authorization"),
            GucKind::SessionAuthPseudo
        );
        assert_eq!(classify_guc("search_path"), GucKind::SearchPath);
        assert_eq!(classify_guc("statement_timeout"), GucKind::Known);
        assert_eq!(
            classify_guc("session.authorization"),
            GucKind::UnknownCompat
        );
    }

    #[test]
    fn reserved_pseudo_write_and_reset_rules_match_pg_compat() {
        use crate::sql::error::SqlError;

        let err = check_reserved_guc_write("is_superuser").unwrap_err();
        assert!(err
            .to_string()
            .contains("parameter \"is_superuser\" cannot be changed"));
        // SQLSTATE must be 55P02 (cant_change_runtime_param), not 22023.
        let sql_err = err.downcast_ref::<SqlError>().expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "55P02");

        let reset_err = check_reserved_guc_reset("is_superuser").unwrap_err();
        let sql_err = reset_err
            .downcast_ref::<SqlError>()
            .expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "55P02");

        // session_authorization is no longer hard-blocked by check_reserved_guc_write;
        // per-callsite check_session_auth_write handles it.
        check_reserved_guc_write("session_authorization").unwrap();

        check_reserved_guc_reset("session_authorization").unwrap();
    }

    #[test]
    fn session_auth_write_allows_same_user() {
        check_session_auth_write("session_authorization", "postgres", "postgres").unwrap();
    }

    #[test]
    fn session_auth_write_rejects_different_user() {
        let err = check_session_auth_write("session_authorization", "evil_user", "postgres")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("permission denied to set session authorization"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn session_auth_write_rejects_mixed_case_same_user() {
        // PostgreSQL uses case-sensitive comparison for session_authorization.
        let err = check_session_auth_write("session_authorization", "Postgres", "postgres")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("permission denied to set session authorization"),
            "mixed-case should be rejected: {err}"
        );
    }

    #[test]
    fn session_auth_write_ignores_non_session_auth_params() {
        // Non-session_authorization params pass through without error.
        check_session_auth_write("statement_timeout", "anything", "postgres").unwrap();
    }
}
