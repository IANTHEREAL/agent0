//! Session management for transactions.
//!
//! Contains the `Session` struct (per-connection state), `TransactionState`,
//! and `SessionSettings` (GUC parameters).

pub(crate) mod settings;
mod transaction;

#[cfg(test)]
mod tests;

pub(crate) use settings::SessionSettings;

use crate::config::SharedServerConfig;
use crate::observability::TenantObservability;
use crate::sql::error::SqlError;
use crate::sql::executor::core::plan_cache::PreparedPlanCache;
use crate::sql::executor::core::prepared_stmt::PreparedStatement as SqlPreparedStatement;
use crate::sql::query_context::{QueryContext, XactAdvisorySavepointTracker};
use crate::sql::sequences::SequenceSession;
use crate::storage::TikvStore;
use crate::txn::SavepointState;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tikv_client::{TimestampExt, Transaction};

pub(crate) const DEFAULT_MAX_SORT_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const DEFAULT_DML_TABLE_SCAN_MAX_ROWS: usize = 10_000;

pub enum TransactionState {
    Idle,
    Active(Transaction),
    Failed(Transaction),
}

#[derive(Clone, Default)]
pub(crate) struct ExtensionDelta {
    pub(crate) created: HashSet<String>,
    pub(crate) dropped: HashSet<String>,
}

#[derive(Clone)]
pub(crate) struct ExtensionDeltaSavepoint {
    name: String,
    snapshot: ExtensionDelta,
}

/// Saved session authorization state for SET LOCAL session_authorization.
/// Captured before the first SET LOCAL in a transaction, restored on COMMIT/ROLLBACK.
#[derive(Clone)]
struct LocalSessionAuthSave {
    session_user: Option<String>,
    session_user_is_superuser: bool,
    current_user: Option<String>,
    is_superuser: bool,
    bypass_rls: bool,
}

/// Savepoint-scoped snapshot of session authorization identity fields.
/// Used to restore identity on ROLLBACK TO SAVEPOINT (PG parity).
#[derive(Clone)]
struct SessionAuthSavepoint {
    name: String,
    session_user: Option<String>,
    session_user_is_superuser: bool,
    current_user: Option<String>,
    is_superuser: bool,
    bypass_rls: bool,
    local_session_auth_save: Option<LocalSessionAuthSave>,
}

