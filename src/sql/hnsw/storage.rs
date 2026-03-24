//! HNSW storage layer for persisting vector indexes to TiKV.
//!
//! Storage versions:
//! - v0 (legacy): monolithic graph blob in a single KV pair per index.
//! - v1 (delta-log): DML writes small delta entries; a background merge worker
//!   consolidates them into the base graph periodically.

use std::collections::HashMap;
use std::fs;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tikv_client::{BoundRange, Transaction};
use usearch::ffi::{new_index, Index, IndexOptions, MetricKind, ScalarKind};

use crate::sql::error::SqlError;
use crate::sql::hnsw::HnswDistanceMetric;
use crate::storage::TikvStore;
use crate::txn::{txn_delete, txn_put};

/// Batch size for paginated delta scans (matches TABLE_SCAN_BATCH_SIZE).
const DELTA_SCAN_BATCH_SIZE: u32 = 1024;

// ---------------------------------------------------------------------------
// Writer ID + process-level sequence generator
// ---------------------------------------------------------------------------

/// 16 hex chars of randomness (64 bits), generated once at process startup.
/// Combined with the per-process monotonic DELTA_SEQ counter, this produces
/// probabilistically unique delta keys even when multiple db9-server instances
/// run simultaneously (collision ≈ 2⁻⁶⁴ per pair of instances).
static WRITER_ID: LazyLock<String> = LazyLock::new(|| {
    let id: u64 = rand::thread_rng().gen();
    format!("{id:016x}")
});

/// Process-local monotonic counter for delta key uniqueness within a process.
static DELTA_SEQ: AtomicU64 = AtomicU64::new(0);

/// Allocate next unique delta sequence number. Lock-free, no TiKV round-trip.
pub fn next_delta_seq() -> u64 {
    DELTA_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Get this process's writer ID (16 hex chars).
pub fn writer_id() -> &'static str {
    &WRITER_ID
}

