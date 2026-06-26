use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::auth::fs_plane_token::{fs_plane_access_for, Fs9Access, Fs9Principal};
use crate::auth::{dispatch_db9_auth, AuthManager, Db9AuthDispatchFailure};
use crate::config;
use crate::extensions::fs::backend::{init_backend_with_args_for_database, FsBackend};
use crate::extensions::fs::config::fs9_config;
use crate::extensions::fs::ws::protocol::{WsErrorCode, WsResponse};
use crate::extensions::fs::ws::tenant_from_keyspace;
use crate::pool::{TenantHandle, TikvClientPool};
use crate::protocol::parse_tenant_username;
use tokio::sync::{Mutex as TokioMutex, OwnedSemaphorePermit, Semaphore};

/// File-system access mode derived from the authenticated PG role.
///
/// Determined once at session creation via [`access_mode_for_role`] and immutable for the
/// lifetime of the WebSocket connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsAccessMode {
    ReadOnly,
    ReadWrite,
}

impl From<Fs9Access> for FsAccessMode {
    fn from(value: Fs9Access) -> Self {
        match value {
            Fs9Access::ReadWrite => FsAccessMode::ReadWrite,
            Fs9Access::ReadOnly => FsAccessMode::ReadOnly,
        }
    }
}

/// Derive the WS session's access mode from privilege facts. Delegates
/// to [`crate::auth::fs_plane_token::fs_plane_access_for`] — the same
/// single source of truth that the SQL fs9 backend init consults — so
/// SQL and WS share one rule. `is_superuser` lets a custom-named
/// superuser (`postgres`, `svc_admin`, etc.) pass; name matching is
/// reserved for the fixed system read-only role.
pub(crate) fn access_mode_for_principal(
    is_superuser: bool,
    role: &str,
) -> Result<FsAccessMode, String> {
    fs_plane_access_for(is_superuser, role)
        .map(FsAccessMode::from)
        .ok_or_else(|| {
            format!(
                "fs9: role \"{role}\" has no fs-plane capability \
                 (not a superuser and not the system read-only role)"
            )
        })
}

pub(crate) struct WsSession {
    pub(crate) _tenant_handle: TenantHandle,
    pub(crate) backend: Arc<dyn FsBackend>,
    pub(crate) user: String,
    pub(crate) keyspace: String,
    pub(crate) database_id: u64,
    pub(crate) database_name: String,
    pub(crate) access_mode: FsAccessMode,
    upload_slots: Arc<Semaphore>,
    inflight_uploads: TokioMutex<HashMap<String, OwnedSemaphorePermit>>,
}

