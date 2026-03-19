//! Per-database storage usage accounting.
//!
//! This module provides:
//! - Data types for per-database and per-table storage statistics.
//! - A global in-memory cache for fast reads.
//! - Versioned binary serialization for TiKV persistence.
//! - Key classification helpers for the background scanner.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

// ============================================================================
// Version header for persisted stats
// ============================================================================

/// Magic header for versioned persistence. Allows schema evolution without
/// breaking old persisted values.
const STORAGE_STATS_MAGIC: &[u8] = b"DB9_STORAGE_STATS_V1\0";

// ============================================================================
// Key classification
// ============================================================================

/// Category of a key within a database keyspace, determined by binary prefix
/// inspection after stripping the `d_{db_id:8BE}_` (11 bytes) database prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCategory {
    /// Row data key: remaining starts with `t_`, table_id = next 8 bytes BE.
    Data,
    /// Index key: remaining starts with `i_`, table_id = next 8 bytes BE.
    Index,
    /// Metadata key: remaining starts with `sys_`.
    Metadata,
    /// Unknown prefix — shouldn't happen for well-formed keys.
    Unknown,
}

/// Result of classifying a raw TiKV key that belongs to a database range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyClassification {
    pub category: KeyCategory,
    /// For Data and Index keys, the table_id extracted from the key.
    /// None for Metadata and Unknown categories.
    pub table_id: Option<u64>,
}

/// Database prefix length: `b"d_"` (2) + 8-byte BE u64 + `b"_"` (1) = 11 bytes.
const DB_PREFIX_LEN: usize = 11;

/// Classify a raw TiKV key that is known to fall within a database's key range.
///
/// The key must start with `d_{db_id:8BE}_` (11 bytes). After stripping this
/// prefix, we inspect the remaining bytes to determine the category and
/// optionally extract the table_id.
///
/// This function performs zero-allocation binary inspection by byte offsets.
pub fn classify_key(key: &[u8]) -> KeyClassification {
    if key.len() < DB_PREFIX_LEN {
        return KeyClassification {
            category: KeyCategory::Unknown,
            table_id: None,
        };
    }

    let remainder = &key[DB_PREFIX_LEN..];

    // t_ prefix: data key, next 8 bytes are table_id (BE u64)
    if remainder.starts_with(b"t_") {
        let table_id = if remainder.len() >= 2 + 8 {
            Some(u64::from_be_bytes(
                remainder[2..10].try_into().unwrap_or([0; 8]),
            ))
        } else {
            None
        };
        return KeyClassification {
            category: KeyCategory::Data,
            table_id,
        };
    }

    // i_ prefix: index key, next 8 bytes are table_id (BE u64)
    if remainder.starts_with(b"i_") {
        let table_id = if remainder.len() >= 2 + 8 {
            Some(u64::from_be_bytes(
                remainder[2..10].try_into().unwrap_or([0; 8]),
            ))
        } else {
            None
        };
        return KeyClassification {
            category: KeyCategory::Index,
            table_id,
        };
    }

    // sys_ prefix: metadata key
    if remainder.starts_with(b"sys_") {
        return KeyClassification {
            category: KeyCategory::Metadata,
            table_id: None,
        };
    }

    KeyClassification {
        category: KeyCategory::Unknown,
        table_id: None,
    }
}

