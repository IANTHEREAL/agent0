//! Session management for transactions

use crate::storage::TikvStore;
use crate::observability::TenantObservability;
use crate::txn::SavepointState;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tikv_client::Transaction;

#[derive(Debug)]
pub struct InFailedSqlTransaction;

impl std::fmt::Display for InFailedSqlTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "current transaction is aborted, commands ignored until end of transaction block"
        )
    }
}

impl std::error::Error for InFailedSqlTransaction {}

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

    // pg_dump startup variables we keep for readback (`SHOW`) and later timeout enforcement.
    statement_timeout_ms: u64,
    lock_timeout_ms: u64,
    idle_in_transaction_session_timeout_ms: u64,
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
}

impl SessionSettings {
    pub(crate) fn new() -> Self {
        Self {
            search_path: vec!["public".to_string()],
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
            return Err(anyhow!("invalid timeout value '{}'", value));
        }
        let (num_part, unit_part) = s.split_at(i);
        let unit = unit_part.trim();

        let num: f64 = num_part
            .parse()
            .map_err(|_| anyhow!("invalid timeout value '{}'", value))?;
        if !num.is_finite() || num < 0.0 {
            return Err(anyhow!("invalid timeout value '{}'", value));
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
            return Err(anyhow!("invalid timeout unit '{}'", unit));
        };

        let ms = (num * multiplier).ceil();
        if ms > u64::MAX as f64 {
            return Err(anyhow!("timeout value out of range '{}'", value));
        }
        Ok(ms as u64)
    }

    /// Set a known session setting. Returns `true` if the setting name is recognized.
    pub(crate) fn set_known_setting(&mut self, name: &str, value: String) -> Result<bool> {
        match name {
            "statement_timeout" => self.statement_timeout_ms = Self::parse_timeout_millis(&value)?,
            "lock_timeout" => self.lock_timeout_ms = Self::parse_timeout_millis(&value)?,
            "idle_in_transaction_session_timeout" => {
                self.idle_in_transaction_session_timeout_ms = Self::parse_timeout_millis(&value)?
            }
            "timezone" => self.timezone = Some(value),
            "application_name" => self.application_name = Some(value),
            "client_encoding" => self.client_encoding = Some(value),
            "standard_conforming_strings" => self.standard_conforming_strings = Some(value),
            "check_function_bodies" => self.check_function_bodies = Some(value),
            "xmloption" => self.xmloption = Some(value),
            "client_min_messages" => self.client_min_messages = Some(value),
            "row_security" => self.row_security = Some(value),
            "default_tablespace" => self.default_tablespace = Some(value),
            "default_table_access_method" => self.default_table_access_method = Some(value),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Get a session setting value in a Postgres-like string form, for `SHOW`.
    pub(crate) fn show_value(&self, name: &str) -> Option<String> {
        match name {
            // These are used heavily by drivers for feature detection.
            "server_version" => Some("16.0".to_string()),
            "server_version_num" => Some("160000".to_string()),
            "search_path" => Some(self.search_path.join(", ")),
            "statement_timeout" => Some(self.statement_timeout_ms.to_string()),
            "lock_timeout" => Some(self.lock_timeout_ms.to_string()),
            "idle_in_transaction_session_timeout" => {
                Some(self.idle_in_transaction_session_timeout_ms.to_string())
            }
            "timezone" => Some(self.timezone.as_deref().unwrap_or("UTC").to_string()),
            "application_name" => Some(self.application_name.as_deref().unwrap_or("").to_string()),
            "client_encoding" => Some(self.client_encoding.as_deref().unwrap_or("UTF8").to_string()),
            "standard_conforming_strings" => Some(
                self.standard_conforming_strings
                    .as_deref()
                    .unwrap_or("on")
                    .to_string(),
            ),
            "check_function_bodies" => Some(self.check_function_bodies.as_deref().unwrap_or("on").to_string()),
            "xmloption" => Some(self.xmloption.as_deref().unwrap_or("content").to_string()),
            "client_min_messages" => Some(
                self.client_min_messages
                    .as_deref()
                    .unwrap_or("notice")
                    .to_string(),
            ),
            "row_security" => Some(self.row_security.as_deref().unwrap_or("on").to_string()),
            "default_tablespace" => Some(self.default_tablespace.as_deref().unwrap_or("").to_string()),
            "default_table_access_method" => Some(
                self.default_table_access_method
                    .as_deref()
                    .unwrap_or("heap")
                    .to_string(),
            ),
            // Transaction isolation level - TiKV uses snapshot isolation which maps to
            // "repeatable read" in PostgreSQL terminology. Support both forms:
            // - "transaction_isolation" (standard PostgreSQL GUC name)
            // - "transaction.isolation.level" (how SHOW transaction isolation level parses)
            "transaction_isolation" | "transaction.isolation.level" => {
                Some("read committed".to_string())
            }
            _ => None,
        }
    }

    pub(crate) fn statement_timeout(&self) -> Option<Duration> {
        if self.statement_timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(self.statement_timeout_ms))
        }
    }
}

