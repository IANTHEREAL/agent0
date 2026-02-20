//! Session management for transactions

use crate::config::SharedServerConfig;
use crate::observability::TenantObservability;
use crate::sql::error::SqlError;
use crate::sql::query_context::QueryContext;
use crate::storage::TikvStore;
use crate::txn::SavepointState;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tikv_client::Transaction;

pub(crate) const DEFAULT_MAX_SORT_BYTES: usize = 256 * 1024 * 1024;

pub enum TransactionState {
    Idle,
    Active(Transaction),
    Failed(Transaction),
}

/// A small, per-connection container for session-level settings (GUCs).
///
/// This is intentionally compact and avoids heap allocations unless a setting is
/// explicitly changed by the client (`SET` / `set_config`).
#[derive(Debug, Default)]
pub(crate) struct SessionSettings {
    search_path: Vec<String>,

    /// Maximum bytes allowed for in-memory sort (ORDER BY).
    /// Default: 256 MB. 0 = unlimited.
    max_sort_bytes: usize,

    // pg_dump startup variables we keep for readback (`SHOW`) and later timeout enforcement.
    statement_timeout_ms: u64,
    default_statement_timeout_ms: u64,
    lock_timeout_ms: u64,
    idle_in_transaction_session_timeout_ms: u64,
    default_idle_in_transaction_session_timeout_ms: u64,
    timezone: Option<String>,
    application_name: Option<String>,
    client_encoding: Option<String>,
    standard_conforming_strings: Option<String>,
    check_function_bodies: Option<String>,
    xmloption: Option<String>,
    client_min_messages: Option<String>,
    row_security: Option<String>,
    default_tablespace: Option<String>,
    default_table_access_method: Option<String>,
    transaction_isolation: Option<String>,
    default_transaction_read_only: Option<String>,

    /// Generic storage for GUC parameters that tipg does not actively use but
    /// drivers expect to SET/SHOW without error (e.g. `extra_float_digits`,
    /// `DateStyle`, `work_mem`). Values are stored as-is for `SHOW` readback.
    extra_settings: HashMap<String, String>,
}

impl SessionSettings {
    fn default_search_path() -> Vec<String> {
        vec!["$user".to_string(), "public".to_string()]
    }

    fn quote_search_path_schema(schema: &str) -> String {
        if schema == "$user" {
            return "\"$user\"".to_string();
        }
        let simple_ident = schema.chars().enumerate().all(|(i, c)| {
            if i == 0 {
                c.is_ascii_lowercase() || c == '_'
            } else {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
            }
        });
        if simple_ident {
            schema.to_string()
        } else {
            format!("\"{}\"", schema.replace('"', "\"\""))
        }
    }

