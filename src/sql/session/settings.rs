//! Session-level GUC settings (`SessionSettings`).
//!
//! A small, per-connection container for session-level settings (GUCs).
//! This is intentionally compact and avoids heap allocations unless a setting is
//! explicitly changed by the client (`SET` / `set_config`).

mod defs;
mod show;
mod validate;

use crate::sql::error::SqlError;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use super::{DEFAULT_DML_TABLE_SCAN_MAX_ROWS, DEFAULT_HASH_JOIN_WORK_MEM, DEFAULT_MAX_SORT_BYTES};
use validate::validate_session_replication_role;
const DML_TABLE_SCAN_MAX_ROWS_UPPER_BOUND: usize = i64::MAX as usize;

pub(crate) use defs::{find_guc_def, GUC_TABLE};

// ── GUC type classification ──────────────────────────────────────────────

/// Semantic type of a GUC parameter. Used for type-level validation before
/// any custom `validate_fn` is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Real: reserved for future GUCs
pub(crate) enum GucType {
    Bool,
    Int,
    Real,
    String,
    Enum,
    Timeout,
    ByteSize,
}

/// Determines who may SET a GUC and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Suset: used in P4 for session_replication_role
pub(crate) enum GucContext {
    /// Immutable server properties (server_version, server_encoding, etc.).
    /// Cannot be changed at runtime.
    Internal,
    /// Superuser-only at runtime.
    Suset,
    /// Any user may SET.
    Userset,
}

// ── GUC flags ────────────────────────────────────────────────────────────

/// GUC is accepted for driver/pg_dump compatibility but has no runtime effect.
#[allow(dead_code)] // used in P2+ for HOLLOW dispatch
pub(crate) const GUC_HOLLOW: u32 = 1 << 0;
/// GUC value changes should be reported to the client via ParameterStatus.
#[allow(dead_code)] // used in P2+ for REPORT dispatch
pub(crate) const GUC_REPORT: u32 = 1 << 1;
/// Boot default is computed at runtime (not a compile-time constant).
#[allow(dead_code)] // used in P2+ for runtime default handling
pub(crate) const GUC_RUNTIME_DEFAULT: u32 = 1 << 2;
/// Excluded from RESET ALL.
#[allow(dead_code)]
pub(crate) const GUC_NO_RESET_ALL: u32 = 1 << 3;

// ── GucDef ───────────────────────────────────────────────────────────────

/// Declarative definition of a single GUC parameter.
#[allow(dead_code)] // flags: used in P2+ for HOLLOW/REPORT dispatch
pub(crate) struct GucDef {
    pub(crate) name: &'static str,
    pub(crate) guc_type: GucType,
    pub(crate) context: GucContext,
    pub(crate) description: &'static str,
    pub(crate) boot_default: &'static str,
    pub(crate) flags: u32,
    /// Custom validator. Called AFTER type-level validation.
    /// If `None`, type-level validation is sufficient.
    pub(crate) validate_fn: Option<fn(&str) -> Result<String>>,
}

// Validator and GUC registry tables are split into focused submodules.

#[derive(Clone, Debug, Default)]
struct SettingsSavepoint {
    name: String,
    overrides: HashMap<String, String>,
    search_path: Option<Vec<String>>,
}

/// A small, per-connection container for session-level settings (GUCs).
///
/// This is intentionally compact and avoids heap allocations unless a setting is
/// explicitly changed by the client (`SET` / `set_config`).
#[derive(Debug, Default)]
pub(crate) struct SessionSettings {
    search_path: Vec<String>,

    /// Maximum bytes for hash join build side.
    /// Default: 256 MB. 0 = unlimited.
    hash_join_work_mem: usize,

    /// Maximum bytes allowed for in-memory sort (ORDER BY).
    /// Default: 256 MB. 0 = unlimited.
    max_sort_bytes: usize,

    /// Maximum rows per auxiliary source and combined cross-product cap for
    /// UPDATE FROM / DELETE USING.
    /// Also used to clamp dynamic LIMIT expressions at execution time.
    /// Default: 10000. 0 = unlimited.
    dml_table_scan_max_rows: usize,