pub struct Session {
    pub(crate) store: Arc<TikvStore>,
    pub(crate) observability: Arc<TenantObservability>,
    pub(crate) state: TransactionState,
    #[cfg(test)]
    test_force_in_transaction: bool,
    #[cfg(test)]
    test_force_failed_transaction: bool,
    pub(crate) savepoints: Arc<SavepointState>,
    last_sequence_values: SequenceSession,
    settings: SessionSettings,
    /// Original authenticated user at connection time. Never changes after construction.
    /// Used by `SET SESSION AUTHORIZATION DEFAULT` to restore the initial identity.
    authenticated_user: Option<String>,
    authenticated_user_is_superuser: bool,
    authenticated_bypass_rls: bool,
    /// Authenticated session user (login role). This does not change with `SET ROLE`.
    session_user: Option<String>,
    session_user_is_superuser: bool,
    session_bypass_rls: bool,
    /// Current effective role. This can change with `SET ROLE` / `RESET ROLE`.
    current_user: Option<String>,
    is_superuser: bool,
    /// Whether the current effective role has the BYPASSRLS attribute.
    bypass_rls: bool,
    current_database_id: u64,
    current_database_name: Arc<str>,
    /// Internal 64-bit connection identity.
    /// `pg_backend_pid()` remains int4 by truncating this value at function boundary.
    connection_id: i64,
    /// Timestamp (epoch millis) when the current explicit transaction started.
    /// None when not in an explicit transaction block.
    pub(crate) transaction_timestamp_ms: Option<i64>,
    /// Timestamp of the last command completion while in a transaction block.
    /// Used for `idle_in_transaction_session_timeout` enforcement.
    /// Set by `record_command_complete()`, cleared on commit/rollback.
    last_command_complete_at: Option<Instant>,
    /// Number of successfully executed non-transaction-control statements in the
    /// current explicit transaction block.
    tx_statement_count: u32,
    /// Shared server-level configuration (for ALTER SYSTEM SET).
    server_config: Option<SharedServerConfig>,
    /// Pending parameter values from extended-query Bind for the next Execute.
    /// Set by the protocol handler before calling `execute()`, consumed by
    /// `query_context_for_statement()` so they flow into `QUERY_PARAMS`.
    pending_params: Vec<Option<crate::model::Value>>,
    /// Pending parameter types from Parse-time analysis for the next Execute.
    /// Set by the protocol handler, consumed by `query_context_for_statement()`
    /// so they flow into `QUERY_PARAM_TYPES`.
    pending_param_types: Vec<Option<crate::model::DataType>>,
    /// SQL PREPARE/EXECUTE statement cache (session-scoped, PostgreSQL semantics).
    sql_prepared_statements: HashMap<String, SqlPreparedStatement>,
    /// Session-local plan cache for prepared statements (bounded LRU).
    plan_cache: PreparedPlanCache,
    /// True when current transaction used xact-scoped advisory lock functions.
    pub(crate) has_xact_advisory_locks: Arc<AtomicBool>,
    /// Savepoint-scoped tracker for xact advisory lock acquisitions.
    pub(crate) xact_advisory_savepoint_tracker:
        Arc<tokio::sync::Mutex<XactAdvisorySavepointTracker>>,
    /// In-transaction extension DDL delta (created/dropped).
    /// Cleared on COMMIT/ROLLBACK, savepoint-scoped within a transaction block.
    pub(crate) extension_delta: ExtensionDelta,
    pub(crate) extension_delta_savepoints: Vec<ExtensionDeltaSavepoint>,
    /// True when executing a multi-statement simple-query batch (implicit transaction).
    /// LOCAL mutations should persist across statements within the batch, matching
    /// PostgreSQL's implicit transaction semantics for multi-statement simple queries.
    in_implicit_batch: bool,
    /// Pending notices queued during execution (e.g., SET LOCAL warning before
    /// a reserved-GUC error). Drained by the protocol handler after each statement.
    pending_notices: Vec<(String, String, String)>,
    /// Saved session authorization state for SET LOCAL session_authorization.
    /// Captured before the first SET LOCAL in a transaction, restored on COMMIT/ROLLBACK.
    local_session_auth_save: Option<LocalSessionAuthSave>,
    /// Savepoint-scoped snapshots of session authorization identity.
    /// Used to restore identity on ROLLBACK TO SAVEPOINT (PG parity).
    session_auth_savepoints: Vec<SessionAuthSavepoint>,
    /// GC active transaction registry. When Some, begin/commit/rollback
    /// register/unregister this session's start_ts. Set for interactive
    /// SQL connections; None for worker/background sessions.
    active_txn_registry: Option<Arc<crate::worker::active_txn_registry::ActiveTxnRegistry>>,
}

/// Force-insert or overwrite a setting in a sorted `(name, value, description)` vec.
/// Used by `show_all_settings()` to replace pseudo-GUCs with authoritative values.
pub(super) fn force_insert_setting(
    v: &mut Vec<(String, String, String)>,
    name: &str,
    value: String,
) {
    match v.binary_search_by(|(n, _, _)| n.as_str().cmp(name)) {
        Ok(idx) => v[idx].1 = value,
        Err(idx) => v.insert(idx, (name.to_string(), value, String::new())),
    }
}

impl Session {
    /// Create a session for the given database.
    pub fn new_with_database(
        store: Arc<TikvStore>,
        observability: Arc<TenantObservability>,
        connection_id: i64,
        database_id: u64,
        database_name: String,
        default_statement_timeout_ms: u64,
        default_idle_in_txn_timeout_ms: u64,
    ) -> Self {
        let settings = SessionSettings::new_with_defaults(
            default_statement_timeout_ms,
            default_idle_in_txn_timeout_ms,
        );
        let plan_cache = PreparedPlanCache::new(
            settings.prepared_plan_cache_size(),
            settings.prepared_plan_cache_min_exec(),
        );
        Self {
            store,
            observability,
            state: TransactionState::Idle,
            #[cfg(test)]
            test_force_in_transaction: false,
            #[cfg(test)]
            test_force_failed_transaction: false,
            savepoints: Arc::new(SavepointState::new()),
            last_sequence_values: SequenceSession::new(),
            settings,
            authenticated_user: None,
            authenticated_user_is_superuser: false,
            authenticated_bypass_rls: false,
            session_user: None,
            session_user_is_superuser: false,
            session_bypass_rls: false,
            current_user: None,
            is_superuser: false,
            bypass_rls: false,
            current_database_id: database_id,
            current_database_name: Arc::from(database_name),
            connection_id,
            transaction_timestamp_ms: None,
            last_command_complete_at: None,
            tx_statement_count: 0,
            server_config: None,
            pending_params: vec![],
            pending_param_types: vec![],
            sql_prepared_statements: HashMap::new(),
            plan_cache,
            has_xact_advisory_locks: Arc::new(AtomicBool::new(false)),
            xact_advisory_savepoint_tracker: Arc::new(tokio::sync::Mutex::new(
                XactAdvisorySavepointTracker::default(),
            )),
            extension_delta: ExtensionDelta::default(),
            extension_delta_savepoints: Vec::new(),
            in_implicit_batch: false,
            pending_notices: Vec::new(),
            local_session_auth_save: None,
            session_auth_savepoints: Vec::new(),
            active_txn_registry: None,
        }
    }

