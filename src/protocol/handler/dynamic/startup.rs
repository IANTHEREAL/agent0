//! Startup and authentication handling for [`DynamicPgHandler`].
//!
//! Contains `init_executor`, `authenticate_user`, and the
//! [`StartupHandler`] trait implementation.

use super::{AuthenticatedState, DynamicPgHandler};
use crate::auth::AuthManager;
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
    parse_startup_options, PgServerParameterProvider, METADATA_ACTUAL_USER,
    METADATA_AUTH_IS_SUPERUSER, METADATA_KEYSPACE,
};

impl DynamicPgHandler {
    pub(in crate::protocol::handler) async fn init_executor(
        &self,
        keyspace: Option<String>,
        username: Option<String>,
        is_superuser: bool,
        database: String,
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

        let (store, trigger_cache, stats_cache) = if let Some(pool) = &self.client_pool {
            let handle = pool
                .acquire(Some(effective_keyspace.clone()))
                .await
                .map_err(|e| fatal_internal(format!("Failed to get client from pool: {}", e)))?;
            let s = handle.store().clone();
            let tc = handle.trigger_cache().clone();
            let sc = handle.stats_cache().clone();
            let _ = self.tenant_handle.set(handle);
            (s, tc, sc)
        } else {
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
                Arc::new(TableStatsCache::new()),
            )
        };

        let executor = Arc::new(Executor::new(
            store.clone(),
            effective_keyspace.clone(),
            tenant_obs.clone(),
            trigger_cache,
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
            .begin()
            .await
            .map_err(|e| fatal_internal(e.to_string()))?;
        let database_id = match store
            .get_database_id(&mut db_txn, &database_name)
            .await
            .map_err(|e| fatal_internal(e.to_string()))?
        {
            Some(id) => id,
            None => {
                db_txn.rollback().await.ok();
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "FATAL".to_owned(),
                    "3D000".to_owned(),
                    format!("database \"{}\" does not exist", database_name),
                ))));
            }
        };
        db_txn.rollback().await.ok();

        let mut session = match username {
            Some(user) => Session::new_with_user_and_database(
                store,
                tenant_obs,
                user,
                is_superuser,
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
                session: tokio::sync::Mutex::new(session),
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
    ) -> Result<(bool, bool), anyhow::Error> {
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
                    return Ok((false, false));
                }
            }
        } else {
            match TikvStore::new_with_keyspace(self.pd_endpoints.clone(), effective_keyspace).await
            {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    error!("Failed to connect to TiKV: {}", e);
                    return Ok((false, false));
                }
            }
        };

        let auth_manager = AuthManager::new();

        // Try bootstrap. Any failure must deny authentication.
        {
            let mut txn = store.begin().await.context("Failed to bootstrap auth")?;
            auth_manager
                .bootstrap(&mut txn)
                .await
                .context("Failed to bootstrap auth")?;
            txn.commit().await.context("Failed to bootstrap auth")?;
        }

        let mut txn = store.begin().await.context("Failed to begin transaction")?;

        match auth_manager
            .authenticate(&mut txn, username, password)
            .await
        {
            Ok(Some(user)) => {
                txn.commit().await.context("Failed to commit")?;
                Ok((true, user.is_superuser))
            }
            Ok(None) => {
                txn.rollback().await.ok();
                Ok((false, false))
            }
            Err(e) => {
                txn.rollback().await.ok();
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
                let dev_mode = config::env_bool("PGTIKV_DEV");
                let insecure_mode = config::env_bool("PGTIKV_INSECURE");

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
                            "Cleartext password authentication without TLS is disabled by default for non-loopback clients. Enable TLS (PG_TLS_CERT/PG_TLS_KEY) or explicitly opt into insecure mode (PGTIKV_DEV=1 or PGTIKV_INSECURE=1).".to_string(),
                        );
                        return Err(PgWireError::UserError(Box::new(error_info)));
                    }

                    if !peer_ip.is_loopback() && (dev_mode || insecure_mode) {
                        warn!(
                            "Allowing non-TLS cleartext auth for non-loopback connection from {} (PGTIKV_DEV={}, PGTIKV_INSECURE={})",
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
                    Ok((is_authenticated, is_superuser)) => {
                        if is_authenticated {
                            self.init_executor(
                                keyspace.clone(),
                                Some(actual_user.clone()),
                                is_superuser,
                                database,
                            )
                            .await?;

                            {
                                let mut session = self.auth().session.lock().await;
                                if let Some(options) = client.metadata().get("options") {
                                    for (key, value) in parse_startup_options(options) {
                                        if let Err(e) = session
                                            .set_known_setting(&key.to_ascii_lowercase(), value)
                                        {
                                            warn!("Failed to apply startup option {}: {}", key, e);
                                        }
                                    }
                                }

                                if let Some(app_name) = client.metadata().get("application_name") {
                                    if let Err(e) = session
                                        .set_known_setting("application_name", app_name.clone())
                                    {
                                        warn!(
                                            "Failed to apply application_name from startup: {}",
                                            e
                                        );
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
                        } else {
                            let error_info = ErrorInfo::new(
                                "FATAL".to_owned(),
                                "28P01".to_owned(),
                                format!(
                                    "Password authentication failed for user \"{}\"",
                                    actual_user
                                ),
                            );
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
