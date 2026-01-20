//! Built-in extensions framework.
//!
//! Extensions are compiled into the binary (no runtime dynamic loading).
//! Per-tenant install state is persisted in TiKV via `_sys_*` metadata keys.

pub(crate) mod context;
pub(crate) mod http;

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

/// Lookup an extension descriptor by name (case-insensitive).
pub fn descriptor(name: &str) -> Option<&'static ExtensionDescriptor> {
    if name.eq_ignore_ascii_case(HTTP_EXTENSION.name) {
        return Some(&HTTP_EXTENSION);
    }
    None
}
