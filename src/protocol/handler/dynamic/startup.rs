//! Startup and authentication handling for [`DynamicPgHandler`].
//!
//! Contains `init_executor`, `authenticate_user`, and the
//! [`StartupHandler`] trait implementation.

use super::{AuthenticatedState, DynamicPgHandler};
use crate::auth::{dispatch_db9_auth, AuthManager, Db9AuthDispatchFailure};
use crate::config;
use crate::observability;
use crate::sql::{Executor, Session};
use crate::storage::TikvStore;
use anyhow::Context as _;
use async_trait::async_trait;
use futures::{Sink, SinkExt};
use pgwire::api::auth::StartupHandler;
use pgwire::api::{ClientInfo, PgWireConnectionState, METADATA_DATABASE, METADATA_USER};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use std::fmt::Debug;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use super::super::errors::sqlstate_for_executor_error;
use super::super::tenant::parse_tenant_username;
use super::super::{
    startup_setting_overrides, PgServerParameterProvider, METADATA_ACTUAL_USER,
    METADATA_AUTH_IS_SUPERUSER, METADATA_KEYSPACE,
};

/// Result of user authentication.
pub(in crate::protocol::handler) struct AuthResult {
    pub is_authenticated: bool,
    pub is_superuser: bool,
    pub bypass_rls: bool,
    /// PostgreSQL `rolconnlimit`. Negative means unlimited.
    pub connection_limit: i32,
    pub failure_reason: Option<String>,
}