pub struct Session {
    store: Arc<TikvStore>,
    observability: Arc<TenantObservability>,
    state: TransactionState,
    savepoints: Arc<SavepointState>,
    last_sequence_values: HashMap<String, i64>,
    settings: SessionSettings,
    #[allow(dead_code)]
    current_user: Option<String>,
    #[allow(dead_code)]
    is_superuser: bool,
    current_database_id: u64,
    current_database_name: Arc<str>,
    /// Connection ID for pg_backend_pid() support
    connection_id: i32,
}

impl Session {
    /// Create a session for the given database.
    pub fn new_with_database(
        store: Arc<TikvStore>,
        observability: Arc<TenantObservability>,
        connection_id: i32,
        database_id: u64,
        database_name: String,
    ) -> Self {
        Self {
            store,
            observability,
            state: TransactionState::Idle,
            savepoints: Arc::new(SavepointState::new()),
            last_sequence_values: HashMap::new(),
            settings: SessionSettings::new(),
            current_user: None,
            is_superuser: false,
            current_database_id: database_id,
            current_database_name: Arc::from(database_name),
            connection_id,
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
    ) -> Self {
        Self {
            store,
            observability,
            state: TransactionState::Idle,
            savepoints: Arc::new(SavepointState::new()),
            last_sequence_values: HashMap::new(),
            settings: SessionSettings::new(),
            current_user: Some(username),
            is_superuser,
            current_database_id: database_id,
            current_database_name: Arc::from(database_name),
            connection_id,
        }
    }

    #[allow(dead_code)]
    pub fn store(&self) -> Arc<TikvStore> {
        self.store.clone()
    }

    #[allow(dead_code)]
    pub fn current_user(&self) -> Option<&str> {
        self.current_user.as_deref()
    }

    #[allow(dead_code)]
    pub fn is_superuser(&self) -> bool {
        self.is_superuser
    }

    #[allow(dead_code)]
    pub fn set_user(&mut self, username: String, is_superuser: bool) {
        self.current_user = Some(username);
        self.is_superuser = is_superuser;
    }

    pub fn connection_id(&self) -> i32 {
        self.connection_id
    }

    pub fn current_database_id(&self) -> u64 {
        self.current_database_id
    }

    #[allow(dead_code)]
    pub fn current_database(&self) -> &str {
        &self.current_database_name
    }

    pub(crate) fn current_database_name_arc(&self) -> Arc<str> {
        self.current_database_name.clone()
    }

    /// Check if currently in a transaction block
    pub fn is_in_transaction(&self) -> bool {
        matches!(self.state, TransactionState::Active(_) | TransactionState::Failed(_))
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
            TransactionState::Active(txn) | TransactionState::Failed(txn) => {
                Some((txn, &mut self.last_sequence_values, self.settings.search_path()))
            }
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn search_path(&self) -> &[String] {
        self.settings.search_path()
    }

    pub fn set_search_path(&mut self, search_path: Vec<String>) {
        self.settings.set_search_path(search_path);
    }

    pub(crate) fn set_known_setting(&mut self, name: &str, value: String) -> Result<bool> {
        self.settings.set_known_setting(name, value)
    }

    pub(crate) fn show_setting_value(&self, name: &str) -> Option<String> {
        self.settings.show_value(name)
    }

    pub(crate) fn statement_timeout(&self) -> Option<Duration> {
        self.settings.statement_timeout()
    }

    pub fn create_savepoint(&mut self, name: String) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(anyhow!("SAVEPOINT can only be used in transaction blocks"));
        }
        if self.is_transaction_failed() {
            return Err(anyhow::Error::new(InFailedSqlTransaction));
        }
        self.savepoints.create(name)
    }

