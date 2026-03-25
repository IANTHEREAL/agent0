use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Keyspace-local PostgreSQL database metadata (storage format v2).
///
/// In db9-server, a TiKV keyspace maps to a tenant. Within a tenant, multiple logical
/// PostgreSQL databases are supported by partitioning all database-local keys
/// under a fixed `database_id` prefix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseDef {
    /// Database ID (used as the key prefix for all data within this database).
    pub id: u64,
    /// Database name (e.g. "postgres", "myapp").
    pub name: String,
    /// Database OID for `pg_catalog.pg_database` compatibility.
    pub oid: u32,
    /// Owner role/user name (metadata only; no permission enforcement yet).
    pub owner: String,
    /// Encoding name (always UTF8 for now).
    pub encoding: String,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at: i64,
    /// Template database flag (reserved).
    pub is_template: bool,
    /// Allow connections flag (reserved).
    pub allow_conn: bool,
}

impl DatabaseDef {
    pub fn new(id: u64, name: String, owner: String) -> Self {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Self {
            id,
            name,
            oid: u32::try_from(id).unwrap_or(u32::MAX),
            owner,
            encoding: "UTF8".to_string(),
            created_at: i64::try_from(created_at).unwrap_or(i64::MAX),
            is_template: false,
            allow_conn: true,
        }
    }

    pub fn default_postgres(id: u64, owner: String) -> Self {
        Self::new(id, "postgres".to_string(), owner)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationRecord {
    pub name: String,
    pub applied_at: String,
    pub checksum: String,
    #[serde(default)]
    pub sql_preview: String,
}
