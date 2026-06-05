//! HNSW (Hierarchical Navigable Small World) vector search index.
//!
//! This module provides integration with the `usearch` crate for efficient
//! approximate nearest neighbor search on high-dimensional vectors.
//!
//! Design note: the process-level `HnswIndexCache` (in `s3`) caches loaded,
//! read-only base graphs keyed by `(keyspace, db_id, table_id, index_id,
//! graph_version)`. Version changes from merge are natural cache misses.
//! Per-query deltas (MVCC-visible inserts since last merge) are searched
//! in a small per-query delta index and merged with base graph results.
//! usearch `search()` is thread-safe (internal per-thread context pool),
//! so multiple concurrent queries share one `Arc<SharedHnswIndex>`.
//!
//! ## S3 offload (optional)
//!
//! When the `HNSW_S3_BUCKET` env var is set, serialized graph blobs can be
//! cached in S3 as a warm tier between TiKV page storage and the process-level
//! in-memory cache. See [`s3`] for the client implementation.

pub(crate) mod s3;
pub mod storage;

use anyhow::anyhow;
use tikv_client::Transaction;

use crate::model::Value;
use crate::sql::vector::{
    pgvector_cosine_distance, pgvector_l2_distance, pgvector_negative_inner_product,
};
use crate::storage::{encode_pk_values, TikvStore};

pub use storage::{metric_from_string, vec_f64_to_f32, HnswIndexHandle, HnswLabelMode, HnswMeta};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HnswDistanceMetric {
    L2,
    Cosine,
    InnerProduct,
}

impl HnswDistanceMetric {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::L2 => "l2",
            Self::Cosine => "cosine",
            Self::InnerProduct => "ip",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "l2" => Some(Self::L2),
            "cosine" | "cos" => Some(Self::Cosine),
            "ip" => Some(Self::InnerProduct),
            _ => None,
        }
    }

    pub fn from_sql_distance_function(func_name: &str) -> Option<Self> {
        if func_name.eq_ignore_ascii_case("l2_distance")
            || func_name.eq_ignore_ascii_case("vec_embed_l2_distance")
        {
            Some(Self::L2)
        } else if func_name.eq_ignore_ascii_case("cosine_distance")
            || func_name.eq_ignore_ascii_case("vec_embed_cosine_distance")
        {
            Some(Self::Cosine)
        } else if func_name.eq_ignore_ascii_case("vector_negative_inner_product")
            || func_name.eq_ignore_ascii_case("vec_embed_negative_inner_product")
        {
            Some(Self::InnerProduct)
        } else {
            None
        }
    }

    pub fn supports_deferred_embedding(func_name: &str) -> bool {
        func_name.eq_ignore_ascii_case("vec_embed_l2_distance")
            || func_name.eq_ignore_ascii_case("vec_embed_cosine_distance")
            || func_name.eq_ignore_ascii_case("vec_embed_negative_inner_product")
    }

    pub fn deferred_embedding_function(self) -> (&'static str, &'static str) {
        match self {
            Self::L2 => (
                "vec_embed_l2_distance",
                "vec_embed_l2_distance(vector, text)",
            ),
            Self::Cosine => (
                "vec_embed_cosine_distance",
                "vec_embed_cosine_distance(vector, text)",
            ),
            Self::InnerProduct => (
                "vec_embed_negative_inner_product",
                "vec_embed_negative_inner_product(vector, text)",
            ),
        }
    }

    pub fn normalize_search_distance(self, raw: f64) -> f64 {
        match self {
            // usearch returns squared Euclidean distance for MetricKind::L2Sq,
            // while SQL l2_distance() exposes Euclidean distance.
            Self::L2 => raw.max(0.0).sqrt(),
            Self::Cosine => raw,
            // usearch IP distance is (1 - dot), while pgvector's <#> distance
            // exposes the negative dot product.
            Self::InnerProduct => raw - 1.0,
        }
    }

    /// Compute exact distance between two vectors using the SQL-facing metric.
    ///
    /// Used by HNSW scan to re-rank candidates after `batch_get_rows` so that
    /// distances reflect the current row vectors, not stale graph entries.
    pub fn compute_distance(self, a: &[f64], b: &[f64]) -> f64 {
        match self {
            Self::L2 => pgvector_l2_distance(a, b),
            Self::Cosine => pgvector_cosine_distance(a, b),
            Self::InnerProduct => pgvector_negative_inner_product(a, b),
        }
    }
}