    fn format_search_path_show(search_path: &[String]) -> String {
        search_path
            .iter()
            .map(|s| Self::quote_search_path_schema(s))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn format_timeout_show(ms: u64) -> String {
        if ms == 0 {
            "0".to_string()
        } else {
            format!("{}ms", ms)
        }
    }

    pub(crate) fn new() -> Self {
        Self::new_with_defaults(0, 0)
    }

    pub(crate) fn new_with_defaults(
        default_statement_timeout_ms: u64,
        default_idle_in_txn_timeout_ms: u64,
    ) -> Self {
        Self {
            search_path: Self::default_search_path(),
            max_sort_bytes: DEFAULT_MAX_SORT_BYTES,
            statement_timeout_ms: default_statement_timeout_ms,
            default_statement_timeout_ms,
            idle_in_transaction_session_timeout_ms: default_idle_in_txn_timeout_ms,
            default_idle_in_transaction_session_timeout_ms: default_idle_in_txn_timeout_ms,
            ..Default::default()
        }
    }

    pub(crate) fn search_path(&self) -> &[String] {
        &self.search_path
    }

    pub(crate) fn set_search_path(&mut self, search_path: Vec<String>) {
        self.search_path = search_path;
    }

    fn parse_timeout_millis(value: &str) -> Result<u64> {
        let s = value.trim();
        if s.is_empty() {
            return Ok(0);
        }

        // PostgreSQL accepts units like `ms`, `s`, `min`, `h` for timeout GUCs.
        // Keep parsing strict and allocation-free: scan the numeric prefix and then
        // interpret an optional unit suffix.
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == 0 {
            return Err(SqlError::InvalidParameterValue {
                message: format!("invalid timeout value '{}'", value),
            }
            .into());
        }
        let (num_part, unit_part) = s.split_at(i);
        let unit = unit_part.trim();

        let num: f64 = num_part
            .parse()
            .map_err(|_| SqlError::InvalidParameterValue {
                message: format!("invalid timeout value '{}'", value),
            })?;
        if !num.is_finite() || num < 0.0 {
            return Err(SqlError::InvalidParameterValue {
                message: format!("invalid timeout value '{}'", value),
            }
            .into());
        }
        if num == 0.0 {
            return Ok(0);
        }

        let multiplier: f64 = if unit.is_empty() {
            1.0
        } else if unit.eq_ignore_ascii_case("ms") {
            1.0
        } else if unit.eq_ignore_ascii_case("s") {
            1_000.0
        } else if unit.eq_ignore_ascii_case("min") {
            60_000.0
        } else if unit.eq_ignore_ascii_case("h") {
            3_600_000.0
        } else {
            return Err(SqlError::InvalidParameterValue {
                message: format!("invalid timeout unit '{}'", unit),
            }
            .into());
        };

        let ms = (num * multiplier).ceil();
        if ms > u64::MAX as f64 {
            return Err(SqlError::InvalidParameterValue {
                message: format!("timeout value out of range '{}'", value),
            }
            .into());
        }
        Ok(ms as u64)
    }

    /// Public wrapper for timeout value parsing. Used by ALTER SYSTEM SET handler.
    pub(crate) fn parse_timeout_value(value: &str) -> Result<u64> {
        Self::parse_timeout_millis(value)
    }

    fn parse_byte_size(value: &str) -> Result<usize> {
        let s = value.trim();
        if s.is_empty() {
            return Err(SqlError::InvalidParameterValue {
                message: format!("invalid byte size value '{}'", value),
            }
            .into());
        }

        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }

        if i == 0 {
            return Err(SqlError::InvalidParameterValue {
                message: format!("invalid byte size value '{}'", value),
            }
            .into());
        }

        let (num_part, unit_part) = s.split_at(i);
        let num: u128 = num_part
            .parse()
            .map_err(|_| SqlError::InvalidParameterValue {
                message: format!("invalid byte size value '{}'", value),
            })?;
        let unit = unit_part.trim();

        let multiplier: u128 = if unit.is_empty() {
            1
        } else if unit.eq_ignore_ascii_case("kb") {
            1024
        } else if unit.eq_ignore_ascii_case("mb") {
            1024 * 1024
        } else if unit.eq_ignore_ascii_case("gb") {
            1024 * 1024 * 1024
        } else {
            return Err(SqlError::InvalidParameterValue {
                message: format!("invalid byte size unit '{}'", unit),
            }
            .into());
        };

        let total = num
            .checked_mul(multiplier)
            .ok_or_else(|| SqlError::InvalidParameterValue {
                message: format!("byte size value out of range '{}'", value),
            })?;
        if total > usize::MAX as u128 {
            return Err(SqlError::InvalidParameterValue {
                message: format!("byte size value out of range '{}'", value),
            }
            .into());
        }

        Ok(total as usize)
    }

