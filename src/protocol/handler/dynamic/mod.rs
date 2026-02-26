//! Dynamic pgwire handler — dispatches protocol events to the TiKV-backed executor.
//!
//! This module defines [`DynamicPgHandler`] (the per-connection handler) and
//! [`DynamicHandlerFactory`] (the factory that pgwire uses to create handlers).
//! Protocol-specific trait implementations are split into sub-modules:
//!
//! - `startup` -- authentication and executor initialization
//! - `query`   -- simple-query and extended-query protocol
//! - `copy`    -- COPY FROM STDIN / COPY TO STDOUT

mod copy;
mod query;
mod startup;

// Re-export the free functions that tests and sibling modules reference
// via `super::dynamic::*`.
// These are used by test code via `super::dynamic::*`
#[allow(unused_imports)]
pub(super) use query::{
    is_data_statement, merge_parameter_types, reject_unanalyzed_if_needed, utility_describe_fields,
};

use super::portal::SuspendedPortalState;
use super::Db9QueryParser;
use super::{CopyContext, CONNECTION_ID_COUNTER};
use crate::config::SharedServerConfig;
use crate::pool::TikvClientPool;
use crate::sql::{Executor, Session};
use pgwire::api::{NoopErrorHandler, PgWireServerHandlers};
use pgwire::tokio::CancellationToken;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

use crate::observability;
use crate::pool::TenantHandle;

/// Post-authentication query state. The pgwire state machine guarantees this
/// exists before any query method runs: `ReadyForQuery` is only reached after
/// `finish_authentication()`, called only after `init_executor()` succeeds.
pub(super) struct AuthenticatedState {
    pub executor: Arc<Executor>,
    pub session: Arc<Mutex<Session>>,
}

pub struct DynamicPgHandler {
    pub(super) client_pool: Option<Arc<TikvClientPool>>,
    pub(super) pd_endpoints: Vec<String>,
    pub(super) default_keyspace: Option<String>,
    pub(super) auth_state: OnceCell<AuthenticatedState>,
    pub(super) connection_guard: OnceCell<observability::ConnectionGuard>,
    pub(super) tenant_handle: OnceCell<TenantHandle>,
    pub(super) copy_context: Mutex<Option<CopyContext>>,
    pub(super) suspended_portals: Mutex<HashMap<String, SuspendedPortalState>>,
    pub(super) query_parser: Arc<Db9QueryParser>,
    pub(super) connection_id: i64,
    pub(super) server_config: SharedServerConfig,
    pub(super) cancel_token: CancellationToken,
}

impl DynamicPgHandler {
    pub fn new_with_pool(
        client_pool: Arc<TikvClientPool>,
        default_keyspace: Option<String>,
        server_config: SharedServerConfig,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            client_pool: Some(client_pool),
            pd_endpoints: Vec::new(),
            default_keyspace,
            auth_state: OnceCell::new(),
            connection_guard: OnceCell::new(),
            tenant_handle: OnceCell::new(),
            copy_context: Mutex::new(None),
            suspended_portals: Mutex::new(HashMap::new()),
            query_parser: Arc::new(Db9QueryParser::new()),
            connection_id: CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            server_config,
            cancel_token,
        }
    }

    /// Returns the post-authentication state (executor + session).
    ///
    /// # Panics
    ///
    /// Panics if called before authentication completes. This is a structural
    /// invariant assertion — the pgwire state machine guarantees query methods
    /// are only dispatched after `finish_authentication()`.
    #[inline]
    pub(in crate::protocol::handler) fn auth(&self) -> &AuthenticatedState {
        self.auth_state
            .get()
            .expect("BUG: query method called before authentication completed")
    }
}

impl Drop for DynamicPgHandler {
    fn drop(&mut self) {
        self.cancel_token.cancel(); // stop idle-in-transaction watchdog
        crate::sql::advisory_locks::global_lock_manager()
            .release_all_for_connection(self.connection_id);
    }
}

pub struct DynamicHandlerFactory {
    handler: Arc<DynamicPgHandler>,
    cancel_token: CancellationToken,
}

impl DynamicHandlerFactory {
    pub fn new_with_pool(
        client_pool: Arc<TikvClientPool>,
        default_keyspace: Option<String>,
        server_config: SharedServerConfig,
    ) -> Self {
        let cancel_token = CancellationToken::new();
        Self {
            handler: Arc::new(DynamicPgHandler::new_with_pool(
                client_pool,
                default_keyspace,
                server_config,
                cancel_token.clone(),
            )),
            cancel_token,
        }
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

impl PgWireServerHandlers for DynamicHandlerFactory {
    type StartupHandler = DynamicPgHandler;
    type SimpleQueryHandler = DynamicPgHandler;
    type ExtendedQueryHandler = DynamicPgHandler;
    type CopyHandler = DynamicPgHandler;
    type ErrorHandler = NoopErrorHandler;

    fn simple_query_handler(&self) -> Arc<Self::SimpleQueryHandler> {
        self.handler.clone()
    }

    fn extended_query_handler(&self) -> Arc<Self::ExtendedQueryHandler> {
        self.handler.clone()
    }

    fn startup_handler(&self) -> Arc<Self::StartupHandler> {
        self.handler.clone()
    }

    fn copy_handler(&self) -> Arc<Self::CopyHandler> {
        self.handler.clone()
    }

    fn error_handler(&self) -> Arc<Self::ErrorHandler> {
        Arc::new(NoopErrorHandler)
    }
}