impl WsSession {
    /// Test-only constructor that creates a session with a mock backend.
    /// Avoids needing a real TenantHandle / TikvStore for unit tests.
    #[cfg(test)]
    pub(crate) fn new_for_test(backend: Arc<dyn FsBackend>) -> Self {
        Self::new_for_test_with_mode(backend, FsAccessMode::ReadWrite)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_with_mode(
        backend: Arc<dyn FsBackend>,
        access_mode: FsAccessMode,
    ) -> Self {
        Self {
            _tenant_handle: TenantHandle::dummy_for_test(),
            backend,
            user: "test_user".to_string(),
            keyspace: "db9_tenant_test".to_string(),
            database_id: 42,
            database_name: "postgres".to_string(),
            access_mode,
            upload_slots: Arc::new(Semaphore::new(16)),
            inflight_uploads: TokioMutex::new(HashMap::new()),
        }
    }

    /// Build the auth success response data. This is the single source of
    /// truth for the auth response JSON shape — used by both the WebSocket
    /// handler and contract tests.
    pub(crate) fn build_auth_success_data(&self) -> serde_json::Value {
        let mut capabilities: Vec<&str> = vec!["watch"];
        if self.backend.supports_batch_write_atomic() {
            capabilities.push("batch_write_atomic");
        }
        if !self.backend.supports_presigned() {
            capabilities.push("streaming_only");
        }
        serde_json::json!({
            "user": self.user,
            "tenant": tenant_from_keyspace(&self.keyspace),
            "keyspace": self.keyspace,
            "database": self.database_name,
            "database_id": self.database_id,
            "capabilities": capabilities,
        })
    }

    pub(crate) fn try_acquire_upload_slot(&self) -> Option<OwnedSemaphorePermit> {
        self.upload_slots.clone().try_acquire_owned().ok()
    }

    pub(crate) async fn register_inflight_upload(
        &self,
        upload_token: String,
        permit: OwnedSemaphorePermit,
    ) {
        let mut guard = self.inflight_uploads.lock().await;
        guard.insert(upload_token, permit);
    }

    pub(crate) async fn release_inflight_upload(&self, upload_token: &str) {
        let mut guard = self.inflight_uploads.lock().await;
        guard.remove(upload_token);
    }
}

pub(crate) async fn handle_auth(
    id: &str,
    username: &str,
    password: &str,
    database: Option<&str>,
    pool: &TikvClientPool,
    default_keyspace: Option<&str>,
    is_secure: bool,
) -> Result<WsSession, WsResponse> {
    let (keyspace, actual_user) = resolve_auth_target(username, default_keyspace);
    let database_name = resolve_database_name(database);

    let auth_mode = config::db9_auth_mode();
    let dev_mode = config::env_bool("DB9_DEV");
    let insecure_mode = config::env_bool("DB9_INSECURE");
    if matches!(
        auth_mode,
        config::Db9AuthMode::Both | config::Db9AuthMode::Token
    ) && !is_secure
        && !dev_mode
        && !insecure_mode
    {
        return Err(WsResponse::error(
            id,
            WsErrorCode::Eauth,
            format!(
                "TLS is required for token authentication (DB9_AUTH_MODE={}). Reconnect using WSS and ensure server TLS is configured (PG_TLS_CERT/PG_TLS_KEY).",
                auth_mode.canonical_name()
            ),
        ));
    }

    let tenant_handle = pool.acquire(Some(keyspace.clone())).await.map_err(|err| {
        WsResponse::error(id, WsErrorCode::Eio, format!("pool acquire failed: {err}"))
    })?;

    let store = tenant_handle.store().clone();
    let auth_manager = AuthManager::new();

    if !auth_manager.is_initialized(&store).await.unwrap_or(false) {
        let mut bootstrap_txn = store.begin().await.map_err(|err| {
            WsResponse::error(id, WsErrorCode::Eio, format!("txn begin failed: {err}"))
        })?;
        match auth_manager.bootstrap(&mut bootstrap_txn).await {
            Ok(()) => {
                if let Err(err) = bootstrap_txn.commit().await {
                    let _ = bootstrap_txn.rollback().await;
                    if !auth_manager.is_initialized(&store).await.unwrap_or(false) {
                        return Err(WsResponse::error(
                            id,
                            WsErrorCode::Eio,
                            format!("txn commit failed: {err}"),
                        ));
                    }
                }
            }
            Err(err) => {
                let _ = bootstrap_txn.rollback().await;
                if !auth_manager.is_initialized(&store).await.unwrap_or(false) {
                    return Err(WsResponse::error(
                        id,
                        WsErrorCode::Eio,
                        format!("auth bootstrap failed: {err}"),
                    ));
                }
            }
        }
    }

    let mut auth_txn = store.begin().await.map_err(|err| {
        WsResponse::error(id, WsErrorCode::Eio, format!("txn begin failed: {err}"))
    })?;

    let (success, failure) = match dispatch_db9_auth(
        &auth_manager,
        &mut auth_txn,
        auth_mode,
        &keyspace,
        &actual_user,
        password,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            let _ = auth_txn.rollback().await;
            return Err(WsResponse::error(
                id,
                WsErrorCode::Eio,
                format!("authentication query failed: {err}"),
            ));
        }
    };

    let success = match success {
        Some(s) => s,
        None => {
            let _ = auth_txn.rollback().await;
            let response = map_auth_failure(id, &actual_user, failure);
            return Err(response);
        }
    };

    if !success.user.can_login {
        let _ = auth_txn.rollback().await;
        return Err(WsResponse::error(
            id,
            WsErrorCode::Eauth,
            format!("role \"{actual_user}\" is not permitted to log in"),
        ));
    }

    // Determine fs access mode from verified privilege facts, not
    // from the role name string. A custom-named superuser
    // (`DB9_BOOTSTRAP_ADMIN_USER=postgres` or any later
    // `CREATE ROLE ... SUPERUSER`) gets the same ReadWrite tier a
    // session named `admin` would, matching SQL fs9_* perms.
    let access_mode = match access_mode_for_principal(success.user.is_superuser, &actual_user) {
        Ok(mode) => mode,
        Err(msg) => {
            let _ = auth_txn.rollback().await;
            return Err(WsResponse::error(id, WsErrorCode::Eacces, msg));
        }
    };