/// Parse the `table_id` from a legacy HNSW key suffix.
///
/// Legacy HNSW storage uses string-formatted keys (decimal IDs) instead of the
/// binary `encode_database_data_prefix()` layout, e.g.:
/// - `d_{db_id}_hnsw_{table_id}_{index_id}_graph`
/// - `d_{db_id}_hnsw_{table_id}_{index_id}_delta_{writer_id}_{seq}`
/// - `d_{db_id}_hnsw_rid_pk2rid_{table_id}_...`
/// - `d_{db_id}_hnsw_rid_rid2pk_{table_id}_...`
/// - `d_{db_id}_hnsw_rid_seq_{table_id}`
///
/// This helper is used by the storage accounting scanner to attribute legacy
/// HNSW bytes to per-table index usage.
pub(crate) fn parse_legacy_hnsw_table_id(remainder: &[u8]) -> Option<u64> {
    const RID_PK2RID: &[u8] = b"rid_pk2rid_";
    const RID_RID2PK: &[u8] = b"rid_rid2pk_";
    const RID_SEQ: &[u8] = b"rid_seq_";

    fn parse_decimal_u64_prefix(bytes: &[u8]) -> Option<(u64, usize)> {
        let mut value: u64 = 0;
        let mut len: usize = 0;
        for &b in bytes {
            if !b.is_ascii_digit() {
                break;
            }
            value = value.checked_mul(10)?.checked_add((b - b'0') as u64)?;
            len += 1;
        }
        if len == 0 {
            None
        } else {
            Some((value, len))
        }
    }

    let (digits, require_trailing_underscore) = if remainder.starts_with(RID_PK2RID) {
        (&remainder[RID_PK2RID.len()..], true)
    } else if remainder.starts_with(RID_RID2PK) {
        (&remainder[RID_RID2PK.len()..], true)
    } else if remainder.starts_with(RID_SEQ) {
        (&remainder[RID_SEQ.len()..], false)
    } else {
        (remainder, true)
    };

    let (table_id, n) = parse_decimal_u64_prefix(digits)?;
    if require_trailing_underscore {
        if digits.get(n) != Some(&b'_') {
            return None;
        }
    }
    Some(table_id)
}

// ============================================================================
// Stats data types
// ============================================================================

/// Per-table storage breakdown within a database.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableStorageStats {
    pub table_id: u64,
    pub data_bytes: u64,
    pub index_bytes: u64,
}

impl TableStorageStats {
    pub fn total_bytes(&self) -> u64 {
        self.data_bytes.saturating_add(self.index_bytes)
    }
}

/// Per-database storage statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DbStorageStats {
    pub database_id: u64,
    pub data_bytes: u64,
    pub index_bytes: u64,
    pub metadata_bytes: u64,
    /// Per-table breakdown keyed by table_id.
    pub tables: HashMap<u64, TableStorageStats>,
    /// Epoch milliseconds when the scan completed.
    pub scanned_at_ms: i64,
    /// Duration of the scan in milliseconds.
    pub scan_duration_ms: i64,
}

impl DbStorageStats {
    pub fn total_bytes(&self) -> u64 {
        self.data_bytes
            .saturating_add(self.index_bytes)
            .saturating_add(self.metadata_bytes)
    }
}

// ============================================================================
// Versioned serialization
// ============================================================================

/// Serialize `DbStorageStats` with a version header for future-proof persistence.
pub fn serialize_storage_stats(stats: &DbStorageStats) -> Vec<u8> {
    let payload = bincode::serialize(stats).expect("DbStorageStats serialization should not fail");
    let mut buf = Vec::with_capacity(STORAGE_STATS_MAGIC.len() + payload.len());
    buf.extend_from_slice(STORAGE_STATS_MAGIC);
    buf.extend_from_slice(&payload);
    buf
}

/// Deserialize `DbStorageStats` from versioned binary encoding.
///
/// Returns `None` if the magic header doesn't match (unknown version).
pub fn deserialize_storage_stats(data: &[u8]) -> Option<DbStorageStats> {
    if !data.starts_with(STORAGE_STATS_MAGIC) {
        return None;
    }
    let payload = &data[STORAGE_STATS_MAGIC.len()..];
    bincode::deserialize(payload).ok()
}

// ============================================================================
// In-memory cache
// ============================================================================

/// Cache key: (keyspace, database_id).
type CacheKey = (String, u64);

/// Global in-memory cache for storage stats, providing fast reads for
/// virtual table queries. Updated by the background scanner and on-demand
/// refresh.
pub struct StorageStatsCache {
    inner: RwLock<HashMap<CacheKey, DbStorageStats>>,
}

impl StorageStatsCache {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Insert or update stats for a database.
    pub fn put(&self, keyspace: &str, db_id: u64, stats: DbStorageStats) {
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        map.insert((keyspace.to_string(), db_id), stats);
    }

    /// Get a clone of stats for a database, if available.
    pub fn get(&self, keyspace: &str, db_id: u64) -> Option<DbStorageStats> {
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        map.get(&(keyspace.to_string(), db_id)).cloned()
    }

    /// Get stats for all databases in a keyspace.
    pub fn get_all_for_keyspace(&self, keyspace: &str) -> Vec<DbStorageStats> {
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .filter(|((ks, _), _)| ks == keyspace)
            .map(|(_, stats)| stats.clone())
            .collect()
    }