    pub fn release_savepoint(&mut self, name: &str) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(anyhow!(
                "RELEASE SAVEPOINT can only be used in transaction blocks"
            ));
        }
        if self.is_transaction_failed() {
            return Err(anyhow::Error::new(InFailedSqlTransaction));
        }
        self.savepoints.release(name)
    }

    pub async fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        if !self.is_in_transaction() {
            return Err(anyhow!(
                "ROLLBACK TO SAVEPOINT can only be used in transaction blocks"
            ));
        }

        let mut prepared = self.savepoints.prepare_rollback_to(name)?;
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
                let txn = self.store.begin().await?;
                self.savepoints.reset()?;
                self.state = TransactionState::Active(txn);
                Ok(())
            }
            TransactionState::Active(_) | TransactionState::Failed(_) => {
                // Already in transaction, ignore
                Ok(())
            }
        }
    }

    /// Commit a transaction block (COMMIT)
    pub async fn commit(&mut self) -> Result<()> {
        // Move txn out of state to take ownership
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(mut txn) => {
                self.savepoints.reset()?;
                txn.commit().await.map(|_| ()).map_err(|e| anyhow!(e))?;
                self.observability.record_commit();
                Ok(())
            }
            TransactionState::Failed(mut txn) => {
                self.savepoints.reset()?;
                txn.rollback().await.map_err(|e| anyhow!(e))
            }
            TransactionState::Idle => {
                Ok(()) // No-op
            }
        }
    }

    /// Rollback a transaction block (ROLLBACK)
    pub async fn rollback(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, TransactionState::Idle) {
            TransactionState::Active(mut txn) => {
                self.savepoints.reset()?;
                txn.rollback().await.map_err(|e| anyhow!(e))
            }
            TransactionState::Failed(mut txn) => {
                self.savepoints.reset()?;
                txn.rollback().await.map_err(|e| anyhow!(e))
            }
            TransactionState::Idle => {
                Ok(()) // No-op
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InFailedSqlTransaction, SessionSettings};

    #[test]
    fn test_session_settings_defaults_and_overrides() {
        let mut settings = SessionSettings::new();

        assert_eq!(settings.show_value("server_version").as_deref(), Some("16.0"));
        assert_eq!(
            settings.show_value("server_version_num").as_deref(),
            Some("160000")
        );
        assert_eq!(settings.show_value("timezone").as_deref(), Some("UTC"));
        assert_eq!(settings.show_value("application_name").as_deref(), Some(""));
        assert_eq!(settings.show_value("search_path").as_deref(), Some("public"));
        assert_eq!(settings.show_value("statement_timeout").as_deref(), Some("0"));
        assert_eq!(settings.show_value("client_encoding").as_deref(), Some("UTF8"));

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
            .set_known_setting("application_name", "pg-tikv-tests".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("application_name").as_deref(),
            Some("pg-tikv-tests")
        );

        assert!(!settings
            .set_known_setting("unknown_setting", "x".to_string())
            .unwrap());
        assert_eq!(settings.show_value("unknown_setting"), None);

        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("read committed")
        );
        assert_eq!(
            settings.show_value("transaction.isolation.level").as_deref(),
            Some("read committed")
        );
    }

    #[test]
    fn test_session_settings_timeout_parsing() {
        let mut settings = SessionSettings::new();

        settings
            .set_known_setting("statement_timeout", "20".to_string())
            .unwrap();
        assert_eq!(settings.show_value("statement_timeout").as_deref(), Some("20"));

        settings
            .set_known_setting("statement_timeout", "1s".to_string())
            .unwrap();
        assert_eq!(settings.show_value("statement_timeout").as_deref(), Some("1000"));

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
    fn test_in_failed_sql_transaction_message() {
        assert_eq!(
            InFailedSqlTransaction.to_string(),
            "current transaction is aborted, commands ignored until end of transaction block"
        );
    }
}