    auth_txn.commit().await.map_err(|err| {
        WsResponse::error(id, WsErrorCode::Eio, format!("txn commit failed: {err}"))
    })?;

    let database_id = resolve_database_id(id, &store, &database_name).await?;

    let client = store.transaction_client().ok_or_else(|| {
        WsResponse::error(
            id,
            WsErrorCode::Eio,
            "failed to initialize fs backend: missing transaction client",
        )
    })?;

    // Plumb identity + capability together so the fs-plane mint sees
    // both (and the gRPC backend reaches the same scope `access_mode`
    // already enforces at WS layer). Two enforcement layers for one
    // access decision.
    let principal = Some(Fs9Principal {
        role: actual_user.clone(),
        access: match access_mode {
            FsAccessMode::ReadWrite => Fs9Access::ReadWrite,
            FsAccessMode::ReadOnly => Fs9Access::ReadOnly,
        },
    });

    let backend =
        init_backend_with_args_for_database(&keyspace, client, principal, Some(database_id))
            .await
            .map_err(|err| {
                WsResponse::error(
                    id,
                    WsErrorCode::Eio,
                    format!("failed to initialize fs backend: {err}"),
                )
            })?;

    Ok(WsSession {
        _tenant_handle: tenant_handle,
        backend,
        user: actual_user,
        keyspace,
        database_id,
        database_name,
        access_mode,
        upload_slots: Arc::new(Semaphore::new(
            fs9_config().ws_max_inflight_uploads_per_connection,
        )),
        inflight_uploads: TokioMutex::new(HashMap::new()),
    })
}

fn resolve_database_name(database: Option<&str>) -> String {
    let database = database.unwrap_or("postgres").trim();
    if database.is_empty() {
        "postgres".to_string()
    } else {
        database.to_ascii_lowercase()
    }
}

fn default_database_bootstrap_owner() -> String {
    config::env_string("DB9_BOOTSTRAP_ADMIN_USER").unwrap_or_else(|| "admin".to_string())
}

async fn resolve_database_id(
    id: &str,
    store: &crate::storage::TikvStore,
    database_name: &str,
) -> Result<u64, WsResponse> {
    let database_id = if database_name == "postgres" {
        store
            .ensure_default_database_visible(&default_database_bootstrap_owner())
            .await
            .map_err(|err| {
                WsResponse::error(
                    id,
                    WsErrorCode::Eio,
                    format!("database lookup failed: {err}"),
                )
            })?
    } else {
        match store
            .lookup_database_id(database_name)
            .await
            .map_err(|err| {
                WsResponse::error(
                    id,
                    WsErrorCode::Eio,
                    format!("database lookup failed: {err}"),
                )
            })? {
            Some(database_id) => database_id,
            None => {
                return Err(WsResponse::error(
                    id,
                    WsErrorCode::Enoent,
                    format!("database \"{database_name}\" does not exist"),
                ));
            }
        }
    };

    crate::worker::database_lifecycle::ensure_database_lifecycle_accepts_traffic().map_err(
        |err| {
            WsResponse::error(
                id,
                WsErrorCode::Eio,
                format!("database lifecycle lease check failed: {err}"),
            )
        },
    )?;

    if store.database_active(database_id).await.map_err(|err| {
        WsResponse::error(
            id,
            WsErrorCode::Eio,
            format!("database lifecycle check failed: {err}"),
        )
    })? {
        Ok(database_id)
    } else {
        Err(WsResponse::error(
            id,
            WsErrorCode::Enoent,
            format!("database \"{database_name}\" does not exist"),
        ))
    }
}

fn resolve_auth_target(username: &str, default_keyspace: Option<&str>) -> (String, String) {
    let (parsed_keyspace, actual_user) = parse_tenant_username(username);
    let keyspace = parsed_keyspace
        .or_else(|| default_keyspace.map(ToOwned::to_owned))
        .unwrap_or_else(|| "default".to_string());
    (keyspace, actual_user)
}

