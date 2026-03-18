//! Session-level GUC settings (`SessionSettings`).
//!
//! A small, per-connection container for session-level settings (GUCs).
//! This is intentionally compact and avoids heap allocations unless a setting is
//! explicitly changed by the client (`SET` / `set_config`).

use crate::sql::error::SqlError;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use super::{DEFAULT_DML_TABLE_SCAN_MAX_ROWS, DEFAULT_MAX_SORT_BYTES};
const DML_TABLE_SCAN_MAX_ROWS_UPPER_BOUND: usize = i64::MAX as usize;

/// Metadata for a single known GUC parameter.
pub(crate) struct GucMeta {
    pub(crate) name: &'static str,
    immutable: bool,
    description: &'static str,
    /// Static default for GUCs handled in the default_value fallback path.
    /// `None` for GUCs whose value is computed from typed struct fields in show_value().
    static_default: Option<&'static str>,
}

/// Single source of truth for all known GUC parameters. MUST be sorted alphabetically by name.
pub(crate) const KNOWN_GUCS: &[GucMeta] = &[
    GucMeta {
        name: "application_name",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "bytea_output",
        immutable: false,
        description: "",
        static_default: Some("hex"),
    },
    GucMeta {
        name: "check_function_bodies",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "client_encoding",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "client_min_messages",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "datestyle",
        immutable: true,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "db9.dml_table_scan_max_rows",
        immutable: false,
        description:
            "Maximum rows per auxiliary source and combined cross-product cap for UPDATE FROM / DELETE USING (0 = unlimited)",
        static_default: None,
    },
    GucMeta {
        name: "db9.max_sort_bytes",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "db9.prepared_plan_cache_min_exec",
        immutable: false,
        description: "",
        static_default: Some("5"),
    },
    GucMeta {
        name: "db9.prepared_plan_cache_size",
        immutable: false,
        description: "",
        static_default: Some("128"),
    },
    GucMeta {
        name: "db9.retry_max_attempts",
        immutable: false,
        description: "Maximum retry attempts for autocommit DML/DDL on write conflict",
        static_default: None,
    },
    GucMeta {
        name: "db9.retry_timeout",
        immutable: false,
        description: "Maximum wall-time for retries per statement (0 = no limit)",
        static_default: None,
    },
    GucMeta {
        name: "db9.use_optimizer",
        immutable: true,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "default_table_access_method",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "default_tablespace",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "default_text_search_config",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "default_transaction_deferrable",
        immutable: true,
        description: "DEFERRABLE transactions require SERIALIZABLE isolation, which is not supported",
        static_default: Some("off"),
    },
    GucMeta {
        name: "default_transaction_isolation",
        immutable: true,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "default_transaction_read_only",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "embedding.api_key",
        immutable: false,
        description: "Embedding service API key (session override)",
        static_default: None,
    },
    GucMeta {
        name: "embedding.concurrency",
        immutable: false,
        description: "",
        static_default: Some("5"),
    },
    GucMeta {
        name: "embedding.dimensions",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "embedding.endpoint",
        immutable: false,
        description: "Embedding service endpoint URL (session override)",
        static_default: None,
    },
    GucMeta {
        name: "embedding.max_calls",
        immutable: false,
        description: "",
        static_default: Some("100"),
    },
    GucMeta {
        name: "embedding.model",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "embedding.provider",
        immutable: false,
        description: "Embedding provider: openai or bedrock (session override)",
        static_default: None,
    },
    GucMeta {
        name: "extra_float_digits",
        immutable: false,
        description: "",
        static_default: Some("1"),
    },
    GucMeta {
        name: "hnsw.ef_search",
        immutable: false,
        description: "Sets the size of the dynamic candidate list for HNSW index search.",
        static_default: Some("40"),
    },
    GucMeta {
        name: "idle_in_transaction_session_timeout",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "in_hot_standby",
        immutable: false,
        description: "",
        static_default: Some("off"),
    },
    GucMeta {
        name: "integer_datetimes",
        immutable: true,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "intervalstyle",
        immutable: true,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "lc_messages",
        immutable: false,
        description: "",
        static_default: Some("C"),
    },
    GucMeta {
        name: "lc_monetary",
        immutable: false,
        description: "",
        static_default: Some("C"),
    },
    GucMeta {
        name: "lc_numeric",
        immutable: false,
        description: "",
        static_default: Some("C"),
    },
    GucMeta {
        name: "lc_time",
        immutable: false,
        description: "",
        static_default: Some("C"),
    },
    GucMeta {
        name: "lock_timeout",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "max_identifier_length",
        immutable: false,
        description: "",
        static_default: Some("63"),
    },
    GucMeta {
        name: "max_index_keys",
        immutable: false,
        description: "",
        static_default: Some("32"),
    },
    GucMeta {
        name: "password_encryption",
        immutable: false,
        description: "",
        static_default: Some("scram-sha-256"),
    },
    GucMeta {
        name: "row_security",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "search_path",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "server_encoding",
        immutable: true,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "server_version",
        immutable: true,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "server_version_num",
        immutable: true,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "standard_conforming_strings",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "statement_timeout",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "timezone",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "transaction_deferrable",
        immutable: true,
        description: "DEFERRABLE transactions require SERIALIZABLE isolation, which is not supported",
        static_default: Some("off"),
    },
    GucMeta {
        name: "transaction_isolation",
        immutable: false,
        description: "",
        static_default: None,
    },
    GucMeta {
        name: "work_mem",
        immutable: false,
        description: "",
        static_default: Some("4MB"),
    },
    GucMeta {
        name: "xmloption",
        immutable: false,
        description: "",
        static_default: None,
    },
];

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
            "db9.max_sort_bytes" => "db9.max_sort_bytes",
            _ => name,
        }
    }

    fn is_immutable_setting(name: &str) -> bool {
        KNOWN_GUCS.iter().any(|g| g.name == name && g.immutable)
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
        match Self::canonical_setting_name(name) {
            "statement_timeout" | "lock_timeout" | "idle_in_transaction_session_timeout" => {
                let ms = Self::parse_timeout_millis(value)?;
                Ok(Self::format_timeout_show(ms))
            }
            "db9.dml_table_scan_max_rows" => {
                let v: u128 =
                    value
                        .trim()
                        .parse()
                        .map_err(|_| SqlError::InvalidParameterValue {
                            message: format!(
                                "invalid value for parameter \"{}\": \"{}\"",
                                name, value
                            ),
                        })?;
                if v > DML_TABLE_SCAN_MAX_ROWS_UPPER_BOUND as u128 {
                    return Err(SqlError::InvalidParameterValue {
                        message: format!(
                            "invalid value for parameter \"{}\": \"{}\" (must be between 0 and {})",
                            name, value, DML_TABLE_SCAN_MAX_ROWS_UPPER_BOUND
                        ),
                    }
                    .into());
                }
                Ok(v.to_string())
            }
            "db9.max_sort_bytes" => {
                let bytes = Self::parse_byte_size(value)?;
                Ok(bytes.to_string())
            }
            "db9.prepared_plan_cache_size" | "db9.prepared_plan_cache_min_exec" => {
                let v: u64 = value
                    .trim()
                    .parse()
                    .map_err(|_| SqlError::InvalidParameterValue {
                        message: format!("invalid value for parameter \"{}\": \"{}\"", name, value),
                    })?;
                Ok(v.to_string())
            }
            "db9.retry_max_attempts" => {
                let v: u64 = value
                    .trim()
                    .parse()
                    .map_err(|_| SqlError::InvalidParameterValue {
                        message: format!("invalid value for parameter \"{}\": \"{}\"", name, value),
                    })?;
                if v == 0 {
                    return Err(SqlError::InvalidParameterValue {
                        message: format!(
                            "invalid value for parameter \"{}\": \"{}\" must be at least 1",
                            name, value
                        ),
                    }
                    .into());
                }
                Ok(v.to_string())
            }
            "db9.retry_timeout" => {
                let ms = Self::parse_timeout_millis(value)?;
                Ok(Self::format_timeout_show(ms))
            }
            "embedding.dimensions" => {
                let v: u32 = value
                    .trim()
                    .parse()
                    .map_err(|_| SqlError::InvalidParameterValue {
                        message: format!("invalid value for parameter \"{}\": \"{}\"", name, value),
                    })?;
                if v == 0 {
                    return Err(SqlError::InvalidParameterValue {
                        message: format!(
                            "invalid value for parameter \"{}\": \"{}\" must be at least 1",
                            name, value
                        ),
                    }
                    .into());
                }
                Ok(v.to_string())
            }
            "embedding.model" => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(SqlError::InvalidParameterValue {
                        message: "embedding.model must not be empty".into(),
                    }
                    .into());
                }
                Ok(trimmed.to_string())
            }
            "embedding.provider" => {
                let normalized = value.trim().to_lowercase();
                match normalized.as_str() {
                    "openai" | "openai_compatible" | "openai-compatible" => {
                        Ok("openai".to_string())
                    }
                    "bedrock" | "aws_bedrock" | "aws-bedrock" => Ok("bedrock".to_string()),
                    _ => Err(SqlError::InvalidParameterValue {
                        message: format!(
                            "invalid value for parameter \"embedding.provider\": \"{}\"; \
                             expected 'openai' or 'bedrock'",
                            value
                        ),
                    }
                    .into()),
                }
            }
            "embedding.endpoint" => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(SqlError::InvalidParameterValue {
                        message: "embedding.endpoint must not be empty".into(),
                    }
                    .into());
                }
                Ok(trimmed.to_string())
            }
            "embedding.api_key" => {
                // Accept any non-empty string. Value is stored as-is but
                // SHOW will return '****' for security.
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(SqlError::InvalidParameterValue {
                        message: "embedding.api_key must not be empty".into(),
                    }
                    .into());
                }
                Ok(trimmed.to_string())
            }
            "embedding.max_calls" | "embedding.concurrency" => {
                let v: u32 = value
                    .trim()
                    .parse()
                    .map_err(|_| SqlError::InvalidParameterValue {
                        message: format!("invalid value for parameter \"{}\": \"{}\"", name, value),
                    })?;
                if v == 0 {
                    return Err(SqlError::InvalidParameterValue {
                        message: format!(
                            "invalid value for parameter \"{}\": \"{}\" must be at least 1",
                            name, value
                        ),
                    }
                    .into());
                }
                Ok(v.to_string())
            }
            "bytea_output" => {
                let normalized = value.trim().to_lowercase();
                match normalized.as_str() {
                    "hex" | "escape" => Ok(normalized),
                    _ => Err(SqlError::InvalidParameterValue {
                        message: format!(
                            "invalid value for parameter \"bytea_output\": \"{}\"; \
                             available values: \"hex\", \"escape\"",
                            value
                        ),
                    }
                    .into()),
                }
            }
            "hnsw.ef_search" => {
                let v: u16 = value
                    .trim()
                    .parse()
                    .map_err(|_| SqlError::InvalidParameterValue {
                        message: format!("invalid value for parameter \"{}\": \"{}\"", name, value),
                    })?;
                if !(1..=1000).contains(&v) {
                    return Err(SqlError::InvalidParameterValue {
                        message: format!("hnsw.ef_search must be between 1 and 1000, got {}", v),
                    }
                    .into());
                }
                Ok(v.to_string())
            }
            "db9.use_optimizer" => {
                let normalized = value.trim().to_lowercase();
                match normalized.as_str() {
                    "on" | "true" | "yes" | "1" => Ok(value.to_string()),
                    "off" | "false" | "no" | "0" => {
                        tracing::info!(
                            "NOTICE: optimizer cannot be disabled; \
                             db9.use_optimizer setting ignored"
                        );
                        Ok(value.to_string())
                    }
                    _ => Err(SqlError::InvalidParameterValue {
                        message: "parameter \"db9.use_optimizer\" requires a Boolean value".into(),
                    }
                    .into()),
                }
            }
            "timezone" => {
                crate::model::timestamp::TimeZoneSpec::try_parse(value)?;
                Ok(value.to_string())
            }
            "client_encoding" => {
                let enc = value.trim();
                if enc.eq_ignore_ascii_case("utf8") || enc.eq_ignore_ascii_case("utf-8") {
                    Ok("UTF8".to_string())
                } else {
                    Err(SqlError::Unsupported(format!(
                        "unsupported client_encoding '{}'; only UTF8 is supported",
                        value
                    ))
                    .into())
                }
            }
            "transaction_deferrable" | "default_transaction_deferrable" => {
                Err(SqlError::CantChangeRuntimeParam {
                    message: format!(
                        "parameter \"{}\" cannot be changed \
                         (DEFERRABLE transactions require SERIALIZABLE isolation, \
                         which is not supported)",
                        name
                    ),
                }
                .into())
            }
            "transaction_isolation" => {
                let normalized = value.trim().to_lowercase();
                match normalized.as_str() {
                    "read uncommitted" | "read committed" => {
                        tracing::warn!(
                            requested = normalized.as_str(),
                            actual = "repeatable read",
                            "TiKV provides snapshot isolation (REPEATABLE READ); \
                             the requested isolation level has been upgraded"
                        );
                        Ok("repeatable read".to_string())
                    }
                    "repeatable read" => Ok("repeatable read".to_string()),
                    "serializable" => {
                        tracing::warn!(
                            requested = "serializable",
                            actual = "repeatable read",
                            "TiKV cannot provide PostgreSQL SERIALIZABLE semantics; \
                             the requested isolation level has been downgraded"
                        );
                        Ok("repeatable read".to_string())
                    }
                    _ => Err(SqlError::InvalidParameterValue {
                        message: format!(
                            "invalid value for parameter \"transaction_isolation\": \"{}\"",
                            value
                        ),
                    }
                    .into()),
                }
            }
            "default_transaction_read_only" => {
                let normalized = value.trim().to_lowercase();
                match normalized.as_str() {
                    "on" | "true" | "yes" | "1" => Ok("on".to_string()),
                    "off" | "false" | "no" | "0" => Ok("off".to_string()),
                    _ => Err(SqlError::InvalidParameterValue {
                        message:
                            "parameter \"default_transaction_read_only\" requires a Boolean value"
                                .into(),
                    }
                    .into()),
                }
            }
            // session_replication_role has real trigger-suppression semantics in
            // PostgreSQL that db9 does not implement.  Reject explicitly instead
            // of silently storing it through the generic GUC path (#1535).
            "session_replication_role" => Err(SqlError::Unsupported(
                "session_replication_role is not supported; \
                 triggers always fire as in \"origin\" mode"
                    .to_string(),
            )
            .into()),
            _ => Ok(value.to_string()),
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

    /// Return the boot-default display value for a GUC (what SHOW returns after RESET).
    ///
    /// Static variant that returns the hardcoded PG boot default.
    /// For tenant-configurable timeouts, prefer `reset_default_show_value()`
    /// which uses the session's configured defaults.
    pub(crate) fn boot_default_show_value(name: &str) -> String {
        let canonical = Self::canonical_setting_name(name);
        match canonical {
            "search_path" => Self::format_search_path_show(&Self::default_search_path()),
            "statement_timeout"
            | "lock_timeout"
            | "idle_in_transaction_session_timeout"
            | "db9.retry_timeout" => Self::format_timeout_show(0),
            "db9.dml_table_scan_max_rows" => DEFAULT_DML_TABLE_SCAN_MAX_ROWS.to_string(),
            "db9.max_sort_bytes" => DEFAULT_MAX_SORT_BYTES.to_string(),
            "db9.prepared_plan_cache_size" => "128".to_string(),
            "db9.prepared_plan_cache_min_exec" => "5".to_string(),
            "hnsw.ef_search" => "40".to_string(),
            "db9.retry_max_attempts" => "64".to_string(),
            "db9.use_optimizer" => "on".to_string(),
            "timezone" => "UTC".to_string(),
            "application_name" => String::new(),
            "client_encoding" => "UTF8".to_string(),
            "standard_conforming_strings" => "on".to_string(),
            "check_function_bodies" => "on".to_string(),
            "xmloption" => "content".to_string(),
            "client_min_messages" => "notice".to_string(),
            "row_security" => "on".to_string(),
            "default_tablespace" => String::new(),
            "default_table_access_method" => "heap".to_string(),
            "transaction_deferrable" | "default_transaction_deferrable" => "off".to_string(),
            "transaction_isolation" => "repeatable read".to_string(),
            "default_transaction_read_only" => "off".to_string(),
            // Immutable / computed GUCs.
            "server_version" => "16.0".to_string(),
            "server_version_num" => "160000".to_string(),
            "server_encoding" => "UTF8".to_string(),
            "datestyle" => "ISO, MDY".to_string(),
            "integer_datetimes" => "on".to_string(),
            "intervalstyle" => "postgres".to_string(),
            _ => {
                // Fall back to static_default from KNOWN_GUCS registry.
                KNOWN_GUCS
                    .iter()
                    .find(|g| g.name == canonical)
                    .and_then(|g| g.static_default)
                    .map(String::from)
                    .unwrap_or_default()
            }
        }
    }

    pub(crate) fn resettable_unknown_guc(name: &str) -> bool {
        matches!(
            Self::canonical_setting_name(name),
            "session_replication_role"
        )
    }

    /// Return the effective post-reset display value for a GUC.
    ///
    /// Like `boot_default_show_value()` but uses the session's configured
    /// defaults for `statement_timeout` and
    /// `idle_in_transaction_session_timeout` instead of hardcoded 0.
    pub(crate) fn reset_default_show_value(&self, name: &str) -> String {
        let canonical = Self::canonical_setting_name(name);
        match canonical {
            "statement_timeout" => Self::format_timeout_show(self.default_statement_timeout_ms),
            "idle_in_transaction_session_timeout" => {
                Self::format_timeout_show(self.default_idle_in_transaction_session_timeout_ms)
            }
            _ => Self::boot_default_show_value(name),
        }
    }

    /// Get a session setting value in a Postgres-like string form, for `SHOW`.
    pub(crate) fn show_value(&self, name: &str) -> Option<String> {
        let canonical = Self::canonical_setting_name(name);
        if !Self::is_immutable_setting(canonical) {
            if let Some(v) = self.local_overrides.get(canonical) {
                return Some(v.clone());
            }
        }

        // ── Structural reverse guard ──
        // If the name is not in KNOWN_GUCS and not in dynamic maps, return None early.
        // This is a best-effort runtime guard: any typed-field match arm below for an
        // unregistered name would be unreachable dead code, nudging developers to add
        // new GUCs to KNOWN_GUCS first.
        let is_registered = KNOWN_GUCS.iter().any(|g| g.name == canonical);
        if !is_registered
            && !self.server_reserved_settings.contains_key(canonical)
            && !self.extra_settings.contains_key(canonical)
            && !self.local_overrides.contains_key(canonical)
        {
            return None;
        }

        match canonical {
            // These are used heavily by drivers for feature detection.
            "server_version" => Some("16.0".to_string()),
            "server_version_num" => Some("160000".to_string()),
            "server_encoding" => Some("UTF8".to_string()),
            "search_path" => Some(Self::format_search_path_show(
                self.local_search_path
                    .as_deref()
                    .unwrap_or(&self.search_path),
            )),
            "datestyle" => Some("ISO, MDY".to_string()),
            "integer_datetimes" => Some("on".to_string()),
            "intervalstyle" => Some("postgres".to_string()),
            "statement_timeout" => Some(Self::format_timeout_show(self.statement_timeout_ms)),
            "lock_timeout" => Some(Self::format_timeout_show(self.lock_timeout_ms)),
            "idle_in_transaction_session_timeout" => Some(Self::format_timeout_show(
                self.idle_in_transaction_session_timeout_ms,
            )),
            "db9.dml_table_scan_max_rows" => Some(self.dml_table_scan_max_rows.to_string()),
            "db9.max_sort_bytes" => Some(self.max_sort_bytes.to_string()),
            "db9.prepared_plan_cache_size" => Some(self.prepared_plan_cache_size.to_string()),
            "db9.prepared_plan_cache_min_exec" => {
                Some(self.prepared_plan_cache_min_exec.to_string())
            }
            "hnsw.ef_search" => Some(self.hnsw_ef_search.to_string()),
            "db9.retry_max_attempts" => Some(self.retry_max_attempts.to_string()),
            "db9.retry_timeout" => Some(Self::format_timeout_show(self.retry_timeout_ms)),
            "db9.use_optimizer" => Some("on".to_string()),
            "embedding.model" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| crate::config::get_embedding_config().model.clone()),
            ),
            "embedding.dimensions" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| {
                        crate::config::get_embedding_config().dimensions.to_string()
                    }),
            ),
            "embedding.max_calls" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| "100".to_string()),
            ),
            "embedding.concurrency" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| "5".to_string()),
            ),
            "embedding.provider" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| crate::config::get_embedding_config().provider_name.clone()),
            ),
            "embedding.endpoint" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| crate::config::get_embedding_config().endpoint.clone()),
            ),
            "embedding.api_key" => {
                // Return the raw value so that internal snapshot consumers
                // (e.g. call_embedding_api) receive the real key.
                // Public SQL readback masking is applied by public_setting_value()
                // through Session::show_setting_value() / QueryContext lookups.
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .or_else(|| crate::config::get_embedding_config().api_key.clone())
                    .or_else(|| Some("".to_string()))
            }
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
            "transaction_deferrable" | "default_transaction_deferrable" => Some("off".to_string()),
            // `transaction.isolation.level` alias is canonicalized above.
            "transaction_isolation" => Some(
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
                .server_reserved_settings
                .get(canonical)
                .cloned()
                .or_else(|| self.extra_settings.get(canonical).cloned())
                .or_else(|| {
                    // default_text_search_config uses a runtime OnceLock fn, not a const.
                    if canonical == "default_text_search_config" {
                        return Some(
                            crate::sql::fts_tokenizers::default_text_search_config().to_string(),
                        );
                    }
                    KNOWN_GUCS
                        .iter()
                        .find(|g| g.name == canonical)
                        .and_then(|g| g.static_default)
                        .map(String::from)
                }),
        }
    }

    /// Collect all settings for SHOW ALL.
    /// Returns Vec<(name, value, description)> sorted alphabetically by name.
    pub(crate) fn show_all(&self) -> Vec<(String, String, String)> {
        let mut result = BTreeMap::new();

        // 1. All registered GUCs (respects local_override > typed field > default precedence)
        for guc in KNOWN_GUCS {
            if let Some(value) = self.show_value(guc.name) {
                result.insert(guc.name.to_string(), (value, guc.description.to_string()));
            }
        }

        // 2. Server-authored reserved settings not in registry.
        for name in self.server_reserved_settings.keys() {
            result
                .entry(name.clone())
                .or_insert_with(|| (self.show_value(name).unwrap_or_default(), String::new()));
        }

        // 3. User-SET extra_settings not in registry
        for name in self.extra_settings.keys() {
            result
                .entry(name.clone())
                .or_insert_with(|| (self.show_value(name).unwrap_or_default(), String::new()));
        }

        // 4. Local overrides not already covered
        for name in self.local_overrides.keys() {
            result
                .entry(name.clone())
                .or_insert_with(|| (self.show_value(name).unwrap_or_default(), String::new()));
        }

        result.into_iter().map(|(n, (v, d))| (n, v, d)).collect()
    }

    pub(crate) fn statement_timeout(&self) -> Option<Duration> {
        if let Some(v) = self.local_overrides.get("statement_timeout") {
            match Self::parse_timeout_millis(v) {
                Ok(0) => return None,
                Ok(ms) => return Some(Duration::from_millis(ms)),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local statement_timeout override"
                    );
                }
            }
        }

        if self.statement_timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(self.statement_timeout_ms))
        }
    }

    pub(crate) fn lock_timeout(&self) -> Option<Duration> {
        if let Some(v) = self.local_overrides.get("lock_timeout") {
            match Self::parse_timeout_millis(v) {
                Ok(0) => return None,
                Ok(ms) => return Some(Duration::from_millis(ms)),
                Err(e) => {
                    tracing::error!(error = %e, value = v, "invalid local lock_timeout override");
                }
            }
        }

        if self.lock_timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(self.lock_timeout_ms))
        }
    }

    pub(crate) fn idle_in_transaction_session_timeout(&self) -> Option<Duration> {
        if let Some(v) = self
            .local_overrides
            .get("idle_in_transaction_session_timeout")
        {
            match Self::parse_timeout_millis(v) {
                Ok(0) => return None,
                Ok(ms) => return Some(Duration::from_millis(ms)),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local idle_in_transaction_session_timeout override"
                    );
                }
            }
        }

        if self.idle_in_transaction_session_timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(
                self.idle_in_transaction_session_timeout_ms,
            ))
        }
    }

    pub(crate) fn max_sort_bytes(&self) -> usize {
        if let Some(v) = self.local_overrides.get("db9.max_sort_bytes") {
            match Self::parse_byte_size(v) {
                Ok(bytes) => return bytes,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local db9.max_sort_bytes override"
                    );
                }
            }
        }
        self.max_sort_bytes
    }

    #[allow(dead_code)] // framework: accessed via settings snapshot in DML executor
    pub(crate) fn dml_table_scan_max_rows(&self) -> usize {
        if let Some(v) = self.local_overrides.get("db9.dml_table_scan_max_rows") {
            match v.parse::<usize>() {
                Ok(rows) => return rows,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local db9.dml_table_scan_max_rows override"
                    );
                }
            }
        }
        self.dml_table_scan_max_rows
    }

    #[allow(dead_code)]
    pub(crate) fn prepared_plan_cache_size(&self) -> usize {
        if let Some(v) = self.local_overrides.get("db9.prepared_plan_cache_size") {
            match v.parse::<usize>() {
                Ok(size) => return size,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local db9.prepared_plan_cache_size override"
                    );
                }
            }
        }
        self.prepared_plan_cache_size
    }

    #[allow(dead_code)]
    pub(crate) fn prepared_plan_cache_min_exec(&self) -> u64 {
        if let Some(v) = self.local_overrides.get("db9.prepared_plan_cache_min_exec") {
            match v.parse::<u64>() {
                Ok(min_exec) => return min_exec,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local db9.prepared_plan_cache_min_exec override"
                    );
                }
            }
        }
        self.prepared_plan_cache_min_exec
    }

    /// All keys that `show_value()` and `default_value()` handle explicitly.
    ///
    /// Kept adjacent to `show_value()` so that adding a new built-in GUC
    /// to `show_value()` without adding it here is an obvious oversight.
    const KNOWN_SETTING_KEYS: &'static [&'static str] = &[
        // show_value() match arms
        "server_version",
        "server_version_num",
        "server_encoding",
        "search_path",
        "datestyle",
        "integer_datetimes",
        "intervalstyle",
        "statement_timeout",
        "lock_timeout",
        "idle_in_transaction_session_timeout",
        "db9.dml_table_scan_max_rows",
        "db9.max_sort_bytes",
        "db9.prepared_plan_cache_size",
        "db9.prepared_plan_cache_min_exec",
        "db9.retry_max_attempts",
        "db9.retry_timeout",
        "db9.use_optimizer",
        "embedding.model",
        "embedding.dimensions",
        "embedding.max_calls",
        "embedding.concurrency",
        "embedding.provider",
        "embedding.endpoint",
        "embedding.api_key",
        "timezone",
        "application_name",
        "client_encoding",
        "standard_conforming_strings",
        "check_function_bodies",
        "xmloption",
        "client_min_messages",
        "row_security",
        "default_tablespace",
        "default_table_access_method",
        "transaction_deferrable",
        "transaction_isolation",
        "default_transaction_deferrable",
        "default_transaction_isolation",
        "default_transaction_read_only",
        "hnsw.ef_search",
        // default_value() fallthrough keys
        "extra_float_digits",
        "bytea_output",
        "lc_messages",
        "lc_monetary",
        "lc_numeric",
        "lc_time",
        "max_identifier_length",
        "max_index_keys",
        "work_mem",
        "default_text_search_config",
        "in_hot_standby",
        "password_encryption",
    ];

    /// Collect all current settings into a flat map.
    ///
    /// Resolves every key through `show_value()` so precedence
    /// (local_overrides > typed fields > server_reserved_settings >
    /// extra_settings > default_value) is identical to `SHOW`.
    pub(crate) fn all_values(&self) -> HashMap<String, String> {
        use std::collections::HashSet;

        let all_keys: HashSet<&str> = Self::KNOWN_SETTING_KEYS
            .iter()
            .copied()
            .chain(self.server_reserved_settings.keys().map(String::as_str))
            .chain(self.extra_settings.keys().map(String::as_str))
            .chain(self.local_overrides.keys().map(String::as_str))
            .collect();

        let mut map = HashMap::with_capacity(all_keys.len());
        for key in all_keys {
            if let Some(v) = self.show_value(key) {
                map.insert(key.to_string(), v);
            }
        }
        map
    }
}