impl DynamicPgHandler {
    pub(in crate::protocol::handler) async fn init_executor(
        &self,
        keyspace: Option<String>,
        username: Option<String>,
        is_superuser: bool,
        bypass_rls: bool,
        database: String,
        connection_limit: i32,
    ) -> PgWireResult<()> {
        let fatal_internal = |message: String| -> PgWireError {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_owned(),
                "XX000".to_owned(),
                message,
            )))
        };

        let effective_keyspace = keyspace
            .or_else(|| self.default_keyspace.clone())
            .unwrap_or_else(|| "default".to_string());

        let tenant_obs = observability::registry().tenant(&effective_keyspace);
        if self.connection_guard.get().is_none() {
            let _ = self.connection_guard.set(tenant_obs.connection_open());
        }

        let (store, trigger_cache, rls_policy_cache, stats_cache, memory_accountant) =
            if let Some(pool) = &self.client_pool {
                let mut handle = pool
                    .acquire(Some(effective_keyspace.clone()))
                    .await
                    .map_err(|e| {
                        fatal_internal(format!("Failed to get client from pool: {}", e))
                    })?;
                if let Some(ref user) = username {
                    handle
                        .try_bind_user(user.clone(), connection_limit)
                        .map_err(|msg| {
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "FATAL".to_owned(),
                                "53300".to_owned(),
                                msg,
                            )))
                        })?;
                }
                let s = handle.store().clone();
                let tc = handle.trigger_cache().clone();
                let rpc = handle.rls_policy_cache().clone();
                let sc = handle.stats_cache().clone();
                let ma = handle.memory_accountant();
                let _ = self.tenant_handle.set(handle);
                (s, tc, rpc, sc, ma)
            } else {
                use crate::sql::rls::cache::RlsPolicyCache;
                use crate::sql::stats::TableStatsCache;
                use crate::sql::triggers::TriggerBodyCache;
                let s = TikvStore::new_with_keyspace(
                    self.pd_endpoints.clone(),
                    Some(effective_keyspace.clone()),
                )
                .await
                .map_err(|e| fatal_internal(format!("Failed to connect to TiKV: {}", e)))?;
                (
                    Arc::new(s),
                    Arc::new(TriggerBodyCache::new()),
                    Arc::new(RlsPolicyCache::new()),
                    Arc::new(TableStatsCache::new()),
                    crate::pool::TenantMemoryAccountant::unlimited(effective_keyspace.clone()),
                )
            };

        let executor = Arc::new(Executor::new(
            store.clone(),
            effective_keyspace.clone(),
            tenant_obs.clone(),
            memory_accountant,
            trigger_cache,
            rls_policy_cache,
            stats_cache,
        ));

        let database_name = database.trim();
        let database_name = if database_name.is_empty() {
            "postgres"
        } else {
            database_name
        };
        let database_name = database_name.to_ascii_lowercase();
        let (default_stmt_timeout, default_idle_txn_timeout) = {
            let cfg = self.server_config.read().unwrap();
            (
                cfg.statement_timeout_ms,
                cfg.idle_in_transaction_session_timeout_ms,
            )
        };

        let mut db_txn = store
            .begin_optimistic()
            .await
            .map_err(|e| fatal_internal(e.to_string()))?;
        let database_id = match store
            .get_database_id(&mut db_txn, &database_name)
            .await
            .map_err(|e| fatal_internal(e.to_string()))?
        {
            Some(id) => id,
            None => {
                if let Err(e) = db_txn.rollback().await {
                    warn!("rollback failed during database lookup: {}", e);
                }
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "FATAL".to_owned(),
                    "3D000".to_owned(),
                    format!("database \"{}\" does not exist", database_name),
                ))));
            }
        };
        if let Err(e) = db_txn.rollback().await {
            warn!("rollback failed after database lookup: {}", e);
        }

        let mut session = match username {
            Some(user) => Session::new_with_user_and_database(
                store,
                tenant_obs,
                user,
                is_superuser,
                bypass_rls,
                self.connection_id,
                database_id,
                database_name,
                default_stmt_timeout,
                default_idle_txn_timeout,
            ),
            None => Session::new_with_database(
                store,
                tenant_obs,
                self.connection_id,
                database_id,
                database_name,
                default_stmt_timeout,
                default_idle_txn_timeout,
            ),
        };
        session.set_server_config(self.server_config.clone());

        self.auth_state
            .set(AuthenticatedState {
                executor,
                session: Arc::new(tokio::sync::Mutex::new(session)),
            })
            .map_err(|_| {
                fatal_internal(
                    "Internal error: executor already initialized (double authentication)"
                        .to_string(),
                )
            })?;

        debug!(
            "Initialized executor with keyspace: {:?}",
            effective_keyspace
        );
        Ok(())
    }

    pub(in crate::protocol::handler) async fn authenticate_user(
        &self,
        keyspace: &Option<String>,
        username: &str,
        password: &str,
    ) -> Result<AuthResult, anyhow::Error> {
        let effective_keyspace = keyspace
            .clone()
            .or_else(|| self.default_keyspace.clone())
            .or_else(|| Some("default".to_string()));

        let ks_name = effective_keyspace
            .clone()
            .unwrap_or_else(|| "default".to_string());

        let store = if let Some(pool) = &self.client_pool {
            match pool.get_client(effective_keyspace).await {
                Ok(s) => s,
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("does not exist") {
                        error!("Tenant '{}' does not exist (user: {})", ks_name, username);
                    } else {
                        error!("Failed to connect to TiKV for tenant '{}': {}", ks_name, e);
                    }
                    return Ok(AuthResult {
                        is_authenticated: false,
                        is_superuser: false,
                        bypass_rls: false,
                        connection_limit: -1,
                        failure_reason: None,
                    });
                }
            }
        } else {
            match TikvStore::new_with_keyspace(self.pd_endpoints.clone(), effective_keyspace).await
            {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    error!("Failed to connect to TiKV: {}", e);
                    return Ok(AuthResult {
                        is_authenticated: false,
                        is_superuser: false,
                        bypass_rls: false,
                        connection_limit: -1,
                        failure_reason: None,
                    });
                }
            }
        };

        let auth_manager = AuthManager::new();

        // Skip bootstrap write transaction if auth is already initialized.
        if !auth_manager.is_initialized(&store).await.unwrap_or(false) {
            let mut txn = store.begin().await.context("Failed to bootstrap auth")?;
            match auth_manager.bootstrap(&mut txn).await {
                Ok(()) => {
                    if let Err(commit_err) = txn.commit().await {
                        if let Err(e) = txn.rollback().await {
                            tracing::warn!("rollback failed: {e}");
                        }
                        if !auth_manager.is_initialized(&store).await.unwrap_or(false) {
                            return Err(commit_err).context("Failed to bootstrap auth");
                        }
                    }
                }
                Err(e) => {
                    // Race: another connection may have bootstrapped concurrently.
                    // Re-check and proceed if now initialized; otherwise propagate.
                    if let Err(rb_err) = txn.rollback().await {
                        warn!("rollback failed after auth bootstrap error: {}", rb_err);
                    }
                    if !auth_manager.is_initialized(&store).await.unwrap_or(false) {
                        return Err(e.context("Failed to bootstrap auth"));
                    }
                }
            }
        }

        let mut txn = store
            .begin_optimistic()
            .await
            .context("Failed to begin transaction")?;

        let auth_mode = config::db9_auth_mode();
        let auth_outcome: Result<(Option<crate::auth::User>, Option<String>), anyhow::Error> =
            dispatch_db9_auth(
                &auth_manager,
                &mut txn,
                auth_mode,
                &ks_name,
                username,
                password,
            )
            .await
            .and_then(|(user, failure)| {
                if let Some(user) = user.as_ref() {
                    if !user.can_login {
                        return Err(
                            crate::sql::error::SqlError::InvalidAuthorizationSpecification {
                                message: format!(
                                    "role \"{}\" is not permitted to log in",
                                    username
                                ),
                            }
                            .into(),
                        );
                    }
                }

                let failure_reason = match failure {
                    Some(Db9AuthDispatchFailure::TokenRequired) => {
                        Some("Token authentication required (DB9_AUTH_MODE=token)".to_string())
                    }
                    Some(Db9AuthDispatchFailure::JwtFailed(err)) => Some(format!(
                        "Token authentication failed for user \"{username}\": {err}"
                    )),
                    Some(Db9AuthDispatchFailure::JwtUserNotFound) => Some(format!(
                        "Token authentication failed for user \"{username}\""
                    )),
                    Some(Db9AuthDispatchFailure::ConnectKeyFailed(err)) => Some(format!(
                        "Connect-key authentication failed for user \"{username}\": {err}"
                    )),
                    Some(Db9AuthDispatchFailure::ConnectKeyUserNotFound) => Some(format!(
                        "Connect-key authentication failed for user \"{username}\""
                    )),
                    None => None,
                };

                Ok((user, failure_reason))
            });

        match auth_outcome {
            Ok((Some(user), _)) => {
                if let Err(e) = txn.rollback().await {
                    warn!("rollback failed after auth success: {}", e);
                }
                Ok(AuthResult {
                    is_authenticated: true,
                    is_superuser: user.is_superuser,
                    bypass_rls: user.bypass_rls,
                    connection_limit: user.connection_limit,
                    failure_reason: None,
                })
            }
            Ok((None, failure_reason)) => {
                if let Err(e) = txn.rollback().await {
                    warn!("rollback failed after auth rejection: {}", e);
                }
                Ok(AuthResult {
                    is_authenticated: false,
                    is_superuser: false,
                    bypass_rls: false,
                    connection_limit: -1,
                    failure_reason,
                })
            }
            Err(e) => {
                if let Err(rb_err) = txn.rollback().await {
                    warn!("rollback failed after authentication error: {}", rb_err);
                }
                Err(e.context("Authentication error"))
            }
        }
    }
}

