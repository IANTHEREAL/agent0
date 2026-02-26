use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use crate::auth::AuthManager;
use crate::extensions::fs::backend::FsBackend;
use crate::extensions::fs::embedded::EmbeddedFsBackend;
use crate::extensions::fs::ws::protocol::{WsErrorCode, WsResponse};
use crate::pool::{TenantHandle, TikvClientPool};
use crate::protocol::parse_tenant_username;

pub(crate) struct WsSession {
    pub(crate) _tenant_handle: TenantHandle,
    pub(crate) backend: Box<dyn FsBackend>,
    pub(crate) user: String,
    pub(crate) keyspace: String,
}

pub(crate) async fn handle_auth(
    id: &str,
    username: &str,
    password: &str,
    pool: &TikvClientPool,
) -> Result<WsSession, WsResponse> {
    let (parsed_keyspace, actual_user) = parse_tenant_username(username);
    let keyspace = parsed_keyspace.unwrap_or_else(|| "default".to_string());

    let tenant_handle = pool.acquire(Some(keyspace.clone())).await.map_err(|err| {
        WsResponse::error(id, WsErrorCode::Eio, format!("pool acquire failed: {err}"))
    })?;

    let store = tenant_handle.store().clone();
    let auth_manager = AuthManager::new();

    {
        let mut bootstrap_txn = store.begin().await.map_err(|err| {
            WsResponse::error(id, WsErrorCode::Eio, format!("txn begin failed: {err}"))
        })?;
        auth_manager
            .bootstrap(&mut bootstrap_txn)
            .await
            .map_err(|err| {
                WsResponse::error(
                    id,
                    WsErrorCode::Eio,
                    format!("auth bootstrap failed: {err}"),
                )
            })?;
        bootstrap_txn.commit().await.map_err(|err| {
            WsResponse::error(id, WsErrorCode::Eio, format!("txn commit failed: {err}"))
        })?;
    }

    let mut auth_txn = store.begin().await.map_err(|err| {
        WsResponse::error(id, WsErrorCode::Eio, format!("txn begin failed: {err}"))
    })?;

    let user = auth_manager
        .authenticate(&mut auth_txn, &actual_user, password)
        .await
        .map_err(|err| {
            WsResponse::error(
                id,
                WsErrorCode::Eio,
                format!("authentication query failed: {err}"),
            )
        })?;

    let user = match user {
        Some(user) => user,
        None => {
            let _ = auth_txn.rollback().await;
            return Err(WsResponse::error(
                id,
                WsErrorCode::Eauth,
                format!("password authentication failed for user \"{actual_user}\""),
            ));
        }
    };

    if !user.is_superuser {
        let _ = auth_txn.rollback().await;
        return Err(WsResponse::error(
            id,
            WsErrorCode::Eacces,
            "fs9: permission denied (superuser required)",
        ));
    }

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

    let backend = EmbeddedFsBackend::new(client).await.map_err(|err| {
        WsResponse::error(
            id,
            WsErrorCode::Eio,
            format!("failed to initialize fs backend: {err}"),
        )
    })?;

    Ok(WsSession {
        _tenant_handle: tenant_handle,
        backend: Box::new(backend),
        user: actual_user,
        keyspace,
    })
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
            let mut counts = self.counts.write().map_err(|_| {
                WsResponse::error(
                    "",
                    WsErrorCode::Eio,
                    "fs9: connection tracker lock poisoned",
                )
            })?;
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
        let counts = tracker
            .counts
            .read()
            .expect("connection tracker lock should not be poisoned");
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
}