fn map_auth_failure(id: &str, user: &str, failure: Option<Db9AuthDispatchFailure>) -> WsResponse {
    match failure {
        Some(Db9AuthDispatchFailure::TokenRequired) => WsResponse::error(
            id,
            WsErrorCode::Eauth,
            "token authentication required (DB9_AUTH_MODE=token)",
        ),
        Some(Db9AuthDispatchFailure::JwtFailed(err)) => WsResponse::error(
            id,
            WsErrorCode::Eauth,
            format!("token authentication failed for user \"{user}\": {err}"),
        ),
        Some(Db9AuthDispatchFailure::JwtUserNotFound) => WsResponse::error(
            id,
            WsErrorCode::Eauth,
            format!("token authentication failed for user \"{user}\""),
        ),
        Some(Db9AuthDispatchFailure::ConnectKeyFailed(err)) => WsResponse::error(
            id,
            WsErrorCode::Eauth,
            format!("connect-key authentication failed for user \"{user}\": {err}"),
        ),
        Some(Db9AuthDispatchFailure::ConnectKeyUserNotFound) => WsResponse::error(
            id,
            WsErrorCode::Eauth,
            format!("connect-key authentication failed for user \"{user}\""),
        ),
        _ => WsResponse::error(
            id,
            WsErrorCode::Eauth,
            format!("authentication failed for user \"{user}\""),
        ),
    }
}

pub(crate) struct WsConnectionTracker {
    counts: RwLock<HashMap<String, Arc<AtomicU32>>>,
    max_per_tenant: u32,
}

pub(crate) struct WsConnectionGuard {
    #[allow(dead_code)]
    keyspace: String,
    counter: Arc<AtomicU32>,
}

impl WsConnectionTracker {
    pub(crate) fn new(max_per_tenant: u32) -> Self {
        Self {
            counts: RwLock::new(HashMap::new()),
            max_per_tenant,
        }
    }

    pub(crate) fn try_acquire(&self, keyspace: &str) -> Result<WsConnectionGuard, WsResponse> {
        let counter = {
            let mut counts = self.counts.write();
            counts
                .entry(keyspace.to_string())
                .or_insert_with(|| Arc::new(AtomicU32::new(0)))
                .clone()
        };

        loop {
            let current = counter.load(Ordering::Relaxed);
            if current >= self.max_per_tenant {
                return Err(WsResponse::error(
                    "",
                    WsErrorCode::Eacces,
                    format!(
                        "fs9: too many websocket connections for keyspace '{keyspace}' (limit: {})",
                        self.max_per_tenant
                    ),
                ));
            }

            if counter
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(WsConnectionGuard {
                    keyspace: keyspace.to_string(),
                    counter,
                });
            }
        }
    }
}

impl WsConnectionGuard {
    #[cfg(test)]
    fn keyspace(&self) -> &str {
        &self.keyspace
    }
}