    /// Remove cached stats for a database (e.g., on DROP DATABASE).
    #[allow(dead_code)] // used by DROP DATABASE cleanup (Phase 3)
    pub fn evict(&self, keyspace: &str, db_id: u64) {
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        map.remove(&(keyspace.to_string(), db_id));
    }
}

// ============================================================================
// Global singleton
// ============================================================================

static STORAGE_STATS_CACHE: std::sync::OnceLock<StorageStatsCache> = std::sync::OnceLock::new();

/// Get the global storage stats cache. Initializes on first call.
pub fn global_storage_stats_cache() -> &'static StorageStatsCache {
    STORAGE_STATS_CACHE.get_or_init(StorageStatsCache::new)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_data_key() {
        // Construct: d_ + db_id(8 BE) + _ + t_ + table_id(8 BE) + pk_bytes
        let db_id: u64 = 42;
        let table_id: u64 = 7;
        let mut key = Vec::new();
        key.extend_from_slice(b"d_");
        key.extend_from_slice(&db_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(b"t_");
        key.extend_from_slice(&table_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(b"some_pk_data");

        let result = classify_key(&key);
        assert_eq!(result.category, KeyCategory::Data);
        assert_eq!(result.table_id, Some(7));
    }

    #[test]
    fn classify_index_key() {
        let db_id: u64 = 1;
        let table_id: u64 = 99;
        let index_id: u64 = 3;
        let mut key = Vec::new();
        key.extend_from_slice(b"d_");
        key.extend_from_slice(&db_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(b"i_");
        key.extend_from_slice(&table_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(&index_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(b"index_values");

        let result = classify_key(&key);
        assert_eq!(result.category, KeyCategory::Index);
        assert_eq!(result.table_id, Some(99));
    }

    #[test]
    fn classify_metadata_key() {
        let db_id: u64 = 5;
        let mut key = Vec::new();
        key.extend_from_slice(b"d_");
        key.extend_from_slice(&db_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(b"sys_schema_users");

        let result = classify_key(&key);
        assert_eq!(result.category, KeyCategory::Metadata);
        assert_eq!(result.table_id, None);
    }

    #[test]
    fn classify_unknown_key() {
        let db_id: u64 = 5;
        let mut key = Vec::new();
        key.extend_from_slice(b"d_");
        key.extend_from_slice(&db_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(b"unknown_prefix");

        let result = classify_key(&key);
        assert_eq!(result.category, KeyCategory::Unknown);
        assert_eq!(result.table_id, None);
    }

    #[test]
    fn classify_too_short_key() {
        let result = classify_key(b"d_");
        assert_eq!(result.category, KeyCategory::Unknown);
    }

    #[test]
    fn serialization_roundtrip() {
        let mut tables = HashMap::new();
        tables.insert(
            7,
            TableStorageStats {
                table_id: 7,
                data_bytes: 1000,
                index_bytes: 500,
            },
        );
        let stats = DbStorageStats {
            database_id: 42,
            data_bytes: 1000,
            index_bytes: 500,
            metadata_bytes: 50,
            tables,
            scanned_at_ms: 1700000000000,
            scan_duration_ms: 350,
        };

        let encoded = serialize_storage_stats(&stats);
        assert!(encoded.starts_with(STORAGE_STATS_MAGIC));

        let decoded = deserialize_storage_stats(&encoded).expect("should deserialize");
        assert_eq!(decoded.database_id, 42);
        assert_eq!(decoded.data_bytes, 1000);
        assert_eq!(decoded.index_bytes, 500);
        assert_eq!(decoded.metadata_bytes, 50);
        assert_eq!(decoded.total_bytes(), 1550);
        assert_eq!(decoded.scanned_at_ms, 1700000000000);
        assert_eq!(decoded.scan_duration_ms, 350);
        assert_eq!(decoded.tables.len(), 1);
        let t = &decoded.tables[&7];
        assert_eq!(t.data_bytes, 1000);
        assert_eq!(t.index_bytes, 500);
        assert_eq!(t.total_bytes(), 1500);
    }

    #[test]
    fn deserialize_rejects_unknown_version() {
        let data = b"DB9_STORAGE_STATS_V2\0garbage";
        assert!(deserialize_storage_stats(data).is_none());
    }

    #[test]
    fn deserialize_rejects_empty() {
        assert!(deserialize_storage_stats(b"").is_none());
    }

    #[test]
    fn cache_put_get_evict() {
        let cache = StorageStatsCache::new();

        assert!(cache.get("ks", 1).is_none());

        let stats = DbStorageStats {
            database_id: 1,
            data_bytes: 100,
            ..Default::default()
        };
        cache.put("ks", 1, stats);

        let retrieved = cache.get("ks", 1).unwrap();
        assert_eq!(retrieved.data_bytes, 100);

        cache.evict("ks", 1);
        assert!(cache.get("ks", 1).is_none());
    }

    #[test]
    fn cache_get_all_for_keyspace() {
        let cache = StorageStatsCache::new();

        cache.put(
            "ks1",
            1,
            DbStorageStats {
                database_id: 1,
                data_bytes: 100,
                ..Default::default()
            },
        );
        cache.put(
            "ks1",
            2,
            DbStorageStats {
                database_id: 2,
                data_bytes: 200,
                ..Default::default()
            },
        );
        cache.put(
            "ks2",
            3,
            DbStorageStats {
                database_id: 3,
                data_bytes: 300,
                ..Default::default()
            },
        );

        let ks1_stats = cache.get_all_for_keyspace("ks1");
        assert_eq!(ks1_stats.len(), 2);

        let ks2_stats = cache.get_all_for_keyspace("ks2");
        assert_eq!(ks2_stats.len(), 1);
        assert_eq!(ks2_stats[0].data_bytes, 300);

        let empty = cache.get_all_for_keyspace("ks3");
        assert!(empty.is_empty());
    }

    fn build_db_prefix(db_id: u64) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(b"d_");
        key.extend_from_slice(&db_id.to_be_bytes());
        key.push(b'_');
        key
    }

    #[test]
    fn classify_matches_encode_data_key_v2_layout() {
        let db_id = 42u64;
        let table_id = 7u64;
        let mut key = build_db_prefix(db_id);
        key.extend_from_slice(b"t_");
        key.extend_from_slice(&table_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(b"\x01\x02\x03");

        let result = classify_key(&key);
        assert_eq!(result.category, KeyCategory::Data);
        assert_eq!(result.table_id, Some(table_id));
    }

    #[test]
    fn classify_matches_encode_index_key_v2_layout() {
        let db_id = 42u64;
        let table_id = 7u64;
        let index_id = 3u64;
        let mut key = build_db_prefix(db_id);
        key.extend_from_slice(b"i_");
        key.extend_from_slice(&table_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(&index_id.to_be_bytes());
        key.push(b'_');

        let result = classify_key(&key);
        assert_eq!(result.category, KeyCategory::Index);
        assert_eq!(result.table_id, Some(table_id));
    }

    #[test]
    fn classify_matches_schema_key_v2_layout() {
        let db_id = 42u64;
        let mut key = build_db_prefix(db_id);
        key.extend_from_slice(b"sys_schema_public.users");

        let result = classify_key(&key);
        assert_eq!(result.category, KeyCategory::Metadata);
        assert_eq!(result.table_id, None);
    }

    #[test]
    fn parse_legacy_hnsw_table_id_parses_graph_suffix() {
        assert_eq!(parse_legacy_hnsw_table_id(b"123_456_graph"), Some(123));
    }

    #[test]
    fn parse_legacy_hnsw_table_id_parses_delta_suffix() {
        assert_eq!(
            parse_legacy_hnsw_table_id(b"99_3_delta_deadbeef00000000_0000000000000001"),
            Some(99)
        );
    }

    #[test]
    fn parse_legacy_hnsw_table_id_parses_rid_mapping_suffix() {
        assert_eq!(
            parse_legacy_hnsw_table_id(b"rid_pk2rid_7_deadbeef"),
            Some(7)
        );
        assert_eq!(
            parse_legacy_hnsw_table_id(b"rid_rid2pk_7_0000000000000001"),
            Some(7)
        );
    }

    #[test]
    fn parse_legacy_hnsw_table_id_parses_rid_seq_suffix() {
        assert_eq!(parse_legacy_hnsw_table_id(b"rid_seq_42"), Some(42));
    }

    #[test]
    fn parse_legacy_hnsw_table_id_rejects_invalid() {
        assert_eq!(parse_legacy_hnsw_table_id(b"rid_seq_"), None);
        assert_eq!(parse_legacy_hnsw_table_id(b"not_a_key"), None);
        assert_eq!(parse_legacy_hnsw_table_id(b"123no_underscore"), None);
    }
}