    /// Create a session for the given user and database.
    pub fn new_with_user_and_database(
        store: Arc<TikvStore>,
        observability: Arc<TenantObservability>,
        username: String,
        is_superuser: bool,
        bypass_rls: bool,
        connection_id: i64,
        database_id: u64,
        database_name: String,
        default_statement_timeout_ms: u64,
        default_idle_in_txn_timeout_ms: u64,
    ) -> Self {
        let settings = SessionSettings::new_with_defaults(
            default_statement_timeout_ms,
            default_idle_in_txn_timeout_ms,
        );
        let plan_cache = PreparedPlanCache::new(
            settings.prepared_plan_cache_size(),
            settings.prepared_plan_cache_min_exec(),
        );
        Self {
            store,
            observability,
            state: TransactionState::Idle,
            #[cfg(test)]
            test_force_in_transaction: false,
            #[cfg(test)]
            test_force_failed_transaction: false,
            savepoints: Arc::new(SavepointState::new()),
            last_sequence_values: SequenceSession::new(),
            settings,
            authenticated_user: Some(username.clone()),
            authenticated_user_is_superuser: is_superuser,
            authenticated_bypass_rls: bypass_rls,
            session_user: Some(username.clone()),
            session_user_is_superuser: is_superuser,
            session_bypass_rls: bypass_rls,
            current_user: Some(username),
            is_superuser,
            bypass_rls,
            current_database_id: database_id,
            current_database_name: Arc::from(database_name),
            connection_id,
            transaction_timestamp_ms: None,
            last_command_complete_at: None,
            tx_statement_count: 0,
            server_config: None,
            pending_params: vec![],
            pending_param_types: vec![],
            sql_prepared_statements: HashMap::new(),
            plan_cache,
            has_xact_advisory_locks: Arc::new(AtomicBool::new(false)),
            xact_advisory_savepoint_tracker: Arc::new(tokio::sync::Mutex::new(
                XactAdvisorySavepointTracker::default(),
            )),
            extension_delta: ExtensionDelta::default(),
            extension_delta_savepoints: Vec::new(),
            in_implicit_batch: false,
            pending_notices: Vec::new(),
            local_session_auth_save: None,
            session_auth_savepoints: Vec::new(),
            active_txn_registry: None,
        }
    }

    /// Set the GC active transaction registry. Called once after construction
    /// for interactive SQL connections. Worker/background sessions leave this None.
    pub fn set_active_txn_registry(
        &mut self,
        registry: Arc<crate::worker::active_txn_registry::ActiveTxnRegistry>,
    ) {
        self.active_txn_registry = Some(registry);
    }

    pub fn current_user(&self) -> Option<&str> {
        self.current_user.as_deref()
    }

    pub fn session_user(&self) -> Option<&str> {
        self.session_user.as_deref()
    }

    /// Whether the original authenticated (session) user is a superuser.
    /// Unlike [`is_superuser`], this is not affected by `SET ROLE`.
    pub fn session_user_is_superuser(&self) -> bool {
        self.session_user_is_superuser
    }

    /// Whether the initial authenticated user (at connection time) is a superuser.
    /// Never changes after construction. Used for `SET SESSION AUTHORIZATION`
    /// permission checks (PG bases permission on the login role, not the
    /// currently effective session user).
    pub fn authenticated_user_is_superuser(&self) -> bool {
        self.authenticated_user_is_superuser
    }

    pub fn is_superuser(&self) -> bool {
        self.is_superuser
    }

    pub fn bypass_rls(&self) -> bool {
        self.bypass_rls
    }

    pub(crate) fn set_current_role(&mut self, role: String, is_superuser: bool, bypass_rls: bool) {
        self.current_user = Some(role);
        self.is_superuser = is_superuser;
        self.bypass_rls = bypass_rls;
    }

    /// Change the session authorization to a new role.
    /// Updates both session_user and current_user to the target role.
    pub(crate) fn set_session_authorization(
        &mut self,
        role: String,
        is_superuser: bool,
        bypass_rls: bool,
    ) {
        self.session_user = Some(role.clone());
        self.session_user_is_superuser = is_superuser;
        self.session_bypass_rls = bypass_rls;
        self.current_user = Some(role);
        self.is_superuser = is_superuser;
        self.bypass_rls = bypass_rls;
    }

