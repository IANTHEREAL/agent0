//! HNSW storage layer for persisting vector indexes to TiKV.

use std::fs;
use std::ops::Deref;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use usearch::ffi::{new_index, Index, IndexOptions, MetricKind, ScalarKind};

use tikv_client::Transaction;

use crate::sql::error::SqlError;
use crate::storage::TikvStore;
use crate::txn::txn_put;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswMeta {
    pub count: u64,
    pub capacity: u64,
    pub dimensions: usize,
    pub distance_metric: String,
    pub m: usize,
    pub ef_construction: usize,
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
    match s {
        "l2" => Ok(MetricKind::L2Sq),
        "cosine" | "cos" => Ok(MetricKind::Cos),
        "ip" => Ok(MetricKind::IP),
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

/// Load an HNSW graph from an existing transaction.
///
/// This reads graph/meta keys via `txn.get()`, which checks the transaction's
/// local write buffer before hitting TiKV. This is essential for DML
/// maintenance: in a multi-row INSERT/UPDATE, row N's graph write (via
/// `txn_put`) is visible to row N+1's `load_hnsw_graph_from_txn` call,
/// making graph updates accumulative within a single statement/txn.
pub async fn load_hnsw_graph_from_txn(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    index_id: u64,
) -> Result<Option<(HnswIndexHandle, HnswMeta)>, SqlError> {
    let graph_key = hnsw_graph_key(db_id, table_id, index_id);
    let meta_key = hnsw_meta_key(db_id, table_id, index_id);

    let Some(graph_bytes) = txn
        .get(graph_key)
        .await
        .map_err(|e| SqlError::from(anyhow::anyhow!(e)))?
    else {
        return Ok(None);
    };

    let Some(meta_bytes) = txn
        .get(meta_key)
        .await
        .map_err(|e| SqlError::from(anyhow::anyhow!(e)))?
    else {
        return Err(SqlError::Internal(anyhow::anyhow!(
            "HNSW graph exists but metadata missing for d_{}_hnsw_{}_{}",
            db_id,
            table_id,
            index_id
        )));
    };

    let meta: HnswMeta =
        serde_json::from_slice(&meta_bytes).map_err(|e| SqlError::from(anyhow::anyhow!(e)))?;

    let temp_path = temp_file_path(db_id, table_id, index_id, "load");
    fs::write(&temp_path, &graph_bytes).map_err(|e| SqlError::from(anyhow::anyhow!(e)))?;

    let metric = metric_from_string(meta.distance_metric.as_str())?;
    let options = IndexOptions {
        dimensions: meta.dimensions,
        metric,
        quantization: ScalarKind::F32,
        connectivity: meta.m,
        expansion_add: meta.ef_construction,
        expansion_search: super::HNSW_DEFAULT_EF_SEARCH,
    };

    let index = new_index(&options).map_err(|e| SqlError::from(anyhow::anyhow!(e)))?;
    let temp_path_str = temp_path.to_string_lossy().to_string();
    let load_result = index
        .load(&temp_path_str)
        .map_err(|e| SqlError::from(anyhow::anyhow!(e)));
    let _ = fs::remove_file(&temp_path);
    load_result?;

    Ok(Some((HnswIndexHandle::new(index), meta)))
}