/// Default M parameter for HNSW graph connectivity.
pub const HNSW_DEFAULT_M: usize = 16;

/// Default ef_construction parameter for HNSW index building.
pub const HNSW_DEFAULT_EF_CONSTRUCTION: usize = 64;

/// Default ef parameter for HNSW search.
pub const HNSW_DEFAULT_EF_SEARCH: usize = 40;

/// Convert primary key values to a u64 label for usearch (Direct mode only).
///
/// In Direct mode, HNSW indexes require a single INTEGER or BIGINT primary
/// key. The PK value is cast directly to u64. For non-integer PKs, use
/// [`hnsw_resolve_label`] with `HnswLabelMode::Mapped`.
pub fn hnsw_pk_label(pk_values: &[Value]) -> anyhow::Result<u64> {
    if pk_values.len() != 1 {
        return Err(anyhow!(
            "HNSW index requires a single INTEGER/BIGINT primary key"
        ));
    }
    match pk_values.first() {
        Some(Value::Int32(v)) => u64::try_from(*v)
            .map_err(|_| anyhow!("HNSW index requires non-negative INTEGER primary key")),
        Some(Value::Int64(v)) => u64::try_from(*v)
            .map_err(|_| anyhow!("HNSW index requires non-negative BIGINT primary key")),
        Some(other) => Err(anyhow!(
            "HNSW index requires INTEGER/BIGINT primary key, found {}",
            other.type_display_name()
        )),
        None => Err(anyhow!("HNSW index could not read primary key value")),
    }
}

