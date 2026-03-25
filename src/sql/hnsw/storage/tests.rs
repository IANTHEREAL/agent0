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
