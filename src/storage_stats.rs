//! Per-database storage usage accounting.
//!
//! This module provides:
//! - Data types for per-database and per-table storage statistics.
//! - A global in-memory cache for fast reads.
//! - Versioned binary serialization for TiKV persistence.
//! - Key classification helpers for the background scanner.
// TODO(#2335): migrate to parking_lot — phase 2/3
#![allow(clippy::disallowed_types)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

// ============================================================================
// Version header for persisted stats
// ============================================================================

/// Magic header for the original exact logical-byte scanner format.
///
/// Keep accepting this forever: values already persisted with V1 contain only
/// `data_bytes`, `index_bytes`, `metadata_bytes`, and table breakdowns.
const STORAGE_STATS_MAGIC_V1: &[u8] = b"DB9_STORAGE_STATS_V1\0";

/// Magic header for source-aware stats. V2 can represent either the old exact
/// logical scan semantics or the PD Region/MiB estimate semantics.
const STORAGE_STATS_MAGIC_V2: &[u8] = b"DB9_STORAGE_STATS_V2\0";

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

/// Semantics/source of a persisted database storage row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageStatsSource {
    /// Exact logical bytes from the legacy scanner: `key.len() + value.len()`.
    ExactLogicalScanV1,
    /// Physical/MVCC-inclusive PD Region estimate, reported in MiB by PD.
    PdRegionEstimateV2,
}

impl Default for StorageStatsSource {
    fn default() -> Self {
        Self::ExactLogicalScanV1
    }
}

impl StorageStatsSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactLogicalScanV1 => "exact_logical_scan_v1",
            Self::PdRegionEstimateV2 => "pd_region_estimate_v2",
        }
    }
}

/// Per-database storage statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbStorageStats {
    pub database_id: u64,
    pub source: StorageStatsSource,
    /// Total bytes for estimate sources that cannot provide exact data/index
    /// breakdowns. For exact V1 rows, use [`DbStorageStats::total_bytes`].
    pub total_bytes_estimate: u64,
    /// Region count returned by PD for estimate rows.
    pub region_count: u64,
    /// Empty Region count returned by PD for estimate rows.
    pub empty_region_count: u64,
    /// Approximate key count returned by PD for estimate rows.
    pub storage_keys: u64,
    /// Whether `data_bytes`, `index_bytes`, `metadata_bytes`, and `tables` are
    /// measured values. False for PD estimates.
    pub breakdown_available: bool,
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

impl Default for DbStorageStats {
    fn default() -> Self {
        Self {
            database_id: 0,
            source: StorageStatsSource::ExactLogicalScanV1,
            total_bytes_estimate: 0,
            region_count: 0,
            empty_region_count: 0,
            storage_keys: 0,
            breakdown_available: true,
            data_bytes: 0,
            index_bytes: 0,
            metadata_bytes: 0,
            tables: HashMap::new(),
            scanned_at_ms: 0,
            scan_duration_ms: 0,
        }
    }
}

impl DbStorageStats {
    pub fn total_bytes(&self) -> u64 {
        match self.source {
            StorageStatsSource::ExactLogicalScanV1 => self
                .data_bytes
                .saturating_add(self.index_bytes)
                .saturating_add(self.metadata_bytes),
            StorageStatsSource::PdRegionEstimateV2 => self.total_bytes_estimate,
        }
    }

    pub fn exact_breakdown_available(&self) -> bool {
        self.breakdown_available && self.source == StorageStatsSource::ExactLogicalScanV1
    }