/// Resolve a PK to a usearch u64 label, dispatching on the label mode.
///
/// - `Direct`: calls [`hnsw_pk_label`] (sync, no TiKV round-trip).
/// - `Mapped`: looks up or allocates a rowid via [`storage::get_or_alloc_rowid`].
pub async fn hnsw_resolve_label(
    label_mode: HnswLabelMode,
    txn: &mut Transaction,
    store: &TikvStore,
    db_id: u64,
    table_id: u64,
    pk_values: &[Value],
) -> anyhow::Result<u64> {
    match label_mode {
        HnswLabelMode::Direct => hnsw_pk_label(pk_values),
        HnswLabelMode::Mapped => {
            let pk_bytes = encode_pk_values(pk_values);
            storage::get_or_alloc_rowid(txn, store, db_id, table_id, &pk_bytes)
                .await
                .map_err(|e| anyhow!("HNSW rowid allocation failed: {}", e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HnswDistanceMetric;
    use usearch::ffi::{new_index, IndexOptions, MetricKind, ScalarKind};

    #[test]
    fn test_hnsw_smoke() {
        let options = IndexOptions {
            dimensions: 3,
            metric: MetricKind::L2Sq,
            quantization: ScalarKind::F32,
            connectivity: super::HNSW_DEFAULT_M,
            expansion_add: super::HNSW_DEFAULT_EF_CONSTRUCTION,
            expansion_search: super::HNSW_DEFAULT_EF_SEARCH,
        };

        let index = new_index(&options).expect("Failed to create index");

        // Reserve capacity
        assert!(index.reserve(10).is_ok());
        assert!(index.capacity() >= 10);
        assert_eq!(index.size(), 0);

        // Add vectors
        let vec1: [f32; 3] = [1.0, 2.0, 3.0];
        assert!(index.add(1, &vec1).is_ok());
        assert_eq!(index.size(), 1);

        let vec2: [f32; 3] = [4.0, 5.0, 6.0];
        assert!(index.add(2, &vec2).is_ok());
        assert_eq!(index.size(), 2);

        // Search — should find exact match first
        let query: [f32; 3] = [1.0, 2.0, 3.0];
        let results = index.search(&query, 1).expect("Failed to search");
        assert_eq!(results.count, 1);
        assert_eq!(results.labels[0], 1);

        // Search for k=2 — should return both
        let results2 = index.search(&query, 2).expect("Failed to search k=2");
        assert_eq!(results2.count, 2);
        assert_eq!(results2.labels[0], 1); // closest first

        // Serialize to file and reload
        let tmp_path = "/tmp/usearch_test_hnsw_smoke.usearch";
        assert!(index.save(tmp_path).is_ok());

        let index2 = new_index(&options).expect("Failed to create second index");
        assert!(index2.load(tmp_path).is_ok());
        assert_eq!(index2.size(), 2);

        // Verify search works after reload
        let results3 = index2
            .search(&query, 1)
            .expect("Failed to search after reload");
        assert_eq!(results3.count, 1);
        assert_eq!(results3.labels[0], 1);

        // Cleanup
        let _ = std::fs::remove_file(tmp_path);
    }

    /// Regression test: usearch save/load shrinks capacity to size.
    /// The DML paths must sync meta.capacity after load to avoid
    /// skipping reserve() when the index is actually full.
    #[test]
    fn test_capacity_shrinks_after_save_load() {
        let options = IndexOptions {
            dimensions: 3,
            metric: MetricKind::L2Sq,
            quantization: ScalarKind::F32,
            connectivity: super::HNSW_DEFAULT_M,
            expansion_add: super::HNSW_DEFAULT_EF_CONSTRUCTION,
            expansion_search: super::HNSW_DEFAULT_EF_SEARCH,
        };

        let index = new_index(&options).expect("create");
        index.reserve(10).expect("reserve");
        for i in 0u64..6 {
            let v = [i as f32 * 0.1, i as f32 * 0.2, i as f32 * 0.3];
            index.add(i + 1, &v).expect("add");
        }
        assert_eq!(index.size(), 6);
        assert!(index.capacity() >= 10); // reserved 10

        let tmp = "/tmp/usearch_test_cap_shrink.usearch";
        index.save(tmp).expect("save");

        let loaded = new_index(&options).expect("create");
        loaded.load(tmp).expect("load");
        assert_eq!(loaded.size(), 6);
        // Key assertion: capacity shrinks to exactly count after load.
        // DML code must sync meta.capacity with this value.
        assert_eq!(loaded.capacity(), loaded.size());

        // After reserve, add must succeed without heap corruption
        loaded.reserve(12).expect("reserve after load");
        assert!(loaded.capacity() >= 12);
        loaded
            .add(1, &[0.1f32, 0.2, 0.3])
            .expect("add after reserve");

        let _ = std::fs::remove_file(tmp);
    }

    /// Verify usearch 0.21 add() behavior: duplicate labels APPEND
    /// (do not overwrite), causing size inflation and duplicate search
    /// results. The scan operator must deduplicate by label.
    #[test]
    fn test_duplicate_label_appends_and_search_returns_dupes() {
        let options = IndexOptions {
            dimensions: 3,
            metric: MetricKind::L2Sq,
            quantization: ScalarKind::F32,
            connectivity: super::HNSW_DEFAULT_M,
            expansion_add: super::HNSW_DEFAULT_EF_CONSTRUCTION,
            expansion_search: super::HNSW_DEFAULT_EF_SEARCH,
        };

        let index = new_index(&options).expect("create");
        index.reserve(10).expect("reserve");

        // Add label=1 with two different vectors (simulates vector UPDATE).
        let v1: [f32; 3] = [1.0, 0.0, 0.0];
        let v2: [f32; 3] = [0.0, 1.0, 0.0];
        index.add(1, &v1).expect("add first");
        index.add(1, &v2).expect("add duplicate");

        // Size grows to 2 even though there's only 1 unique label.
        assert_eq!(index.size(), 2, "add() must append, not overwrite");

        // Add a second distinct label to make results interesting.
        index.add(2, &[0.0f32, 0.0, 1.0]).expect("add label 2");
        assert_eq!(index.size(), 3);

        // Search for k=3 — should return label=1 twice and label=2 once.
        let query: [f32; 3] = [1.0, 0.0, 0.0];
        let results = index.search(&query, 3).expect("search");
        let labels: Vec<u64> = results.labels[..results.count].to_vec();

        // Confirm duplicate labels are present in raw search results.
        let label_1_count = labels.iter().filter(|&&l| l == 1).count();
        assert!(
            label_1_count >= 2,
            "search should return duplicate labels; got {:?}",
            labels
        );
    }

    #[test]
    fn l2_metric_normalizes_squared_distance_to_sql_distance() {
        assert_eq!(HnswDistanceMetric::L2.normalize_search_distance(25.0), 5.0);
        assert_eq!(
            HnswDistanceMetric::Cosine.normalize_search_distance(0.25),
            0.25
        );
        assert_eq!(
            HnswDistanceMetric::InnerProduct.normalize_search_distance(0.25),
            -0.75
        );
    }

    #[test]
    fn deferred_embedding_function_supports_inner_product() {
        let (name, signature) = HnswDistanceMetric::InnerProduct.deferred_embedding_function();
        assert_eq!(name, "vec_embed_negative_inner_product");
        assert_eq!(signature, "vec_embed_negative_inner_product(vector, text)");
    }

    // ── compute_distance tests ───────────────────────────────

    #[test]
    fn compute_distance_l2() {
        let d = HnswDistanceMetric::L2.compute_distance(&[0.0, 0.0], &[3.0, 4.0]);
        assert!((d - 5.0).abs() < 1e-10);
    }

    #[test]
    fn compute_distance_l2_identical() {
        let d = HnswDistanceMetric::L2.compute_distance(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]);
        assert!(d.abs() < 1e-10);
    }

    #[test]
    fn compute_distance_cosine_identical() {
        let d = HnswDistanceMetric::Cosine.compute_distance(&[1.0, 0.0], &[1.0, 0.0]);
        assert!(d.abs() < 1e-10);
    }

    #[test]
    fn compute_distance_cosine_orthogonal() {
        let d = HnswDistanceMetric::Cosine.compute_distance(&[1.0, 0.0], &[0.0, 1.0]);
        assert!((d - 1.0).abs() < 1e-10);
    }

    #[test]
    fn compute_distance_cosine_opposite() {
        let d = HnswDistanceMetric::Cosine.compute_distance(&[1.0, 0.0], &[-1.0, 0.0]);
        assert!((d - 2.0).abs() < 1e-10);
    }

    #[test]
    fn compute_distance_cosine_zero_vector() {
        let d = HnswDistanceMetric::Cosine.compute_distance(&[0.0, 0.0], &[1.0, 0.0]);
        assert!(d.is_nan());
    }

    #[test]
    fn compute_distance_inner_product() {
        // pgvector-compatible: negative dot product
        let d = HnswDistanceMetric::InnerProduct.compute_distance(&[1.0, 2.0], &[3.0, 4.0]);
        assert!((d - (-11.0)).abs() < 1e-10); // -(1*3 + 2*4) = -11
    }

    #[test]
    fn compute_distance_decimal_values_match_pgvector_float4_accumulation() {
        let left = [0.4_f32 as f64, 0.5_f32 as f64, 0.6_f32 as f64];
        let right = [0.1_f32 as f64, 0.2_f32 as f64, 0.3_f32 as f64];

        assert_eq!(
            HnswDistanceMetric::L2.compute_distance(&left, &right),
            0.5196152525944904
        );
        assert_eq!(
            HnswDistanceMetric::Cosine.compute_distance(&left, &right),
            0.02536811254398652
        );
        assert_eq!(
            HnswDistanceMetric::InnerProduct.compute_distance(&left, &right),
            -0.320000022649765
        );
    }
}