    /// Plan cache capacity (max cached plans per session). Default: 128.
    prepared_plan_cache_size: usize,
    /// Plan cache promotion threshold (executions before caching). Default: 5.
    prepared_plan_cache_min_exec: u64,

    /// HNSW ef_search beam width during search. Default: 40, range: 1-1000.
    hnsw_ef_search: u16,

    /// Maximum retry attempts for autocommit DML/DDL on write conflict. Default: 64.
    pub(crate) retry_max_attempts: u64,
    /// Maximum wall-time for retries per statement, in milliseconds. Default: 0 (disabled).
    pub(crate) retry_timeout_ms: u64,

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

    /// Generic storage for GUC parameters that db9 does not actively use but
    /// drivers expect to SET/SHOW without error (e.g. `extra_float_digits`,
    /// `DateStyle`, `work_mem`). Values are stored as-is for `SHOW` readback.
    extra_settings: HashMap<String, String>,

    /// Server-authored settings in reserved namespaces (`request.jwt.*`,
    /// `auth.*`). These are never writable by client `SET` paths.
    server_reserved_settings: HashMap<String, String>,

    /// Transaction-local overrides populated by `SET LOCAL`.
    local_overrides: HashMap<String, String>,
    /// Parsed search_path override for local scope.
    local_search_path: Option<Vec<String>>,
    /// Savepoint snapshots for transaction-local overrides.
    settings_savepoint_stack: Vec<SettingsSavepoint>,
}