    /// Reset session authorization to the original authenticated user.
    /// Called by `SET SESSION AUTHORIZATION DEFAULT`.
    pub(crate) fn reset_session_authorization(&mut self) {
        self.session_user = self.authenticated_user.clone();
        self.session_user_is_superuser = self.authenticated_user_is_superuser;
        self.session_bypass_rls = self.authenticated_bypass_rls;
        self.current_user = self.authenticated_user.clone();
        self.is_superuser = self.authenticated_user_is_superuser;
        self.bypass_rls = self.authenticated_bypass_rls;
    }

    /// Save session authorization state before applying SET LOCAL session_authorization.
    /// Only saves if no prior save exists (first SET LOCAL in the transaction wins).
    pub(crate) fn save_session_auth_for_local(&mut self) {
        if self.local_session_auth_save.is_none() {
            self.local_session_auth_save = Some(LocalSessionAuthSave {
                session_user: self.session_user.clone(),
                session_user_is_superuser: self.session_user_is_superuser,
                current_user: self.current_user.clone(),
                is_superuser: self.is_superuser,
                bypass_rls: self.bypass_rls,
            });
        }
    }

    /// Clear any saved SET LOCAL session_authorization snapshot.
    /// Called when a non-LOCAL SET session_authorization succeeds inside a transaction,
    /// so that COMMIT/ROLLBACK does not restore stale identity state.
    pub(crate) fn clear_local_session_auth_save(&mut self) {
        self.local_session_auth_save = None;
    }

    /// Restore session authorization state saved by SET LOCAL session_authorization.
    /// Called during clear_local_overrides (COMMIT/ROLLBACK).
    fn restore_local_session_auth(&mut self) {
        if let Some(save) = self.local_session_auth_save.take() {
            self.session_user = save.session_user;
            self.session_user_is_superuser = save.session_user_is_superuser;
            self.current_user = save.current_user;
            self.is_superuser = save.is_superuser;
            self.bypass_rls = save.bypass_rls;
        }
    }

    pub(crate) fn push_session_auth_savepoint(&mut self, name: String) {
        self.session_auth_savepoints.push(SessionAuthSavepoint {
            name,
            session_user: self.session_user.clone(),
            session_user_is_superuser: self.session_user_is_superuser,
            current_user: self.current_user.clone(),
            is_superuser: self.is_superuser,
            bypass_rls: self.bypass_rls,
            local_session_auth_save: self.local_session_auth_save.clone(),
        });
    }

    pub(crate) fn rollback_session_auth_to_savepoint(&mut self, name: &str) {
        let Some(target_idx) = self
            .session_auth_savepoints
            .iter()
            .rposition(|sp| sp.name == name)
        else {
            return;
        };

        let snapshot = self.session_auth_savepoints[target_idx].clone();
        self.session_user = snapshot.session_user;
        self.session_user_is_superuser = snapshot.session_user_is_superuser;
        self.current_user = snapshot.current_user;
        self.is_superuser = snapshot.is_superuser;
        self.bypass_rls = snapshot.bypass_rls;
        self.local_session_auth_save = snapshot.local_session_auth_save;
        self.session_auth_savepoints.truncate(target_idx + 1);
    }

    pub(crate) fn release_session_auth_savepoint(&mut self, name: &str) {
        let Some(target_idx) = self
            .session_auth_savepoints
            .iter()
            .rposition(|sp| sp.name == name)
        else {
            return;
        };

        self.session_auth_savepoints.truncate(target_idx);
    }

    pub(crate) fn reset_role(&mut self) {
        self.current_user = self.session_user.clone();
        self.is_superuser = self.session_user_is_superuser;
        self.bypass_rls = self.session_bypass_rls;
    }

    pub fn connection_id(&self) -> i64 {
        self.connection_id
    }

    pub(crate) fn session_txn_tracker(
        &self,
    ) -> Option<Arc<crate::session_context::SessionTxnTracker>> {
        self.active_txn_registry.as_ref().map(|registry| {
            Arc::new(crate::session_context::SessionTxnTracker::new(
                self.connection_id,
                registry.clone(),
            ))
        })
    }

    pub fn current_database_id(&self) -> u64 {
        self.current_database_id
    }

    pub(crate) fn active_txn_start_ts_version(&self) -> Option<u64> {
        match &self.state {
            TransactionState::Active(txn) | TransactionState::Failed(txn) => {
                Some(txn.start_timestamp().version())
            }
            TransactionState::Idle => None,
        }
    }

