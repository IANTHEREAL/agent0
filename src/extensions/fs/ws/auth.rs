use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::auth::{dispatch_db9_auth, AuthManager, Db9AuthDispatchFailure};
use crate::config;
use crate::extensions::fs::backend::FsBackend;
use crate::extensions::fs::config::fs9_config;
use crate::extensions::fs::embedded::EmbeddedFsBackend;
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

/// Centralized role → access-mode mapping.
///
/// Design constraints (locked 2026-03-27):
/// - Single source of truth — no scattered role checks elsewhere.
/// - Unknown roles fail-closed (reject).
pub(crate) fn access_mode_for_role(role: &str) -> Result<FsAccessMode, String> {
    match role {
        "_db9_sys_readonly" => Ok(FsAccessMode::ReadOnly),
        "admin" => Ok(FsAccessMode::ReadWrite),
        _ => Err(format!(
            "fs9: unknown role \"{role}\" — cannot determine access mode"
        )),
    }
}

pub(crate) struct WsSession {
    pub(crate) _tenant_handle: TenantHandle,
    pub(crate) backend: Arc<dyn FsBackend>,
    pub(crate) user: String,
    pub(crate) keyspace: String,
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
        serde_json::json!({
            "user": self.user,
            "tenant": tenant_from_keyspace(&self.keyspace),
            "keyspace": self.keyspace,
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
    pool: &TikvClientPool,
    default_keyspace: Option<&str>,
    is_secure: bool,
) -> Result<WsSession, WsResponse> {
    let (keyspace, actual_user) = resolve_auth_target(username, default_keyspace);

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

    let (user, _trusted_jwt_claims, failure) = match dispatch_db9_auth(
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

    let user = match user {
        Some(user) => user,
        None => {
            let _ = auth_txn.rollback().await;
            let response = map_auth_failure(id, &actual_user, failure);
            return Err(response);
        }
    };

    if !user.can_login {
        let _ = auth_txn.rollback().await;
        return Err(WsResponse::error(
            id,
            WsErrorCode::Eauth,
            format!("role \"{actual_user}\" is not permitted to log in"),
        ));
    }

    // Determine fs access mode from the verified PG role identity.
    // This is the single source of truth for read/write permissions on this session.
    // Unknown roles are rejected (fail-closed).
    let access_mode = match access_mode_for_role(&actual_user) {
        Ok(mode) => mode,
        Err(msg) => {
            let _ = auth_txn.rollback().await;
            return Err(WsResponse::error(id, WsErrorCode::Eacces, msg));
        }
    };

    auth_txn.commit().await.map_err(|err| {
        WsResponse::error(id, WsErrorCode::Eio, format!("txn commit failed: {err}"))
    })?;

    let client = store.transaction_client().ok_or_else(|| {
        WsResponse::error(
            id,
            WsErrorCode::Eio,
            "failed to initialize fs backend: missing transaction client",
        )
    })?;

    let backend = EmbeddedFsBackend::new(client, keyspace.clone())
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
        backend: Arc::new(backend),
        user: actual_user,
        keyspace,
        access_mode,
        upload_slots: Arc::new(Semaphore::new(
            fs9_config().ws_max_inflight_uploads_per_connection,
        )),
        inflight_uploads: TokioMutex::new(HashMap::new()),
    })
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
    fn test_access_mode_admin_is_readwrite() {
        assert_eq!(
            access_mode_for_role("admin").unwrap(),
            FsAccessMode::ReadWrite
        );
    }

    #[test]
    fn test_access_mode_readonly_role() {
        assert_eq!(
            access_mode_for_role("_db9_sys_readonly").unwrap(),
            FsAccessMode::ReadOnly
        );
    }

    #[test]
    fn test_access_mode_unknown_role_rejected() {
        let err = access_mode_for_role("mysterious_user").unwrap_err();
        assert!(err.contains("unknown role"));
    }
}
