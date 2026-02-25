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
use crate::sql::executor::core::prepared_stmt::PreparedStatement as SqlPreparedStatement;
use crate::sql::query_context::{QueryContext, XactAdvisorySavepointTracker};
use crate::storage::TikvStore;
use crate::txn::SavepointState;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tikv_client::Transaction;

pub(crate) const DEFAULT_MAX_SORT_BYTES: usize = 256 * 1024 * 1024;

pub enum TransactionState {
    Idle,
    Active(Transaction),
    Failed(Transaction),
}

pub struct Session {
    pub(crate) store: Arc<TikvStore>,
    pub(crate) observability: Arc<TenantObservability>,
    pub(crate) state: TransactionState,
    pub(crate) savepoints: Arc<SavepointState>,
    last_sequence_values: HashMap<String, i64>,
    settings: SessionSettings,
    /// Authenticated session user (login role). This does not change with `SET ROLE`.
    session_user: Option<String>,
    session_user_is_superuser: bool,
    /// Current effective role. This can change with `SET ROLE` / `RESET ROLE`.
    current_user: Option<String>,
    is_superuser: bool,
    current_database_id: u64,
    current_database_name: Arc<str>,
    /// Connection ID for pg_backend_pid() support
    connection_id: i32,
    /// Timestamp (epoch millis) when the current explicit transaction started.
    /// None when not in an explicit transaction block.
    pub(crate) transaction_timestamp_ms: Option<i64>,
    /// Timestamp of the last command completion while in a transaction block.
    /// Used for `idle_in_transaction_session_timeout` enforcement.
    /// Set by `record_command_complete()`, cleared on commit/rollback.
    last_command_complete_at: Option<Instant>,
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
    /// True when current transaction used xact-scoped advisory lock functions.
    pub(crate) has_xact_advisory_locks: Arc<AtomicBool>,
    /// Savepoint-scoped tracker for xact advisory lock acquisitions.
    pub(crate) xact_advisory_savepoint_tracker: Arc<StdMutex<XactAdvisorySavepointTracker>>,
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
        connection_id: i32,
        database_id: u64,
        database_name: String,
        default_statement_timeout_ms: u64,
        default_idle_in_txn_timeout_ms: u64,
    ) -> Self {
        Self {
            store,
            observability,
            state: TransactionState::Idle,
            savepoints: Arc::new(SavepointState::new()),
            last_sequence_values: HashMap::new(),
            settings: SessionSettings::new_with_defaults(
                default_statement_timeout_ms,
                default_idle_in_txn_timeout_ms,
            ),
            session_user: None,
            session_user_is_superuser: false,
            current_user: None,
            is_superuser: false,
            current_database_id: database_id,
            current_database_name: Arc::from(database_name),
            connection_id,
            transaction_timestamp_ms: None,
            last_command_complete_at: None,
            server_config: None,
            pending_params: vec![],
            pending_param_types: vec![],
            sql_prepared_statements: HashMap::new(),
            has_xact_advisory_locks: Arc::new(AtomicBool::new(false)),
            xact_advisory_savepoint_tracker: Arc::new(StdMutex::new(
                XactAdvisorySavepointTracker::default(),
            )),
        }
    }

    /// Create a session for the given user and database.
    pub fn new_with_user_and_database(
        store: Arc<TikvStore>,
        observability: Arc<TenantObservability>,
        username: String,
        is_superuser: bool,
        connection_id: i32,
        database_id: u64,
        database_name: String,
        default_statement_timeout_ms: u64,
        default_idle_in_txn_timeout_ms: u64,
    ) -> Self {
        Self {
            store,
            observability,
            state: TransactionState::Idle,
            savepoints: Arc::new(SavepointState::new()),
            last_sequence_values: HashMap::new(),
            settings: SessionSettings::new_with_defaults(
                default_statement_timeout_ms,
                default_idle_in_txn_timeout_ms,
            ),
            session_user: Some(username.clone()),
            session_user_is_superuser: is_superuser,
            current_user: Some(username),
            is_superuser,
            current_database_id: database_id,
            current_database_name: Arc::from(database_name),
            connection_id,
            transaction_timestamp_ms: None,
            last_command_complete_at: None,
            server_config: None,
            pending_params: vec![],
            pending_param_types: vec![],
            sql_prepared_statements: HashMap::new(),
            has_xact_advisory_locks: Arc::new(AtomicBool::new(false)),
            xact_advisory_savepoint_tracker: Arc::new(StdMutex::new(
                XactAdvisorySavepointTracker::default(),
            )),
        }
    }

    pub fn current_user(&self) -> Option<&str> {
        self.current_user.as_deref()
    }

    pub fn session_user(&self) -> Option<&str> {
        self.session_user.as_deref()
    }

    pub fn is_superuser(&self) -> bool {
        self.is_superuser
    }

    pub(crate) fn set_current_role(&mut self, role: String, is_superuser: bool) {
        self.current_user = Some(role);
        self.is_superuser = is_superuser;
    }

    pub(crate) fn reset_role(&mut self) {
        self.current_user = self.session_user.clone();
        self.is_superuser = self.session_user_is_superuser;
    }

    pub fn connection_id(&self) -> i32 {
        self.connection_id
    }

    pub fn current_database_id(&self) -> u64 {
        self.current_database_id
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
        // Snapshot all session settings so current_setting() works in expression contexts.
        qctx.settings_snapshot = Some(Arc::new(self.all_settings_snapshot()));
        qctx.lock_timeout = self.lock_timeout();
        qctx.xact_advisory_lock_used = Some(self.has_xact_advisory_locks.clone());
        qctx.xact_advisory_savepoint_tracker = Some(self.xact_advisory_savepoint_tracker.clone());
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

    /// Check if currently in a transaction block
    pub fn is_in_transaction(&self) -> bool {
        matches!(
            self.state,
            TransactionState::Active(_) | TransactionState::Failed(_)
        )
    }

    pub fn is_transaction_failed(&self) -> bool {
        matches!(self.state, TransactionState::Failed(_))
    }

    pub(crate) fn mark_transaction_failed(&mut self) {
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(txn) => self.state = TransactionState::Failed(txn),
            other => self.state = other,
        }
    }

    pub(crate) fn clear_failed_transaction(&mut self) {
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
    ) -> Option<(&mut Transaction, &mut HashMap<String, i64>, &[String])> {
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
        self.settings.set_known_setting(name, value)
    }

    pub(crate) fn set_local_setting(&mut self, name: &str, value: String) -> Result<bool> {
        self.settings.set_local_override(name, value)
    }

    pub(crate) fn set_local_search_path(&mut self, search_path: Vec<String>) {
        self.settings.set_local_search_path(search_path);
    }

    pub(crate) fn clear_local_overrides(&mut self) {
        self.settings.clear_local_overrides();
    }

    pub(crate) fn reset_setting(&mut self, name: &str) {
        self.settings.reset_setting(name);
    }

    pub(crate) fn reset_all_settings(&mut self) {
        self.settings.reset_all_settings();
    }

    /// Snapshot all session settings into a flat map for `current_setting()` in
    /// expression contexts. Includes both `SessionSettings` values and session-level
    /// values (`is_superuser`, `session_authorization`).
    pub(crate) fn all_settings_snapshot(&self) -> HashMap<String, String> {
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
        map.insert("session_authorization".to_string(), session_auth.clone());
        map.insert("session.authorization".to_string(), session_auth);
        map
    }

    pub(crate) fn show_setting_value(&self, name: &str) -> Option<String> {
        match name {
            "is_superuser" => Some(if self.is_superuser { "on" } else { "off" }.to_string()),
            // Session authorization is the authenticated session user (login role).
            "session_authorization" | "session.authorization" => Some(
                self.session_user
                    .as_deref()
                    .or(self.current_user.as_deref())
                    .unwrap_or("postgres")
                    .to_string(),
            ),
            _ => self.settings.show_value(name),
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
}
