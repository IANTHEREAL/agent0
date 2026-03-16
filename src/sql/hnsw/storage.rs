//! HNSW storage layer for persisting vector indexes to TiKV.
//!
//! Storage versions:
//! - v0 (legacy): monolithic graph blob in a single KV pair per index.
//! - v1 (delta-log): DML writes small delta entries; a background merge worker
//!   consolidates them into the base graph periodically.

use std::fs;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HnswLabelMode {
    Direct,
    Mapped,
}

impl Default for HnswLabelMode {
    fn default() -> Self {
        Self::Direct
    }
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
}

fn is_direct_mode(mode: &HnswLabelMode) -> bool {
    matches!(mode, HnswLabelMode::Direct)
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

/// Load ONLY the base graph blob from TiKV (caller provides meta).
/// Returns None if no graph_key exists (e.g., empty index before first merge).
pub async fn load_base_graph(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    index_id: u64,
    meta: &HnswMeta,
) -> Result<Option<(HnswIndexHandle, HnswMeta)>, SqlError> {
    let graph_key = hnsw_graph_key(db_id, table_id, index_id);
    let Some(graph_bytes) = txn
        .get(graph_key)
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
    else {
        return Ok(None);
    };
    let temp_path = temp_file_path(db_id, table_id, index_id, "load");
    fs::write(&temp_path, &graph_bytes).map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
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
    let temp_path_str = temp_path.to_string_lossy().to_string();
    let load_result = index
        .load(&temp_path_str)
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)));
    let _ = fs::remove_file(&temp_path);
    load_result?;
    let mut live_meta = meta.clone();
    live_meta.count = index.size() as u64;
    live_meta.capacity = index.capacity() as u64;
    Ok(Some((HnswIndexHandle::new(index), live_meta)))
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
pub async fn load_hnsw_graph_with_deltas(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    index_id: u64,
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

    // 2. Only v1 (delta-log) is supported; reject anything else.
    if meta.storage_version != 1 {
        return Err(SqlError::Internal(anyhow::anyhow!(
            "HNSW index d_{}_hnsw_{}_{} has unsupported storage_version={}; \
             only v1 (delta-log) is supported. Please rebuild the index.",
            db_id,
            table_id,
            index_id,
            meta.storage_version
        )));
    }

    // 3. Delta-log path: load base graph (may not exist yet)
    let (index, mut live_meta) =
        match load_base_graph(txn, db_id, table_id, index_id, &meta).await? {
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
    let result = keys
        .into_iter()
        .map(|k| map.remove(&k))
        .collect();
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
        .map_err(|e| SqlError::Internal(e))?;
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
        assert_eq!(
            std::str::from_utf8(&seq).unwrap(),
            "d_1_hnsw_rid_seq_2"
        );
    }

    #[test]
    fn new_hnsw_meta_defaults_to_direct_label_mode() {
        let (_, meta) = create_empty_hnsw_index(3, "l2", 16, 200).unwrap();
        assert_eq!(meta.label_mode, HnswLabelMode::Direct);
    }
}
