//! HNSW (Hierarchical Navigable Small World) vector search index.
//!
//! This module provides integration with the `usearch` crate for efficient
//! approximate nearest neighbor search on high-dimensional vectors.
//!
//! Design note: there is intentionally no process-level HNSW graph cache.
//! Each scan/write operation loads the graph directly from TiKV, which
//! guarantees it always sees committed state. A prior cache introduced a
//! race condition where INSERT could invalidate before commit, allowing a
//! concurrent reader to repopulate with stale data that persisted permanently.
//! When a proper MVCC-aware cache is needed, it should validate freshness
//! against TiKV timestamps rather than relying on eager invalidation.

pub mod storage;

use anyhow::anyhow;

use crate::model::Value;

pub use storage::{metric_from_string, vec_f64_to_f32, HnswIndexHandle, HnswMeta};

/// Default M parameter for HNSW graph connectivity.
pub const HNSW_DEFAULT_M: usize = 16;

/// Default ef_construction parameter for HNSW index building.
pub const HNSW_DEFAULT_EF_CONSTRUCTION: usize = 64;

/// Default ef parameter for HNSW search.
pub const HNSW_DEFAULT_EF_SEARCH: usize = 40;

/// Convert primary key values to a u64 label for usearch.
///
/// HNSW indexes require a single INTEGER or BIGINT primary key (enforced
/// at CREATE INDEX time). This function is the single source of truth for
/// the PK → label conversion used by INSERT, UPDATE, and CREATE INDEX
/// backfill.
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

#[cfg(test)]
mod tests {
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
}