    pub(crate) fn current_database_name_arc(&self) -> Arc<str> {
        self.current_database_name.clone()
    }

    pub(crate) fn current_user_arc(&self) -> Arc<str> {
        Arc::from(self.current_user().unwrap_or("postgres"))
    }

    pub(crate) fn timezone_arc(&self) -> Arc<str> {
        Arc::from(
            self.show_setting_value("timezone")
                .unwrap_or_else(|| "UTC".to_string()),
        )
    }

    pub(crate) fn query_context_for_statement(
        &mut self,
        statement_timestamp_ms: i64,
        transaction_timestamp_ms: i64,
    ) -> QueryContext {
        let mut qctx = QueryContext::new(
            self.connection_id(),
            self.current_database_name_arc(),
            self.current_user_arc(),
            statement_timestamp_ms,
            transaction_timestamp_ms,
            self.timezone_arc(),
        );
        // Drain pending params (set by extended-query Execute) into the context.
        // This ensures QUERY_PARAMS task-local is populated for this statement.
        if !self.pending_params.is_empty() {
            qctx.params = std::mem::take(&mut self.pending_params);
        }
        // Drain pending param types (set by extended-query Execute) into the context.
        // This ensures QUERY_PARAM_TYPES task-local is populated for this statement.
        if !self.pending_param_types.is_empty() {
            qctx.param_types = std::mem::take(&mut self.pending_param_types);
        }
        // Snapshot both the public SQL-facing settings view and the raw
        // execution settings view for this statement.
        qctx.settings_snapshot = Some(Arc::new(self.all_settings_snapshot()));
        qctx.execution_settings_snapshot = Some(Arc::new(self.all_execution_settings_snapshot()));
        qctx.lock_timeout = self.lock_timeout();
        qctx.xact_advisory_lock_used = Some(self.has_xact_advisory_locks.clone());
        qctx.xact_advisory_savepoint_tracker = Some(self.xact_advisory_savepoint_tracker.clone());
        qctx.store_ref = Some(crate::sql::query_context::StoreRef(self.store.clone()));
        qctx
    }

    /// Set parameter values for the next statement execution.
    /// Called by the protocol handler after decoding Bind parameters.
    /// Consumed (drained) by `query_context_for_statement()`.
    pub fn set_pending_params(&mut self, params: Vec<Option<crate::model::Value>>) {
        self.pending_params = params;
    }

    /// Set parameter types for the next statement execution.
    /// Called by the protocol handler to thread Parse-time types to Execute-time Analyzer.
    /// Consumed (drained) by `query_context_for_statement()`.
    pub fn set_pending_param_types(&mut self, types: Vec<Option<crate::model::DataType>>) {
        self.pending_param_types = types;
    }

    pub(crate) fn put_sql_prepared_statement(&mut self, name: String, stmt: SqlPreparedStatement) {
        self.sql_prepared_statements.insert(name, stmt);
    }

    pub(crate) fn get_sql_prepared_statement_cloned(
        &self,
        name: &str,
    ) -> Option<SqlPreparedStatement> {
        self.sql_prepared_statements.get(name).cloned()
    }

    pub(crate) fn remove_sql_prepared_statement(&mut self, name: &str) -> bool {
        self.sql_prepared_statements.remove(name).is_some()
    }

    pub(crate) fn clear_sql_prepared_statements(&mut self) {
        self.sql_prepared_statements.clear();
    }

    pub(crate) fn plan_cache(&mut self) -> &mut PreparedPlanCache {
        &mut self.plan_cache
    }

    /// Invalidate all cached plans that depend on the given table_id.
    /// Called after DDL that changes a table's schema.
    #[allow(dead_code)]
    pub(crate) fn invalidate_plan_cache_for_table(&mut self, table_id: u64) {
        self.plan_cache.invalidate_by_table_id(table_id);
    }

    /// Clear the entire plan cache. Called on transaction rollback.
    pub(crate) fn clear_plan_cache(&mut self) {
        self.plan_cache.clear();
    }

    pub(crate) fn note_extension_created(&mut self, name: &str) {
        let normalized = name.to_ascii_lowercase();
        self.extension_delta.dropped.remove(&normalized);
        self.extension_delta.created.insert(normalized);
    }

    pub(crate) fn note_extension_dropped(&mut self, name: &str) {
        let normalized = name.to_ascii_lowercase();
        self.extension_delta.created.remove(&normalized);
        self.extension_delta.dropped.insert(normalized);
    }

    pub(crate) fn extension_delta_snapshot(
        &self,
    ) -> Arc<crate::session_context::ExtensionTxnDelta> {
        Arc::new((
            self.extension_delta.created.clone(),
            self.extension_delta.dropped.clone(),
        ))
    }