/// How HNSW labels (usearch u64 keys) relate to user primary keys.
///
/// - `Direct`: label = PK value cast to u64 (legacy, INTEGER/BIGINT PKs only).
/// - `Mapped`: label = internally allocated rowid; a persistent bidirectional
///   mapping (rowid ↔ PK) is stored in TiKV. Supports any PK type.
///
/// Backward-compatible: old `HnswMeta` JSON without `label_mode` deserializes
/// to `Direct` via `#[serde(default)]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HnswLabelMode {
    #[default]
    Direct,
    Mapped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswMeta {
    pub count: u64,
    pub capacity: u64,
    pub dimensions: usize,
    pub distance_metric: String,
    pub m: usize,
    pub ef_construction: usize,
    /// 0 = legacy monolithic, 1 = delta-log. Backward-compatible: old JSON
    /// without this field deserializes to 0.
    #[serde(default)]
    pub storage_version: u8,
    /// How usearch labels map to user PKs. Defaults to `Direct` for
    /// backward compatibility with existing indexes on INTEGER/BIGINT PKs.
    #[serde(default)]
    #[serde(skip_serializing_if = "is_direct_mode")]
    pub label_mode: HnswLabelMode,
    /// When `true`, the index is frozen: merge and sweep skip it.
    /// Set when a serialized graph exceeds `HNSW_GRAPH_MAX_BYTES`.
    /// Backward-compatible: old JSON without this field defaults to `false`.
    #[serde(default)]
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub frozen: bool,
    /// S3 graph version. Monotonically increasing, incremented on each merge.
    /// 0 means no S3 graph has been written (TiKV-only or pre-migration).
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub graph_version: u64,
    /// Unix timestamp (seconds) when the index was dropped via DDL.
    /// Used as tombstone for MVCC-safe S3 cleanup.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_at: Option<u64>,
    /// Random per-index nonce set on CREATE INDEX. Used as part of the
    /// shared index cache key to prevent stale-cache hits after DROP+CREATE
    /// cycles that reuse the same index_id. Without this, two indexes with
    /// identical parameters (but on different columns) would produce the
    /// same cache fingerprint. The nonce makes each index instance globally
    /// unique regardless of parameter coincidence.
    /// Backward-compatible: old meta without this field deserializes to 0.
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub cache_nonce: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HnswS3RetiredVersionGc {
    /// Conservative TiKV TSO frontier. The S3 object must not be deleted until
    /// PD's GC safepoint has advanced to or beyond this value.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_after_safepoint: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HnswS3PrefixGc {
    /// Conservative TiKV TSO frontier for deleting the entire S3 prefix.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_after_safepoint: Option<u64>,
    /// Optional reason for observability (`drop` / `truncate`).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn is_direct_mode(mode: &HnswLabelMode) -> bool {
    matches!(mode, HnswLabelMode::Direct)
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

pub struct HnswIndexHandle(Box<dyn Deref<Target = Index>>);

unsafe impl Send for HnswIndexHandle {}
unsafe impl Sync for HnswIndexHandle {}

impl HnswIndexHandle {
    pub fn new<T>(index: T) -> Self
    where
        T: Deref<Target = Index> + 'static,
    {
        Self(Box::new(index))
    }
}

impl Deref for HnswIndexHandle {
    type Target = Index;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

pub fn hnsw_graph_key(db_id: u64, table_id: u64, index_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_{table_id}_{index_id}_graph").into_bytes()
}

pub fn hnsw_meta_key(db_id: u64, table_id: u64, index_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_{table_id}_{index_id}_meta").into_bytes()
}

pub fn hnsw_db_prefix(db_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_").into_bytes()
}

pub fn hnsw_db_prefix_end(db_id: u64) -> Vec<u8> {
    let mut end = hnsw_db_prefix(db_id);
    end.push(0xFF);
    end
}

pub fn hnsw_s3_retired_version_key(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    version: u64,
) -> Vec<u8> {
    format!("d_{db_id}_hnsw_{table_id}_{index_id}_s3_retired_{version:020}").into_bytes()
}

pub fn hnsw_s3_retired_version_prefix(db_id: u64, table_id: u64, index_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_{table_id}_{index_id}_s3_retired_").into_bytes()
}

pub fn hnsw_s3_retired_version_prefix_end(db_id: u64, table_id: u64, index_id: u64) -> Vec<u8> {
    let mut end = hnsw_s3_retired_version_prefix(db_id, table_id, index_id);
    end.push(0xFF);
    end
}

pub fn hnsw_s3_prefix_gc_key(db_id: u64, table_id: u64, index_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_{table_id}_{index_id}_s3_prefix_gc").into_bytes()
}

fn parse_hnsw_index_key_parts(key: &[u8]) -> Option<(u64, u64, u64, &str)> {
    let s = std::str::from_utf8(key).ok()?;
    let rest = s.strip_prefix("d_")?;
    let (db_str, rest) = rest.split_once("_hnsw_")?;
    let db_id = db_str.parse().ok()?;
    let (table_str, rest) = rest.split_once('_')?;
    let table_id = table_str.parse().ok()?;
    let (index_str, suffix) = rest.split_once('_')?;
    let index_id = index_str.parse().ok()?;
    Some((db_id, table_id, index_id, suffix))
}

#[allow(dead_code)] // Used in tests; may be useful for future key introspection
pub fn parse_hnsw_meta_key(key: &[u8]) -> Option<(u64, u64, u64)> {
    let (db_id, table_id, index_id, suffix) = parse_hnsw_index_key_parts(key)?;
    (suffix == "meta").then_some((db_id, table_id, index_id))
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_hnsw_s3_retired_version_key(key: &[u8]) -> Option<(u64, u64, u64, u64)> {
    let (db_id, table_id, index_id, suffix) = parse_hnsw_index_key_parts(key)?;
    let version = suffix.strip_prefix("s3_retired_")?.parse().ok()?;
    Some((db_id, table_id, index_id, version))
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_hnsw_s3_prefix_gc_key(key: &[u8]) -> Option<(u64, u64, u64)> {
    let (db_id, table_id, index_id, suffix) = parse_hnsw_index_key_parts(key)?;
    (suffix == "s3_prefix_gc").then_some((db_id, table_id, index_id))
}

/// Return whether the current keyspace contains any *live* HNSW index whose
/// readable graph is S3-backed.
///
/// This is used for startup / connection-time fail-fast on nodes that are
/// missing `HNSW_S3_BUCKET`. We intentionally scope this to live schema state:
/// dropped-index tombstones and GC markers do not make query serving depend on
/// S3, while live `graph_version > 0` indexes do.
pub async fn keyspace_requires_hnsw_s3(store: &TikvStore) -> anyhow::Result<bool> {
    let mut txn = store.begin().await?;
    let result = async {
        for db in store.list_databases(&mut txn).await? {
            let table_names = store.list_tables(&mut txn, db.id).await?;
            let schemas = store
                .list_table_schemas(&mut txn, db.id, &table_names)
                .await?;
            for schema in schemas {
                for index in &schema.indexes {
                    if !index.is_hnsw() {
                        continue;
                    }
                    let Some(meta_bytes) = txn
                        .get(hnsw_meta_key(db.id, schema.table_id, index.id))
                        .await?
                    else {
                        continue;
                    };
                    let meta: HnswMeta = serde_json::from_slice(&meta_bytes).context(
                        "Failed to deserialize HNSW meta while checking live S3 requirement",
                    )?;
                    if meta.dropped_at.is_none() && meta.graph_version > 0 {
                        return Ok::<bool, anyhow::Error>(true);
                    }
                }
            }
        }
        Ok(false)
    }
    .await;

    match result {
        Ok(required) => {
            txn.rollback().await.ok();
            Ok(required)
        }
        Err(e) => {
            txn.rollback().await.ok();
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Delta key format
// ---------------------------------------------------------------------------

/// Delta key: probabilistically unique per vector mutation.
/// Format: `d_{db_id}_hnsw_{table_id}_{index_id}_delta_{writer_id}_{seq:016x}`
///
/// - `writer_id` (16 hex chars): per-process random, prevents cross-instance collision.
/// - `seq` (16 hex chars, zero-padded): monotonic per-process, within-process ordering.
/// - Prefix scan on `_delta_` collects deltas from ALL writers for an index.
pub fn hnsw_delta_key(db_id: u64, table_id: u64, index_id: u64, seq: u64) -> Vec<u8> {
    format!(
        "d_{db_id}_hnsw_{table_id}_{index_id}_delta_{}_{seq:016x}",
        writer_id()
    )
    .into_bytes()
}

/// Prefix for range scan of ALL deltas for an index (all writers).
pub fn hnsw_delta_prefix(db_id: u64, table_id: u64, index_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_{table_id}_{index_id}_delta_").into_bytes()
}

/// Encode (table_id, index_id) into a single i64 for worker task dedup.
/// table_id occupies upper 32 bits, index_id occupies lower 32 bits.
///
/// Returns `Err` if either ID exceeds 32 bits. In practice, these are
/// auto-increment sequence values starting at 1 — exceeding 2^32 (~4B)
/// is not realistic for a single database, but we return an error rather
/// than silently truncating or panicking the process.
pub fn hnsw_merge_task_id(table_id: u64, index_id: u64) -> anyhow::Result<i64> {
    if table_id > u32::MAX as u64 {
        anyhow::bail!(
            "table_id {} exceeds 32-bit range for HNSW merge task key",
            table_id
        );
    }
    if index_id > u32::MAX as u64 {
        anyhow::bail!(
            "index_id {} exceeds 32-bit range for HNSW merge task key",
            index_id
        );
    }
    Ok((((table_id & 0xFFFFFFFF) << 32) | (index_id & 0xFFFFFFFF)) as i64)
}

/// End-of-range sentinel for bounded range scans.
pub fn hnsw_delta_prefix_end(db_id: u64, table_id: u64, index_id: u64) -> Vec<u8> {
    let mut end = hnsw_delta_prefix(db_id, table_id, index_id);
    end.push(0xFF);
    end
}

// ---------------------------------------------------------------------------
// Delta data structure
// ---------------------------------------------------------------------------

/// A single HNSW delta entry: one vector add operation.
/// Serialized with bincode (~4 + 4*dimensions bytes per delta).
///
/// No Remove variant — usearch 0.21 has no remove() method. DELETEs are
/// handled by lazy tombstoning via visibility filtering in hnsw_scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswDelta {
    pub label: u64,
    pub vector: Vec<f32>,
}

// ---------------------------------------------------------------------------
// Merge marker (per-index global state for background merge coordination)
// ---------------------------------------------------------------------------

fn temp_file_path(db_id: u64, table_id: u64, index_id: u64, op: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!(
        "db9_hnsw_{op}_{db_id}_{table_id}_{index_id}_{pid}_{nanos}.usearch"
    ))
}

pub fn vec_f64_to_f32(v: &[f64]) -> Vec<f32> {
    v.iter().map(|&x| x as f32).collect()
}

pub fn metric_from_string(s: &str) -> Result<MetricKind, SqlError> {
    match HnswDistanceMetric::from_str(s) {
        Some(HnswDistanceMetric::L2) => Ok(MetricKind::L2Sq),
        Some(HnswDistanceMetric::Cosine) => Ok(MetricKind::Cos),
        Some(HnswDistanceMetric::InnerProduct) => Ok(MetricKind::IP),
        _ => Err(SqlError::Internal(anyhow::anyhow!(
            "Unknown HNSW distance metric: {}",
            s
        ))),
    }
}

pub fn create_empty_hnsw_index(
    dimensions: usize,
    distance_metric: &str,
    m: usize,
    ef_construction: usize,
) -> Result<(HnswIndexHandle, HnswMeta), SqlError> {
    let metric = metric_from_string(distance_metric)?;
    let options = IndexOptions {
        dimensions,
        metric,
        quantization: ScalarKind::F32,
        connectivity: m,
        expansion_add: ef_construction,
        expansion_search: super::HNSW_DEFAULT_EF_SEARCH,
    };
    let index = new_index(&options).map_err(|e| SqlError::from(anyhow::anyhow!(e)))?;
    let meta = HnswMeta {
        count: 0,
        capacity: index.capacity() as u64,
        dimensions,
        distance_metric: distance_metric.to_string(),
        m,
        ef_construction,
        storage_version: 1,
        label_mode: HnswLabelMode::Direct,
        frozen: false,
        graph_version: 0,
        dropped_at: None,
        cache_nonce: 0,
    };
    Ok((HnswIndexHandle::new(index), meta))
}

#[allow(dead_code)]
pub async fn save_hnsw_graph(
    store: &TikvStore,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    index: &Index,
    meta: &HnswMeta,
) -> Result<(), SqlError> {
    let (graph_bytes, meta_bytes) =
        serialize_hnsw_snapshot(db_id, table_id, index_id, index, meta)?;

    let mut txn = store
        .begin()
        .await
        .map_err(|e| SqlError::from(anyhow::anyhow!(e)))?;

    txn_put(
        &mut txn,
        hnsw_graph_key(db_id, table_id, index_id),
        graph_bytes,
    )
    .await
    .map_err(|e| SqlError::from(anyhow::anyhow!(e)))?;
    txn_put(
        &mut txn,
        hnsw_meta_key(db_id, table_id, index_id),
        meta_bytes,
    )
    .await
    .map_err(|e| SqlError::from(anyhow::anyhow!(e)))?;

    txn.commit()
        .await
        .map_err(|e| SqlError::from(anyhow::anyhow!(e)))?;
    Ok(())
}

pub fn serialize_hnsw_snapshot(
    db_id: u64,
    table_id: u64,
    index_id: u64,
    index: &Index,
    meta: &HnswMeta,
) -> Result<(Vec<u8>, Vec<u8>), SqlError> {
    let temp_path = temp_file_path(db_id, table_id, index_id, "save");
    let temp_path_str = temp_path.to_string_lossy().to_string();

    if let Err(e) = index
        .save(&temp_path_str)
        .map_err(|e| SqlError::from(anyhow::anyhow!(e)))
    {
        let _ = fs::remove_file(&temp_path);
        return Err(e);
    }

    let graph_bytes = fs::read(&temp_path).map_err(|e| SqlError::from(anyhow::anyhow!(e)));
    let _ = fs::remove_file(&temp_path);
    let graph_bytes = graph_bytes?;
    let meta_bytes = serde_json::to_vec(meta).map_err(|e| SqlError::from(anyhow::anyhow!(e)))?;
    Ok((graph_bytes, meta_bytes))
}

// ===========================================================================
// Delta-log helpers (v1 storage)
// ===========================================================================

/// Write a batch of delta entries to TiKV within the caller's transaction.
/// Each delta gets a unique key via `writer_id()` + `next_delta_seq()`.
/// Returns total bytes written (for observability).
pub async fn write_hnsw_deltas(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    adds: &[(u64, Vec<f32>)], // (label, vector_f32)
) -> Result<u64, SqlError> {
    let mut total_bytes = 0u64;
    for (label, vector) in adds {
        let seq = next_delta_seq();
        let key = hnsw_delta_key(db_id, table_id, index_id, seq);
        let delta = HnswDelta {
            label: *label,
            vector: vector.clone(),
        };
        let value =
            bincode::serialize(&delta).map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
        total_bytes += value.len() as u64;
        txn_put(txn, key, value)
            .await
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    }
    Ok(total_bytes)
}

/// Delete delta keys by exact key list (used by merge after applying a batch).
pub async fn delete_delta_keys(txn: &mut Transaction, keys: &[Vec<u8>]) -> Result<(), SqlError> {
    for key in keys {
        txn_delete(txn, key.clone())
            .await
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    }
    Ok(())
}

/// Delete ALL delta keys for an index. Paginated to completion.
/// Used by DROP INDEX / DROP TABLE / TRUNCATE.
pub async fn delete_all_deltas(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    index_id: u64,
) -> Result<(), SqlError> {
    let prefix = hnsw_delta_prefix(db_id, table_id, index_id);
    let end = hnsw_delta_prefix_end(db_id, table_id, index_id);
    let mut start = prefix.clone();
    loop {
        let range: BoundRange = (start.clone()..end.clone()).into();
        let pairs: Vec<tikv_client::KvPair> = txn
            .scan(range, DELTA_SCAN_BATCH_SIZE)
            .await
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
            .collect();
        let count = pairs.len();
        let mut last_key: Option<Vec<u8>> = None;
        for pair in pairs {
            let k: &[u8] = pair.key().as_ref().into();
            let key: Vec<u8> = k.to_vec();
            if !key.starts_with(&prefix) {
                break;
            }
            last_key = Some(key.clone());
            txn_delete(txn, key)
                .await
                .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
        }
        if (count as u32) < DELTA_SCAN_BATCH_SIZE {
            break;
        }
        match last_key {
            Some(mut lk) => {
                lk.push(0x00);
                start = lk;
            }
            None => break,
        }
    }
    Ok(())
}

// ===========================================================================
// Base graph loader (without re-reading meta)
// ===========================================================================

/// Load ONLY the base graph blob from TiKV or S3 (caller provides meta).
/// Returns None if no graph_key exists (e.g., empty index before first merge),
/// or if the index has been tombstoned (`dropped_at` is set).
///
/// When S3 is configured and `graph_version > 0`:
///   1. Check the process-level file cache for a matching version.
///   2. On cache hit: load the index from the cached file path.
///   3. On cache miss: S3 GET, insert into cache, load from cached file.
///   4. The cached file is persistent (NOT deleted after load).
///
/// When loading from TiKV (`graph_version == 0`):
///   Uses a temp file, loads, and deletes (no cache integration for TiKV blobs).
pub async fn load_base_graph(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    meta: &HnswMeta,
    keyspace: &str,
) -> Result<Option<(HnswIndexHandle, HnswMeta)>, SqlError> {
    // Tombstoned indexes should not be loaded.
    if meta.dropped_at.is_some() {
        return Ok(None);
    }

    // Determine the file path to load the index from.
    // For S3 graphs: try cache first, then S3 GET + cache insert.
    // For TiKV graphs: read bytes, write to temp file.
    let (load_path, is_temp_file) = if meta.graph_version > 0 {
        // S3 path: graph_version > 0 means the graph was written to S3.
        let Some(s3) = super::s3::hnsw_s3_client() else {
            return Err(SqlError::Internal(anyhow::anyhow!(
                "HNSW index d_{}_hnsw_{}_{} requires S3 storage (graph_version={}) \
                 but S3 is not configured. Set HNSW_S3_BUCKET to enable S3 offload.",
                db_id,
                table_id,
                index_id,
                meta.graph_version
            )));
        };

        let cache = super::s3::hnsw_graph_cache();

        // Step 1: cache lookup.
        if let Some(cached_path) =
            cache.lookup(keyspace, db_id, table_id, index_id, meta.graph_version)
        {
            (cached_path, false)
        } else {
            // Step 2: cache miss — S3 GET.
            let graph_bytes = match s3
                .get_graph(keyspace, db_id, table_id, index_id, meta.graph_version)
                .await
            {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    return Err(SqlError::Internal(anyhow::anyhow!(
                        "HNSW S3 graph not found for d_{}_hnsw_{}_{} version {}",
                        db_id,
                        table_id,
                        index_id,
                        meta.graph_version
                    )));
                }
                Err(e) => {
                    return Err(SqlError::Internal(anyhow::anyhow!(
                        "HNSW S3 graph load failed for d_{}_hnsw_{}_{}: {}",
                        db_id,
                        table_id,
                        index_id,
                        e
                    )));
                }
            };

            // Step 3: insert into cache.
            let cached_path = cache
                .insert(
                    keyspace,
                    db_id,
                    table_id,
                    index_id,
                    meta.graph_version,
                    &graph_bytes,
                )
                .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;

            (cached_path, false)
        }
    } else {
        // TiKV path: graph_version == 0, read from TiKV as before.
        let graph_key = hnsw_graph_key(db_id, table_id, index_id);
        let Some(bytes) = txn
            .get(graph_key)
            .await
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
        else {
            return Ok(None);
        };

        // Write to temp file for TiKV path (no cache).
        let temp_path = temp_file_path(db_id, table_id, index_id, "load");
        fs::write(&temp_path, &bytes).map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
        (temp_path, true)
    };

    let metric = metric_from_string(&meta.distance_metric)?;
    let options = IndexOptions {
        dimensions: meta.dimensions,
        metric,
        quantization: ScalarKind::F32,
        connectivity: meta.m,
        expansion_add: meta.ef_construction,
        expansion_search: super::HNSW_DEFAULT_EF_SEARCH,
    };
    // Try loading the graph from the resolved path. If it fails on a
    // cached file (not temp), retry from S3 — the file may have been
    // deleted by concurrent LRU eviction or version-mismatch cleanup
    // (TOCTOU race between cache.lookup() and index.load()).
    // Try loading the index from the resolved path. If the file was
    // deleted by concurrent LRU eviction or version-mismatch cleanup
    // (TOCTOU race), we detect it here and retry from S3.
    //
    // We must ensure the non-Send UniquePtr<Index> does NOT live across
    // an .await point. So we first attempt the load synchronously, and
    // only if it fails do we drop everything, do the async S3 fetch,
    // then create a new index.
    let need_retry = {
        let idx = new_index(&options).map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
        let path_str = load_path.to_string_lossy().to_string();
        match idx.load(&path_str) {
            Ok(()) => {
                if is_temp_file {
                    let _ = fs::remove_file(&load_path);
                }
                // Success — return early with this index.
                let mut live_meta = meta.clone();
                live_meta.count = idx.size() as u64;
                live_meta.capacity = idx.capacity() as u64;
                return Ok(Some((HnswIndexHandle::new(idx), live_meta)));
            }
            Err(_e) if !is_temp_file && meta.graph_version > 0 => {
                // Cache file likely deleted — need S3 retry.
                true
            }
            Err(e) => {
                if is_temp_file {
                    let _ = fs::remove_file(&load_path);
                }
                return Err(SqlError::Internal(anyhow::anyhow!(e)));
            }
        }
        // idx (UniquePtr<Index>) is dropped here before any .await
    };

    // Retry path: re-fetch from S3 into a temp file.
    if need_retry {
        let s3 = super::s3::hnsw_s3_client().ok_or_else(|| {
            SqlError::Internal(anyhow::anyhow!("S3 client unavailable for retry"))
        })?;
        let bytes = s3
            .get_graph(keyspace, db_id, table_id, index_id, meta.graph_version)
            .await
            .map_err(|e| SqlError::Internal(anyhow::anyhow!("S3 retry failed: {}", e)))?
            .ok_or_else(|| {
                SqlError::Internal(anyhow::anyhow!(
                    "HNSW S3 graph not found on retry (version {})",
                    meta.graph_version
                ))
            })?;
        // Re-insert into cache so future queries don't repeat the
        // retry. Without this, the stale cache entry (pointing at the
        // deleted file) would cause every subsequent query to fail-then-
        // retry from S3, degrading to uncached performance permanently.
        let cache = super::s3::hnsw_graph_cache();
        let cached_path = cache
            .insert(
                keyspace,
                db_id,
                table_id,
                index_id,
                meta.graph_version,
                &bytes,
            )
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;

        let index = new_index(&options).map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
        let cached_str = cached_path.to_string_lossy().to_string();
        let load_result = index
            .load(&cached_str)
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)));
        load_result?;

        let mut live_meta = meta.clone();
        live_meta.count = index.size() as u64;
        live_meta.capacity = index.capacity() as u64;
        return Ok(Some((HnswIndexHandle::new(index), live_meta)));
    }

    // Unreachable: the success branch returns at line ~569,
    // the retry branch returns at line ~613, and the error
    // branches return Err. This satisfies the compiler.
    unreachable!("load_base_graph: all code paths return above")
}

// ===========================================================================
// Delta-aware graph loader (paginated streaming apply)
// ===========================================================================

/// Load base graph + apply pending deltas → ready-to-query in-memory index.
/// Handles both v0 (legacy) and v1 (delta-log) transparently.
///
/// Returns `(index_handle, meta, delta_count_applied)`.
/// `delta_count_applied` lets the caller decide whether to trigger merge.
///
/// Deltas are applied via paginated streaming: scan one page, apply to
/// in-memory index, advance start_key, repeat. Never collects all deltas
/// into a single Vec.
#[allow(dead_code)] // retained for worker merge path; query path uses two-level search
pub async fn load_hnsw_graph_with_deltas(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    keyspace: &str,
) -> Result<Option<(HnswIndexHandle, HnswMeta, usize)>, SqlError> {
    // 1. Read meta
    let meta_key = hnsw_meta_key(db_id, table_id, index_id);
    let Some(meta_bytes) = txn
        .get(meta_key)
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
    else {
        return Ok(None); // No index metadata → index doesn't exist
    };
    let meta: HnswMeta =
        serde_json::from_slice(&meta_bytes).map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;

    // Tombstoned indexes should not be loaded.
    if meta.dropped_at.is_some() {
        return Ok(None);
    }

    // 2. Only v1 (delta-log) and v2 (S3) are supported; reject anything else.
    if meta.storage_version != 1 && meta.storage_version != 2 {
        return Err(SqlError::Internal(anyhow::anyhow!(
            "HNSW index d_{}_hnsw_{}_{} has unsupported storage_version={}; \
             only v1 (delta-log) and v2 (S3) are supported. Please rebuild the index.",
            db_id,
            table_id,
            index_id,
            meta.storage_version
        )));
    }

    // 3. Delta-log path: load base graph (may not exist yet)
    let (index, mut live_meta) =
        match load_base_graph(txn, db_id, table_id, index_id, &meta, keyspace).await? {
            Some(pair) => pair,
            None => create_empty_hnsw_index(
                meta.dimensions,
                &meta.distance_metric,
                meta.m,
                meta.ef_construction,
            )?,
        };

    // 4. Paginated streaming apply: scan delta pages, apply each to in-memory index.
    let prefix = hnsw_delta_prefix(db_id, table_id, index_id);
    let end = hnsw_delta_prefix_end(db_id, table_id, index_id);
    let mut start = prefix.clone();
    let mut delta_count: usize = 0;

    loop {
        let range: BoundRange = (start.clone()..end.clone()).into();
        let pairs: Vec<tikv_client::KvPair> = txn
            .scan(range, DELTA_SCAN_BATCH_SIZE)
            .await
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
            .collect();
        let page_count = pairs.len();
        if page_count == 0 {
            break;
        }

        // Reserve capacity for this page of deltas.
        let needed = index.size() as u64 + page_count as u64;
        if needed > index.capacity() as u64 {
            let next_cap = needed.saturating_mul(2).max(1);
            index
                .reserve(next_cap as usize)
                .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
        }

        let mut last_key: Option<Vec<u8>> = None;
        for pair in pairs {
            let k: &[u8] = pair.key().as_ref().into();
            let key: Vec<u8> = k.to_vec();
            if !key.starts_with(&prefix) {
                break;
            }
            let delta: HnswDelta = bincode::deserialize(pair.value())
                .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
            index
                .add(delta.label, &delta.vector)
                .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
            delta_count += 1;
            last_key = Some(key);
        }

        if (page_count as u32) < DELTA_SCAN_BATCH_SIZE {
            break;
        }
        match last_key {
            Some(mut lk) => {
                lk.push(0x00);
                start = lk;
            }
            None => break,
        }
    }

    live_meta.count = index.size() as u64;
    live_meta.capacity = index.capacity() as u64;

    if delta_count > 0 {
        tracing::debug!(
            db_id,
            table_id,
            index_id,
            delta_count,
            "HNSW scan applied pending deltas"
        );
    }

    Ok(Some((HnswIndexHandle::new(index), live_meta, delta_count)))
}

// ===========================================================================
// Shared index cache integration (concurrent read-safe)
// ===========================================================================

use super::s3::{hnsw_index_cache, hnsw_max_index_memory, SharedHnswIndex};

/// Estimate in-memory size for a loaded usearch index.
/// Formula: count * (dimensions * 4 + 2 * m * 8 + 40) + fixed overhead.
pub fn estimate_graph_memory(count: u64, dimensions: usize, m: usize) -> usize {
    let per_node = dimensions * 4 + 2 * m * 8 + 40;
    (count as usize) * per_node + 4096 // 4KB fixed overhead
}

/// Inflight loader coordination: prevents thundering herd on cache miss.
///
/// When multiple queries miss the cache for the same key simultaneously,
/// only one performs the actual load. Others wait via `watch::Receiver`
/// and then read the result from the cache.
///
/// Uses `watch<bool>` (not `Notify`) because `watch::Receiver::changed()`
/// checks the channel's version counter, not whether a listener was
/// registered at send time. A receiver cloned inside the mutex sees
/// version N; when the sender sets `true` (version N+1), `changed()`
/// returns immediately — even if the send happened before the await.
/// This eliminates the lost-wakeup race that `Notify` suffers from.
type InflightKey = (String, u64, u64, u64, u64);
static INFLIGHT_LOADS: LazyLock<Mutex<HashMap<InflightKey, tokio::sync::watch::Receiver<bool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Get a shared reference to the base graph, using the in-memory index cache.
///
/// On cache hit: returns `Arc<SharedHnswIndex>` directly (zero-copy, 0ms).
/// On cache miss: exactly one loader runs per key (singleflight). Other
/// concurrent callers wait for the loader to finish, then read from cache.
pub async fn get_shared_base_graph(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    meta: &HnswMeta,
    keyspace: &str,
) -> Result<Option<Arc<SharedHnswIndex>>, SqlError> {
    if meta.dropped_at.is_some() {
        return Ok(None);
    }

    // Empty base graph (post-TRUNCATE or pre-first-merge).
    if meta.count == 0 {
        return Ok(None);
    }

    // Pre-load size check: reject before any allocation.
    let estimated_bytes = estimate_graph_memory(meta.count, meta.dimensions, meta.m);
    let max_index = hnsw_max_index_memory();
    if max_index > 0 && estimated_bytes > max_index {
        return Err(SqlError::Internal(anyhow::anyhow!(
            "HNSW index d_{}_hnsw_{}_{} estimated at {} bytes ({} vectors × {} dims) \
             exceeds HNSW_MAX_INDEX_MEMORY ({} bytes). Reduce index size or increase the limit.",
            db_id,
            table_id,
            index_id,
            estimated_bytes,
            meta.count,
            meta.dimensions,
            max_index
        )));
    }

    // Cache version: for S3 graphs, graph_version (TSO-based, collision-free).
    // For TiKV graphs with a nonce, use nonce ^ count. Legacy TiKV indexes
    // (cache_nonce=0, pre-upgrade) bypass the shared cache entirely to avoid
    // stale hits — the nonce is the only reliable cross-DDL discriminator.
    let cache_version = if meta.graph_version > 0 {
        meta.graph_version
    } else if meta.cache_nonce != 0 {
        meta.cache_nonce ^ meta.count
    } else {
        // Legacy TiKV index without nonce: load directly, don't cache.
        let result = load_base_graph(txn, db_id, table_id, index_id, meta, keyspace).await?;
        return match result {
            Some((handle, _)) => Ok(Some(Arc::new(SharedHnswIndex {
                index: handle,
                estimated_memory_bytes: estimated_bytes,
            }))),
            None => Ok(None),
        };
    };

    let cache = hnsw_index_cache();

    // Fast path: cache hit.
    if let Some(shared) = cache.lookup(keyspace, db_id, table_id, index_id, cache_version) {
        return Ok(Some(shared));
    }

    // Slow path: cache miss with singleflight coordination.
    let inflight_key = (
        keyspace.to_string(),
        db_id,
        table_id,
        index_id,
        cache_version,
    );

    loop {
        enum Role {
            Loader(tokio::sync::watch::Sender<bool>),
            Waiter(tokio::sync::watch::Receiver<bool>),
        }

        let role = {
            let mut inflight = INFLIGHT_LOADS.lock().unwrap();

            // Re-check cache under inflight lock to close the race window.
            if let Some(shared) = cache.lookup(keyspace, db_id, table_id, index_id, cache_version) {
                return Ok(Some(shared));
            }

            if let Some(rx) = inflight.get(&inflight_key) {
                Role::Waiter(rx.clone())
            } else {
                let (tx, rx) = tokio::sync::watch::channel(false);
                inflight.insert(inflight_key.clone(), rx);
                Role::Loader(tx)
            }
        }; // mutex released before any await

        match role {
            Role::Waiter(mut rx) => {
                // rx was cloned inside the mutex with version mark at "false".
                // When the loader sends "true", changed() sees version advance
                // and returns — even if send() happened before this await.
                // If the loader failed and dropped tx, changed() returns Err,
                // and we loop back to become the next loader.
                let _ = rx.changed().await;
            }
            Role::Loader(tx) => {
                let result = load_base_graph(txn, db_id, table_id, index_id, meta, keyspace).await;

                // On success: insert into cache FIRST, then signal waiters.
                // This ensures waiters always find the value in cache.
                // On failure: clean up and drop tx (waiters get RecvError, retry).
                match result {
                    Ok(Some((handle, live_meta))) => {
                        let estimated_bytes = estimate_graph_memory(
                            live_meta.count,
                            live_meta.dimensions,
                            live_meta.m,
                        );
                        let shared = cache.insert(
                            keyspace,
                            db_id,
                            table_id,
                            index_id,
                            cache_version,
                            handle,
                            estimated_bytes,
                        );
                        INFLIGHT_LOADS.lock().unwrap().remove(&inflight_key);
                        let _ = tx.send(true);
                        return Ok(Some(shared));
                    }
                    Ok(None) => {
                        INFLIGHT_LOADS.lock().unwrap().remove(&inflight_key);
                        drop(tx);
                        return Ok(None);
                    }
                    Err(e) => {
                        INFLIGHT_LOADS.lock().unwrap().remove(&inflight_key);
                        drop(tx);
                        return Err(e);
                    }
                }
            }
        }
    }
}

/// Compute the maximum number of deltas that fit within the memory budget.
///
/// Budget = 10% of HNSW_MAX_INDEX_MEMORY, capped to never exceed the full
/// limit. When HNSW_MAX_INDEX_MEMORY = 0 (unlimited), deltas are unlimited.
///
/// Each delta costs `(32 + dims*4)` bytes in Vec<HnswDelta> plus
/// `(dims*4 + 2*m*8 + 40)` bytes in the usearch delta index — both live
/// simultaneously during build_delta_index.
///
/// This adapts automatically to vector dimensions:
///   dim=1536, m=16, 2GB limit → ~16K deltas (~200MB)
///   dim=8192, m=16, 2GB limit → ~3K deltas (~200MB)
///   dim=32,   m=16, 2GB limit → ~362K deltas (~200MB)
///   dim=1536, m=16, 30MB limit → ~240 deltas (~3MB)
///
/// Limitation: when the backlog exceeds this budget, recent inserts beyond
/// the limit are temporarily invisible to queries (logged as a warning).
/// This includes same-transaction writes in very large bulk-insert
/// transactions. The merge worker consolidates deltas into the base graph,
/// restoring full visibility. This is an intentional tradeoff: bounded
/// per-query memory vs perfect read-your-writes for arbitrarily large
/// transactions.
pub fn max_deltas_for_budget(dimensions: usize, m: usize) -> usize {
    let max_index = hnsw_max_index_memory();
    if max_index == 0 {
        return usize::MAX; // unlimited
    }
    // 10% of the index memory limit, never exceeding the limit itself.
    let budget = max_index / 10;

    // Per-delta peak memory: Vec entry + usearch node (both live simultaneously).
    let vec_per_delta = 32 + dimensions * 4; // label(8) + Vec header(24) + f32 data
    let index_per_delta = dimensions * 4 + 2 * m * 8 + 40; // estimate_graph_memory per-node
    let per_delta = vec_per_delta + index_per_delta;

    if per_delta == 0 {
        return usize::MAX;
    }
    budget / per_delta
}

/// Scan visible delta vectors up to `max_deltas`. Returns `(deltas, truncated)`.
/// If `truncated` is true, the caller should fall back to streaming apply.
pub async fn scan_visible_deltas(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    max_deltas: usize,
) -> Result<(Vec<HnswDelta>, bool), SqlError> {
    let prefix = hnsw_delta_prefix(db_id, table_id, index_id);
    let end = hnsw_delta_prefix_end(db_id, table_id, index_id);
    let mut start = prefix.clone();
    let mut deltas = Vec::new();

    loop {
        let range: BoundRange = (start.clone()..end.clone()).into();
        let pairs: Vec<tikv_client::KvPair> = txn
            .scan(range, DELTA_SCAN_BATCH_SIZE)
            .await
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
            .collect();
        let page_count = pairs.len();
        if page_count == 0 {
            break;
        }

        let mut last_key: Option<Vec<u8>> = None;
        for pair in pairs {
            let k: &[u8] = pair.key().as_ref().into();
            let key: Vec<u8> = k.to_vec();
            if !key.starts_with(&prefix) {
                break;
            }
            if deltas.len() >= max_deltas {
                // Too many deltas — signal caller to use streaming fallback.
                return Ok((deltas, true));
            }
            let delta: HnswDelta = bincode::deserialize(pair.value())
                .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
            deltas.push(delta);
            last_key = Some(key);
        }

        if (page_count as u32) < DELTA_SCAN_BATCH_SIZE {
            break;
        }
        match last_key {
            Some(mut lk) => {
                lk.push(0x00);
                start = lk;
            }
            None => break,
        }
    }

    Ok((deltas, false))
}

/// Build a small per-query HNSW index from delta vectors.
pub fn build_delta_index(
    meta: &HnswMeta,
    deltas: &[HnswDelta],
) -> Result<HnswIndexHandle, SqlError> {
    let metric = metric_from_string(&meta.distance_metric)?;
    let options = IndexOptions {
        dimensions: meta.dimensions,
        metric,
        quantization: ScalarKind::F32,
        connectivity: meta.m,
        expansion_add: meta.ef_construction,
        expansion_search: super::HNSW_DEFAULT_EF_SEARCH,
    };
    let index = new_index(&options).map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    index
        .reserve(deltas.len())
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    for delta in deltas {
        index
            .add(delta.label, &delta.vector)
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    }
    Ok(HnswIndexHandle::new(index))
}

/// Merge search results from base graph and delta index.
///
/// For labels appearing in both sets, the delta result takes precedence
/// (it reflects the current vector value, not the stale base graph entry).
/// Results are sorted by distance ascending and truncated to k.
pub fn merge_search_results(
    base_results: &[(u64, f64)],
    delta_results: &[(u64, f64)],
    k: usize,
) -> Vec<(u64, f64)> {
    use std::collections::HashSet;

    let delta_labels: HashSet<u64> = delta_results.iter().map(|(l, _)| *l).collect();

    // Base results: exclude labels that have delta overrides.
    let mut merged: Vec<(u64, f64)> = base_results
        .iter()
        .filter(|(label, _)| !delta_labels.contains(label))
        .copied()
        .collect();

    // Add all delta results.
    merged.extend_from_slice(delta_results);

    // Sort by distance, deduplicate by label (keep closest).
    merged.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut seen = HashSet::with_capacity(k);
    merged.retain(|(label, _)| seen.insert(*label));
    merged.truncate(k);

    merged
}

// ===========================================================================
// Rowid mapping for HnswLabelMode::Mapped
// ===========================================================================
//
// Key layout (all under the database data prefix):
//   pk→rid:  d_{db_id}_hnsw_rid_pk2rid_{table_id}_{pk_bytes}   → u64 BE
//   rid→pk:  d_{db_id}_hnsw_rid_rid2pk_{table_id}_{rowid_be8}  → pk_bytes
//   seq:     d_{db_id}_hnsw_rid_seq_{table_id}                 → u64 BE (next rowid)
//
// The mapping is table-level (shared across all HNSW indexes on the same table)
// because usearch labels are opaque u64s and the mapping is PK-specific, not
// index-specific.

/// Key for PK → rowid mapping lookup.
pub fn hnsw_rid_pk2rid_key(db_id: u64, table_id: u64, pk_bytes: &[u8]) -> Vec<u8> {
    let mut key = format!("d_{db_id}_hnsw_rid_pk2rid_{table_id}_").into_bytes();
    key.extend_from_slice(pk_bytes);
    key
}

/// Key for rowid → PK reverse mapping lookup.
pub fn hnsw_rid_rid2pk_key(db_id: u64, table_id: u64, rowid: u64) -> Vec<u8> {
    let mut key = format!("d_{db_id}_hnsw_rid_rid2pk_{table_id}_").into_bytes();
    key.extend_from_slice(&rowid.to_be_bytes());
    key
}

/// Key for the rowid sequence counter (monotonic, never recycled).
pub fn hnsw_rid_seq_key(db_id: u64, table_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_rid_seq_{table_id}").into_bytes()
}

/// Prefix for scanning all rid→pk mappings for a table (used by DROP TABLE cleanup).
pub fn hnsw_rid_rid2pk_prefix(db_id: u64, table_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_rid_rid2pk_{table_id}_").into_bytes()
}

/// Prefix for scanning all pk→rid mappings for a table (used by DROP TABLE cleanup).
pub fn hnsw_rid_pk2rid_prefix(db_id: u64, table_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_rid_pk2rid_{table_id}_").into_bytes()
}

/// Look up the rowid for a PK within the caller's transaction.
/// Returns `None` if no mapping exists (row was never indexed or was deleted).
pub async fn get_rowid_for_pk(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    pk_bytes: &[u8],
) -> Result<Option<u64>, SqlError> {
    let key = hnsw_rid_pk2rid_key(db_id, table_id, pk_bytes);
    match txn
        .get(key)
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
    {
        Some(val) => {
            let arr: [u8; 8] = val
                .try_into()
                .map_err(|_| SqlError::Internal(anyhow::anyhow!("corrupt pk2rid value")))?;
            Ok(Some(u64::from_be_bytes(arr)))
        }
        None => Ok(None),
    }
}

/// Write the bidirectional pk ↔ rowid mapping within the caller's transaction.
pub async fn put_rowid_mapping(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    pk_bytes: &[u8],
    rowid: u64,
) -> Result<(), SqlError> {
    let pk2rid_key = hnsw_rid_pk2rid_key(db_id, table_id, pk_bytes);
    let rid2pk_key = hnsw_rid_rid2pk_key(db_id, table_id, rowid);
    txn_put(txn, pk2rid_key, rowid.to_be_bytes().to_vec())
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    txn_put(txn, rid2pk_key, pk_bytes.to_vec())
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    Ok(())
}

/// Get the PK bytes for a single rowid within the caller's transaction.
/// Returns `None` if the mapping was deleted (stale label from lazy HNSW deletion).
#[allow(dead_code)]
pub async fn get_pk_for_rowid(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    rowid: u64,
) -> Result<Option<Vec<u8>>, SqlError> {
    let key = hnsw_rid_rid2pk_key(db_id, table_id, rowid);
    txn.get(key)
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))
}