    /// Set a session setting. Known settings (timeout, encoding, etc.) are validated
    /// and stored in typed fields. Unknown GUCs are stored in a generic map for
    /// `SHOW` readback — this allows drivers that SET parameters like `extra_float_digits`
    /// or `DateStyle` to work without error.
    pub(crate) fn set_known_setting(&mut self, name: &str, value: String) -> Result<bool> {
        match name {
            "statement_timeout" => self.statement_timeout_ms = Self::parse_timeout_millis(&value)?,
            "lock_timeout" => self.lock_timeout_ms = Self::parse_timeout_millis(&value)?,
            "idle_in_transaction_session_timeout" => {
                self.idle_in_transaction_session_timeout_ms = Self::parse_timeout_millis(&value)?
            }
            "pgtikv.max_sort_bytes" => {
                self.max_sort_bytes = Self::parse_byte_size(&value)?;
            }
            "tipg.use_optimizer" => {
                let normalized = value.trim().to_lowercase();
                match normalized.as_str() {
                    "on" | "true" | "yes" | "1" => { /* already on, no-op */ }
                    "off" | "false" | "no" | "0" => {
                        tracing::info!(
                            "NOTICE: optimizer cannot be disabled; \
                             tipg.use_optimizer setting ignored"
                        );
                    }
                    _ => {
                        return Err(SqlError::InvalidParameterValue {
                            message: "parameter \"tipg.use_optimizer\" requires a Boolean value"
                                .into(),
                        }
                        .into())
                    }
                }
            }
            "timezone" => {
                crate::types::timestamp::TimeZoneSpec::try_parse(&value)?;
                self.timezone = Some(value);
            }
            "application_name" => self.application_name = Some(value),
            "client_encoding" => {
                let enc = value.trim();
                if enc.eq_ignore_ascii_case("utf8") || enc.eq_ignore_ascii_case("utf-8") {
                    // Server is UTF-8 only and does not support transcoding. Accept UTF-8 aliases
                    // and store the canonical Postgres spelling.
                    self.client_encoding = Some("UTF8".to_string());
                } else {
                    return Err(SqlError::Unsupported(format!(
                        "unsupported client_encoding '{}'; only UTF8 is supported",
                        value
                    ))
                    .into());
                }
            }
            "standard_conforming_strings" => self.standard_conforming_strings = Some(value),
            "check_function_bodies" => self.check_function_bodies = Some(value),
            "xmloption" => self.xmloption = Some(value),
            "client_min_messages" => self.client_min_messages = Some(value),
            "row_security" => self.row_security = Some(value),
            "default_tablespace" => self.default_tablespace = Some(value),
            "default_table_access_method" => self.default_table_access_method = Some(value),
            "transaction_isolation" => {
                let normalized = value.trim().to_lowercase();
                match normalized.as_str() {
                    "read uncommitted" | "read committed" => {
                        // TiKV snapshot isolation is equivalent to REPEATABLE READ.
                        // Accept these levels for driver compatibility but honestly
                        // report what the engine actually provides.
                        tracing::warn!(
                            requested = normalized.as_str(),
                            actual = "repeatable read",
                            "TiKV provides snapshot isolation (REPEATABLE READ); \
                             the requested isolation level has been upgraded"
                        );
                        self.transaction_isolation = Some("repeatable read".to_string());
                    }
                    "repeatable read" => {
                        self.transaction_isolation = Some("repeatable read".to_string());
                    }
                    "serializable" => {
                        return Err(SqlError::Unsupported(
                            "SERIALIZABLE isolation level is not supported".into(),
                        )
                        .into());
                    }
                    _ => {
                        return Err(SqlError::InvalidParameterValue {
                            message: format!(
                                "invalid value for parameter \"transaction_isolation\": \"{}\"",
                                value
                            ),
                        }
                        .into());
                    }
                }
            }
            "default_transaction_read_only" => {
                let normalized = value.trim().to_lowercase();
                match normalized.as_str() {
                    "on" | "true" | "yes" | "1" => {
                        self.default_transaction_read_only = Some("on".to_string());
                    }
                    "off" | "false" | "no" | "0" => {
                        self.default_transaction_read_only = Some("off".to_string());
                    }
                    _ => {
                        return Err(SqlError::InvalidParameterValue {
                            message:
                                "parameter \"default_transaction_read_only\" requires a Boolean value"
                                    .into(),
                        }
                        .into());
                    }
                }
            }
            _ => {
                // Generic storage for GUC parameters that tipg does not actively
                // use but drivers expect to SET/SHOW (e.g. extra_float_digits,
                // DateStyle, work_mem). Store as-is for SHOW readback.
                self.extra_settings.insert(name.to_string(), value);
                return Ok(true);
            }
        }
        Ok(true)
    }

    /// Reset a single session setting to its default value.
    pub(crate) fn reset_setting(&mut self, name: &str) {
        match name {
            "search_path" => self.search_path = Self::default_search_path(),
            "statement_timeout" => self.statement_timeout_ms = self.default_statement_timeout_ms,
            "lock_timeout" => self.lock_timeout_ms = 0,
            "idle_in_transaction_session_timeout" => {
                self.idle_in_transaction_session_timeout_ms =
                    self.default_idle_in_transaction_session_timeout_ms
            }
            "pgtikv.max_sort_bytes" | "tipg.max_sort_bytes" => {
                self.max_sort_bytes = DEFAULT_MAX_SORT_BYTES
            }
            "tipg.use_optimizer" => {}
            "timezone" => self.timezone = None,
            "application_name" => self.application_name = None,
            "client_encoding" => self.client_encoding = None,
            "standard_conforming_strings" => self.standard_conforming_strings = None,
            "check_function_bodies" => self.check_function_bodies = None,
            "xmloption" => self.xmloption = None,
            "client_min_messages" => self.client_min_messages = None,
            "row_security" => self.row_security = None,
            "default_tablespace" => self.default_tablespace = None,
            "default_table_access_method" => self.default_table_access_method = None,
            "transaction_isolation" => self.transaction_isolation = None,
            "default_transaction_read_only" => self.default_transaction_read_only = None,
            _ => {
                self.extra_settings.remove(name);
            }
        }
    }

    /// Reset all session settings to their defaults.
    pub(crate) fn reset_all_settings(&mut self) {
        *self = Self::new_with_defaults(
            self.default_statement_timeout_ms,
            self.default_idle_in_transaction_session_timeout_ms,
        );
    }