    pub(crate) fn push_extension_delta_savepoint(&mut self, name: String) {
        self.extension_delta_savepoints
            .push(ExtensionDeltaSavepoint {
                name,
                snapshot: self.extension_delta.clone(),
            });
    }

    pub(crate) fn rollback_extension_delta_to_savepoint(&mut self, name: &str) {
        let Some(target_idx) = self
            .extension_delta_savepoints
            .iter()
            .rposition(|sp| sp.name == name)
        else {
            return;
        };

        self.extension_delta = self.extension_delta_savepoints[target_idx].snapshot.clone();
        self.extension_delta_savepoints.truncate(target_idx + 1);
    }

    pub(crate) fn release_extension_delta_savepoint(&mut self, name: &str) {
        let Some(target_idx) = self
            .extension_delta_savepoints
            .iter()
            .rposition(|sp| sp.name == name)
        else {
            return;
        };

        self.extension_delta_savepoints.truncate(target_idx);
    }

    #[cfg(test)]
    pub(crate) fn force_test_transaction_state(&mut self, in_transaction: bool, failed: bool) {
        self.test_force_in_transaction = in_transaction;
        self.test_force_failed_transaction = in_transaction && failed;
    }

    /// Check if currently in a transaction block
    pub fn is_in_transaction(&self) -> bool {
        #[cfg(test)]
        if self.test_force_in_transaction {
            return true;
        }
        matches!(
            self.state,
            TransactionState::Active(_) | TransactionState::Failed(_)
        )
    }

    /// Whether we are inside a multi-statement simple-query batch (implicit transaction).
    pub(crate) fn in_implicit_batch(&self) -> bool {
        self.in_implicit_batch
    }

    /// Mark the session as inside (or outside) a multi-statement simple-query batch.
    pub(crate) fn set_in_implicit_batch(&mut self, v: bool) {
        self.in_implicit_batch = v;
    }

    pub fn is_transaction_failed(&self) -> bool {
        #[cfg(test)]
        if self.test_force_failed_transaction {
            return true;
        }
        matches!(self.state, TransactionState::Failed(_))
    }

    /// Returns true if the current explicit transaction has already executed at
    /// least one non-transaction-control statement.
    pub(crate) fn has_executed_statement_in_transaction(&self) -> bool {
        self.is_in_transaction() && self.tx_statement_count > 0
    }

    /// Record successful completion of a non-transaction-control statement in
    /// the current explicit transaction block.
    pub(crate) fn note_statement_success_in_transaction(&mut self) {
        if self.is_in_transaction() {
            self.tx_statement_count = self.tx_statement_count.saturating_add(1);
        }
    }