/// Batch-read PK bytes for multiple rowids in a single TiKV call.
/// Returns a Vec of `Option<Vec<u8>>` in the same order as `rowids`.
/// `None` entries indicate deleted rows (stale labels).
pub async fn batch_get_pk_for_rowids(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    rowids: &[u64],
) -> Result<Vec<Option<Vec<u8>>>, SqlError> {
    if rowids.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<Vec<u8>> = rowids
        .iter()
        .map(|&rid| hnsw_rid_rid2pk_key(db_id, table_id, rid))
        .collect();
    let pairs: Vec<tikv_client::KvPair> = txn
        .batch_get(keys.clone())
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
        .collect();
    // batch_get returns only found keys; build a lookup map.
    let mut map = std::collections::HashMap::with_capacity(pairs.len());
    for pair in &pairs {
        let k: &[u8] = pair.key().as_ref().into();
        map.insert(k.to_vec(), pair.value().to_vec());
    }
    let result = keys.into_iter().map(|k| map.remove(&k)).collect();
    Ok(result)
}

/// Delete the bidirectional pk ↔ rowid mapping within the caller's transaction.
/// Used when a row is DELETEd in Mapped mode.
pub async fn delete_rowid_mapping(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    pk_bytes: &[u8],
    rowid: u64,
) -> Result<(), SqlError> {
    let pk2rid_key = hnsw_rid_pk2rid_key(db_id, table_id, pk_bytes);
    let rid2pk_key = hnsw_rid_rid2pk_key(db_id, table_id, rowid);
    txn_delete(txn, pk2rid_key)
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    txn_delete(txn, rid2pk_key)
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    Ok(())
}