pub(crate) fn public_setting_value(canonical: &str, value: String) -> String {
    if canonical.eq_ignore_ascii_case("embedding.api_key") && !value.is_empty() {
        "****".to_string()
    } else {
        value
    }
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

    pub(crate) fn format_search_path_show(search_path: &[String]) -> String {
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

    pub(crate) fn canonical_setting_name(name: &str) -> &str {
        match name {
            "transaction.isolation.level" => "transaction_isolation",
            "db9.hash_join_work_mem" => "db9.hash_join_work_mem",
            "db9.max_sort_bytes" => "db9.max_sort_bytes",
            _ => name,
        }
    }

    fn is_immutable_setting(name: &str) -> bool {
        GUC_TABLE
            .iter()
            .any(|g| g.name == name && g.context == GucContext::Internal)
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
            hash_join_work_mem: DEFAULT_HASH_JOIN_WORK_MEM,
            max_sort_bytes: DEFAULT_MAX_SORT_BYTES,
            dml_table_scan_max_rows: DEFAULT_DML_TABLE_SCAN_MAX_ROWS,
            prepared_plan_cache_size: 128,
            prepared_plan_cache_min_exec: 5,
            hnsw_ef_search: 40,
            retry_max_attempts: 64,
            retry_timeout_ms: 0,
            statement_timeout_ms: default_statement_timeout_ms,
            default_statement_timeout_ms,
            idle_in_transaction_session_timeout_ms: default_idle_in_txn_timeout_ms,
            default_idle_in_transaction_session_timeout_ms: default_idle_in_txn_timeout_ms,
            ..Default::default()
        }
    }

    pub(crate) fn search_path(&self) -> &[String] {
        self.local_search_path
            .as_deref()
            .unwrap_or(&self.search_path)
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

        let multiplier: f64 = if unit.is_empty() || unit.eq_ignore_ascii_case("ms") {
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

    pub(crate) fn validate_and_normalize_value(name: &str, value: &str) -> Result<String> {
        let canonical = Self::canonical_setting_name(name);

        // session_replication_role is not in GUC_TABLE (not a known GUC) but
        // must be explicitly rejected rather than accepted through the generic
        // unknown-GUC path (#1535).
        if canonical == "session_replication_role" {
            return validate_session_replication_role(value);
        }

        // Look up the GUC definition.
        if let Some(def) = find_guc_def(canonical) {
            // 0. Context check — Internal GUCs cannot be SET.
            if def.context == GucContext::Internal {
                return Err(SqlError::CantChangeRuntimeParam {
                    message: format!("parameter \"{}\" cannot be changed", canonical),
                }
                .into());
            }

            // 1. Type-level validation.
            let type_validated = match def.guc_type {
                GucType::Timeout => {
                    let ms = Self::parse_timeout_millis(value)?;
                    Self::format_timeout_show(ms)
                }
                GucType::ByteSize => {
                    let bytes = Self::parse_byte_size(value)?;
                    bytes.to_string()
                }
                GucType::Bool => {
                    let normalized = value.trim().to_lowercase();
                    match normalized.as_str() {
                        "on" | "true" | "yes" | "1" => "on".to_string(),
                        "off" | "false" | "no" | "0" => "off".to_string(),
                        _ => {
                            return Err(SqlError::InvalidParameterValue {
                                message: format!(
                                    "parameter \"{}\" requires a Boolean value",
                                    canonical
                                ),
                            }
                            .into())
                        }
                    }
                }
                GucType::Int => {
                    let trimmed = value.trim();
                    trimmed
                        .parse::<i64>()
                        .map_err(|_| SqlError::InvalidParameterValue {
                            message: format!(
                                "invalid value for parameter \"{}\": \"{}\"",
                                canonical, value
                            ),
                        })?;
                    trimmed.to_string()
                }
                GucType::Real => {
                    let trimmed = value.trim();
                    trimmed
                        .parse::<f64>()
                        .map_err(|_| SqlError::InvalidParameterValue {
                            message: format!(
                                "invalid value for parameter \"{}\": \"{}\"",
                                canonical, value
                            ),
                        })?;
                    trimmed.to_string()
                }
                GucType::Enum => {
                    // Enum validation is handled by validate_fn; pass through here.
                    value.to_string()
                }
                GucType::String => value.to_string(),
            };

            // 2. Custom validation (via function pointer).
            if let Some(vfn) = def.validate_fn {
                vfn(&type_validated)
            } else {
                Ok(type_validated)
            }
        } else {
            // Unknown GUC: accept as-is.
            Ok(value.to_string())
        }
    }

    /// Set a session setting. Known settings (timeout, encoding, etc.) are validated
    /// and stored in typed fields. Unknown GUCs are stored in a generic map for
    /// `SHOW` readback — this allows drivers that SET parameters like `extra_float_digits`
    /// or `DateStyle` to work without error.
    pub(crate) fn set_known_setting(&mut self, name: &str, value: String) -> Result<bool> {
        let canonical = Self::canonical_setting_name(name);
        if crate::sql::executor::is_server_reserved_guc(canonical) {
            return Err(SqlError::InsufficientPrivilege {
                message: format!(
                    "parameter \"{}\" is reserved for server-side use only",
                    name
                ),
            }
            .into());
        }
        let normalized = Self::validate_and_normalize_value(canonical, &value)?;

        match canonical {
            "statement_timeout" => {
                self.statement_timeout_ms = Self::parse_timeout_millis(&normalized)?;
            }
            "lock_timeout" => self.lock_timeout_ms = Self::parse_timeout_millis(&normalized)?,
            "idle_in_transaction_session_timeout" => {
                self.idle_in_transaction_session_timeout_ms =
                    Self::parse_timeout_millis(&normalized)?
            }
            "db9.dml_table_scan_max_rows" => {
                self.dml_table_scan_max_rows = normalized
                    .parse()
                    .unwrap_or(DEFAULT_DML_TABLE_SCAN_MAX_ROWS);
            }
            "db9.hash_join_work_mem" => {
                self.hash_join_work_mem = Self::parse_byte_size(&normalized)?;
            }
            "db9.max_sort_bytes" => {
                self.max_sort_bytes = Self::parse_byte_size(&normalized)?;
            }
            "db9.prepared_plan_cache_size" => {
                self.prepared_plan_cache_size = normalized.parse().unwrap_or(128);
            }
            "db9.prepared_plan_cache_min_exec" => {
                self.prepared_plan_cache_min_exec = normalized.parse().unwrap_or(5);
            }
            "db9.retry_max_attempts" => {
                self.retry_max_attempts = normalized.parse().unwrap_or(64);
            }
            "db9.retry_timeout" => {
                self.retry_timeout_ms = Self::parse_timeout_millis(&normalized)?;
            }
            "hnsw.ef_search" => {
                let v: u16 = normalized
                    .parse()
                    .map_err(|_| SqlError::InvalidParameterValue {
                        message: format!(
                            "invalid value for parameter \"{}\": \"{}\"",
                            canonical, normalized
                        ),
                    })?;
                if !(1..=1000).contains(&v) {
                    return Err(SqlError::InvalidParameterValue {
                        message: format!("hnsw.ef_search must be between 1 and 1000, got {}", v),
                    }
                    .into());
                }
                self.hnsw_ef_search = v;
            }
            "db9.use_optimizer" => {}
            "timezone" => self.timezone = Some(normalized.clone()),
            "application_name" => self.application_name = Some(normalized.clone()),
            "client_encoding" => self.client_encoding = Some(normalized.clone()),
            "standard_conforming_strings" => {
                self.standard_conforming_strings = Some(normalized.clone())
            }
            "check_function_bodies" => self.check_function_bodies = Some(normalized.clone()),
            "xmloption" => self.xmloption = Some(normalized.clone()),
            "client_min_messages" => self.client_min_messages = Some(normalized.clone()),
            "row_security" => self.row_security = Some(normalized.clone()),
            "default_tablespace" => self.default_tablespace = Some(normalized.clone()),
            "default_table_access_method" => {
                self.default_table_access_method = Some(normalized.clone())
            }
            "transaction_isolation" => self.transaction_isolation = Some(normalized.clone()),
            "default_transaction_read_only" => {
                self.default_transaction_read_only = Some(normalized.clone())
            }
            _ => {
                self.extra_settings
                    .insert(canonical.to_string(), normalized.clone());
            }
        }
        self.remove_local_override(canonical);
        Ok(true)
    }

    /// Set a server-authored reserved GUC entry. This bypasses the client
    /// write guards and is used only by trusted auth/session plumbing.
    pub(crate) fn set_server_reserved_setting(
        &mut self,
        name: &str,
        value: String,
    ) -> Result<bool> {
        let lowered = name.to_ascii_lowercase();
        let canonical = Self::canonical_setting_name(&lowered);
        if !crate::sql::executor::is_server_reserved_guc(canonical) {
            return Err(SqlError::InvalidParameterValue {
                message: format!(
                    "parameter \"{}\" is not in a server-reserved namespace",
                    name
                ),
            }
            .into());
        }

        let normalized = Self::validate_and_normalize_value(canonical, &value)?;
        self.extra_settings.remove(canonical);
        self.server_reserved_settings
            .insert(canonical.to_string(), normalized);
        self.remove_local_override(canonical);
        Ok(true)
    }

    pub(crate) fn set_local_override(&mut self, name: &str, value: String) -> Result<bool> {
        let canonical = Self::canonical_setting_name(name);
        if crate::sql::executor::is_server_reserved_guc(canonical) {
            return Err(SqlError::InsufficientPrivilege {
                message: format!(
                    "parameter \"{}\" is reserved for server-side use only",
                    name
                ),
            }
            .into());
        }
        let normalized = Self::validate_and_normalize_value(canonical, &value)?;
        self.local_overrides
            .insert(canonical.to_string(), normalized);
        Ok(true)
    }

    pub(crate) fn set_local_search_path(&mut self, search_path: Vec<String>) {
        self.local_overrides.insert(
            "search_path".to_string(),
            Self::format_search_path_show(&search_path),
        );
        self.local_search_path = Some(search_path);
    }

    pub(crate) fn clear_local_overrides(&mut self) {
        self.local_overrides.clear();
        self.local_search_path = None;
        self.settings_savepoint_stack.clear();
        // transaction_isolation is transaction-scoped (set by BEGIN ISOLATION LEVEL).
        // Revert to None so SHOW falls back to the default "repeatable read".
        self.transaction_isolation = None;
    }

    pub(crate) fn remove_local_override(&mut self, name: &str) {
        let canonical = Self::canonical_setting_name(name);
        self.local_overrides.remove(canonical);
        if canonical == "search_path" {
            self.local_search_path = None;
        }
    }

    pub(crate) fn push_settings_savepoint(&mut self, name: String) {
        self.settings_savepoint_stack.push(SettingsSavepoint {
            name,
            overrides: self.local_overrides.clone(),
            search_path: self.local_search_path.clone(),
        });
    }

    pub(crate) fn rollback_settings_to_savepoint(&mut self, name: &str) {
        let Some(target_idx) = self
            .settings_savepoint_stack
            .iter()
            .rposition(|sp| sp.name == name)
        else {
            return;
        };

        let snapshot = self.settings_savepoint_stack[target_idx].clone();
        self.local_overrides = snapshot.overrides;
        self.local_search_path = snapshot.search_path;
        self.settings_savepoint_stack.truncate(target_idx + 1);
    }

    pub(crate) fn release_settings_savepoint(&mut self, name: &str) {
        let Some(target_idx) = self
            .settings_savepoint_stack
            .iter()
            .rposition(|sp| sp.name == name)
        else {
            return;
        };

        self.settings_savepoint_stack.truncate(target_idx);
    }

    /// Reset a single session setting to its default value.
    pub(crate) fn reset_setting(&mut self, name: &str) {
        let canonical = Self::canonical_setting_name(name);
        if crate::sql::executor::is_server_reserved_guc(canonical) {
            self.remove_local_override(canonical);
            return;
        }
        match canonical {
            "search_path" => self.search_path = Self::default_search_path(),
            "statement_timeout" => self.statement_timeout_ms = self.default_statement_timeout_ms,
            "lock_timeout" => self.lock_timeout_ms = 0,
            "idle_in_transaction_session_timeout" => {
                self.idle_in_transaction_session_timeout_ms =
                    self.default_idle_in_transaction_session_timeout_ms
            }
            "db9.dml_table_scan_max_rows" => {
                self.dml_table_scan_max_rows = DEFAULT_DML_TABLE_SCAN_MAX_ROWS
            }
            "db9.hash_join_work_mem" => self.hash_join_work_mem = DEFAULT_HASH_JOIN_WORK_MEM,
            "db9.max_sort_bytes" => self.max_sort_bytes = DEFAULT_MAX_SORT_BYTES,
            "db9.prepared_plan_cache_size" => self.prepared_plan_cache_size = 128,
            "db9.prepared_plan_cache_min_exec" => self.prepared_plan_cache_min_exec = 5,
            "hnsw.ef_search" => self.hnsw_ef_search = 40,
            "db9.retry_max_attempts" => self.retry_max_attempts = 64,
            "db9.retry_timeout" => self.retry_timeout_ms = 0,
            "db9.use_optimizer" => {}
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
                self.extra_settings.remove(canonical);
            }
        }
        self.remove_local_override(canonical);
    }

    /// Reset all session settings to their defaults.
    ///
    /// Server-reserved GUC entries (`request.jwt.*`, `auth.*`) are preserved
    /// across RESET ALL so that a client cannot indirectly clear auth-pipeline
    /// values set after JWT verification.
    pub(crate) fn reset_all_settings(&mut self) {
        let reserved = self.server_reserved_settings.clone();

        let savepoint_stack = std::mem::take(&mut self.settings_savepoint_stack);
        *self = Self::new_with_defaults(
            self.default_statement_timeout_ms,
            self.default_idle_in_transaction_session_timeout_ms,
        );
        self.settings_savepoint_stack = savepoint_stack;

        // Restore server-reserved entries.
        if !reserved.is_empty() {
            self.server_reserved_settings.extend(reserved);
        }
    }
}