    /// Get a session setting value in a Postgres-like string form, for `SHOW`.
    pub(crate) fn show_value(&self, name: &str) -> Option<String> {
        match name {
            // These are used heavily by drivers for feature detection.
            "server_version" => Some("16.0".to_string()),
            "server_version_num" => Some("160000".to_string()),
            "server_encoding" => Some("UTF8".to_string()),
            "search_path" => Some(Self::format_search_path_show(&self.search_path)),
            "datestyle" => Some("ISO, MDY".to_string()),
            "integer_datetimes" => Some("on".to_string()),
            "intervalstyle" => Some("postgres".to_string()),
            "statement_timeout" => Some(Self::format_timeout_show(self.statement_timeout_ms)),
            "lock_timeout" => Some(Self::format_timeout_show(self.lock_timeout_ms)),
            "idle_in_transaction_session_timeout" => Some(Self::format_timeout_show(
                self.idle_in_transaction_session_timeout_ms,
            )),
            "pgtikv.max_sort_bytes" => Some(self.max_sort_bytes.to_string()),
            "tipg.use_optimizer" => Some("on".to_string()),
            "timezone" => Some(self.timezone.as_deref().unwrap_or("UTC").to_string()),
            "application_name" => Some(self.application_name.as_deref().unwrap_or("").to_string()),
            "client_encoding" => Some(
                self.client_encoding
                    .as_deref()
                    .unwrap_or("UTF8")
                    .to_string(),
            ),
            "standard_conforming_strings" => Some(
                self.standard_conforming_strings
                    .as_deref()
                    .unwrap_or("on")
                    .to_string(),
            ),
            "check_function_bodies" => Some(
                self.check_function_bodies
                    .as_deref()
                    .unwrap_or("on")
                    .to_string(),
            ),
            "xmloption" => Some(self.xmloption.as_deref().unwrap_or("content").to_string()),
            "client_min_messages" => Some(
                self.client_min_messages
                    .as_deref()
                    .unwrap_or("notice")
                    .to_string(),
            ),
            "row_security" => Some(self.row_security.as_deref().unwrap_or("on").to_string()),
            "default_tablespace" => {
                Some(self.default_tablespace.as_deref().unwrap_or("").to_string())
            }
            "default_table_access_method" => Some(
                self.default_table_access_method
                    .as_deref()
                    .unwrap_or("heap")
                    .to_string(),
            ),
            // Transaction isolation level. Support both forms:
            // - "transaction_isolation" (standard PostgreSQL GUC name)
            // - "transaction.isolation.level" (how SHOW transaction isolation level parses)
            "transaction_isolation" | "transaction.isolation.level" => Some(
                self.transaction_isolation
                    .as_deref()
                    .unwrap_or("repeatable read")
                    .to_string(),
            ),
            "default_transaction_isolation" => Some("read committed".to_string()),
            "default_transaction_read_only" => Some(
                self.default_transaction_read_only
                    .as_deref()
                    .unwrap_or("off")
                    .to_string(),
            ),
            _ => self.extra_settings.get(name).cloned(),
        }
    }

    pub(crate) fn statement_timeout(&self) -> Option<Duration> {
        if self.statement_timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(self.statement_timeout_ms))
        }
    }

    pub(crate) fn idle_in_transaction_session_timeout(&self) -> Option<Duration> {
        if self.idle_in_transaction_session_timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(
                self.idle_in_transaction_session_timeout_ms,
            ))
        }
    }

    pub(crate) fn max_sort_bytes(&self) -> usize {
        self.max_sort_bytes
    }
}

