//! Built-in extensions framework.
//!
//! Extensions are compiled into the binary (no runtime dynamic loading).
//! Per-tenant install state is persisted in TiKV via `_sys_*` metadata keys.

pub(crate) mod context;
pub(crate) mod embedding;
pub(crate) mod fs;
pub(crate) mod http;
#[cfg(feature = "parquet")]
pub(crate) mod parquet;

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Built-in schema that exposes extension functions (Supabase-style).
pub const EXTENSIONS_SCHEMA: &str = "extensions";

/// A built-in extension descriptor compiled into the server binary.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionDescriptor {
    /// Extension name (e.g. `"http"`).
    pub name: &'static str,
    /// Stable catalog OID used for `pg_extension.oid`.
    pub oid: i64,
    /// Extension version string (e.g. `"1.0.0"`).
    pub version: &'static str,
    /// Default schema where functions are exposed (e.g. `"extensions"`).
    pub default_schema: &'static str,
}

/// Per-tenant persisted install state for an extension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledExtension {
    pub name: String,
    pub version: String,
    pub schema: String,
    pub installed_at_ms: u64,
    pub enabled: bool,
}

impl InstalledExtension {
    pub fn new(descriptor: &ExtensionDescriptor) -> Self {
        Self {
            name: descriptor.name.to_string(),
            version: descriptor.version.to_string(),
            schema: descriptor.default_schema.to_string(),
            installed_at_ms: now_ms(),
            enabled: true,
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

const HTTP_EXTENSION: ExtensionDescriptor = ExtensionDescriptor {
    name: "http",
    oid: 2000,
    version: "1.0.0",
    default_schema: EXTENSIONS_SCHEMA,
};

// PostgreSQL built-in extension commonly used for UUID generation.
// db9-server implements the required UUID functions as built-ins; installing the
// extension records metadata for compatibility with `pg_dump`/`pg_restore` flows.
const UUID_OSSP_EXTENSION: ExtensionDescriptor = ExtensionDescriptor {
    name: "uuid-ossp",
    oid: 2001,
    version: "1.1",
    default_schema: "public",
};

// PostgreSQL contrib extension providing the `hstore` type.
// We expose it as metadata for client compatibility; db9-server does not currently
// implement full hstore semantics.
const HSTORE_EXTENSION: ExtensionDescriptor = ExtensionDescriptor {
    name: "hstore",
    oid: 2002,
    version: "1.0",
    default_schema: "public",
};

const FS9_EXTENSION: ExtensionDescriptor = ExtensionDescriptor {
    name: "fs9",
    oid: 2003,
    version: "1.0.0",
    default_schema: EXTENSIONS_SCHEMA,
};

const PG_CRON_EXTENSION: ExtensionDescriptor = ExtensionDescriptor {
    name: "pg_cron",
    oid: 2004,
    version: "1.0.0",
    default_schema: "cron",
};

const PARQUET_EXTENSION: ExtensionDescriptor = ExtensionDescriptor {
    name: "parquet",
    oid: 2005,
    version: "1.0.0",
    default_schema: EXTENSIONS_SCHEMA,
};

// zhparser — Chinese full-text search parser (built-in via jieba-rs).
// CREATE EXTENSION zhparser is a no-op; the tokenizer is always available.
const ZHPARSER_EXTENSION: ExtensionDescriptor = ExtensionDescriptor {
    name: "zhparser",
    oid: 2006,
    version: "2.0.0",
    default_schema: "public",
};

// pgvector compatibility shim.
// db9-server ships vector type/operators as built-ins; CREATE EXTENSION vector
// records extension metadata for ORM/agent bootstrap compatibility.
const VECTOR_EXTENSION: ExtensionDescriptor = ExtensionDescriptor {
    name: "vector",
    oid: 2007,
    version: "0.8.1",
    default_schema: "public",
};

const EMBEDDING_EXTENSION: ExtensionDescriptor = ExtensionDescriptor {
    name: "embedding",
    oid: 2008,
    version: "1.0.0",
    default_schema: EXTENSIONS_SCHEMA,
};

// ── Standardized extension error constructors ────────────────────────
// All extension check sites should use these instead of ad-hoc `anyhow!()` strings.
// SQLSTATE 0A000 (feature_not_supported) for not-installed/disabled,
// SQLSTATE 42501 (insufficient_privilege) for permission denied.

/// Extension is not installed. Includes actionable hint.
pub fn ext_not_installed(name: &str) -> anyhow::Error {
    crate::sql::error::SqlError::Unsupported(format!(
        "extension \"{}\" is not installed. Run: CREATE EXTENSION {}",
        name, name
    ))
    .into()
}

/// Extension is installed but disabled.
pub fn ext_disabled(name: &str) -> anyhow::Error {
    crate::sql::error::SqlError::Unsupported(format!("extension \"{}\" is disabled", name)).into()
}

/// Permission denied for an extension (non-superuser).
pub fn ext_permission_denied(name: &str) -> anyhow::Error {
    crate::sql::error::SqlError::PermissionDenied {
        object_type: "extension".into(),
        object_name: format!("\"{}\"", name),
    }
    .into()
}

/// Lookup an extension descriptor by name (case-insensitive).
pub fn descriptor(name: &str) -> Option<&'static ExtensionDescriptor> {
    if name.eq_ignore_ascii_case(HTTP_EXTENSION.name) {
        return Some(&HTTP_EXTENSION);
    }
    if name.eq_ignore_ascii_case(UUID_OSSP_EXTENSION.name) {
        return Some(&UUID_OSSP_EXTENSION);
    }
    if name.eq_ignore_ascii_case(HSTORE_EXTENSION.name) {
        return Some(&HSTORE_EXTENSION);
    }
    if name.eq_ignore_ascii_case(FS9_EXTENSION.name) {
        return Some(&FS9_EXTENSION);
    }
    if name.eq_ignore_ascii_case(PG_CRON_EXTENSION.name) {
        return Some(&PG_CRON_EXTENSION);
    }
    if name.eq_ignore_ascii_case(PARQUET_EXTENSION.name) {
        return Some(&PARQUET_EXTENSION);
    }
    if name.eq_ignore_ascii_case(ZHPARSER_EXTENSION.name) {
        return Some(&ZHPARSER_EXTENSION);
    }
    if name.eq_ignore_ascii_case(VECTOR_EXTENSION.name) {
        return Some(&VECTOR_EXTENSION);
    }
    if name.eq_ignore_ascii_case(EMBEDDING_EXTENSION.name) {
        return Some(&EMBEDDING_EXTENSION);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_uuid_ossp_is_registered() {
        let desc = descriptor("uuid-ossp").expect("uuid-ossp must be registered");
        assert_eq!(desc.name, "uuid-ossp");
        assert_eq!(desc.default_schema, "public");
        assert_eq!(desc.oid, 2001);
    }

    #[test]
    fn descriptor_is_case_insensitive() {
        assert!(descriptor("UUID-OSSP").is_some());
        assert!(descriptor("Http").is_some());
    }

    #[test]
    fn descriptor_hstore_is_registered() {
        let desc = descriptor("hstore").expect("hstore must be registered");
        assert_eq!(desc.name, "hstore");
        assert_eq!(desc.default_schema, "public");
        assert_eq!(desc.oid, 2002);
    }

    #[test]
    fn descriptor_fs9_is_registered() {
        let desc = descriptor("fs9").expect("fs9 must be registered");
        assert_eq!(desc.name, "fs9");
        assert_eq!(desc.default_schema, "extensions");
        assert_eq!(desc.oid, 2003);
    }

    #[test]
    fn descriptor_pg_cron_is_registered() {
        let desc = descriptor("pg_cron").expect("pg_cron must be registered");
        assert_eq!(desc.name, "pg_cron");
        assert_eq!(desc.default_schema, "cron");
        assert_eq!(desc.oid, 2004);
    }

    #[test]
    fn descriptor_parquet_is_registered() {
        let desc = descriptor("parquet").expect("parquet must be registered");
        assert_eq!(desc.name, "parquet");
        assert_eq!(desc.default_schema, "extensions");
        assert_eq!(desc.oid, 2005);
    }

    #[test]
    fn descriptor_vector_is_registered() {
        let desc = descriptor("vector").expect("vector must be registered");
        assert_eq!(desc.name, "vector");
        assert_eq!(desc.default_schema, "public");
        assert_eq!(desc.oid, 2007);
    }

    #[test]
    fn descriptor_embedding_is_registered() {
        let desc = descriptor("embedding").expect("embedding must be registered");
        assert_eq!(desc.name, "embedding");
        assert_eq!(desc.default_schema, "extensions");
        assert_eq!(desc.oid, 2008);
    }
}