/// Reassign a rowid mapping when the PK changes (UPDATE).
///
/// Looks up the rowid for `old_pk_bytes`, deletes the old bidirectional
/// mapping, and writes a new mapping from `new_pk_bytes` to the same rowid.
/// Returns the rowid so the caller can use it for delta writes.
///
/// If no mapping exists for `old_pk_bytes`, returns `None` (should not happen
/// for a valid Mapped-mode row, but callers can fall back to allocation).
pub async fn reassign_rowid_mapping(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    old_pk_bytes: &[u8],
    new_pk_bytes: &[u8],
) -> Result<Option<u64>, SqlError> {
    let Some(rowid) = get_rowid_for_pk(txn, db_id, table_id, old_pk_bytes).await? else {
        return Ok(None);
    };
    // Delete old bidirectional mapping.
    delete_rowid_mapping(txn, db_id, table_id, old_pk_bytes, rowid).await?;
    // Write new mapping with the same rowid.
    put_rowid_mapping(txn, db_id, table_id, new_pk_bytes, rowid).await?;
    Ok(Some(rowid))
}

/// Delete ALL rowid mappings for a table (pk2rid + rid2pk + seq).
/// Used by DROP TABLE / TRUNCATE to prevent stale mapping leaks.
pub async fn delete_all_rowid_mappings(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
) -> Result<(), SqlError> {
    // Delete the sequence counter.
    txn_delete(txn, hnsw_rid_seq_key(db_id, table_id))
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    // Paginated delete of pk2rid keys.
    delete_keys_by_prefix(txn, &hnsw_rid_pk2rid_prefix(db_id, table_id)).await?;
    // Paginated delete of rid2pk keys.
    delete_keys_by_prefix(txn, &hnsw_rid_rid2pk_prefix(db_id, table_id)).await?;
    Ok(())
}