#[async_trait]
impl StartupHandler for DynamicPgHandler {
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        match message {
            PgWireFrontendMessage::Startup(ref startup) => {
                pgwire::api::auth::save_startup_parameters_to_metadata(client, startup);

                let raw_user = client
                    .metadata()
                    .get(METADATA_USER)
                    .cloned()
                    .filter(|u| !u.trim().is_empty())
                    .ok_or(PgWireError::UserNameRequired)?;

                let (keyspace, actual_user) = parse_tenant_username(&raw_user);

                if let Some(ks) = &keyspace {
                    client
                        .metadata_mut()
                        .insert(METADATA_KEYSPACE.to_string(), ks.clone());
                    debug!("Extracted keyspace '{}' from username '{}'", ks, raw_user);
                }
                client
                    .metadata_mut()
                    .insert(METADATA_ACTUAL_USER.to_string(), actual_user.clone());
                debug!("Actual user: {}", actual_user);

                client.set_state(PgWireConnectionState::AuthenticationInProgress);

                let require_tls = config::env_bool("PG_REQUIRE_TLS");
                let dev_mode = config::env_bool("DB9_DEV");
                let insecure_mode = config::env_bool("DB9_INSECURE");
                let auth_mode = config::db9_auth_mode();

                if matches!(
                    auth_mode,
                    config::Db9AuthMode::Both | config::Db9AuthMode::Token
                ) && !client.is_secure()
                    && !dev_mode
                    && !insecure_mode
                {
                    let error_info = ErrorInfo::new(
                        "FATAL".to_owned(),
                        "28000".to_owned(),
                        format!(
                            "TLS is required for token authentication (DB9_AUTH_MODE={}). Reconnect with sslmode=require and ensure server TLS is configured (PG_TLS_CERT/PG_TLS_KEY).",
                            auth_mode.canonical_name()
                        ),
                    );
                    return Err(PgWireError::UserError(Box::new(error_info)));
                }

                if require_tls && !client.is_secure() {
                    let error_info = ErrorInfo::new(
                        "FATAL".to_owned(),
                        "28000".to_owned(),
                        "TLS is required (PG_REQUIRE_TLS=1). Reconnect with sslmode=require and ensure server TLS is configured (PG_TLS_CERT/PG_TLS_KEY).".to_string(),
                    );
                    return Err(PgWireError::UserError(Box::new(error_info)));
                }

                if !client.is_secure() {
                    let peer_ip = client.socket_addr().ip();
                    let allow_cleartext = peer_ip.is_loopback() || dev_mode || insecure_mode;
                    if !allow_cleartext {
                        let error_info = ErrorInfo::new(
                            "FATAL".to_owned(),
                            "28000".to_owned(),
                            "Cleartext password authentication without TLS is disabled by default for non-loopback clients. Enable TLS (PG_TLS_CERT/PG_TLS_KEY) or explicitly opt into insecure mode (DB9_DEV=1 or DB9_INSECURE=1).".to_string(),
                        );
                        return Err(PgWireError::UserError(Box::new(error_info)));
                    }

                    if !peer_ip.is_loopback() && (dev_mode || insecure_mode) {
                        warn!(
                            "Allowing non-TLS cleartext auth for non-loopback connection from {} (DB9_DEV={}, DB9_INSECURE={})",
                            peer_ip, dev_mode, insecure_mode
                        );
                    }
                }
                client
                    .send(PgWireBackendMessage::Authentication(
                        Authentication::CleartextPassword,
                    ))
                    .await?;
            }
            PgWireFrontendMessage::PasswordMessageFamily(pwd) => {
                let pwd = pwd.into_password()?;
                let provided_password = pwd.password.clone();
                let keyspace = client.metadata().get(METADATA_KEYSPACE).cloned();
                let actual_user = client
                    .metadata()
                    .get(METADATA_ACTUAL_USER)
                    .cloned()
                    .ok_or(PgWireError::UserNameRequired)?;
                let database = client
                    .metadata()
                    .get(METADATA_DATABASE)
                    .cloned()
                    .unwrap_or_else(|| "postgres".to_string());

                let auth_result = self
                    .authenticate_user(&keyspace, &actual_user, &provided_password)
                    .await;

                match auth_result {
                    Ok(AuthResult {
                        is_authenticated,
                        is_superuser,
                        bypass_rls,
                        connection_limit,
                        failure_reason,
                    }) => {
                        if is_authenticated {
                            self.init_executor(
                                keyspace.clone(),
                                Some(actual_user.clone()),
                                is_superuser,
                                bypass_rls,
                                database,
                                connection_limit,
                            )
                            .await?;

                            {
                                let mut session = self.auth().session.lock().await;
                                for (key, value) in startup_setting_overrides(client) {
                                    if let Err(e) = session.set_known_setting(&key, value) {
                                        warn!("Failed to apply startup option {}: {}", key, e);
                                    }
                                }
                            }

                            client.metadata_mut().insert(
                                METADATA_AUTH_IS_SUPERUSER.to_string(),
                                if is_superuser { "on" } else { "off" }.to_string(),
                            );

                            pgwire::api::auth::finish_authentication(
                                client,
                                &PgServerParameterProvider,
                            )
                            .await?;
                            let peer = client.socket_addr();
                            info!(
                                "New connection from {}:{} user='{}' keyspace='{}'",
                                peer.ip(),
                                peer.port(),
                                actual_user,
                                keyspace.as_deref().unwrap_or("default"),
                            );

                            // Spawn idle-in-transaction watchdog task.
                            // Periodically checks if the session has been idle in a
                            // transaction too long. On timeout: rollback + cancel the
                            // connection so the pgwire loop sends FATAL 25P03.
                            {
                                let session_lock = Arc::clone(&self.auth().session);
                                let cancel = self.cancel_token.clone();
                                tokio::spawn(async move {
                                    idle_in_transaction_watchdog(session_lock, cancel).await;
                                });
                            }
                        } else {
                            let message = failure_reason.unwrap_or_else(|| {
                                format!(
                                    "Password authentication failed for user \"{}\"",
                                    actual_user
                                )
                            });
                            let error_info =
                                ErrorInfo::new("FATAL".to_owned(), "28P01".to_owned(), message);
                            return Err(PgWireError::UserError(Box::new(error_info)));
                        }
                    }
                    Err(e) => {
                        let sqlstate = sqlstate_for_executor_error(&e);
                        let error_info =
                            ErrorInfo::new("FATAL".to_owned(), sqlstate.to_owned(), e.to_string());
                        return Err(PgWireError::UserError(Box::new(error_info)));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Per-connection watchdog that terminates the session when it has been
/// idle in a transaction beyond the configured timeout.
///
/// Runs as a `tokio::spawn`-ed task with `'static` ownership of the session
/// `Arc` and cancel token. Exits when:
///   - Timeout detected (rollback + cancel token)
///   - Connection closed normally (cancel token already cancelled from Drop)
async fn idle_in_transaction_watchdog(
    session_lock: Arc<tokio::sync::Mutex<crate::sql::Session>>,
    cancel: pgwire::tokio::CancellationToken,
) {
    use std::time::Duration;

    // Minimum sleep to avoid busy-looping.
    const MIN_SLEEP: Duration = Duration::from_millis(100);
    // Maximum sleep when timeout is disabled (slow poll).
    const DISABLED_SLEEP: Duration = Duration::from_secs(5);
    // Cap for not-in-transaction polling so we quickly detect BEGIN.
    const IDLE_POLL_CAP: Duration = Duration::from_millis(500);

    loop {
        // Compute how long to sleep before the next check.
        let sleep_dur = {
            let session = session_lock.lock().await;
            let timeout = session.settings().idle_in_transaction_session_timeout();
            match timeout {
                Some(t) if session.is_in_transaction() => {
                    // In a transaction — sleep until the deadline (capped to 1s
                    // for responsiveness).
                    if let Some(remaining) = session.idle_in_transaction_remaining(t) {
                        remaining.max(MIN_SLEEP).min(t.min(Duration::from_secs(1)))
                    } else {
                        // No command completed yet — check at half-timeout.
                        (t / 2).max(MIN_SLEEP).min(t.min(Duration::from_secs(1)))
                    }
                }
                Some(t) => {
                    // Timeout enabled but not in a transaction — short poll so
                    // we detect BEGIN quickly and avoid timeout overshoot.
                    t.min(IDLE_POLL_CAP).max(MIN_SLEEP)
                }
                None => DISABLED_SLEEP, // disabled (0) — slow poll
            }
        };

        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(sleep_dur) => {}
        }

        // Atomically check + rollback + cancel under a single lock guard.
        {
            let mut session = session_lock.lock().await;
            if session.check_idle_in_transaction_timeout().is_err() {
                if let Err(e) = session.rollback().await {
                    tracing::warn!("rollback failed: {e}");
                }
                cancel.cancel();
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AuthResult;

    #[test]
    fn auth_result_authenticated_superuser() {
        let r = AuthResult {
            is_authenticated: true,
            is_superuser: true,
            bypass_rls: false,
            connection_limit: -1,
            failure_reason: None,
        };
        assert!(r.is_authenticated);
        assert!(r.is_superuser);
    }

    #[test]
    fn auth_result_not_authenticated() {
        let r = AuthResult {
            is_authenticated: false,
            is_superuser: false,
            bypass_rls: false,
            connection_limit: -1,
            failure_reason: None,
        };
        assert!(!r.is_authenticated);
        assert!(!r.is_superuser);
    }

    #[test]
    fn auth_result_destructure() {
        let r = AuthResult {
            is_authenticated: true,
            is_superuser: false,
            bypass_rls: false,
            connection_limit: 5,
            failure_reason: None,
        };
        let AuthResult {
            is_authenticated,
            is_superuser,
            connection_limit,
            ..
        } = r;
        assert!(is_authenticated);
        assert!(!is_superuser);
        assert_eq!(connection_limit, 5);
    }

    /// Watchdog terminates a session that has been idle in a transaction
    /// beyond the configured timeout (FATAL 25P03 scenario).
    #[tokio::test]
    async fn watchdog_cancels_idle_in_transaction_session() {
        use super::idle_in_transaction_watchdog;
        use crate::storage::TikvStore;
        use pgwire::tokio::CancellationToken;
        use std::sync::Arc;
        use std::time::Duration;

        let store = TikvStore::new_stub();
        let observability = crate::observability::registry().tenant("watchdog_idle_in_txn_test");
        let mut session = crate::sql::Session::new_with_database(
            store,
            observability,
            999_001,
            1,
            "testdb".to_string(),
            0,   // no statement timeout
            500, // 500ms idle-in-transaction timeout
        );

        // Simulate: BEGIN; SELECT 1; (then go idle)
        session.force_test_transaction_state(true, false);
        session.record_command_complete();

        let session_lock = Arc::new(tokio::sync::Mutex::new(session));
        let cancel = CancellationToken::new();

        tokio::spawn({
            let session_lock = Arc::clone(&session_lock);
            let cancel = cancel.clone();
            async move {
                idle_in_transaction_watchdog(session_lock, cancel).await;
            }
        });

        // The watchdog should cancel within ~600ms (500ms timeout + polling).
        // Give generous headroom to avoid flaky CI.
        tokio::select! {
            _ = cancel.cancelled() => { /* expected */ }
            _ = tokio::time::sleep(Duration::from_secs(3)) => {
                panic!("watchdog did not cancel the session within 3 s");
            }
        }
    }

    /// Watchdog exits cleanly when the connection is closed (cancel token
    /// cancelled externally) without the session being idle too long.
    #[tokio::test]
    async fn watchdog_exits_on_connection_close() {
        use super::idle_in_transaction_watchdog;
        use crate::storage::TikvStore;
        use pgwire::tokio::CancellationToken;
        use std::sync::Arc;
        use std::time::Duration;

        let store = TikvStore::new_stub();
        let observability = crate::observability::registry().tenant("watchdog_conn_close_test");
        let session = crate::sql::Session::new_with_database(
            store,
            observability,
            999_002,
            1,
            "testdb".to_string(),
            0,
            1000, // 1s timeout (should never fire)
        );

        let session_lock = Arc::new(tokio::sync::Mutex::new(session));
        let cancel = CancellationToken::new();

        let handle = tokio::spawn({
            let session_lock = Arc::clone(&session_lock);
            let cancel = cancel.clone();
            async move {
                idle_in_transaction_watchdog(session_lock, cancel).await;
            }
        });

        // Simulate connection close after 100ms.
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();

        // Watchdog task should finish promptly.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("watchdog did not exit within 2 s")
            .expect("watchdog task panicked");
    }
}
