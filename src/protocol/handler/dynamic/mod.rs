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
pub(super) mod query;
mod startup;

// Re-export the free functions that tests and sibling modules reference
// via `super::dynamic::*`.
// These are used by test code via `super::dynamic::*`
#[allow(unused_imports)]
pub(super) use query::{
    is_data_statement, is_data_statement_stmts, is_transaction_control_stmts,
    merge_parameter_types, reject_unanalyzed_if_needed, utility_describe_fields,
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
use std::sync::Mutex as StdMutex;
use tokio::sync::{Mutex, OnceCell};
use tokio::task::JoinHandle;

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
    pub(super) idle_watchdog_handle: StdMutex<Option<JoinHandle<()>>>,
    /// Cached principal identity for concurrency tracking.
    /// Set once during authentication so the query path never needs to lock
    /// the session just to read the username.
    pub(super) principal_identity: OnceCell<String>,
    /// Per-budget-owner admission token bucket. Set during authentication for
    /// connect-token sessions that carry budget claims. Direct pgwire sessions
    /// do not have an admission budget (this stays `None`).
    pub(super) admission_budget: OnceCell<Arc<crate::pool::TokenBucket>>,
    /// Per-session concurrency limit from JWT `budget_max_concurrent` claim.
    /// When set and lower than the server's per-principal limit, this tighter
    /// value is used. `None` means use the server default.
    pub(super) budget_max_concurrent: OnceCell<u32>,
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
            idle_watchdog_handle: StdMutex::new(None),
            principal_identity: OnceCell::new(),
            admission_budget: OnceCell::new(),
            budget_max_concurrent: OnceCell::new(),
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
        if let Some(watchdog) = self
            .idle_watchdog_handle
            .lock()
            .expect("idle_watchdog_handle poisoned")
            .take()
        {
            watchdog.abort();
        }
        crate::sql::advisory_locks::global_lock_manager()
            .release_all_for_connection(self.connection_id);
        // Do NOT unregister the GC active transaction registry here.
        // The Session (which owns the TiKV Transaction) is held behind an Arc
        // and may outlive the handler briefly via the watchdog task. Early
        // unregister would remove GC protection before the transaction itself
        // is dropped or rolled back. commit/rollback and Session::Drop are the
        // authoritative cleanup points.
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

#[cfg(test)]
mod tests {
    #[test]
    fn dynamic_handler_drop_does_not_unregister_gc_registry_early() {
        let source = include_str!("mod.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("dynamic/mod.rs must contain #[cfg(test)]");
        let drop_impl = prod_source
            .split("impl Drop for DynamicPgHandler")
            .nth(1)
            .and_then(|rest| rest.split("pub struct DynamicHandlerFactory").next())
            .expect("dynamic/mod.rs must define DynamicPgHandler::drop");

        assert!(
            !drop_impl.contains("unregister_connection(self.connection_id)"),
            "DynamicPgHandler::drop must not unregister the GC registry before Session drops"
        );
    }

    #[test]
    fn dynamic_handler_drop_aborts_idle_watchdog() {
        let source = include_str!("mod.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("dynamic/mod.rs must contain #[cfg(test)]");
        let drop_impl = prod_source
            .split("impl Drop for DynamicPgHandler")
            .nth(1)
            .and_then(|rest| rest.split("pub struct DynamicHandlerFactory").next())
            .expect("dynamic/mod.rs must define DynamicPgHandler::drop");

        assert!(
            drop_impl.contains("idle_watchdog_handle"),
            "DynamicPgHandler::drop must own the idle watchdog handle"
        );
        assert!(
            drop_impl.contains("watchdog.abort()"),
            "DynamicPgHandler::drop must abort the idle watchdog so Session cleanup cannot outlive the connection task"
        );
    }
}