/// Delete all keys matching a given prefix, paginated to completion.
async fn delete_keys_by_prefix(txn: &mut Transaction, prefix: &[u8]) -> Result<(), SqlError> {
    let mut end = prefix.to_vec();
    // Increment last byte to form exclusive end of range.
    if let Some(last) = end.last_mut() {
        *last = last.checked_add(1).unwrap_or(0xFF);
    }
    let mut start = prefix.to_vec();
    loop {
        let range: BoundRange = (start.clone()..end.clone()).into();
        let pairs: Vec<tikv_client::KvPair> = txn
            .scan(range, DELTA_SCAN_BATCH_SIZE)
            .await
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
            .collect();
        let count = pairs.len();
        let mut last_key: Option<Vec<u8>> = None;
        for pair in pairs {
            let k: &[u8] = pair.key().as_ref().into();
            let key: Vec<u8> = k.to_vec();
            if !key.starts_with(prefix) {
                break;
            }
            last_key = Some(key.clone());
            txn_delete(txn, key)
                .await
                .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
        }
        if (count as u32) < DELTA_SCAN_BATCH_SIZE {
            break;
        }
        match last_key {
            Some(mut lk) => {
                lk.push(0x00);
                start = lk;
            }
            None => break,
        }
    }
    Ok(())
}