pub struct Session {
    store: Arc<TikvStore>,
    observability: Arc<TenantObservability>,
    state: TransactionState,
    savepoints: Arc<SavepointState>,
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
    transaction_timestamp_ms: Option<i64>,
    /// Timestamp of the last command completion while in a transaction block.
    /// Used for `idle_in_transaction_session_timeout` enforcement.
    /// Set by `record_command_complete()`, cleared on commit/rollback.
    last_command_complete_at: Option<Instant>,
    /// Shared server-level configuration (for ALTER SYSTEM SET).
    server_config: Option<SharedServerConfig>,
    /// Pending parameter values from extended-query Bind for the next Execute.
    /// Set by the protocol handler before calling `execute()`, consumed by
    /// `query_context_for_statement()` so they flow into `QUERY_PARAMS`.
    pending_params: Vec<Option<crate::types::Value>>,
    /// Pending parameter types from Parse-time analysis for the next Execute.
    /// Set by the protocol handler, consumed by `query_context_for_statement()`
    /// so they flow into `QUERY_PARAM_TYPES`.
    pending_param_types: Vec<Option<crate::types::DataType>>,
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
            is_superuser: is_superuser,
            current_database_id: database_id,
            current_database_name: Arc::from(database_name),
            connection_id,
            transaction_timestamp_ms: None,
            last_command_complete_at: None,
            server_config: None,
            pending_params: vec![],
            pending_param_types: vec![],
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
        qctx
    }

    /// Set parameter values for the next statement execution.
    /// Called by the protocol handler after decoding Bind parameters.
    /// Consumed (drained) by `query_context_for_statement()`.
    pub fn set_pending_params(&mut self, params: Vec<Option<crate::types::Value>>) {
        self.pending_params = params;
    }

    /// Set parameter types for the next statement execution.
    /// Called by the protocol handler to thread Parse-time types to Execute-time Analyzer.
    /// Consumed (drained) by `query_context_for_statement()`.
    pub fn set_pending_param_types(&mut self, types: Vec<Option<crate::types::DataType>>) {
        self.pending_param_types = types;
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
    }

    pub(crate) fn set_known_setting(&mut self, name: &str, value: String) -> Result<bool> {
        self.settings.set_known_setting(name, value)
    }

    pub(crate) fn reset_setting(&mut self, name: &str) {
        self.settings.reset_setting(name);
    }

    pub(crate) fn reset_all_settings(&mut self) {
        self.settings.reset_all_settings();
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

    pub(crate) fn statement_timeout(&self) -> Option<Duration> {
        self.settings.statement_timeout()
    }

    pub(crate) fn max_sort_bytes(&self) -> usize {
        self.settings.max_sort_bytes()
    }

    pub async fn create_savepoint(&mut self, name: String) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(SqlError::NoActiveTransaction {
                message: "SAVEPOINT can only be used in transaction blocks".into(),
            }
            .into());
        }
        if self.is_transaction_failed() {
            return Err(SqlError::InFailedTransaction.into());
        }
        self.savepoints.create(name).await
    }

    pub async fn release_savepoint(&mut self, name: &str) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(SqlError::NoActiveTransaction {
                message: "RELEASE SAVEPOINT can only be used in transaction blocks".into(),
            }
            .into());
        }
        if self.is_transaction_failed() {
            return Err(SqlError::InFailedTransaction.into());
        }
        self.savepoints.release(name).await
    }

    pub async fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(SqlError::NoActiveTransaction {
                message: "ROLLBACK TO SAVEPOINT can only be used in transaction blocks".into(),
            }
            .into());
        }

        let mut prepared = self.savepoints.prepare_rollback_to(name).await?;
        let res = {
            let txn = self.get_mut_txn().expect("transaction must be active");

            // Undo nested savepoints first, then the target savepoint itself.
            for sp in prepared.popped.iter_mut().rev() {
                let mut entries = Vec::with_capacity(sp.undo.len());
                entries.extend(sp.undo.drain());
                entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                for (key, prev) in entries {
                    match prev {
                        Some(val) => txn.put(key, val).await.map_err(|e| anyhow!(e))?,
                        None => txn.delete(key).await.map_err(|e| anyhow!(e))?,
                    }
                }
            }

            prepared.target_undo.sort_by(|a, b| a.key.cmp(&b.key));
            for rec in prepared.target_undo.drain(..) {
                match rec.prev {
                    Some(val) => txn.put(rec.key, val).await.map_err(|e| anyhow!(e))?,
                    None => txn.delete(rec.key).await.map_err(|e| anyhow!(e))?,
                }
            }

            Ok::<(), anyhow::Error>(())
        };

        if let Err(e) = res {
            // If undo fails, the transaction is likely in an unknown state. Abort it.
            let _ = self.rollback().await;
            return Err(e);
        }
        self.clear_failed_transaction();
        Ok(())
    }

    /// Start a transaction block (BEGIN)
    pub async fn begin(&mut self) -> Result<()> {
        match self.state {
            TransactionState::Idle => {
                // Capture the transaction start timestamp before awaiting store/savepoint
                // operations, so TiKV begin latency does not skew NOW()/TRANSACTION_TIMESTAMP().
                // Use the task-local statement timestamp when available (i.e. when called
                // from within execute_single's with_timestamps scope) to match PostgreSQL
                // semantics where transaction_timestamp = start of the BEGIN statement.
                let ts = super::statement_time::statement_timestamp_millis_or_now();
                let txn = self.store.begin().await?;
                self.savepoints.reset().await?;
                self.state = TransactionState::Active(txn);
                self.transaction_timestamp_ms = Some(ts);
                Ok(())
            }
            TransactionState::Active(_) | TransactionState::Failed(_) => {
                // Already in transaction, ignore
                Ok(())
            }
        }
    }

    /// Returns the transaction start timestamp, or None if not in an explicit transaction.
    pub fn transaction_timestamp_ms(&self) -> Option<i64> {
        self.transaction_timestamp_ms
    }

    /// Commit a transaction block (COMMIT)
    pub async fn commit(&mut self) -> Result<()> {
        self.transaction_timestamp_ms = None;
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(mut txn) => {
                self.savepoints.reset().await?;
                match txn.commit().await {
                    Ok(_) => {
                        self.observability.record_commit();
                        Ok(())
                    }
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Failed(mut txn) => {
                self.savepoints.reset().await?;
                match txn.rollback().await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Idle => Ok(()), // No-op
        }
    }

    /// Rollback a transaction block (ROLLBACK)
    pub async fn rollback(&mut self) -> Result<()> {
        self.transaction_timestamp_ms = None;
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(mut txn) => {
                self.savepoints.reset().await?;
                match txn.rollback().await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Failed(mut txn) => {
                self.savepoints.reset().await?;
                match txn.rollback().await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        self.state = TransactionState::Failed(txn);
                        Err(anyhow!(e))
                    }
                }
            }
            TransactionState::Idle => Ok(()), // No-op
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionSettings;
    use std::time::Duration;

    #[test]
    fn test_session_settings_new_with_defaults() {
        let settings = SessionSettings::new_with_defaults(1_500, 2_500);

        assert_eq!(settings.statement_timeout_ms, 1_500);
        assert_eq!(settings.default_statement_timeout_ms, 1_500);
        assert_eq!(settings.idle_in_transaction_session_timeout_ms, 2_500);
        assert_eq!(
            settings.default_idle_in_transaction_session_timeout_ms,
            2_500
        );
        assert_eq!(
            settings.statement_timeout(),
            Some(Duration::from_millis(1_500))
        );
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("2500ms")
        );
    }

    #[test]
    fn test_session_settings_defaults_and_overrides() {
        let mut settings = SessionSettings::new();

        assert_eq!(
            settings.show_value("server_version").as_deref(),
            Some("16.0")
        );
        assert_eq!(
            settings.show_value("server_version_num").as_deref(),
            Some("160000")
        );
        assert_eq!(
            settings.show_value("server_encoding").as_deref(),
            Some("UTF8")
        );
        assert_eq!(
            settings.show_value("datestyle").as_deref(),
            Some("ISO, MDY")
        );
        assert_eq!(
            settings.show_value("integer_datetimes").as_deref(),
            Some("on")
        );
        assert_eq!(
            settings.show_value("intervalstyle").as_deref(),
            Some("postgres")
        );
        assert_eq!(settings.show_value("timezone").as_deref(), Some("UTC"));
        assert_eq!(settings.show_value("application_name").as_deref(), Some(""));
        assert_eq!(
            settings.show_value("search_path").as_deref(),
            Some("\"$user\", public")
        );
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("0")
        );
        assert_eq!(
            settings.show_value("pgtikv.max_sort_bytes").as_deref(),
            Some("268435456")
        );
        assert_eq!(
            settings.show_value("client_encoding").as_deref(),
            Some("UTF8")
        );

        assert!(settings
            .set_known_setting("client_min_messages", "warning".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("client_min_messages").as_deref(),
            Some("warning")
        );

        assert!(settings
            .set_known_setting("timezone", "Asia/Shanghai".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("timezone").as_deref(),
            Some("Asia/Shanghai")
        );

        assert!(settings
            .set_known_setting("timezone", "localtime".to_string())
            .is_err());
        assert_eq!(
            settings.show_value("timezone").as_deref(),
            Some("Asia/Shanghai")
        );

        assert!(settings
            .set_known_setting("application_name", "pg-tikv-tests".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("application_name").as_deref(),
            Some("pg-tikv-tests")
        );

        // Unknown GUCs are stored in extra_settings for driver compatibility
        assert!(settings
            .set_known_setting("unknown_setting", "x".to_string())
            .unwrap());
        assert_eq!(settings.show_value("unknown_setting").as_deref(), Some("x"));

        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );
        assert_eq!(
            settings
                .show_value("transaction.isolation.level")
                .as_deref(),
            Some("repeatable read")
        );
    }

    #[test]
    fn test_session_settings_client_encoding_utf8_only() {
        let mut settings = SessionSettings::new();

        assert!(settings
            .set_known_setting("client_encoding", "LATIN1".to_string())
            .is_err());
        assert_eq!(
            settings.show_value("client_encoding").as_deref(),
            Some("UTF8")
        );

        assert!(settings
            .set_known_setting("client_encoding", "UTF-8".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("client_encoding").as_deref(),
            Some("UTF8")
        );
    }

    #[test]
    fn test_session_settings_timeout_parsing() {
        let mut settings = SessionSettings::new();

        settings
            .set_known_setting("statement_timeout", "20".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("20ms")
        );

        settings
            .set_known_setting("statement_timeout", "1s".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("1000ms")
        );

        assert!(settings
            .set_known_setting("statement_timeout", "-1".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("statement_timeout", "abc".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("statement_timeout", "1unknown".to_string())
            .is_err());
    }

    #[test]
    fn test_session_settings_max_sort_bytes_parsing() {
        let mut settings = SessionSettings::new();

        settings
            .set_known_setting("pgtikv.max_sort_bytes", "268435456".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("pgtikv.max_sort_bytes").as_deref(),
            Some("268435456")
        );

        settings
            .set_known_setting("pgtikv.max_sort_bytes", "256MB".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("pgtikv.max_sort_bytes").as_deref(),
            Some("268435456")
        );

        settings
            .set_known_setting("pgtikv.max_sort_bytes", "1gb".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("pgtikv.max_sort_bytes").as_deref(),
            Some("1073741824")
        );

        settings
            .set_known_setting("pgtikv.max_sort_bytes", "0".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("pgtikv.max_sort_bytes").as_deref(),
            Some("0")
        );

        assert!(settings
            .set_known_setting("pgtikv.max_sort_bytes", "-1".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("pgtikv.max_sort_bytes", "1TB".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("pgtikv.max_sort_bytes", "abc".to_string())
            .is_err());
    }

    #[test]
    fn test_session_settings_transaction_isolation() {
        let mut settings = SessionSettings::new();

        // Default value — TiKV snapshot isolation = REPEATABLE READ
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );
        assert_eq!(
            settings
                .show_value("default_transaction_read_only")
                .as_deref(),
            Some("off")
        );

        // Set and readback
        assert!(settings
            .set_known_setting("transaction_isolation", "repeatable read".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );
        // SHOW transaction isolation level parses as "transaction.isolation.level"
        assert_eq!(
            settings
                .show_value("transaction.isolation.level")
                .as_deref(),
            Some("repeatable read")
        );

        assert!(settings
            .set_known_setting("default_transaction_read_only", "on".to_string())
            .unwrap());
        assert_eq!(
            settings
                .show_value("default_transaction_read_only")
                .as_deref(),
            Some("on")
        );

        // Reset back
        assert!(settings
            .set_known_setting("default_transaction_read_only", "off".to_string())
            .unwrap());
        assert_eq!(
            settings
                .show_value("default_transaction_read_only")
                .as_deref(),
            Some("off")
        );

        // SERIALIZABLE must be rejected
        assert!(settings
            .set_known_setting("transaction_isolation", "serializable".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("transaction_isolation", "SERIALIZABLE".to_string())
            .is_err());
        // Value should remain unchanged after rejection
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );

        // Garbage values must be rejected
        assert!(settings
            .set_known_setting("transaction_isolation", "garbage".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("transaction_isolation", "snapshot".to_string())
            .is_err());

        // READ UNCOMMITTED is upgraded to REPEATABLE READ (TiKV snapshot isolation)
        assert!(settings
            .set_known_setting("transaction_isolation", "read uncommitted".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );

        // READ COMMITTED is also upgraded to REPEATABLE READ
        assert!(settings
            .set_known_setting("transaction_isolation", "read committed".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );

        // default_transaction_read_only: boolean aliases
        for val in &["true", "yes", "1", "on", "TRUE", "Yes"] {
            assert!(settings
                .set_known_setting("default_transaction_read_only", val.to_string())
                .unwrap());
            assert_eq!(
                settings
                    .show_value("default_transaction_read_only")
                    .as_deref(),
                Some("on")
            );
        }
        for val in &["false", "no", "0", "off", "FALSE", "No"] {
            assert!(settings
                .set_known_setting("default_transaction_read_only", val.to_string())
                .unwrap());
            assert_eq!(
                settings
                    .show_value("default_transaction_read_only")
                    .as_deref(),
                Some("off")
            );
        }
        // Garbage boolean must be rejected
        assert!(settings
            .set_known_setting("default_transaction_read_only", "maybe".to_string())
            .is_err());
    }

    #[test]
    fn test_session_settings_reset_setting() {
        let mut settings = SessionSettings::new();

        // Change a few settings
        settings
            .set_known_setting("statement_timeout", "5000".to_string())
            .unwrap();
        settings
            .set_known_setting("timezone", "US/Eastern".to_string())
            .unwrap();
        settings
            .set_known_setting("extra_float_digits", "3".to_string())
            .unwrap();

        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("5000ms")
        );
        assert_eq!(
            settings.show_value("timezone").as_deref(),
            Some("US/Eastern")
        );
        assert_eq!(
            settings.show_value("extra_float_digits").as_deref(),
            Some("3")
        );

        // Reset individual settings
        settings.reset_setting("statement_timeout");
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("0")
        );

        settings.reset_setting("timezone");
        assert_eq!(settings.show_value("timezone").as_deref(), Some("UTC"));

        settings.reset_setting("extra_float_digits");
        assert_eq!(settings.show_value("extra_float_digits"), None);
    }

    #[test]
    fn test_session_settings_reset_all() {
        let mut settings = SessionSettings::new();

        settings
            .set_known_setting("statement_timeout", "5000".to_string())
            .unwrap();
        settings
            .set_known_setting("timezone", "US/Eastern".to_string())
            .unwrap();
        settings
            .set_known_setting("application_name", "myapp".to_string())
            .unwrap();

        settings.reset_all_settings();

        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("0")
        );
        assert_eq!(settings.show_value("timezone").as_deref(), Some("UTC"));
        assert_eq!(settings.show_value("application_name").as_deref(), Some(""));
        assert_eq!(
            settings.show_value("search_path").as_deref(),
            Some("\"$user\", public")
        );
    }

    #[test]
    fn test_session_settings_reset_preserves_set_sentinel_value() {
        // SET application_name TO '__RESET__' must NOT trigger a reset — it's a normal SET.
        let mut settings = SessionSettings::new();
        settings
            .set_known_setting("application_name", "__RESET__".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("application_name").as_deref(),
            Some("__RESET__")
        );
    }

    #[test]
    fn test_in_failed_sql_transaction_message() {
        use crate::sql::error::SqlError;
        assert_eq!(
            SqlError::InFailedTransaction.to_string(),
            "current transaction is aborted, commands ignored until end of transaction block"
        );
    }

    #[test]
    fn test_reset_statement_timeout_restores_server_default() {
        let mut settings = SessionSettings::new_with_defaults(5_000, 3_000);

        // Verify initial value is the server default
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("5000ms")
        );

        // Change it
        settings
            .set_known_setting("statement_timeout", "10000".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("10000ms")
        );

        // RESET should restore to server default (5000), not 0
        settings.reset_setting("statement_timeout");
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("5000ms")
        );
        assert_eq!(
            settings.statement_timeout(),
            Some(Duration::from_millis(5_000))
        );
    }

    #[test]
    fn test_reset_idle_in_transaction_timeout_restores_server_default() {
        let mut settings = SessionSettings::new_with_defaults(5_000, 3_000);

        // Verify initial value
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("3000ms")
        );

        // Change it
        settings
            .set_known_setting("idle_in_transaction_session_timeout", "10000".to_string())
            .unwrap();
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("10000ms")
        );

        // RESET should restore to server default (3000), not 0
        settings.reset_setting("idle_in_transaction_session_timeout");
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("3000ms")
        );
    }

    #[test]
    fn test_reset_all_restores_server_defaults() {
        let mut settings = SessionSettings::new_with_defaults(5_000, 3_000);

        // Change both timeouts
        settings
            .set_known_setting("statement_timeout", "20000".to_string())
            .unwrap();
        settings
            .set_known_setting("idle_in_transaction_session_timeout", "15000".to_string())
            .unwrap();

        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("20000ms")
        );
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("15000ms")
        );

        // RESET ALL should restore both to server defaults
        settings.reset_all_settings();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("5000ms")
        );
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("3000ms")
        );
    }

    #[test]
    fn test_statement_timeout_getter_with_nonzero_default() {
        let settings = SessionSettings::new_with_defaults(5_000, 0);
        assert_eq!(
            settings.statement_timeout(),
            Some(Duration::from_millis(5_000))
        );

        // Zero default means no timeout
        let settings_zero = SessionSettings::new_with_defaults(0, 0);
        assert_eq!(settings_zero.statement_timeout(), None);
    }

    #[test]
    fn test_parse_timeout_value() {
        // Pure numeric (milliseconds)
        assert_eq!(SessionSettings::parse_timeout_value("5000").unwrap(), 5_000);
        assert_eq!(SessionSettings::parse_timeout_value("0").unwrap(), 0);

        // With suffix
        assert_eq!(SessionSettings::parse_timeout_value("1s").unwrap(), 1_000);
        assert_eq!(SessionSettings::parse_timeout_value("5s").unwrap(), 5_000);
        assert_eq!(SessionSettings::parse_timeout_value("100ms").unwrap(), 100);
        assert_eq!(
            SessionSettings::parse_timeout_value("1min").unwrap(),
            60_000
        );
        assert_eq!(
            SessionSettings::parse_timeout_value("2h").unwrap(),
            7_200_000
        );

        // Invalid
        assert!(SessionSettings::parse_timeout_value("-1").is_err());
        assert!(SessionSettings::parse_timeout_value("abc").is_err());
        assert!(SessionSettings::parse_timeout_value("1unknown").is_err());
    }
}