    pub fn pd_region_estimate(
        database_id: u64,
        total_bytes_estimate: u64,
        region_count: u64,
        empty_region_count: u64,
        storage_keys: u64,
        scanned_at_ms: i64,
        scan_duration_ms: i64,
    ) -> Self {
        Self {
            database_id,
            source: StorageStatsSource::PdRegionEstimateV2,
            total_bytes_estimate,
            region_count,
            empty_region_count,
            storage_keys,
            breakdown_available: false,
            data_bytes: 0,
            index_bytes: 0,
            metadata_bytes: 0,
            tables: HashMap::new(),
            scanned_at_ms,
            scan_duration_ms,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DbStorageStatsV1 {
    database_id: u64,
    data_bytes: u64,
    index_bytes: u64,
    metadata_bytes: u64,
    tables: HashMap<u64, TableStorageStats>,
    scanned_at_ms: i64,
    scan_duration_ms: i64,
}

impl From<DbStorageStatsV1> for DbStorageStats {
    fn from(v1: DbStorageStatsV1) -> Self {
        Self {
            database_id: v1.database_id,
            source: StorageStatsSource::ExactLogicalScanV1,
            total_bytes_estimate: 0,
            region_count: 0,
            empty_region_count: 0,
            storage_keys: 0,
            breakdown_available: true,
            data_bytes: v1.data_bytes,
            index_bytes: v1.index_bytes,
            metadata_bytes: v1.metadata_bytes,
            tables: v1.tables,
            scanned_at_ms: v1.scanned_at_ms,
            scan_duration_ms: v1.scan_duration_ms,
        }
    }
}

// ============================================================================
// Versioned serialization
// ============================================================================

/// Serialize `DbStorageStats` with a version header for future-proof persistence.
pub fn serialize_storage_stats(stats: &DbStorageStats) -> Vec<u8> {
    let payload = bincode::serialize(stats).expect("DbStorageStats serialization should not fail");
    let mut buf = Vec::with_capacity(STORAGE_STATS_MAGIC_V2.len() + payload.len());
    buf.extend_from_slice(STORAGE_STATS_MAGIC_V2);
    buf.extend_from_slice(&payload);
    buf
}

/// Deserialize `DbStorageStats` from versioned binary encoding.
///
/// Returns `None` if the magic header doesn't match (unknown version).
pub fn deserialize_storage_stats(data: &[u8]) -> Option<DbStorageStats> {
    if data.starts_with(STORAGE_STATS_MAGIC_V2) {
        let payload = &data[STORAGE_STATS_MAGIC_V2.len()..];
        return bincode::deserialize(payload).ok();
    }
    if data.starts_with(STORAGE_STATS_MAGIC_V1) {
        let payload = &data[STORAGE_STATS_MAGIC_V1.len()..];
        let v1: DbStorageStatsV1 = bincode::deserialize(payload).ok()?;
        return Some(v1.into());
    }
    None
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
    #[cfg(test)]
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
            ..Default::default()
        };

        let encoded = serialize_storage_stats(&stats);
        assert!(encoded.starts_with(STORAGE_STATS_MAGIC_V2));

        let decoded = deserialize_storage_stats(&encoded).expect("should deserialize");
        assert_eq!(decoded.database_id, 42);
        assert_eq!(decoded.source, StorageStatsSource::ExactLogicalScanV1);
        assert!(decoded.exact_breakdown_available());
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
    fn deserialize_v1_exact_stats_remains_supported() {
        let legacy = DbStorageStatsV1 {
            database_id: 42,
            data_bytes: 1000,
            index_bytes: 500,
            metadata_bytes: 50,
            tables: HashMap::new(),
            scanned_at_ms: 1700000000000,
            scan_duration_ms: 350,
        };
        let payload = bincode::serialize(&legacy).expect("legacy stats serialize");
        let mut encoded = Vec::with_capacity(STORAGE_STATS_MAGIC_V1.len() + payload.len());
        encoded.extend_from_slice(STORAGE_STATS_MAGIC_V1);
        encoded.extend_from_slice(&payload);

        let decoded = deserialize_storage_stats(&encoded).expect("v1 stats should deserialize");
        assert_eq!(decoded.database_id, 42);
        assert_eq!(decoded.source, StorageStatsSource::ExactLogicalScanV1);
        assert!(decoded.exact_breakdown_available());
        assert_eq!(decoded.total_bytes(), 1550);
        assert_eq!(decoded.total_bytes_estimate, 0);
    }

    #[test]
    fn pd_estimate_total_bytes_uses_estimate_not_empty_breakdown() {
        let stats =
            DbStorageStats::pd_region_estimate(42, 64 * 1024 * 1024, 3, 1, 1234, 1700000000000, 25);

        assert_eq!(stats.source, StorageStatsSource::PdRegionEstimateV2);
        assert!(!stats.exact_breakdown_available());
        assert_eq!(stats.data_bytes, 0);
        assert_eq!(stats.index_bytes, 0);
        assert_eq!(stats.metadata_bytes, 0);
        assert_eq!(stats.total_bytes(), 64 * 1024 * 1024);

        let encoded = serialize_storage_stats(&stats);
        let decoded = deserialize_storage_stats(&encoded).expect("pd stats deserialize");
        assert_eq!(decoded.source, StorageStatsSource::PdRegionEstimateV2);
        assert_eq!(decoded.total_bytes(), 64 * 1024 * 1024);
        assert_eq!(decoded.region_count, 3);
        assert_eq!(decoded.empty_region_count, 1);
        assert_eq!(decoded.storage_keys, 1234);
    }

    #[test]
    fn deserialize_rejects_unknown_version() {
        let data = b"DB9_STORAGE_STATS_V3\0garbage";
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
}