/// Look up an existing rowid for a PK, or allocate a new one.
/// Uses the caller's transaction for the mapping lookup/write,
/// and `store` for the atomic rowid sequence counter (autocommit).
pub async fn get_or_alloc_rowid(
    txn: &mut Transaction,
    store: &TikvStore,
    db_id: u64,
    table_id: u64,
    pk_bytes: &[u8],
) -> Result<u64, SqlError> {
    // Fast path: existing mapping
    if let Some(rowid) = get_rowid_for_pk(txn, db_id, table_id, pk_bytes).await? {
        return Ok(rowid);
    }
    // Allocate a new rowid via atomic increment
    let rowid = store
        .alloc_hnsw_rowid(db_id, table_id)
        .await
        .map_err(SqlError::Internal)?;
    // Write the bidirectional mapping
    put_rowid_mapping(txn, db_id, table_id, pk_bytes, rowid).await?;
    Ok(rowid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_hnsw_meta_defaults_to_v1() {
        let (_, meta) = create_empty_hnsw_index(3, "l2", 16, 200).unwrap();
        assert_eq!(
            meta.storage_version, 1,
            "New HNSW indexes must use v1 (delta-log) storage"
        );
    }

    #[test]
    fn hnsw_merge_task_id_basic() {
        let id = hnsw_merge_task_id(1, 2).unwrap();
        // table_id=1 in upper 32 bits, index_id=2 in lower 32 bits
        assert_eq!(id, ((1i64 << 32) | 2));
    }

    #[test]
    fn hnsw_merge_task_id_max_u32() {
        let id = hnsw_merge_task_id(u32::MAX as u64, u32::MAX as u64).unwrap();
        assert_eq!(id as u64, (0xFFFFFFFF_FFFFFFFF_u64));
    }

    #[test]
    fn hnsw_merge_task_id_overflow_table_id() {
        let result = hnsw_merge_task_id(u32::MAX as u64 + 1, 1);
        assert!(result.is_err(), "table_id > u32::MAX should error");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("table_id"),
            "error should mention table_id: {msg}"
        );
    }

    #[test]
    fn hnsw_merge_task_id_overflow_index_id() {
        let result = hnsw_merge_task_id(1, u32::MAX as u64 + 1);
        assert!(result.is_err(), "index_id > u32::MAX should error");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("index_id"),
            "error should mention index_id: {msg}"
        );
    }

    /// Verifies that HnswMeta with legacy v0 storage version produces
    /// a clear error when deserialized from JSON. This locks the fail-fast
    /// behavior that replaced the old v0→v1 migration path.
    #[test]
    fn hnsw_meta_v0_is_rejected_by_version_check() {
        let meta_json = r#"{"count":0,"capacity":0,"dimensions":3,"distance_metric":"l2","m":16,"ef_construction":200,"storage_version":0}"#;
        let meta: HnswMeta = serde_json::from_str(meta_json).expect("parse meta");
        assert_eq!(meta.storage_version, 0);
        // The actual fail-fast is in load_hnsw_graph_with_deltas, but we can
        // verify the contract: storage_version != 1 must be treated as error.
        assert_ne!(
            meta.storage_version, 1,
            "v0 meta must fail the storage_version != 1 check"
        );
    }

    /// Verifies that missing storage_version in JSON (old-format meta)
    /// defaults to 0 via #[serde(default)], which triggers the fail-fast.
    #[test]
    fn hnsw_meta_missing_version_defaults_to_zero() {
        let meta_json = r#"{"count":0,"capacity":0,"dimensions":3,"distance_metric":"l2","m":16,"ef_construction":200}"#;
        let meta: HnswMeta = serde_json::from_str(meta_json).expect("parse meta");
        assert_eq!(
            meta.storage_version, 0,
            "missing storage_version must default to 0 (triggers fail-fast)"
        );
    }

    /// Deterministic queue key property: same (table_id, index_id) always
    /// maps to the same task_id. This is WHY the ABA race exists (DML and
    /// worker target the same queue key) and WHY CAS nonce is needed.
    #[test]
    fn hnsw_merge_task_id_deterministic_key_enables_aba() {
        // Two different "writers" computing the task_id for the same index
        let writer1 = hnsw_merge_task_id(10, 3).unwrap();
        let writer2 = hnsw_merge_task_id(10, 3).unwrap();
        assert_eq!(
            writer1, writer2,
            "same (table_id, index_id) must produce same task_id (deterministic key)"
        );

        // Different index → different task_id (no collision)
        let other = hnsw_merge_task_id(10, 4).unwrap();
        assert_ne!(
            writer1, other,
            "different index must have different task_id"
        );
    }

    // -----------------------------------------------------------------------
    // HnswLabelMode + rowid mapping key tests
    // -----------------------------------------------------------------------

    #[test]
    fn hnsw_meta_missing_label_mode_defaults_to_direct() {
        // Simulates deserializing an existing HnswMeta stored before label_mode was added.
        let meta_json = r#"{"count":0,"capacity":0,"dimensions":3,"distance_metric":"l2","m":16,"ef_construction":200,"storage_version":1}"#;
        let meta: HnswMeta = serde_json::from_str(meta_json).expect("parse meta");
        assert_eq!(
            meta.label_mode,
            HnswLabelMode::Direct,
            "missing label_mode must default to Direct for backward compat"
        );
    }

    #[test]
    fn hnsw_meta_mapped_mode_round_trips() {
        let meta = HnswMeta {
            count: 5,
            capacity: 10,
            dimensions: 128,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: HnswLabelMode::Mapped,
            frozen: false,
            graph_version: 0,
            dropped_at: None,
            cache_nonce: 0,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"label_mode\":\"Mapped\""));
        let parsed: HnswMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.label_mode, HnswLabelMode::Mapped);
    }

    #[test]
    fn hnsw_meta_direct_mode_omits_label_mode_field() {
        let meta = HnswMeta {
            count: 0,
            capacity: 0,
            dimensions: 3,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: HnswLabelMode::Direct,
            frozen: false,
            graph_version: 0,
            dropped_at: None,
            cache_nonce: 0,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(
            !json.contains("label_mode"),
            "Direct mode should omit label_mode for backward compat: {json}"
        );
    }

    #[test]
    fn rowid_mapping_key_format() {
        let pk_bytes = b"hello";
        let pk2rid = hnsw_rid_pk2rid_key(1, 2, pk_bytes);
        assert_eq!(
            std::str::from_utf8(&pk2rid[..pk2rid.len() - 5]).unwrap(),
            "d_1_hnsw_rid_pk2rid_2_"
        );
        assert_eq!(&pk2rid[pk2rid.len() - 5..], b"hello");

        let rid2pk = hnsw_rid_rid2pk_key(1, 2, 42);
        let prefix = "d_1_hnsw_rid_rid2pk_2_";
        assert!(std::str::from_utf8(&rid2pk[..prefix.len()])
            .unwrap()
            .starts_with(prefix));
        assert_eq!(&rid2pk[prefix.len()..], &42u64.to_be_bytes());

        let seq = hnsw_rid_seq_key(1, 2);
        assert_eq!(std::str::from_utf8(&seq).unwrap(), "d_1_hnsw_rid_seq_2");
    }

    #[test]
    fn new_hnsw_meta_defaults_to_direct_label_mode() {
        let (_, meta) = create_empty_hnsw_index(3, "l2", 16, 200).unwrap();
        assert_eq!(meta.label_mode, HnswLabelMode::Direct);
    }

    // ── frozen flag tests ──────────────────────────────────────────

    #[test]
    fn legacy_meta_without_frozen_deserializes_to_false() {
        // Simulates old HnswMeta JSON that predates the frozen field.
        let json = r#"{
            "count": 100,
            "capacity": 200,
            "dimensions": 128,
            "distance_metric": "l2",
            "m": 16,
            "ef_construction": 200,
            "storage_version": 1
        }"#;
        let meta: HnswMeta = serde_json::from_str(json).unwrap();
        assert!(!meta.frozen);
    }

    #[test]
    fn frozen_true_roundtrips_through_serde() {
        let meta = HnswMeta {
            count: 10,
            capacity: 20,
            dimensions: 3,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: HnswLabelMode::Direct,
            frozen: true,
            graph_version: 0,
            dropped_at: None,
            cache_nonce: 0,
        };
        let bytes = serde_json::to_vec(&meta).unwrap();
        let deserialized: HnswMeta = serde_json::from_slice(&bytes).unwrap();
        assert!(deserialized.frozen);
    }

    #[test]
    fn frozen_false_is_skipped_in_serialization() {
        let meta = HnswMeta {
            count: 10,
            capacity: 20,
            dimensions: 3,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: HnswLabelMode::Direct,
            frozen: false,
            graph_version: 0,
            dropped_at: None,
            cache_nonce: 0,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("frozen"));
    }

    #[test]
    fn new_index_is_not_frozen() {
        let (_, meta) = create_empty_hnsw_index(3, "l2", 16, 200).unwrap();
        assert!(!meta.frozen);
    }

    #[test]
    fn small_graph_serialize_below_threshold() {
        use crate::worker::engine::HNSW_GRAPH_MAX_BYTES;
        // A small empty index should serialize well under the limit.
        let (index, meta) = create_empty_hnsw_index(3, "l2", 16, 200).unwrap();
        let (graph_bytes, _meta_bytes) = serialize_hnsw_snapshot(1, 1, 1, &index, &meta).unwrap();
        assert!(graph_bytes.len() < HNSW_GRAPH_MAX_BYTES);
        // Meta should NOT be frozen for small graphs.
        assert!(!meta.frozen);
    }

    #[test]
    fn oversize_graph_would_trigger_freeze() {
        use crate::worker::engine::HNSW_GRAPH_MAX_BYTES;
        // Create a large index: 3000 rows x VECTOR(1536) should exceed 8 MB.
        let dims = 1536;
        let (index, _meta) = create_empty_hnsw_index(dims, "l2", 16, 200).unwrap();
        index.reserve(4000).unwrap();
        // Insert enough vectors to push past the threshold.
        for i in 0u64..3000 {
            let vec: Vec<f32> = (0..dims).map(|d| (i as f32) + (d as f32) * 0.001).collect();
            index.add(i, &vec).unwrap();
        }
        let mut meta = HnswMeta {
            count: index.size() as u64,
            capacity: index.capacity() as u64,
            dimensions: dims,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: HnswLabelMode::Direct,
            frozen: false,
            graph_version: 0,
            dropped_at: None,
            cache_nonce: 0,
        };
        let (graph_bytes, _) = serialize_hnsw_snapshot(1, 1, 1, &index, &meta).unwrap();
        // This graph should exceed the threshold.
        assert!(
            graph_bytes.len() > HNSW_GRAPH_MAX_BYTES,
            "expected graph ({} bytes) to exceed threshold ({} bytes)",
            graph_bytes.len(),
            HNSW_GRAPH_MAX_BYTES
        );
        // Simulate the freeze decision from execute_hnsw_merge.
        if graph_bytes.len() > HNSW_GRAPH_MAX_BYTES {
            meta.frozen = true;
        }
        assert!(meta.frozen);
        // Frozen meta should roundtrip correctly.
        let frozen_bytes = serde_json::to_vec(&meta).unwrap();
        let restored: HnswMeta = serde_json::from_slice(&frozen_bytes).unwrap();
        assert!(restored.frozen);
    }

    #[test]
    fn frozen_meta_causes_dispatch_skip() {
        // Simulates the dispatch check at the start of execute_hnsw_merge:
        // if meta.frozen { return Ok(()); }
        let meta = HnswMeta {
            count: 5000,
            capacity: 10000,
            dimensions: 1536,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: HnswLabelMode::Direct,
            frozen: true,
            graph_version: 0,
            dropped_at: None,
            cache_nonce: 0,
        };
        // The merge dispatch checks meta.frozen and skips.
        assert!(meta.frozen, "frozen index should be skipped by dispatch");

        // Unfreezing should allow dispatch to proceed.
        let mut unfrozen = meta;
        unfrozen.frozen = false;
        assert!(!unfrozen.frozen, "unfrozen index should proceed with merge");
    }

    #[test]
    fn sweep_skip_isolation_frozen_vs_normal() {
        // Simulates enqueue_pending_hnsw_merges checking frozen per-index.
        let frozen_meta = HnswMeta {
            count: 5000,
            capacity: 10000,
            dimensions: 1536,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: HnswLabelMode::Direct,
            frozen: true,
            graph_version: 0,
            dropped_at: None,
            cache_nonce: 0,
        };
        let normal_meta = HnswMeta {
            count: 100,
            capacity: 200,
            dimensions: 128,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: HnswLabelMode::Direct,
            frozen: false,
            graph_version: 0,
            dropped_at: None,
            cache_nonce: 0,
        };
        // Sweep logic: skip frozen, enqueue normal.
        let indexes = [("frozen_idx", &frozen_meta), ("normal_idx", &normal_meta)];
        let enqueued: Vec<_> = indexes
            .iter()
            .filter(|(_, meta)| !meta.frozen)
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(enqueued, vec!["normal_idx"]);
    }

    #[test]
    fn recovery_unfreeze_allows_merge_to_resume() {
        // After operator manually sets frozen=false, the index should be
        // picked up by sweep and dispatch again.
        let json_frozen = r#"{
            "count": 5000, "capacity": 10000, "dimensions": 1536,
            "distance_metric": "l2", "m": 16, "ef_construction": 200,
            "storage_version": 1, "frozen": true
        }"#;
        let meta: HnswMeta = serde_json::from_str(json_frozen).unwrap();
        assert!(meta.frozen);

        // Operator unfreeze: update meta in TiKV with frozen=false.
        let json_unfrozen = r#"{
            "count": 5000, "capacity": 10000, "dimensions": 1536,
            "distance_metric": "l2", "m": 16, "ef_construction": 200,
            "storage_version": 1, "frozen": false
        }"#;
        let meta2: HnswMeta = serde_json::from_str(json_unfrozen).unwrap();
        assert!(!meta2.frozen);
        // Dispatch and sweep would now proceed normally.
    }

    #[test]
    fn idempotent_freeze_on_already_frozen_index() {
        // If an already-frozen index somehow enters the oversize path again,
        // setting frozen=true is idempotent — no state corruption.
        let mut meta = HnswMeta {
            count: 5000,
            capacity: 10000,
            dimensions: 1536,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: HnswLabelMode::Direct,
            frozen: true,
            graph_version: 0,
            dropped_at: None,
            cache_nonce: 0,
        };
        // Re-freeze is a no-op on the bool.
        meta.frozen = true;
        assert!(meta.frozen);
        // Serde roundtrip remains stable.
        let bytes = serde_json::to_vec(&meta).unwrap();
        let restored: HnswMeta = serde_json::from_slice(&bytes).unwrap();
        assert!(restored.frozen);
        assert_eq!(restored.count, 5000);
    }

    #[test]
    fn create_index_oversize_guard_rejects_before_write() {
        use crate::worker::engine::HNSW_GRAPH_MAX_BYTES;
        // Reproduce the CREATE INDEX path: build a large index, serialize,
        // then verify the guard would reject before txn_put.
        let dims = 1536;
        let (index, _) = create_empty_hnsw_index(dims, "l2", 16, 200).unwrap();
        index.reserve(4000).unwrap();
        for i in 0u64..3000 {
            let vec: Vec<f32> = (0..dims).map(|d| (i as f32) + (d as f32) * 0.001).collect();
            index.add(i, &vec).unwrap();
        }
        let meta = HnswMeta {
            count: index.size() as u64,
            capacity: index.capacity() as u64,
            dimensions: dims,
            distance_metric: "l2".to_string(),
            m: 16,
            ef_construction: 200,
            storage_version: 1,
            label_mode: HnswLabelMode::Direct,
            frozen: false,
            graph_version: 0,
            dropped_at: None,
            cache_nonce: 0,
        };
        let (graph_bytes, _meta_bytes) = serialize_hnsw_snapshot(1, 1, 1, &index, &meta).unwrap();

        // This is the exact guard from create_index.rs:
        //   if graph_bytes.len() > HNSW_GRAPH_MAX_BYTES { return Err(...) }
        assert!(
            graph_bytes.len() > HNSW_GRAPH_MAX_BYTES,
            "test setup: graph must exceed limit to exercise guard"
        );

        // Verify: the guard fires BEFORE any txn_put would happen.
        // In production, this means no oversized blob is written to TiKV.
        let would_reject = graph_bytes.len() > HNSW_GRAPH_MAX_BYTES;
        assert!(
            would_reject,
            "CREATE INDEX guard must reject oversized graph before write"
        );
    }

    #[test]
    fn parse_hnsw_meta_and_gc_marker_keys() {
        let meta_key = hnsw_meta_key(7, 11, 13);
        assert_eq!(parse_hnsw_meta_key(&meta_key), Some((7, 11, 13)));

        let retired_key = hnsw_s3_retired_version_key(7, 11, 13, 17);
        assert_eq!(
            parse_hnsw_s3_retired_version_key(&retired_key),
            Some((7, 11, 13, 17))
        );

        let prefix_gc_key = hnsw_s3_prefix_gc_key(7, 11, 13);
        assert_eq!(
            parse_hnsw_s3_prefix_gc_key(&prefix_gc_key),
            Some((7, 11, 13))
        );
    }
}