impl Drop for WsConnectionGuard {
    fn drop(&mut self) {
        loop {
            let current = self.counter.load(Ordering::Relaxed);
            if current == 0 {
                break;
            }

            if self
                .counter
                .compare_exchange_weak(current, current - 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_count(tracker: &WsConnectionTracker, keyspace: &str) -> u32 {
        let counts = tracker.counts.read();
        counts
            .get(keyspace)
            .map(|counter| counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    #[test]
    fn test_connection_tracker_basic() {
        let tracker = WsConnectionTracker::new(3);

        let guard1 = tracker
            .try_acquire("db9_tenant_alpha")
            .expect("first acquire should succeed");
        assert_eq!(guard1.keyspace(), "db9_tenant_alpha");
        assert_eq!(active_count(&tracker, "db9_tenant_alpha"), 1);

        let guard2 = tracker
            .try_acquire("db9_tenant_alpha")
            .expect("second acquire should succeed");
        assert_eq!(active_count(&tracker, "db9_tenant_alpha"), 2);

        drop(guard2);
        assert_eq!(active_count(&tracker, "db9_tenant_alpha"), 1);

        drop(guard1);
        assert_eq!(active_count(&tracker, "db9_tenant_alpha"), 0);
    }

    #[test]
    fn test_connection_tracker_limit() {
        let tracker = WsConnectionTracker::new(2);

        let _guard1 = tracker
            .try_acquire("db9_tenant_beta")
            .expect("first acquire should succeed");
        let _guard2 = tracker
            .try_acquire("db9_tenant_beta")
            .expect("second acquire should succeed");

        let err = match tracker.try_acquire("db9_tenant_beta") {
            Ok(_) => panic!("third acquire should hit the limit"),
            Err(err) => err,
        };

        assert!(!err.ok);
        let detail = err.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Eacces);
    }

    #[test]
    fn test_map_auth_failure_jwt_user_not_found() {
        let resp = map_auth_failure("r1", "alice", Some(Db9AuthDispatchFailure::JwtUserNotFound));
        assert!(!resp.ok);
        let detail = resp.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Eauth);
        assert_eq!(
            detail.message,
            "token authentication failed for user \"alice\""
        );
    }

    #[test]
    fn test_map_auth_failure_connect_key_user_not_found() {
        let resp = map_auth_failure(
            "r2",
            "bob",
            Some(Db9AuthDispatchFailure::ConnectKeyUserNotFound),
        );
        assert!(!resp.ok);
        let detail = resp.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Eauth);
        assert_eq!(
            detail.message,
            "connect-key authentication failed for user \"bob\""
        );
    }

    #[test]
    fn test_map_auth_failure_generic_fallback() {
        let resp = map_auth_failure("r3", "carol", None);
        assert!(!resp.ok);
        let detail = resp.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Eauth);
        assert_eq!(detail.message, "authentication failed for user \"carol\"");
    }

    #[test]
    fn test_connection_tracker_guard_drop() {
        let tracker = WsConnectionTracker::new(1);

        {
            let _guard = tracker
                .try_acquire("db9_tenant_gamma")
                .expect("acquire should succeed");
            assert_eq!(active_count(&tracker, "db9_tenant_gamma"), 1);
        }

        assert_eq!(active_count(&tracker, "db9_tenant_gamma"), 0);

        let _guard_again = tracker
            .try_acquire("db9_tenant_gamma")
            .expect("acquire should succeed again after drop");
        assert_eq!(active_count(&tracker, "db9_tenant_gamma"), 1);
    }

    #[test]
    fn test_resolve_auth_target_uses_server_default_keyspace() {
        let (keyspace, user) = resolve_auth_target("admin", Some("db9_tenant_smoke"));
        assert_eq!(keyspace, "db9_tenant_smoke");
        assert_eq!(user, "admin");
    }

    #[test]
    fn test_resolve_auth_target_prefers_explicit_tenant_over_server_default() {
        let (keyspace, user) = resolve_auth_target("tenant_a.admin", Some("db9_tenant_smoke"));
        assert_eq!(keyspace, "db9_tenant_tenant_a");
        assert_eq!(user, "admin");
    }

    #[test]
    fn test_resolve_auth_target_falls_back_to_default_keyspace_name() {
        let (keyspace, user) = resolve_auth_target("admin", None);
        assert_eq!(keyspace, "default");
        assert_eq!(user, "admin");
    }

    #[test]
    fn test_resolve_database_name_defaults_and_normalizes() {
        assert_eq!(resolve_database_name(None), "postgres");
        assert_eq!(resolve_database_name(Some("")), "postgres");
        assert_eq!(resolve_database_name(Some("  ")), "postgres");
        assert_eq!(resolve_database_name(Some("AppDB")), "appdb");
        assert_eq!(resolve_database_name(Some(" appdb ")), "appdb");
    }

    #[test]
    fn test_access_mode_superuser_is_readwrite() {
        // Capability, not name. Any superuser → ReadWrite.
        assert_eq!(
            access_mode_for_principal(true, "admin").unwrap(),
            FsAccessMode::ReadWrite
        );
    }

    /// PR #2547 review #1 regression: a deployment bootstrapped with a
    /// non-`admin` superuser name (or any later
    /// `CREATE ROLE ... SUPERUSER`) must still pass the WS access-mode
    /// gate. Previously WS rejected with `Eacces` because the role
    /// string didn't literally match `"admin"`.
    #[test]
    fn test_access_mode_custom_named_superuser_is_readwrite() {
        for role in ["postgres", "svc_admin", "alice", "ops_team"] {
            assert_eq!(
                access_mode_for_principal(true, role).unwrap(),
                FsAccessMode::ReadWrite,
                "superuser named {role:?} must be ReadWrite"
            );
        }
    }

    #[test]
    fn test_access_mode_sys_readonly_role() {
        assert_eq!(
            access_mode_for_principal(false, "_db9_sys_readonly").unwrap(),
            FsAccessMode::ReadOnly
        );
    }

    #[test]
    fn test_access_mode_non_superuser_other_role_rejected() {
        let err = access_mode_for_principal(false, "mysterious_user").unwrap_err();
        assert!(
            err.contains("no fs-plane capability"),
            "expected capability rejection, got: {err}"
        );
    }
}
