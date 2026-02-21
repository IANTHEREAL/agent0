//! Session-level GUC settings (`SessionSettings`).
//!
//! A small, per-connection container for session-level settings (GUCs).
//! This is intentionally compact and avoids heap allocations unless a setting is
//! explicitly changed by the client (`SET` / `set_config`).

use crate::sql::error::SqlError;
use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;

use super::DEFAULT_MAX_SORT_BYTES;

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
    pub(crate) statement_timeout_ms: u64,
    pub(crate) default_statement_timeout_ms: u64,
    lock_timeout_ms: u64,
    pub(crate) idle_in_transaction_session_timeout_ms: u64,
    pub(crate) default_idle_in_transaction_session_timeout_ms: u64,
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
            _ => self
                .extra_settings
                .get(name)
                .cloned()
                .or_else(|| Self::default_value(name).map(String::from)),
        }
    }

    /// PostgreSQL-compatible defaults for common GUC parameters that tipg does
    /// not actively track but drivers/ORMs expect to SHOW without error.
    fn default_value(name: &str) -> Option<&'static str> {
        match name {
            "extra_float_digits" => Some("1"),
            "bytea_output" => Some("hex"),
            "lc_messages" => Some("C"),
            "lc_monetary" => Some("C"),
            "lc_numeric" => Some("C"),
            "lc_time" => Some("C"),
            "max_identifier_length" => Some("63"),
            "max_index_keys" => Some("32"),
            "work_mem" => Some("4MB"),
            "default_text_search_config" => {
                Some(crate::sql::fts_tokenizers::default_text_search_config())
            }
            "in_hot_standby" => Some("off"),
            "password_encryption" => Some("scram-sha-256"),
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