    pub(crate) fn mark_transaction_failed(&mut self) {
        #[cfg(test)]
        if self.test_force_in_transaction {
            self.test_force_failed_transaction = true;
            return;
        }
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(txn) => self.state = TransactionState::Failed(txn),
            other => self.state = other,
        }
    }

    pub(crate) fn clear_failed_transaction(&mut self) {
        #[cfg(test)]
        if self.test_force_in_transaction {
            self.test_force_failed_transaction = false;
            return;
        }
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Failed(txn) => self.state = TransactionState::Active(txn),
            other => self.state = other,
        }
    }

    /// Record that a command has completed. If we're in a transaction,
    /// this starts the idle-in-transaction timer.
    pub fn record_command_complete(&mut self) {
        if self.is_in_transaction() {
            self.last_command_complete_at = Some(Instant::now());
        } else {
            self.last_command_complete_at = None;
            // Defensive cleanup for implicit statement completion paths.
            // If a future executor path sets the xact advisory marker without
            // an explicit commit/rollback call, avoid leaking xact locks.
            self.release_xact_advisory_locks_if_needed();
        }
    }

    /// Check if the session has been idle in a transaction for too long.
    /// Returns `Err(SqlError::IdleInTransactionTimeout)` if the timeout has been exceeded.
    pub fn check_idle_in_transaction_timeout(&self) -> std::result::Result<(), SqlError> {
        let timeout = self.settings.idle_in_transaction_session_timeout();
        let timeout = match timeout {
            Some(t) => t,
            None => return Ok(()), // disabled (0)
        };

        if !self.is_in_transaction() {
            return Ok(());
        }

        let last_complete = match self.last_command_complete_at {
            Some(t) => t,
            None => return Ok(()), // no previous command completed yet
        };

        if last_complete.elapsed() > timeout {
            Err(SqlError::IdleInTransactionTimeout)
        } else {
            Ok(())
        }
    }

    /// Returns the remaining duration before idle-in-transaction timeout fires,
    /// given the configured timeout `t`. Returns `None` if no command has
    /// completed yet in the current transaction (no deadline to compute).
    pub(crate) fn idle_in_transaction_remaining(&self, timeout: Duration) -> Option<Duration> {
        let last = self.last_command_complete_at?;
        let elapsed = last.elapsed();
        if elapsed >= timeout {
            Some(Duration::ZERO)
        } else {
            Some(timeout - elapsed)
        }
    }

    /// Read-only access to session settings (for the idle-in-transaction watchdog).
    pub(crate) fn settings(&self) -> &SessionSettings {
        &self.settings
    }

    /// Set the shared server configuration reference.
    pub fn set_server_config(&mut self, config: SharedServerConfig) {
        self.server_config = Some(config);
    }

    /// Get the shared server configuration reference.
    pub fn server_config(&self) -> Option<&SharedServerConfig> {
        self.server_config.as_ref()
    }

    pub(crate) fn savepoints(&self) -> Arc<SavepointState> {
        self.savepoints.clone()
    }

    /// Get mutable reference to active transaction
    pub fn get_mut_txn(&mut self) -> Option<&mut Transaction> {
        match &mut self.state {
            TransactionState::Active(txn) | TransactionState::Failed(txn) => Some(txn),
            _ => None,
        }
    }

    pub fn get_mut_txn_sequence_values_and_search_path(
        &mut self,
    ) -> Option<(&mut Transaction, &mut SequenceSession, &[String])> {
        match &mut self.state {
            TransactionState::Active(txn) | TransactionState::Failed(txn) => Some((
                txn,
                &mut self.last_sequence_values,
                self.settings.search_path(),
            )),
            _ => None,
        }
    }

    pub fn search_path(&self) -> &[String] {
        self.settings.search_path()
    }

    pub fn set_search_path(&mut self, search_path: Vec<String>) {
        self.settings.set_search_path(search_path);
        self.settings.remove_local_override("search_path");
    }

    pub(crate) fn set_known_setting(&mut self, name: &str, value: String) -> Result<bool> {
        let changed = self.settings.set_known_setting(name, value)?;
        self.sync_plan_cache_settings();
        Ok(changed)
    }

    pub(crate) fn set_server_reserved_setting(
        &mut self,
        name: &str,
        value: String,
    ) -> Result<bool> {
        let changed = self.settings.set_server_reserved_setting(name, value)?;
        self.sync_plan_cache_settings();
        Ok(changed)
    }

    pub(crate) fn set_local_setting(&mut self, name: &str, value: String) -> Result<bool> {
        let changed = self.settings.set_local_override(name, value)?;
        self.sync_plan_cache_settings();
        Ok(changed)
    }

    pub(crate) fn set_local_search_path(&mut self, search_path: Vec<String>) {
        self.settings.set_local_search_path(search_path);
    }

    pub(crate) fn clear_local_overrides(&mut self) {
        self.settings.clear_local_overrides();
        self.restore_local_session_auth();
        self.sync_plan_cache_settings();
    }

    pub(crate) fn reset_setting(&mut self, name: &str) {
        self.settings.reset_setting(name);
        self.sync_plan_cache_settings();
    }

    pub(crate) fn reset_all_settings(&mut self) {
        self.settings.reset_all_settings();
        self.sync_plan_cache_settings();
    }

    /// Snapshot the public SQL-facing settings view into a flat map for
    /// `current_setting()` in expression contexts.
    pub(crate) fn all_settings_snapshot(&self) -> HashMap<String, String> {
        self.show_all_settings()
            .into_iter()
            .map(|(name, value, _)| (name, value))
            .collect()
    }

    /// Snapshot the raw execution settings into a flat map for internal
    /// runtime consumers such as embedding resolution.
    pub(crate) fn all_execution_settings_snapshot(&self) -> HashMap<String, String> {
        let mut map = self.settings.all_values();
        map.insert(
            "is_superuser".to_string(),
            if self.is_superuser { "on" } else { "off" }.to_string(),
        );
        let session_auth = self
            .session_user
            .as_deref()
            .or(self.current_user.as_deref())
            .unwrap_or("postgres")
            .to_string();
        map.insert("session_authorization".to_string(), session_auth);
        // Thread effective reset-default values for tenant-configurable
        // timeout GUCs so set_config(name, NULL, ...) can return the
        // correct post-reset value (PG parity #1559).
        map.insert(
            "_reset_default.statement_timeout".to_string(),
            self.settings.reset_default_show_value("statement_timeout"),
        );
        map.insert(
            "_reset_default.idle_in_transaction_session_timeout".to_string(),
            self.settings
                .reset_default_show_value("idle_in_transaction_session_timeout"),
        );
        map
    }

    pub(crate) fn show_setting_value(&self, name: &str) -> Option<String> {
        match name {
            "is_superuser" => Some(if self.is_superuser { "on" } else { "off" }.to_string()),
            // Session authorization is the authenticated session user (login role).
            "session_authorization" => Some(
                self.session_user
                    .as_deref()
                    .or(self.current_user.as_deref())
                    .unwrap_or("postgres")
                    .to_string(),
            ),
            _ => self
                .settings
                .show_value(name)
                .map(|value| settings::public_setting_value(name, value)),
        }
    }

    /// Collect all settings for `SHOW ALL`.
    /// Returns Vec<(name, setting, description)> sorted alphabetically.
    /// Session-level pseudo-GUCs (`is_superuser`, `session_authorization`) are
    /// force-replaced with authoritative Session values.
    pub(crate) fn show_all_settings(&self) -> Vec<(String, String, String)> {
        let mut all = self.settings.show_all();

        force_insert_setting(
            &mut all,
            "is_superuser",
            if self.is_superuser { "on" } else { "off" }.to_string(),
        );
        force_insert_setting(
            &mut all,
            "session_authorization",
            self.session_user
                .as_deref()
                .or(self.current_user.as_deref())
                .unwrap_or("postgres")
                .to_string(),
        );
        force_insert_setting(
            &mut all,
            "embedding.api_key",
            self.show_setting_value("embedding.api_key")
                .unwrap_or_default(),
        );

        all
    }

    pub(crate) fn statement_timeout(&self) -> Option<Duration> {
        self.settings.statement_timeout()
    }

    pub(crate) fn lock_timeout(&self) -> Option<Duration> {
        self.settings.lock_timeout()
    }

    pub(crate) fn max_sort_bytes(&self) -> usize {
        self.settings.max_sort_bytes()
    }

    #[allow(dead_code)] // framework: accessed via settings snapshot in DML executor
    pub(crate) fn dml_table_scan_max_rows(&self) -> usize {
        self.settings.dml_table_scan_max_rows()
    }

    fn sync_plan_cache_settings(&mut self) {
        self.plan_cache.reconfigure(
            self.settings.prepared_plan_cache_size(),
            self.settings.prepared_plan_cache_min_exec(),
        );
    }

    /// Queue a notice to be delivered by the protocol handler.
    /// Used when a warning must precede an error in the same statement
    /// (e.g., SET LOCAL outside transaction before a reserved-GUC error).
    pub(crate) fn push_pending_notice(
        &mut self,
        severity: String,
        sqlstate: String,
        message: String,
    ) {
        self.pending_notices.push((severity, sqlstate, message));
    }

    /// Drain all pending notices. Called by the protocol handler to emit
    /// notices before an error response or at statement completion.
    pub fn drain_pending_notices(&mut self) -> Vec<(String, String, String)> {
        std::mem::take(&mut self.pending_notices)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Safety net: if an active transaction was never committed or rolled
        // back (e.g., client disconnect, panic), quarantine the registration
        // instead of immediately unregistering.  The vendored tikv-client
        // Transaction::Drop does NOT send a rollback RPC — TiKV-side locks
        // persist until lock TTL (~20 s).  Quarantining keeps the start_ts
        // protected during that window; the GC publisher reaps the entry
        // after QUARANTINE_TTL.
        //
        // If commit/rollback already succeeded, the entry was already removed
        // from the registry by transaction.rs and quarantine_connection is a
        // no-op (it checks existence before quarantining).
        if let Some(ref registry) = self.active_txn_registry {
            registry.quarantine_connection(self.connection_id);
        }
    }
}

#[cfg(test)]
mod drop_contract_tests {
    #[test]
    fn session_drop_quarantines_gc_registration() {
        let source = include_str!("mod.rs");
        let prod_source = source
            .split("#[cfg(test)] mod drop_contract_tests")
            .next()
            .expect("session/mod.rs must contain drop_contract_tests");
        let drop_impl = prod_source
            .split("impl Drop for Session")
            .nth(1)
            .expect("session/mod.rs must define Session::drop");

        assert!(
            drop_impl.contains("quarantine_connection(self.connection_id)"),
            "Session::drop must quarantine (not immediately unregister) the GC \
             registration — tikv-client Transaction::Drop does not send a rollback \
             RPC, so locks may persist until TiKV lock TTL"
        );
        assert!(
            !drop_impl.contains("unregister_connection(self.connection_id)"),
            "Session::drop must NOT immediately unregister — use quarantine instead"
        );
    }
}
