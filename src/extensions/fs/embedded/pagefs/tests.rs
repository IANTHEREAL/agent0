use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncReadExt;

static TEST_KEYSPACE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct FakeBatchStatStore {
    dir_entries: HashMap<(u64, String), u64>,
    inodes: HashMap<u64, Inode>,
    lookup_batches: Vec<Vec<(u64, String)>>,
    inode_load_batches: Vec<Vec<u64>>,
    lookup_error: Option<anyhow::Error>,
    inode_errors: HashMap<u64, anyhow::Error>,
}

impl FakeBatchStatStore {
    fn new() -> Self {
        let mut store = Self::default();
        store
            .inodes
            .insert(ROOT_INODE, Inode::new_directory(ROOT_INODE, 0o755));
        store
    }

    fn link_inode(&mut self, parent_inode: u64, name: &str, inode: Inode) {
        self.dir_entries
            .insert((parent_inode, name.to_string()), inode.id);
        self.inodes.insert(inode.id, inode);
    }
}

#[test]
fn retryable_tikv_write_conflict_rejects_undetermined_outcome() {
    let key_err = tikv_client::Error::KeyError(Box::new(tikv_client::proto::kvrpcpb::KeyError {
        conflict: Some(tikv_client::proto::kvrpcpb::WriteConflict::default()),
        ..Default::default()
    }));
    let err = anyhow::anyhow!(tikv_client::Error::UndeterminedError(Box::new(key_err)));
    assert!(!is_retryable_tikv_write_conflict(&err));
}

#[async_trait]
impl BatchStatStore for FakeBatchStatStore {
    async fn load_root_inode(&mut self) -> Result<Option<Inode>> {
        Ok(self.inodes.get(&ROOT_INODE).cloned())
    }

    async fn lookup_dir_entries(
        &mut self,
        requests: &[DirLookupRequest],
    ) -> Result<HashMap<(u64, String), Option<u64>>> {
        if let Some(err) = self.lookup_error.take() {
            return Err(err);
        }
        self.lookup_batches.push(
            requests
                .iter()
                .map(|request| (request.parent_inode, request.name.clone()))
                .collect(),
        );

        let mut out = HashMap::with_capacity(requests.len());
        for request in requests {
            out.insert(
                (request.parent_inode, request.name.clone()),
                self.dir_entries
                    .get(&(request.parent_inode, request.name.clone()))
                    .copied(),
            );
        }
        Ok(out)
    }

    async fn load_inodes(
        &mut self,
        inode_ids: &[u64],
    ) -> Result<HashMap<u64, Result<Option<Inode>>>> {
        self.inode_load_batches.push(inode_ids.to_vec());
        Ok(inode_ids
            .iter()
            .copied()
            .map(|inode_id| {
                let inode = self
                    .inode_errors
                    .remove(&inode_id)
                    .map(Err)
                    .unwrap_or_else(|| Ok(self.inodes.get(&inode_id).cloned()));
                (inode_id, inode)
            })
            .collect())
    }
}

fn assert_not_found(result: &Result<Inode>, path: &str) {
    let err = result.as_ref().expect_err("path must fail");
    let fs_err = err
        .downcast_ref::<EmbeddedFsError>()
        .expect("error must be EmbeddedFsError");
    assert!(
        matches!(fs_err, EmbeddedFsError::NotFound(actual) if actual == path),
        "unexpected error: {err}"
    );
}

fn assert_not_directory(result: &Result<Inode>, part: &str) {
    let err = result.as_ref().expect_err("path must fail");
    let fs_err = err
        .downcast_ref::<EmbeddedFsError>()
        .expect("error must be EmbeddedFsError");
    assert!(
        matches!(fs_err, EmbeddedFsError::NotDirectory(actual) if actual == part),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn test_resolve_paths_batched_shares_sibling_parent_traversal() {
    let mut store = FakeBatchStatStore::new();
    store.link_inode(ROOT_INODE, "data", Inode::new_directory(2, 0o755));
    store.link_inode(2, "alpha.txt", Inode::new_file(3, 0o644));
    store.link_inode(2, "beta.txt", Inode::new_file(4, 0o644));
    store.link_inode(2, "gamma.txt", Inode::new_file(5, 0o644));

    let paths = vec![
        "/data/alpha.txt".to_string(),
        "/data/beta.txt".to_string(),
        "/data/gamma.txt".to_string(),
    ];
    let results = resolve_paths_batched(&mut store, &paths).await;

    assert_eq!(results.len(), paths.len());
    assert!(results.iter().all(|result| result.is_ok()));
    assert_eq!(store.lookup_batches.len(), 2);
    assert_eq!(
        store.lookup_batches[0],
        vec![(ROOT_INODE, "data".to_string())]
    );
    assert_eq!(
        store.lookup_batches[1],
        vec![
            (2, "alpha.txt".to_string()),
            (2, "beta.txt".to_string()),
            (2, "gamma.txt".to_string()),
        ]
    );
    assert_eq!(store.inode_load_batches, vec![vec![2], vec![3, 4, 5]]);
}

#[tokio::test]
async fn test_resolve_paths_with_ids_batched_preserves_inode_ids() {
    let mut store = FakeBatchStatStore::new();
    store.link_inode(ROOT_INODE, "data", Inode::new_directory(2, 0o755));
    store.link_inode(2, "alpha.txt", Inode::new_file(3, 0o644));

    let results = resolve_paths_with_ids_batched(
        &mut store,
        &["/".to_string(), "/data/alpha.txt".to_string()],
    )
    .await;

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].as_ref().unwrap().inode_id, ROOT_INODE);
    assert_eq!(results[1].as_ref().unwrap().inode_id, 3);
}

#[tokio::test]
async fn test_resolve_paths_batched_preserves_mixed_result_semantics() {
    let mut store = FakeBatchStatStore::new();
    store.link_inode(ROOT_INODE, "data", Inode::new_directory(2, 0o755));
    store.link_inode(2, "alpha.txt", Inode::new_file(3, 0o644));
    store.link_inode(ROOT_INODE, "note.txt", Inode::new_file(4, 0o644));

    let paths = vec![
        "/data".to_string(),
        "/data/alpha.txt".to_string(),
        "/missing".to_string(),
        "/note.txt/child".to_string(),
    ];
    let results = resolve_paths_batched(&mut store, &paths).await;

    assert!(results[0].is_ok(), "directory stat must succeed");
    assert!(results[1].is_ok(), "file stat must succeed");
    assert_not_found(&results[2], "/missing");
    assert_not_directory(&results[3], "note.txt");
    assert_eq!(store.lookup_batches.len(), 2);
    assert_eq!(
        store.lookup_batches[0],
        vec![
            (ROOT_INODE, "data".to_string()),
            (ROOT_INODE, "missing".to_string()),
            (ROOT_INODE, "note.txt".to_string()),
        ]
    );
    assert_eq!(store.lookup_batches[1], vec![(2, "alpha.txt".to_string())]);
}

#[tokio::test]
async fn test_resolve_paths_batched_collapses_deep_shared_prefixes() {
    let mut store = FakeBatchStatStore::new();
    store.link_inode(ROOT_INODE, "a", Inode::new_directory(2, 0o755));
    store.link_inode(2, "b", Inode::new_directory(3, 0o755));
    store.link_inode(3, "c", Inode::new_directory(4, 0o755));
    store.link_inode(3, "d", Inode::new_directory(5, 0o755));
    store.link_inode(4, "file1.txt", Inode::new_file(6, 0o644));
    store.link_inode(4, "file2.txt", Inode::new_file(7, 0o644));
    store.link_inode(5, "file3.txt", Inode::new_file(8, 0o644));

    let paths = vec![
        "/a/b/c/file1.txt".to_string(),
        "/a/b/c/file2.txt".to_string(),
        "/a/b/d/file3.txt".to_string(),
    ];
    let results = resolve_paths_batched(&mut store, &paths).await;

    assert!(results.iter().all(|result| result.is_ok()));
    assert_eq!(store.lookup_batches.len(), 4);
    assert_eq!(store.lookup_batches[0], vec![(ROOT_INODE, "a".to_string())]);
    assert_eq!(store.lookup_batches[1], vec![(2, "b".to_string())]);
    assert_eq!(
        store.lookup_batches[2],
        vec![(3, "c".to_string()), (3, "d".to_string())]
    );
    assert_eq!(
        store.lookup_batches[3],
        vec![
            (4, "file1.txt".to_string()),
            (4, "file2.txt".to_string()),
            (5, "file3.txt".to_string()),
        ]
    );
}

#[tokio::test]
async fn test_resolve_paths_batched_isolates_single_inode_decode_error() {
    let mut store = FakeBatchStatStore::new();
    store.link_inode(ROOT_INODE, "data", Inode::new_directory(2, 0o755));
    store.link_inode(2, "alpha.txt", Inode::new_file(3, 0o644));
    store.link_inode(2, "broken.txt", Inode::new_file(4, 0o644));
    store
        .inode_errors
        .insert(4, anyhow!("fs9: corrupt inode data for inode 4: boom."));

    let paths = vec![
        "/data/alpha.txt".to_string(),
        "/data/broken.txt".to_string(),
    ];
    let results = resolve_paths_batched(&mut store, &paths).await;

    assert!(results[0].is_ok(), "healthy sibling must still succeed");
    let err = results[1].as_ref().expect_err("broken inode must fail");
    assert!(
        err.to_string().contains("corrupt inode data for inode 4"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn test_resolve_paths_batched_preserves_resolved_entries_on_shared_lookup_failure() {
    let mut store = FakeBatchStatStore::new();
    store.lookup_error = Some(anyhow!("tikv read failed"));

    let paths = vec!["/".to_string(), "/data/alpha.txt".to_string()];
    let results = resolve_paths_batched(&mut store, &paths).await;

    assert!(
        results[0].is_ok(),
        "already-resolved root entry must stay successful"
    );
    let err = results[1]
        .as_ref()
        .expect_err("pending entry must be converted to per-entry failure");
    assert!(
        err.to_string().contains("tikv read failed"),
        "unexpected error: {err}"
    );
}

fn pending_pack(
    result_idx: usize,
    bundle_id: u64,
    bundle_offset: u64,
    len: usize,
) -> PendingBatchInlineReadPack {
    PendingBatchInlineReadPack {
        result_idx,
        bundle_id,
        bundle_offset,
        len,
    }
}

fn planned_pack_window(
    bundle_id: u64,
    start_offset: u64,
    end_offset: u64,
    entries: &[(usize, u64, usize)],
) -> PlannedBatchInlineReadPackWindow {
    PlannedBatchInlineReadPackWindow {
        bundle_id,
        start_offset,
        end_offset,
        entries: entries
            .iter()
            .map(
                |(result_idx, bundle_offset, len)| PlannedBatchInlineReadPackWindowEntry {
                    result_idx: *result_idx,
                    bundle_offset: *bundle_offset,
                    len: *len,
                },
            )
            .collect(),
    }
}

#[test]
fn test_plan_batch_inline_read_pack_windows_merges_small_gaps_within_bundle() {
    let windows = plan_batch_inline_read_pack_windows(
        vec![
            pending_pack(0, 7, 0, 8),
            pending_pack(1, 7, 10, 4),
            pending_pack(2, 7, 20, 4),
        ],
        64,
    )
    .unwrap();

    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].bundle_id, 7);
    assert_eq!(windows[0].start_offset, 0);
    assert_eq!(windows[0].end_offset, 24);
    assert_eq!(windows[0].entries.len(), 3);
    assert_eq!(windows[0].entries[0].result_idx, 0);
    assert_eq!(windows[0].entries[1].result_idx, 1);
    assert_eq!(windows[0].entries[2].result_idx, 2);
}

#[test]
fn test_plan_batch_inline_read_pack_windows_does_not_merge_across_bundle_or_large_gap() {
    let windows = plan_batch_inline_read_pack_windows(
        vec![
            pending_pack(0, 7, 0, 8),
            pending_pack(1, 7, PACK_BATCH_INLINE_READ_MERGE_GAP_BYTES + 9, 4),
            pending_pack(2, 8, 0, 4),
        ],
        128,
    )
    .unwrap();

    assert_eq!(windows.len(), 3);
    assert_eq!(windows[0].bundle_id, 7);
    assert_eq!(windows[0].start_offset, 0);
    assert_eq!(windows[0].end_offset, 8);
    assert_eq!(windows[1].bundle_id, 7);
    assert_eq!(
        windows[1].start_offset,
        PACK_BATCH_INLINE_READ_MERGE_GAP_BYTES + 9
    );
    assert_eq!(
        windows[1].end_offset,
        PACK_BATCH_INLINE_READ_MERGE_GAP_BYTES + 13
    );
    assert_eq!(windows[2].bundle_id, 8);
    assert_eq!(windows[2].start_offset, 0);
    assert_eq!(windows[2].end_offset, 4);
}

#[test]
fn test_plan_batch_inline_read_pack_windows_respects_window_cap() {
    let windows = plan_batch_inline_read_pack_windows(
        vec![pending_pack(0, 7, 0, 8), pending_pack(1, 7, 8, 8)],
        12,
    )
    .unwrap();

    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].start_offset, 0);
    assert_eq!(windows[0].end_offset, 8);
    assert_eq!(windows[1].start_offset, 8);
    assert_eq!(windows[1].end_offset, 16);
}

#[test]
fn test_split_pack_window_bytes_returns_expected_entry_payloads() {
    let window = planned_pack_window(7, 10, 20, &[(0, 10, 4), (1, 16, 4)]);
    let entries =
        EmbeddedPageFs::split_pack_window_bytes(&window, Bytes::from_static(b"abcdefghij"))
            .unwrap();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, 0);
    assert_eq!(entries[0].1, 10);
    assert_eq!(entries[0].2, b"abcd".to_vec());
    assert_eq!(entries[1].0, 1);
    assert_eq!(entries[1].1, 16);
    assert_eq!(entries[1].2, b"ghij".to_vec());
}

#[test]
fn test_split_pack_window_bytes_rejects_short_window_payload() {
    let window = planned_pack_window(7, 10, 20, &[(0, 10, 4), (1, 16, 4)]);
    let err = EmbeddedPageFs::split_pack_window_bytes(&window, Bytes::from_static(b"abcdefg"))
        .expect_err("short window payload must fail");
    assert!(
        err.to_string()
            .contains("pack read window returned an unexpected byte layout"),
        "unexpected error: {err}"
    );
}

// normalize_path tests
#[test]
fn test_normalize_path_root() {
    assert_eq!(normalize_path("/"), "/");
}

#[test]
fn test_normalize_path_empty() {
    assert_eq!(normalize_path(""), "/");
}

#[test]
fn test_normalize_path_trailing_slash() {
    assert_eq!(normalize_path("/foo/bar/"), "/foo/bar");
}

#[test]
fn test_normalize_path_no_trailing_slash() {
    assert_eq!(normalize_path("/foo/bar"), "/foo/bar");
}

#[test]
fn test_normalize_path_root_trailing_slash() {
    assert_eq!(normalize_path("/"), "/");
}

// pages_needed tests
#[test]
fn test_pages_needed_zero() {
    assert_eq!(pages_needed(0), 0);
}

#[test]
fn test_pages_needed_one_byte() {
    assert_eq!(pages_needed(1), 1);
}

#[test]
fn test_pages_needed_exact_page() {
    assert_eq!(pages_needed(16384), 1); // PAGE_SIZE = 16 * 1024
}

#[test]
fn test_pages_needed_one_over() {
    assert_eq!(pages_needed(16385), 2);
}

#[test]
fn test_pages_needed_two_pages() {
    assert_eq!(pages_needed(32768), 2);
}

#[test]
fn test_pages_needed_large() {
    assert_eq!(pages_needed(1_000_000), 62); // ceil(1000000 / 16384)
}

#[test]
fn test_pages_needed_last_byte_of_page() {
    assert_eq!(pages_needed((PAGE_SIZE - 1) as u64), 1);
}

#[test]
fn test_pages_needed_three_pages_minus_one() {
    assert_eq!(pages_needed((PAGE_SIZE as u64 * 3) - 1), 3);
}

#[test]
fn test_pages_needed_three_pages_plus_one() {
    assert_eq!(pages_needed((PAGE_SIZE as u64 * 3) + 1), 4);
}

#[test]
fn test_pages_needed_u64_max() {
    assert_eq!(pages_needed(u64::MAX), u64::MAX.div_ceil(PAGE_SIZE as u64));
}

#[test]
fn test_page_range_zero_length() {
    assert_eq!(page_range(0, 0), None);
}

#[test]
fn test_read_at_page_range_single_page() {
    assert_eq!(page_range(123, 456), Some((0, 0)));
}

#[test]
fn test_read_at_page_range_cross_boundary() {
    assert_eq!(page_range((PAGE_SIZE - 2) as u64, 4), Some((0, 1)));
}

#[test]
fn test_read_at_page_range_exact_page() {
    assert_eq!(page_range(PAGE_SIZE as u64, PAGE_SIZE as u64), Some((1, 1)));
}

#[test]
fn test_write_at_page_range_partial_first() {
    assert_eq!(
        page_range((PAGE_SIZE / 2) as u64, PAGE_SIZE as u64),
        Some((0, 1))
    );
}

#[test]
fn test_write_at_page_range_full_pages() {
    assert_eq!(
        page_range(PAGE_SIZE as u64, (PAGE_SIZE * 2) as u64),
        Some((1, 2))
    );
}

#[test]
fn test_page_range_single_byte_page_start() {
    assert_eq!(page_range((PAGE_SIZE * 3) as u64, 1), Some((3, 3)));
}

#[test]
fn test_page_range_single_byte_page_end() {
    assert_eq!(page_range((PAGE_SIZE - 1) as u64, 1), Some((0, 0)));
}

#[test]
fn test_page_range_three_pages_plus_tail() {
    assert_eq!(page_range(10, (PAGE_SIZE as u64 * 3) + 5), Some((0, 3)));
}

#[test]
fn test_page_range_large_offset() {
    let base = 1_000_000u64 * PAGE_SIZE as u64;
    assert_eq!(
        page_range(base + 7, (PAGE_SIZE as u64 * 2) + 1),
        Some((1_000_000, 1_000_002))
    );
}

#[test]
fn test_page_range_u64_max_single_byte() {
    assert_eq!(
        page_range(u64::MAX, 1),
        Some((u64::MAX / PAGE_SIZE as u64, u64::MAX / PAGE_SIZE as u64))
    );
}

#[test]
fn test_page_byte_range_single_byte_at_page_start() {
    assert_eq!(
        page_byte_range(2, (PAGE_SIZE as u64) * 2, (PAGE_SIZE as u64) * 2 + 1),
        (0, 1)
    );
}

#[test]
fn test_page_byte_range_single_byte_in_middle() {
    let start = (PAGE_SIZE as u64) * 4 + 1234;
    assert_eq!(page_byte_range(4, start, start + 1), (1234, 1235));
}

#[test]
fn test_page_byte_range_single_byte_at_page_end() {
    let end = (PAGE_SIZE as u64) * 5;
    assert_eq!(page_byte_range(4, end - 1, end), (PAGE_SIZE - 1, PAGE_SIZE));
}

#[test]
fn test_page_byte_range_exact_full_page() {
    let start = PAGE_SIZE as u64;
    let end = start + PAGE_SIZE as u64;
    assert_eq!(page_byte_range(1, start, end), (0, PAGE_SIZE));
}

#[test]
fn test_page_byte_range_first_page_of_cross_boundary() {
    let start = (PAGE_SIZE - 10) as u64;
    let end = start + 100;
    assert_eq!(page_byte_range(0, start, end), (PAGE_SIZE - 10, PAGE_SIZE));
}

#[test]
fn test_page_byte_range_second_page_of_cross_boundary() {
    let start = (PAGE_SIZE - 10) as u64;
    let end = start + 100;
    assert_eq!(page_byte_range(1, start, end), (0, 90));
}

#[test]
fn test_page_byte_range_middle_page_three_pages() {
    let start = (PAGE_SIZE as u64) * 2 + 50;
    let end = start + (PAGE_SIZE as u64 * 3) + 10;
    assert_eq!(page_byte_range(4, start, end), (0, PAGE_SIZE));
}

#[test]
fn test_page_byte_range_last_page_three_pages() {
    let start = (PAGE_SIZE as u64) * 2 + 50;
    let end = start + (PAGE_SIZE as u64 * 3) + 10;
    assert_eq!(page_byte_range(5, start, end), (0, 60));
}

#[test]
fn test_page_byte_range_large_offset() {
    let base = (PAGE_SIZE as u64) * 2_000_000;
    assert_eq!(page_byte_range(2_000_000, base + 7, base + 20), (7, 20));
}

#[test]
fn test_page_byte_range_u64_max_page() {
    let last_page = u64::MAX / PAGE_SIZE as u64;
    let page_start = last_page * PAGE_SIZE as u64;
    let file_end = page_start + 17;
    assert_eq!(
        page_byte_range(last_page, page_start + 5, file_end),
        (5, 17)
    );
}

#[test]
fn test_pages_needed() {
    assert_eq!(pages_needed(0), 0);
    assert_eq!(pages_needed(1), 1);
    assert_eq!(pages_needed((PAGE_SIZE as u64) - 1), 1);
    assert_eq!(pages_needed(PAGE_SIZE as u64), 1);
    assert_eq!(pages_needed((PAGE_SIZE as u64) + 1), 2);
}

#[test]
fn test_validate_superblock_format_accepts_current_layout() {
    let superblock = Superblock::new([1u8; 16], None);
    validate_superblock_format(&superblock).expect("current superblock must validate");
}

#[test]
fn test_validate_superblock_format_rejects_legacy_layout() {
    let superblock_json = r#"{
            "next_inode": 2,
            "next_bundle": 1
        }"#;
    let superblock = parse_superblock_bytes(superblock_json.as_bytes())
        .expect_err("legacy superblock layout must be rejected");
    assert!(
        superblock
            .to_string()
            .contains("storage format version 0 is too old"),
        "unexpected error: {superblock}"
    );
}

#[test]
fn test_validate_superblock_format_rejects_missing_instance_id() {
    let err = validate_superblock_format(&Superblock::default())
        .expect_err("zero fs_instance_id must be rejected");
    assert!(
        err.to_string().contains("missing filesystem instance id"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_validate_superblock_format_rejects_previous_v3_revision() {
    let superblock = Superblock {
        format_version: 3,
        fs_instance_id: [7u8; 16],
        object_store: None,
    };
    let err = validate_superblock_format(&superblock)
        .expect_err("previous prototype revision must be rejected");
    assert!(
        err.to_string()
            .contains("storage format version 3 is too old"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_validate_superblock_format_accepts_v4_explicitly() {
    let sb = Superblock {
        format_version: 4,
        fs_instance_id: [1u8; 16],
        object_store: None,
    };
    validate_superblock_format(&sb).expect("v4 must be accepted");
}

#[test]
fn test_validate_superblock_format_accepts_v5() {
    let sb = Superblock {
        format_version: 5,
        fs_instance_id: [1u8; 16],
        object_store: None,
    };
    validate_superblock_format(&sb).expect("v5 must be accepted");
}

#[test]
fn test_validate_superblock_format_accepts_v6() {
    let sb = Superblock {
        format_version: 6,
        fs_instance_id: [1u8; 16],
        object_store: None,
    };
    validate_superblock_format(&sb).expect("v6 must be accepted (append delta support)");
}

#[test]
fn test_validate_superblock_format_rejects_future_version_with_upgrade_message() {
    let sb = Superblock {
        format_version: 7,
        fs_instance_id: [1u8; 16],
        object_store: None,
    };
    let err = validate_superblock_format(&sb).expect_err("v7 must be rejected");
    assert!(
        err.to_string().contains("Upgrade db9-server"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_deserialize_inode_unknown_type_reports_version_skew() {
    let json = br#"{"id":42,"inode_type":"HardLink","mode":0,"size":0,"generation":1,"data":"None","atime":0,"mtime":0,"nlink":1}"#;
    let err = deserialize_inode(42, json).expect_err("unknown type must fail");
    assert!(
        err.to_string().contains("unrecognized type \"HardLink\""),
        "unexpected error: {err}"
    );
    assert!(
        err.to_string().contains("Upgrade db9-server"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_deserialize_inode_corrupt_json_reports_corruption() {
    let json = b"{not valid json at all";
    let err = deserialize_inode(99, json).expect_err("corrupt json must fail");
    assert!(
        err.to_string().contains("corrupt inode data for inode 99"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_validate_superblock_binding_rejects_binding_mismatch() {
    let persisted = Some(ObjectStoreBinding {
        bucket: "bucket-a".to_string(),
        region: Some("us-east-1".to_string()),
        endpoint: Some("https://s3-a.example.com".to_string()),
        prefix: "tenant-a".to_string(),
        force_path_style: false,
    });
    let current = Some(ObjectStoreBinding {
        bucket: "bucket-b".to_string(),
        region: Some("us-east-1".to_string()),
        endpoint: Some("https://s3-a.example.com".to_string()),
        prefix: "tenant-a".to_string(),
        force_path_style: false,
    });
    let err =
        validate_superblock_binding(&Superblock::new([7u8; 16], persisted), &current, "tenant_a")
            .expect_err("binding mismatch must be rejected");
    assert!(
        err.to_string().contains("object storage binding mismatch"),
        "unexpected error: {err}"
    );
    assert!(
        err.to_string().contains("tenant_a"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_validate_superblock_binding_accepts_exact_match() {
    let binding = Some(ObjectStoreBinding {
        bucket: "bucket-a".to_string(),
        region: Some("us-east-1".to_string()),
        endpoint: Some("https://s3-a.example.com".to_string()),
        prefix: "tenant-a".to_string(),
        force_path_style: true,
    });
    validate_superblock_binding(
        &Superblock::new([9u8; 16], binding.clone()),
        &binding,
        "tenant_a",
    )
    .expect("matching binding must validate");
}

#[test]
fn test_object_and_bundle_keys_include_fs_instance_namespace() {
    let state = FsRuntimeState {
        identity: FsInstanceIdentity::new("tenant-a".to_string(), [0xabu8; 16]),
        fs_instance_id: [0xabu8; 16],
        fs_instance_id_hex: hex::encode([0xabu8; 16]),
        object_store: Some(ObjectStoreBinding {
            bucket: "bucket-a".to_string(),
            region: Some("us-east-1".to_string()),
            endpoint: None,
            prefix: "fs9-prefix".to_string(),
            force_path_style: false,
        }),
    };

    let object_key = build_object_key("tenant-a", &state, 42).expect("object key");
    let encoded_keyspace = encode_s3_key_component("tenant-a");
    assert!(
        object_key.contains("/abababababababababababababababab/objects/"),
        "unexpected object key: {object_key}"
    );
    assert!(object_key.starts_with(&format!("fs9-prefix/{encoded_keyspace}/")));
    assert!(
        object_key.ends_with("/42"),
        "unexpected object key: {object_key}"
    );

    let bundle_key = build_bundle_key("tenant-a", &state, 9).expect("bundle key");
    assert_eq!(
        bundle_key,
        format!("fs9-prefix/{encoded_keyspace}/abababababababababababababababab/packs/9.pack")
    );
}

#[test]
fn test_pack_spool_root_is_scoped_by_storage_format_and_fs_instance() {
    let identity = FsInstanceIdentity::new("tenant-a".to_string(), [0x11u8; 16]);
    let root = pack_spool_root(&identity, &hex::encode(identity.fs_instance_id));
    let rendered = root.display().to_string();
    assert!(
        rendered.ends_with(&format!(
            "{}/format-{}/{}",
            encode_s3_key_component("tenant-a"),
            FS9_SPOOL_LAYOUT_VERSION,
            hex::encode([0x11u8; 16])
        )),
        "unexpected spool root: {rendered}"
    );
}

#[test]
fn test_register_process_identity_rejects_instance_change() {
    let keyspace = format!(
        "fs9_process_identity_test_{}_{}",
        std::process::id(),
        rand::random::<u64>()
    );
    let first = FsInstanceIdentity::new(keyspace.clone(), [1u8; 16]);
    let second = FsInstanceIdentity::new(keyspace.clone(), [2u8; 16]);

    register_process_identity(&first).expect("first identity must register");
    register_process_identity(&first).expect("same identity must re-register");

    let err = register_process_identity(&second)
        .expect_err("different instance for same keyspace must fail");
    assert!(
        err.to_string().contains("restart db9-server"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_maintenance_probe_action_is_fail_closed_on_errors() {
    assert_eq!(
        maintenance_probe_action(&Ok::<bool, ()>(true)),
        MaintenanceProbeAction::Run
    );
    assert_eq!(
        maintenance_probe_action(&Ok::<bool, ()>(false)),
        MaintenanceProbeAction::Stop
    );
    assert_eq!(
        maintenance_probe_action(&Err::<bool, ()>(())),
        MaintenanceProbeAction::Defer
    );
}

// scan_end_key tests
// rename contract: cycle detection + path normalization
#[test]
fn test_rename_cycle_detection_logic() {
    // Simulates the cycle guard: new_path starts with old_path + "/"
    let old = normalize_path("/a/b");
    let new = normalize_path("/a/b/c/d");
    assert!(
        new.starts_with(&format!("{old}/")),
        "moving /a/b into /a/b/c/d is a directory cycle"
    );
}

#[test]
fn test_rename_no_cycle_for_sibling() {
    let old = normalize_path("/a/b");
    let new = normalize_path("/a/b2");
    assert!(
        !new.starts_with(&format!("{old}/")),
        "/a/b → /a/b2 is not a cycle (sibling, not subtree)"
    );
}

#[test]
fn test_rename_no_cycle_for_parent() {
    let old = normalize_path("/a/b/c");
    let new = normalize_path("/a");
    assert!(
        !new.starts_with(&format!("{old}/")),
        "moving deeper path to shallower is not a cycle"
    );
}

#[test]
fn test_rename_same_path_noop() {
    let old = normalize_path("/foo/bar/");
    let new = normalize_path("/foo/bar");
    assert_eq!(
        old, new,
        "trailing slash normalization makes paths equal → no-op"
    );
}

#[test]
fn test_rename_root_normalized() {
    let path = normalize_path("/");
    assert_eq!(path, "/");
}

#[test]
fn test_normalize_path_collapses_repeated_slashes() {
    assert_eq!(normalize_path("/a//b"), "/a/b");
    assert_eq!(normalize_path("/a///b/c"), "/a/b/c");
    assert_eq!(normalize_path("//a//b//"), "/a/b");
    assert_eq!(normalize_path("///"), "/");
}

#[test]
fn test_cycle_guard_with_repeated_slashes() {
    // /a//b/c normalizes to /a/b/c — the cycle guard must detect
    // that moving /a/b under /a/b/c is a cycle even with repeated slashes.
    let old = normalize_path("/a/b");
    let new = normalize_path("/a//b/c");
    assert!(
        new.starts_with(&format!("{old}/")),
        "repeated slashes must not bypass cycle guard"
    );
}

#[test]
fn test_encode_s3_key_component_is_injective_for_unicode_inputs() {
    assert_ne!(
        encode_s3_key_component("db9_tenant_租户"),
        encode_s3_key_component("db9_tenant_用户")
    );
}

// ── Upload TTL / presign / lifecycle refresh tests ──────────────────

#[test]
fn test_presign_part_ttl_is_fixed_and_does_not_decay() {
    // presign_part_ttl_secs must return a fixed value from config,
    // not a decaying value derived from token expires_at.
    let ttl1 = presign_part_ttl_secs();
    let ttl2 = presign_part_ttl_secs();
    assert_eq!(ttl1, ttl2, "presign TTL must be fixed, not time-dependent");
    assert_eq!(
        ttl1,
        fs9_config().presign_ttl_secs,
        "presign TTL must equal FS9_PRESIGN_TTL_SECS config"
    );
    assert!(ttl1 > 0, "presign TTL must be positive");
}

#[test]
fn test_upload_token_ttl_is_longer_than_presign_ttl() {
    // The upload token lifetime must be longer than the per-part presign TTL
    // to allow large file uploads that take hours to complete.
    let token_ttl = fs9_config().upload_token_ttl_secs;
    let presign_ttl = fs9_config().presign_ttl_secs;
    assert!(
        token_ttl > presign_ttl,
        "upload_token_ttl_secs ({token_ttl}) must be greater than presign_ttl_secs ({presign_ttl})"
    );
    // Default: token TTL = 4h, presign TTL = 15min
    assert!(
        token_ttl >= 4 * 3600,
        "default upload_token_ttl_secs must be at least 4 hours, got {token_ttl}s"
    );
}

#[test]
fn test_upload_lifecycle_reapable_respects_extended_ttl() {
    // With extended token TTL (e.g. 4h for large uploads), the GC must
    // not reap uploads before reservation.expires_at, even if updated_at
    // is stale by more than STALE_WRITE_STREAM_SECS.
    let extended_expires_at = 4 * 3600; // 4 hours
    let reservation = UploadReservation {
        fs_instance_id: [5u8; 16],
        path: "/data/large.bin".to_string(),
        path_hash: [1u8; 32],
        expected_parent_inode: Some(ROOT_INODE),
        expected_prior_inode: None,
        expected_prior_generation: None,
        expected_size: 100 * 1024 * 1024 * 1024, // 100GB
        nonce: 7,
        expires_at: extended_expires_at,
    };

    // At t=2h, updated_at is 2h stale (> STALE_WRITE_STREAM_SECS=1h),
    // but reservation.expires_at is still in the future → not reapable.
    let now = 2 * 3600;
    let updated_at = 0;
    assert!(
        !uploading_lifecycle_is_reapable(now, updated_at, Some(&reservation)),
        "must NOT reap upload before reservation.expires_at even if updated_at is stale"
    );

    // After expires_at, it becomes reapable.
    let now = extended_expires_at + 1;
    assert!(
        uploading_lifecycle_is_reapable(now, updated_at, Some(&reservation)),
        "must reap upload after reservation.expires_at"
    );
}

#[test]
fn test_lifecycle_refresh_throttle_in_presign_path() {
    // The presign path should only write TiKV lifecycle when updated_at
    // is older than STAGING_REFRESH_INTERVAL_SECS (300s), matching the
    // streaming write path's behavior.
    let now = 1000;

    // Recently refreshed (50s ago) → should NOT trigger refresh
    let updated_at = now - 50;
    assert!(
        (now - updated_at) < STAGING_REFRESH_INTERVAL_SECS,
        "50s gap should be below the {STAGING_REFRESH_INTERVAL_SECS}s threshold"
    );

    // Stale refresh (400s ago) → should trigger refresh
    let updated_at = now - 400;
    assert!(
        (now - updated_at) >= STAGING_REFRESH_INTERVAL_SECS,
        "400s gap should meet or exceed the {STAGING_REFRESH_INTERVAL_SECS}s threshold"
    );

    // Exactly at boundary → should trigger refresh
    let updated_at = now - STAGING_REFRESH_INTERVAL_SECS;
    assert!(
        (now - updated_at) >= STAGING_REFRESH_INTERVAL_SECS,
        "exactly at threshold should trigger refresh"
    );
}

#[test]
fn test_uploading_lifecycle_is_not_reapable_before_reservation_expiry() {
    let reservation = UploadReservation {
        fs_instance_id: [5u8; 16],
        path: "/data/object.bin".to_string(),
        path_hash: [1u8; 32],
        expected_parent_inode: Some(ROOT_INODE),
        expected_prior_inode: None,
        expected_prior_generation: None,
        expected_size: 1024,
        nonce: 7,
        expires_at: STALE_WRITE_STREAM_SECS + 120,
    };

    assert!(!uploading_lifecycle_is_reapable(
        STALE_WRITE_STREAM_SECS + 1,
        0,
        Some(&reservation)
    ));
    assert!(uploading_lifecycle_is_reapable(
        reservation.expires_at + 1,
        0,
        Some(&reservation)
    ));
}

#[test]
fn test_validate_upload_claims_rejects_filesystem_instance_mismatch() {
    let claims = UploadTokenClaims {
        keyspace: "tenant-a".to_string(),
        fs_instance_id: [1u8; 16],
        staging_inode_id: 42,
        target_path_hash: hex::encode([7u8; 32]),
        expected_parent_inode: Some(ROOT_INODE),
        expected_prior_inode: Some(9),
        expected_prior_generation: Some(3),
        upload_id: "upload-1".to_string(),
        target_version: 42,
        nonce: 77,
        expires_at: 1234,
    };
    let reservation = UploadReservation {
        fs_instance_id: [2u8; 16],
        path: "/data/object.bin".to_string(),
        path_hash: [7u8; 32],
        expected_parent_inode: Some(ROOT_INODE),
        expected_prior_inode: Some(9),
        expected_prior_generation: Some(3),
        expected_size: 1024,
        nonce: 77,
        expires_at: 1234,
    };

    let err = validate_upload_claims(&claims, &reservation, Some("upload-1"))
        .expect_err("instance mismatch must be rejected");
    assert!(
        err.to_string().contains("filesystem instance mismatch"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn test_reap_stale_pack_spool_directories_removes_only_expired_siblings() {
    let root = std::env::temp_dir().join(format!(
        "db9-fs9-spool-reap-test-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let current = root.join("current");
    let stale = root.join("stale");
    let fresh = root.join("fresh");

    touch_pack_spool_heartbeat(&current, 200).await.unwrap();
    touch_pack_spool_heartbeat(&stale, 0).await.unwrap();
    touch_pack_spool_heartbeat(&fresh, 170).await.unwrap();
    fs::write(stale.join("bundle.pack"), b"bundle")
        .await
        .unwrap();

    reap_stale_pack_spool_directories(&current, 200, 60)
        .await
        .unwrap();

    assert!(fs::metadata(&current).await.is_ok());
    assert!(fs::metadata(&fresh).await.is_ok());
    assert!(fs::metadata(&stale).await.is_err());

    remove_dir_all_if_exists(&root).await.unwrap();
}

#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn test_parent_directory_generation_changes_on_child_create_and_remove() {
    let fs = make_fs().await;
    let base = "/test_parent_dir_generation";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let before = fs.stat(base).await.unwrap();
    let child = format!("{base}/child.txt");

    fs.write_file(&child, b"hello", None).await.unwrap();
    let after_create = fs.stat(base).await.unwrap();
    assert!(
        after_create.generation > before.generation,
        "creating a child must bump parent directory generation"
    );

    fs.remove(&child).await.unwrap();
    let after_remove = fs.stat(base).await.unwrap();
    assert!(
        after_remove.generation > after_create.generation,
        "removing a child must bump parent directory generation"
    );

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore = "requires TiKV / PD cluster"]
async fn test_parent_directory_generation_changes_on_cross_directory_rename() {
    let fs = make_fs().await;
    let base = "/test_parent_dir_generation_rename";
    cleanup(&fs, base).await;
    let src_dir = format!("{base}/src");
    let dst_dir = format!("{base}/dst");
    ensure_dir(&fs, &src_dir).await;
    ensure_dir(&fs, &dst_dir).await;

    let src_before = fs.stat(&src_dir).await.unwrap();
    let dst_before = fs.stat(&dst_dir).await.unwrap();

    let old_path = format!("{src_dir}/child.txt");
    let new_path = format!("{dst_dir}/child.txt");
    fs.write_file(&old_path, b"hello", None).await.unwrap();

    let src_after_create = fs.stat(&src_dir).await.unwrap();
    let dst_after_create = fs.stat(&dst_dir).await.unwrap();

    fs.rename(&old_path, &new_path).await.unwrap();

    let src_after_rename = fs.stat(&src_dir).await.unwrap();
    let dst_after_rename = fs.stat(&dst_dir).await.unwrap();

    assert!(
        src_after_create.generation > src_before.generation,
        "creating a child must bump source parent directory generation"
    );
    assert_eq!(
        dst_after_create.generation, dst_before.generation,
        "destination parent generation must not change before rename"
    );
    assert!(
        src_after_rename.generation > src_after_create.generation,
        "rename must bump the old parent directory generation"
    );
    assert!(
        dst_after_rename.generation > dst_after_create.generation,
        "rename must bump the new parent directory generation"
    );

    cleanup(&fs, base).await;
}

#[test]
fn test_scan_end_key_simple() {
    let prefix = b"_fs_D";
    let end = keys::scan_end_key(prefix);
    assert_eq!(end, b"_fs_E"); // 'D' + 1 = 'E'
}

#[test]
fn test_scan_end_key_is_exclusive_upper_bound() {
    let prefix = b"_fs_S";
    let end = keys::scan_end_key(prefix);
    // prefix < end
    assert!(prefix.to_vec() < end);
}

#[test]
fn test_scan_end_key_with_bytes() {
    let prefix = vec![0x01, 0x02, 0x03];
    let end = keys::scan_end_key(&prefix);
    assert_eq!(end, vec![0x01, 0x02, 0x04]);
}

#[test]
fn test_scan_end_key_empty_prefix_is_unbounded() {
    let end = keys::scan_end_key(b"");
    assert!(
        end.is_empty(),
        "empty prefix must produce an unbounded end key"
    );
}

#[test]
fn test_scan_end_key_all_ff_prefix_is_unbounded() {
    let prefix = vec![0xFF, 0xFF, 0xFF];
    let end = keys::scan_end_key(&prefix);
    assert!(
        end.is_empty(),
        "all-0xFF prefix has no exclusive successor and must be unbounded"
    );
}

#[test]
fn test_validate_dir_entry_name_rejects_nested_paths() {
    let err = validate_dir_entry_name("batch/a.txt").expect_err("nested path must be rejected");
    let fs_err = err
        .downcast_ref::<EmbeddedFsError>()
        .expect("expected EmbeddedFsError");
    assert!(matches!(fs_err, EmbeddedFsError::InvalidInput(_)));
}

#[test]
fn test_dir_entry_name_from_scan_key_requires_exact_prefix() {
    let prefix = keys::dir_prefix(ROOT_INODE);
    let key = keys::dir_entry_key(ROOT_INODE + 1, "hello.txt");
    assert_eq!(dir_entry_name_from_scan_key(&prefix, &key), None);
}

#[test]
fn test_dir_entry_name_from_scan_key_rejects_nested_path_suffix() {
    let prefix = keys::dir_prefix(ROOT_INODE);
    let mut key = prefix.clone();
    key.extend_from_slice(b"batch/a.txt");
    assert_eq!(dir_entry_name_from_scan_key(&prefix, &key), None);
}

#[test]
fn test_dir_entry_name_from_scan_key_accepts_exact_fs_scan_key() {
    let prefix = keys::dir_prefix(ROOT_INODE);
    let key = keys::dir_entry_key(ROOT_INODE, "hello.txt");
    assert_eq!(
        dir_entry_name_from_scan_key(&prefix, &key),
        Some("hello.txt")
    );
}

#[test]
fn test_dir_entry_name_from_scan_key_rejects_guessed_keyspace_prefix() {
    let prefix = keys::dir_prefix(ROOT_INODE);
    let mut key = vec![b'x', 0, 0, 7];
    key.extend_from_slice(&keys::dir_entry_key(ROOT_INODE, "hello.txt"));
    assert_eq!(dir_entry_name_from_scan_key(&prefix, &key), None);
}

#[test]
fn test_parse_marked_inode_id_requires_exact_prefix() {
    let prefix = keys::staging_write_prefix();
    let key = keys::staging_write_key(42);
    assert_eq!(parse_marked_inode_id(&prefix, &key), Some(42));
    assert_eq!(
        parse_marked_inode_id(&keys::orphan_inode_prefix(), &key),
        None
    );
}

#[test]
fn test_parse_marked_inode_id_rejects_guessed_keyspace_prefix() {
    let prefix = keys::staging_write_prefix();
    let key = keys::staging_write_key(42);
    let mut prefixed = vec![b'x', 0, 0, 5];
    prefixed.extend_from_slice(&key);
    assert_eq!(parse_marked_inode_id(&prefix, &prefixed), None);
}

// ── Behavioral rename tests (require TiKV) ─────────────────────────
//
// These tests exercise actual rename() calls on EmbeddedPageFs and
// verify filesystem state.  They are #[ignore] because they need a
// running TiKV cluster (PD_ENDPOINTS env var).
//
//   cargo test -p db9-server rename_behavioral -- --ignored

fn behavioral_test_keyspace() -> String {
    if let Ok(keyspace) = std::env::var("TIKV_KEYSPACE") {
        if !keyspace.trim().is_empty() {
            return keyspace;
        }
    }

    assert!(
            std::env::var("TIKV_CA_PATH").is_err(),
            "behavioral fs9 tests in TLS mode require TIKV_KEYSPACE to be set to a fresh, pre-created keyspace"
        );

    let seq = TEST_KEYSPACE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("fs9_behavioral_{}_{}", std::process::id(), seq)
}

async fn ensure_behavioral_test_keyspace(pd_addrs: &[String], keyspace: &str) {
    if std::env::var("TIKV_CA_PATH").is_ok() {
        return;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("failed to build PD HTTP client for behavioral tests");

    let mut last_err = String::new();
    for attempt in 0..15 {
        let pd = &pd_addrs[attempt % pd_addrs.len()];
        let base = format!("http://{}/pd/api/v2/keyspaces", pd);
        let keyspace_url = format!("{}/{}", base, keyspace);

        if let Ok(resp) = client
            .post(&base)
            .json(&serde_json::json!({ "name": keyspace }))
            .send()
            .await
        {
            let status = resp.status();
            if !(status.is_success() || status.as_u16() == 409 || status.as_u16() == 500) {
                let _ = resp.text().await;
            }
        }

        match client.get(&keyspace_url).send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last_err = format!(
                    "verify keyspace via {pd} status={}, body={}",
                    status.as_u16(),
                    body
                );
            }
            Err(err) => {
                last_err = format!("verify keyspace via {pd} error: {err}");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    panic!(
        "unable to provision behavioral fs9 keyspace '{}' via {:?}: {}",
        keyspace, pd_addrs, last_err
    );
}

async fn make_fs_for_keyspace(keyspace: String) -> EmbeddedPageFs {
    let pd_raw = std::env::var("PD_ENDPOINTS").unwrap_or("127.0.0.1:2379".into());
    let pd_addrs: Vec<String> = pd_raw.split(',').map(|s| s.trim().to_string()).collect();
    ensure_behavioral_test_keyspace(&pd_addrs, &keyspace).await;
    let mut config = tikv_client::Config::default().with_keyspace(&keyspace);
    if let (Ok(ca), Ok(cert), Ok(key)) = (
        std::env::var("TIKV_CA_PATH"),
        std::env::var("TIKV_CERT_PATH"),
        std::env::var("TIKV_KEY_PATH"),
    ) {
        config = config.with_security(ca, cert, key);
    }
    let client = TransactionClient::new_with_config(pd_addrs, config)
        .await
        .expect("TiKV connection required for behavioral tests");
    let client = Arc::new(client);
    let superblock = EmbeddedPageFs::load_or_init_superblock(client.clone(), &keyspace)
        .await
        .expect("load_or_init_superblock");
    let fs = EmbeddedPageFs::new(client, keyspace, &superblock);
    fs.init_filesystem().await.expect("init_filesystem");
    fs
}

async fn make_fs() -> EmbeddedPageFs {
    make_fs_for_keyspace(behavioral_test_keyspace()).await
}

/// Helper: ensure directory exists (idempotent).
async fn ensure_dir(fs: &EmbeddedPageFs, path: &str) {
    let _ = fs.mkdir(path, true, None).await;
}

/// Helper: clean up a path (file or dir) — best effort.
async fn cleanup(fs: &EmbeddedPageFs, path: &str) {
    let _ = fs.remove_recursive(path).await;
    let _ = fs.remove(path).await;
}

async fn inode_snapshot(fs: &EmbeddedPageFs, path: &str) -> serde_json::Value {
    let mut txn = fs.begin_internal().await.unwrap();
    let (_, inode) = resolve_path(&mut txn, path).await.unwrap();
    let _ = txn.rollback().await;
    serde_json::to_value(&inode).unwrap()
}

#[tokio::test]
#[ignore]
async fn test_init_filesystem_persists_binding_and_allocator_state() {
    let fs = make_fs().await;

    let mut txn = fs.begin().await.unwrap();
    let superblock = load_superblock(&mut txn).await.unwrap();
    let inode_next = load_allocator_counter(&mut txn, &keys::inode_allocator_key(), "inode")
        .await
        .unwrap();
    let bundle_next = load_allocator_counter(&mut txn, &keys::bundle_allocator_key(), "bundle")
        .await
        .unwrap();
    let _ = txn.rollback().await;

    assert_eq!(superblock.format_version, FS9_FORMAT_VERSION_DEFAULT);
    assert_ne!(superblock.fs_instance_id, [0u8; 16]);
    assert_eq!(superblock.object_store, current_object_store_binding());
    assert_eq!(inode_next, ROOT_INODE + 1);
    assert_eq!(bundle_next, 1);
}

#[tokio::test]
#[ignore]
async fn test_maintenance_probe_detects_superblock_replacement() {
    let fs = make_fs().await;
    fs.write_file("/stale.txt", b"stale", None).await.unwrap();

    let mut txn = fs.begin_unchecked().await.unwrap();
    let mut superblock = load_superblock(&mut txn).await.unwrap();
    superblock.fs_instance_id = new_fs_instance_id();
    save_superblock(&mut txn, &superblock).await.unwrap();
    txn.commit().await.unwrap();

    assert!(
        !fs.current_instance_matches_superblock().await.unwrap(),
        "maintenance probe must stop once the bound fs instance changes"
    );
}

#[tokio::test]
#[ignore]
async fn test_backend_reacquire_requires_restart_after_instance_change() {
    use crate::extensions::fs::embedded::EmbeddedFsBackend;

    let fs = make_fs().await;
    fs.write_file("/stale.txt", b"stale", None).await.unwrap();

    let mut txn = fs.begin_unchecked().await.unwrap();
    let mut superblock = load_superblock(&mut txn).await.unwrap();
    superblock.fs_instance_id = new_fs_instance_id();
    save_superblock(&mut txn, &superblock).await.unwrap();
    txn.commit().await.unwrap();

    let err = match EmbeddedFsBackend::new(fs.client.clone(), fs.keyspace.clone()).await {
        Ok(_) => panic!("backend reacquire must require restart"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("restart db9-server"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
#[ignore]
async fn test_inode_allocator_reserves_ids_without_mutating_superblock() {
    let fs = make_fs().await;

    let mut txn = fs.begin().await.unwrap();
    let before = serde_json::to_vec(&load_superblock(&mut txn).await.unwrap()).unwrap();
    let _ = txn.rollback().await;

    let first = fs.alloc_inode_id().await.unwrap();
    let second = fs.alloc_inode_id().await.unwrap();
    assert_eq!(second, first + 1);

    let mut txn = fs.begin().await.unwrap();
    let after = serde_json::to_vec(&load_superblock(&mut txn).await.unwrap()).unwrap();
    let next_inode = load_allocator_counter(&mut txn, &keys::inode_allocator_key(), "inode")
        .await
        .unwrap();
    let _ = txn.rollback().await;

    assert_eq!(before, after, "inode allocation must not rewrite _fs_S");
    assert!(
        next_inode > ROOT_INODE + 1,
        "inode allocator counter must advance independently"
    );
}

#[tokio::test]
#[ignore]
async fn test_inlineblob_behavioral_write_file_routes_to_blob() {
    let fs = make_fs().await;
    let base = "/test_inlineblob_write_file";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let inline_max = fs9_config().inline_max_bytes;
    if inline_max == 0 {
        // InlineBlob is disabled in this environment.
        return;
    }
    let path = &format!("{base}/tiny.bin");
    let data: Vec<u8> = (0..inline_max.min(32)).map(|v| (v % 251) as u8).collect();
    fs.write_file(path, &data, None).await.unwrap();

    let inode = fs.stat(path).await.unwrap();
    assert_eq!(inode.data, DataRef::InlineBlob);
    assert_eq!(inode.size, data.len() as u64);
    assert_eq!(fs.read_file(path).await.unwrap(), data);

    let mut txn = fs.begin().await.unwrap();
    assert!(
        txn.get(keys::blob_key(inode.id)).await.unwrap().is_some(),
        "InlineBlob must have a blob key"
    );
    let page_prefix = keys::page_prefix(inode.id);
    let page_end = keys::scan_end_key(&page_prefix);
    let mut pages = txn.scan(page_prefix..page_end, u32::MAX).await.unwrap();
    assert!(
        pages.next().is_none(),
        "InlineBlob must not leave staging pages behind"
    );
    let _ = txn.rollback().await;

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
/// Regression for the fs metadata scan/decode path: exact path lookups can
/// still work while `readdir` goes empty if scan keys are decoded through a
/// guessed layout instead of the exact fs prefix.
async fn test_readdir_behavioral_exact_names_are_preserved() {
    let fs = make_fs().await;
    let nonce = rand::thread_rng().gen::<u64>();
    let root_prefix = format!("test_readdir_exact_{nonce}");
    let root_file_1 = format!("/{root_prefix}_hello.txt");
    let root_file_2 = format!("/{root_prefix}_data.bin");
    let batch_dir = format!("/{root_prefix}_batch");
    let batch_file_names = ["alpha.txt", "beta.bin", "gamma.json"];

    cleanup(&fs, &root_file_1).await;
    cleanup(&fs, &root_file_2).await;
    cleanup(&fs, &batch_dir).await;
    ensure_dir(&fs, &batch_dir).await;

    fs.write_file(&root_file_1, b"hello", None).await.unwrap();
    fs.write_file(&root_file_2, b"\x00\x01\x02\x03", None)
        .await
        .unwrap();
    for name in batch_file_names {
        fs.write_file(&format!("{batch_dir}/{name}"), name.as_bytes(), None)
            .await
            .unwrap();
    }

    let root_names: Vec<String> = fs
        .readdir("/")
        .await
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(root_names.iter().any(|name| name == &root_file_1[1..]));
    assert!(root_names.iter().any(|name| name == &root_file_2[1..]));
    assert!(root_names.iter().any(|name| name == &batch_dir[1..]));

    let batch_names: Vec<String> = fs
        .readdir(&batch_dir)
        .await
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        batch_names,
        batch_file_names
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    );

    cleanup(&fs, &root_file_1).await;
    cleanup(&fs, &root_file_2).await;
    cleanup(&fs, &batch_dir).await;
}

#[tokio::test]
#[ignore]
async fn test_readdir_behavioral_ignores_nested_path_dir_entries() {
    let fs = make_fs().await;
    let base = "/test_readdir_invalid_entries";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;
    fs.write_file(&format!("{base}/hello.txt"), b"hello", None)
        .await
        .unwrap();

    let mut txn = fs.begin().await.unwrap();
    let bogus_inode_id = fs.alloc_inode_id().await.unwrap();
    let bogus_inode = Inode::new_file(bogus_inode_id, 0o644);
    save_inode(&mut txn, &bogus_inode).await.unwrap();
    // Test-only: intentionally creates a bogus dir entry to test robustness.
    #[allow(clippy::disallowed_methods)]
    txn.put(
        keys::dir_entry_key(
            ROOT_INODE,
            &format!("{}/bogus.txt", base.trim_start_matches('/')),
        ),
        bogus_inode_id.to_be_bytes().to_vec(),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();

    let names: Vec<String> = fs
        .readdir("/")
        .await
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(
        names.iter().all(|name| !name.contains('/')),
        "root readdir must ignore nested-path dir entries: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|name| name == base.trim_start_matches('/')),
        "root readdir must keep valid directory entries: {names:?}"
    );

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_readdir_recursive_behavioral_caps_frontier_scan_with_batched_hydration() {
    let fs = make_fs().await;
    let nonce = rand::thread_rng().gen::<u64>();
    let base = format!("/test_readdir_recursive_{nonce}");
    let dir_a = format!("{base}/a");
    let dir_b = format!("{dir_a}/b");
    let dir_c = format!("{dir_b}/c");
    let dir_batch = format!("{base}/batch");

    cleanup(&fs, &base).await;
    ensure_dir(&fs, &dir_c).await;
    ensure_dir(&fs, &dir_batch).await;

    fs.write_file(&format!("{base}/root.txt"), b"root", None)
        .await
        .unwrap();
    fs.write_file(&format!("{dir_a}/alpha.txt"), b"alpha", None)
        .await
        .unwrap();
    fs.write_file(&format!("{dir_b}/beta.txt"), b"beta", None)
        .await
        .unwrap();
    fs.write_file(&format!("{dir_c}/charlie.txt"), b"charlie", None)
        .await
        .unwrap();
    fs.write_file(&format!("{dir_batch}/delta.txt"), b"delta", None)
        .await
        .unwrap();

    let result = fs
        .readdir_recursive(
            &base,
            FsRecursiveReaddirOptions {
                max_depth: 8,
                max_entries: 6,
                exclude_set: None,
            },
        )
        .await
        .unwrap();

    let paths = result
        .entries
        .into_iter()
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            format!("{base}/a"),
            format!("{base}/a/alpha.txt"),
            format!("{base}/a/b"),
            format!("{base}/batch"),
            format!("{base}/batch/delta.txt"),
            format!("{base}/root.txt"),
        ]
    );
    assert!(
        result.truncated,
        "recursive traversal should stop once max_entries is exhausted"
    );
    assert_eq!(
        result.total_dirs_scanned, 3,
        "should scan root plus the next frontier before hitting the cap"
    );

    cleanup(&fs, &base).await;
}

#[tokio::test]
#[ignore]
async fn test_readdir_recursive_behavioral_dangling_dirent_does_not_consume_budget() {
    let fs = make_fs().await;
    let nonce = rand::thread_rng().gen::<u64>();
    let base = format!("/test_readdir_recursive_dangling_{nonce}");

    cleanup(&fs, &base).await;
    ensure_dir(&fs, &base).await;
    fs.write_file(&format!("{base}/bb.txt"), b"ok", None)
        .await
        .unwrap();

    let mut txn = fs.begin().await.unwrap();
    let (base_inode_id, _) = resolve_path(&mut txn, &base).await.unwrap();
    // Test-only: intentionally creates a dangling dir entry to test robustness.
    #[allow(clippy::disallowed_methods)]
    txn.put(
        keys::dir_entry_key(base_inode_id, "aa-dangling.txt"),
        u64::MAX.to_be_bytes().to_vec(),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();

    let result = fs
        .readdir_recursive(
            &base,
            FsRecursiveReaddirOptions {
                max_depth: 1,
                max_entries: 1,
                exclude_set: None,
            },
        )
        .await
        .unwrap();
    let paths = result
        .entries
        .into_iter()
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    assert_eq!(paths, vec![format!("{base}/bb.txt")]);
    assert!(
        !result.truncated,
        "dangling dirents should not consume recursive result budget"
    );

    cleanup(&fs, &base).await;
}

#[tokio::test]
#[ignore]
async fn test_inlineblob_behavioral_large_write_routes_to_object() {
    let fs = make_fs().await;
    let base = "/test_inlineblob_large_write";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let inline_max = fs9_config().inline_max_bytes;
    if inline_max == 0 || fs9_config().s3.is_none() {
        return;
    }
    let path = &format!("{base}/large.bin");
    let data: Vec<u8> = (0..(inline_max + 1)).map(|v| (v % 251) as u8).collect();
    fs.write_file(path, &data, None).await.unwrap();

    let inode = fs.stat(path).await.unwrap();
    assert!(matches!(inode.data, DataRef::Object { .. }));
    assert_eq!(inode.size, data.len() as u64);
    assert_eq!(fs.read_file(path).await.unwrap(), data);

    let mut txn = fs.begin().await.unwrap();
    assert!(
        txn.get(keys::blob_key(inode.id)).await.unwrap().is_none(),
        "object route must not leave an inline blob key"
    );
    assert!(
        txn.get(keys::page_key(inode.id, 0))
            .await
            .unwrap()
            .is_none(),
        "published object route must not leave staged pages"
    );
    let _ = txn.rollback().await;

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_write_file_behavioral_replaces_existing_object_file() {
    let fs = make_fs().await;
    let base = "/test_write_file_replace_existing_object";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let inline_max = fs9_config().inline_max_bytes;
    if inline_max == 0 || fs9_config().s3.is_none() {
        return;
    }

    let path = &format!("{base}/replace.bin");
    let original: Vec<u8> = (0..(inline_max + 1)).map(|idx| (idx % 251) as u8).collect();
    fs.write_file(path, &original, None).await.unwrap();

    let original_inode = fs.stat(path).await.unwrap();
    assert!(
        matches!(original_inode.data, DataRef::Object { .. }),
        "large write must route through object storage"
    );
    let original_data_ref = original_inode.data.clone();

    // Append on Object-backed files creates an immutable delta block in TiKV.
    let appended = fs.append_file(path, b"!").await.unwrap();
    assert_eq!(appended, 1);
    let after_append = fs.stat(path).await.unwrap();
    assert_eq!(
        after_append.size,
        original_inode.size + 1,
        "append should increase file size by the appended bytes"
    );
    // DataRef stays Object (delta is stored separately, not in the inode).
    assert!(matches!(after_append.data, DataRef::Object { .. }));
    // Verify read_file returns correct merged content (S3 base + delta).
    let mut expected = original.clone();
    expected.push(b'!');
    assert_eq!(
        fs.read_file(path).await.unwrap(),
        expected,
        "read_file must merge S3 base object with TiKV append deltas"
    );

    let replacement = b"replacement-inline".to_vec();
    fs.write_file(path, &replacement, None).await.unwrap();

    let replaced_inode = fs.stat(path).await.unwrap();
    assert_eq!(
        replaced_inode.id, original_inode.id,
        "full replace should reuse the inode instead of creating a second published file"
    );
    assert_eq!(replaced_inode.data, DataRef::InlineBlob);
    assert_eq!(replaced_inode.size, replacement.len() as u64);
    assert_eq!(fs.read_file(path).await.unwrap(), replacement);

    let mut txn = fs.begin_internal().await.unwrap();
    assert_eq!(
        lifecycle::load_lifecycle(&mut txn, replaced_inode.id)
            .await
            .unwrap(),
        Some(FileLifecycle::Deleting {
            fs_instance_id: fs.runtime_state().fs_instance_id,
            data_ref: original_data_ref,
        }),
        "full replace of an object-backed file must retire the old object via lifecycle cleanup"
    );
    let _ = txn.rollback().await;

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_prepare_download_rejects_object_with_pending_deltas() {
    let fs = make_fs().await;
    let base = "/test_prepare_download_rejects_object_with_pending_deltas";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/large.bin");
    let data = vec![0xABu8; fs9_config().inline_max_bytes.saturating_add(1)];
    fs.write_file(path, &data, None).await.unwrap();
    let inode = fs.stat(path).await.unwrap();
    assert!(matches!(inode.data, DataRef::Object { .. }));

    // Without deltas, prepare_download should succeed.
    fs.prepare_download(path).await.unwrap();

    // After appending, prepare_download should compact the deltas and
    // return a valid presigned URL (not reject).
    fs.append_file(path, b"delta").await.unwrap();
    let download = fs
        .prepare_download(path)
        .await
        .expect("prepare_download should compact deltas and succeed");
    assert_eq!(download.size, data.len() as u64 + 5); // original + "delta"
    assert!(download.range_supported);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_first_object_append_bumps_superblock_to_v6() {
    let fs = make_fs().await;
    let base = "/test_first_object_append_bumps_superblock_to_v6";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/versioned.bin");
    let data = vec![0xCDu8; fs9_config().inline_max_bytes.saturating_add(1)];
    fs.write_file(path, &data, None).await.unwrap();
    let inode = fs.stat(path).await.unwrap();
    assert!(matches!(inode.data, DataRef::Object { .. }));

    // Superblock must be bumped to v6 after the first Object append.
    fs.append_file(path, b"v6").await.unwrap();
    let mut txn = fs.begin_internal().await.unwrap();
    let sb = load_current_superblock_if_present(&mut txn)
        .await
        .unwrap()
        .expect("superblock must exist");
    assert!(
        sb.format_version >= FS9_FORMAT_VERSION_APPEND_DELTA,
        "superblock format version must be >= {} after first object append, got {}",
        FS9_FORMAT_VERSION_APPEND_DELTA,
        sb.format_version,
    );
    let _ = txn.rollback().await;

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_inline_read_paths_do_not_mutate_inode_metadata() {
    let fs = make_fs().await;
    let base = "/test_inline_read_paths_do_not_mutate_inode_metadata";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let inline_max = fs9_config().inline_max_bytes;
    if inline_max == 0 {
        return;
    }

    let path = &format!("{base}/inline.txt");
    let data = b"inline-read-regression".to_vec();
    fs.write_file(path, &data, None).await.unwrap();

    let inode = fs.stat(path).await.unwrap();
    assert_eq!(inode.data, DataRef::InlineBlob);

    let before = inode_snapshot(&fs, path).await;
    let stat_inode = fs.stat(path).await.unwrap();
    assert_eq!(stat_inode.size, data.len() as u64);
    assert_eq!(inode_snapshot(&fs, path).await, before);

    assert_eq!(fs.read_file(path).await.unwrap(), data);
    assert_eq!(inode_snapshot(&fs, path).await, before);

    assert_eq!(
        fs.read_file_at(path, 3, 6).await.unwrap(),
        data[3..9].to_vec()
    );
    assert_eq!(inode_snapshot(&fs, path).await, before);

    assert!(fs
        .read_file_at(path, data.len() as u64, 16)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(inode_snapshot(&fs, path).await, before);

    let mut reader = fs.read_file_stream(path, data.len()).await.unwrap();
    let mut streamed = Vec::new();
    reader.read_to_end(&mut streamed).await.unwrap();
    assert_eq!(streamed, data);
    assert_eq!(inode_snapshot(&fs, path).await, before);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_pack_read_paths_do_not_mutate_inode_metadata() {
    if fs9_config().s3.is_none() {
        return;
    }

    let fs = make_fs().await;
    let base = "/test_pack_read_paths_do_not_mutate_inode_metadata";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let first_path = format!("{base}/first.txt");
    let second_path = format!("{base}/second.txt");
    let first_data = b"pack-entry-first".to_vec();
    let second_data = b"pack-entry-second".to_vec();
    let entries = fs
        .batch_write(vec![
            FsBatchWriteFile {
                path: first_path.clone(),
                data: first_data.clone(),
                mode: None,
            },
            FsBatchWriteFile {
                path: second_path.clone(),
                data: second_data,
                mode: None,
            },
        ])
        .await
        .unwrap();
    assert!(entries.iter().all(|entry| entry.result.is_ok()));

    let inode = fs.stat(&first_path).await.unwrap();
    assert!(
        matches!(inode.data, DataRef::PackEntry { .. }),
        "batch pack write must publish pack-backed files"
    );

    let before = inode_snapshot(&fs, &first_path).await;
    let stat_inode = fs.stat(&first_path).await.unwrap();
    assert_eq!(stat_inode.size, first_data.len() as u64);
    assert_eq!(inode_snapshot(&fs, &first_path).await, before);

    assert_eq!(fs.read_file(&first_path).await.unwrap(), first_data);
    assert_eq!(inode_snapshot(&fs, &first_path).await, before);

    assert_eq!(
        fs.read_file_at(&first_path, 5, 4).await.unwrap(),
        first_data[5..9].to_vec()
    );
    assert_eq!(inode_snapshot(&fs, &first_path).await, before);

    let mut reader = fs
        .read_file_stream(&first_path, first_data.len())
        .await
        .unwrap();
    let mut streamed = Vec::new();
    reader.read_to_end(&mut streamed).await.unwrap();
    assert_eq!(streamed, first_data);
    assert_eq!(inode_snapshot(&fs, &first_path).await, before);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_batch_inline_read_behavioral_pack_entry_round_trip() {
    if fs9_config().s3.is_none() {
        return;
    }

    let fs = make_fs().await;
    let base = "/test_batch_inline_read_behavioral_pack_entry_round_trip";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let first_path = format!("{base}/first.txt");
    let second_path = format!("{base}/second.txt");
    let first_data = b"pack-batch-first".to_vec();
    let second_data = b"pack-batch-second".to_vec();
    let entries = fs
        .batch_write(vec![
            FsBatchWriteFile {
                path: first_path.clone(),
                data: first_data.clone(),
                mode: None,
            },
            FsBatchWriteFile {
                path: second_path.clone(),
                data: second_data.clone(),
                mode: None,
            },
        ])
        .await
        .unwrap();
    assert!(entries.iter().all(|entry| entry.result.is_ok()));

    let first_inode = fs.stat(&first_path).await.unwrap();
    let second_inode = fs.stat(&second_path).await.unwrap();
    assert!(matches!(first_inode.data, DataRef::PackEntry { .. }));
    assert!(matches!(second_inode.data, DataRef::PackEntry { .. }));

    let first_before = inode_snapshot(&fs, &first_path).await;
    let second_before = inode_snapshot(&fs, &second_path).await;
    let results = fs
        .batch_inline_read(
            &[first_path.clone(), second_path.clone()],
            first_data.len().max(second_data.len()),
            first_data.len() + second_data.len(),
        )
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].as_ref().unwrap(), &first_data);
    assert_eq!(results[1].as_ref().unwrap(), &second_data);
    assert_eq!(inode_snapshot(&fs, &first_path).await, first_before);
    assert_eq!(inode_snapshot(&fs, &second_path).await, second_before);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_object_read_paths_do_not_mutate_inode_metadata() {
    let fs = make_fs().await;
    let base = "/test_object_read_paths_do_not_mutate_inode_metadata";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let inline_max = fs9_config().inline_max_bytes;
    if inline_max == 0 || fs9_config().s3.is_none() {
        return;
    }

    let path = &format!("{base}/object.bin");
    let data: Vec<u8> = (0..(inline_max + 1)).map(|idx| (idx % 251) as u8).collect();
    fs.write_file(path, &data, None).await.unwrap();

    let inode = fs.stat(path).await.unwrap();
    assert!(
        matches!(inode.data, DataRef::Object { .. }),
        "large write must route through object storage"
    );

    let before = inode_snapshot(&fs, path).await;
    let stat_inode = fs.stat(path).await.unwrap();
    assert_eq!(stat_inode.size, data.len() as u64);
    assert_eq!(inode_snapshot(&fs, path).await, before);

    assert_eq!(fs.read_file(path).await.unwrap(), data);
    assert_eq!(inode_snapshot(&fs, path).await, before);

    assert_eq!(
        fs.read_file_at(path, 7, 11).await.unwrap(),
        data[7..18].to_vec()
    );
    assert_eq!(inode_snapshot(&fs, path).await, before);

    let mut reader = fs.read_file_stream(path, data.len()).await.unwrap();
    let mut streamed = Vec::new();
    reader.read_to_end(&mut streamed).await.unwrap();
    assert_eq!(streamed, data);
    assert_eq!(inode_snapshot(&fs, path).await, before);

    let prepared = fs.prepare_download(path).await.unwrap();
    assert_eq!(prepared.storage, FsStorage::Object);
    assert_eq!(prepared.size, data.len() as u64);
    assert!(prepared.range_supported);
    assert_eq!(inode_snapshot(&fs, path).await, before);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_batch_inline_read_behavioral_object_round_trip() {
    let fs = make_fs().await;
    let base = "/test_batch_inline_read_behavioral_object_round_trip";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let inline_max = fs9_config().inline_max_bytes;
    if inline_max == 0 || fs9_config().s3.is_none() {
        return;
    }

    let path = format!("{base}/object.bin");
    let data: Vec<u8> = (0..(inline_max + 1)).map(|idx| (idx % 251) as u8).collect();
    fs.write_file(&path, &data, None).await.unwrap();

    let inode = fs.stat(&path).await.unwrap();
    assert!(matches!(inode.data, DataRef::Object { .. }));

    let before = inode_snapshot(&fs, &path).await;
    let results = fs
        .batch_inline_read(std::slice::from_ref(&path), data.len(), data.len())
        .await
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].as_ref().unwrap(), &data);
    assert_eq!(inode_snapshot(&fs, &path).await, before);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_batch_inline_read_behavioral_shared_s3_error_for_external_entries() {
    if fs9_config().s3.is_some() {
        return;
    }

    let fs = make_fs().await;
    let base = "/test_batch_inline_read_behavioral_shared_s3_error";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let object_path = format!("{base}/object.bin");
    let pack_path = format!("{base}/pack.bin");
    let bundle_id = fs.alloc_bundle_id().await.unwrap();
    let object_inode_id = fs.alloc_inode_id().await.unwrap();
    let pack_inode_id = fs.alloc_inode_id().await.unwrap();

    let mut txn = fs.begin().await.unwrap();
    let (object_parent, object_name) =
        ensure_parents_and_resolve_parent(&fs, &mut txn, &object_path)
            .await
            .unwrap();
    let mut object_inode = Inode::new_file(object_inode_id, 0o644);
    object_inode.size = 4;
    object_inode.data = DataRef::Object {
        key: "missing-object".to_string(),
        version: 1,
        checksum: [1u8; 32],
    };
    save_inode(&mut txn, &object_inode).await.unwrap();
    link(&mut txn, object_parent, &object_name, object_inode_id)
        .await
        .unwrap();

    let (pack_parent, pack_name) = ensure_parents_and_resolve_parent(&fs, &mut txn, &pack_path)
        .await
        .unwrap();
    let mut pack_inode = Inode::new_file(pack_inode_id, 0o644);
    pack_inode.size = 4;
    pack_inode.data = DataRef::PackEntry {
        bundle_id,
        offset: 0,
        len: 4,
        checksum: [2u8; 32],
        generation: 1,
    };
    save_inode(&mut txn, &pack_inode).await.unwrap();
    link(&mut txn, pack_parent, &pack_name, pack_inode_id)
        .await
        .unwrap();

    save_bundle_manifest(
        &mut txn,
        &BundleManifest {
            fs_instance_id: fs.instance_identity().fs_instance_id,
            bundle_id,
            key: "missing-pack".to_string(),
            created_at: current_unix_timestamp(),
            object_size: 4,
            footer_offset: 0,
            entry_count: 1,
            live_entries: 1,
            live_bytes: 4,
            stale_entries: 0,
            checksum: [3u8; 32],
            state: BundleManifestState::Active,
        },
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();

    let results = fs
        .batch_inline_read(&[object_path.clone(), pack_path.clone()], 8, 8)
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    for result in results {
        let fs_err = result
            .unwrap_err()
            .downcast::<EmbeddedFsError>()
            .expect("external entry failure should stay typed");
        assert!(matches!(fs_err, EmbeddedFsError::Internal(msg) if msg == "S3 is not configured"));
    }

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_inlineblob_behavioral_truncate_rejects_non_inline_growth() {
    let fs = make_fs().await;
    let base = "/test_inlineblob_truncate_reject_non_inline";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let inline_max = fs9_config().inline_max_bytes;
    if inline_max == 0 {
        return;
    }
    let path = &format!("{base}/file.bin");
    fs.write_file(path, b"hello", None).await.unwrap();

    let err = fs
        .truncate(path, u64::try_from(inline_max + 1).unwrap())
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("truncate is only supported for inline files"),
        "unexpected error: {err}"
    );
    assert_eq!(fs.read_file(path).await.unwrap(), b"hello");

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_inlineblob_behavioral_write_at_rejects_non_inline_growth() {
    let fs = make_fs().await;
    let base = "/test_inlineblob_write_at_reject_non_inline";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let inline_max = fs9_config().inline_max_bytes;
    if inline_max == 0 {
        return;
    }
    let path = &format!("{base}/file.bin");
    fs.write_file(path, b"hello", None).await.unwrap();

    let err = fs
        .write_file_at(path, u64::try_from(inline_max + 1).unwrap(), b"X")
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("partial mutation is only supported for inline files"),
        "unexpected error: {err}"
    );
    assert_eq!(fs.read_file(path).await.unwrap(), b"hello");

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_inlineblob_behavioral_rename_overwrite_deletes_inline_blob() {
    let fs = make_fs().await;
    let base = "/test_inlineblob_rename_overwrite";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let inline_max = fs9_config().inline_max_bytes;
    if inline_max == 0 {
        // InlineBlob is disabled in this environment.
        return;
    }
    let src = &format!("{base}/src.bin");
    let dst = &format!("{base}/dst.bin");
    fs.write_file(src, b"source", None).await.unwrap();
    fs.write_file(dst, b"dest", None).await.unwrap();

    let dst_inode = fs.stat(dst).await.unwrap();
    assert_eq!(dst_inode.data, DataRef::InlineBlob);

    fs.rename(src, dst).await.unwrap();
    assert!(fs.stat(src).await.is_err());
    assert_eq!(fs.read_file(dst).await.unwrap(), b"source");

    let mut txn = fs.begin().await.unwrap();
    assert!(
        txn.get(keys::inode_key(dst_inode.id))
            .await
            .unwrap()
            .is_none(),
        "rename overwrite must delete dest inode"
    );
    assert!(
        txn.get(keys::blob_key(dst_inode.id))
            .await
            .unwrap()
            .is_none(),
        "rename overwrite must delete dest inline blob key"
    );
    let _ = txn.rollback().await;

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_basic_file() {
    let fs = make_fs().await;
    let dir = "/test_rename_basic";
    cleanup(&fs, dir).await;
    ensure_dir(&fs, dir).await;

    let old = &format!("{dir}/a.txt");
    let new = &format!("{dir}/b.txt");
    fs.write_file(old, b"hello", None).await.unwrap();

    fs.rename(old, new).await.unwrap();

    // old name must be gone
    assert!(fs.stat(old).await.is_err(), "old path should not exist");
    // new name must exist with same content
    let data = fs.read_file(new).await.unwrap();
    assert_eq!(data, b"hello", "content must be preserved");

    cleanup(&fs, dir).await;
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_cross_directory() {
    let fs = make_fs().await;
    let base = "/test_rename_cross";
    cleanup(&fs, base).await;
    ensure_dir(&fs, &format!("{base}/src")).await;
    ensure_dir(&fs, &format!("{base}/dst")).await;

    let old = &format!("{base}/src/file.txt");
    let new = &format!("{base}/dst/file.txt");
    fs.write_file(old, b"cross", None).await.unwrap();

    fs.rename(old, new).await.unwrap();

    assert!(fs.stat(old).await.is_err());
    assert_eq!(fs.read_file(new).await.unwrap(), b"cross");

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_read_file_stream_behavioral_matches_read_file() {
    let fs = make_fs().await;
    let base = "/test_read_file_stream";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/large.bin");
    let data: Vec<u8> = (0..(PAGE_SIZE * 6 + 123))
        .map(|idx| (idx % 251) as u8)
        .collect();
    fs.write_file(path, &data, None).await.unwrap();

    let mut reader = fs.read_file_stream(path, data.len()).await.unwrap();
    let mut streamed = Vec::new();
    reader.read_to_end(&mut streamed).await.unwrap();

    assert_eq!(streamed, data);

    cleanup(&fs, base).await;
}

// Drop-bomb invariant tested in `src/extensions/fs/termination_guard.rs`
// (pure Rust unit tests, runs in PR CI). Behavioral `.terminate(Ok)` /
// `.terminate(Err)` paths below run in the nightly integration tier.

#[tokio::test]
#[ignore]
async fn test_begin_write_stream_behavioral_matches_write_file() {
    let fs = make_fs().await;
    let base = "/test_begin_write_stream";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/streamed.bin");
    // Size kept below fs9_config().inline_max_bytes (64 KiB) so the test
    // exercises the staging → inline commit path without requiring S3
    // backing, which make_fs() does not attach.
    let data: Vec<u8> = (0..(PAGE_SIZE * 3 + 77))
        .map(|idx| (idx % 239) as u8)
        .collect();

    let mut writer = fs
        .begin_write_stream(
            path,
            FsWriteStreamOptions {
                expected_size: Some(data.len() as u64),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    for chunk in data.chunks(11_111) {
        writer.write_chunk(chunk).await.unwrap();
    }
    let written = writer.terminate(Ok(())).await.unwrap();

    assert_eq!(written, data.len());
    assert_eq!(fs.read_file(path).await.unwrap(), data);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_symlink_behavioral_persists_across_reopen_and_preserves_rename_unlink_semantics() {
    let fs = make_fs().await;
    let keyspace = fs.keyspace.clone();
    let base = "/test_symlink_reopen_round_trip";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let target = format!("{base}/target.txt");
    let link = format!("{base}/link");
    let renamed = format!("{base}/link-renamed");
    fs.write_file(&target, b"payload", None).await.unwrap();
    fs.symlink(&link, &target).await.unwrap();

    let inode = fs.stat(&link).await.unwrap();
    assert!(inode.is_symlink(), "newly created inode must be a symlink");
    assert_eq!(inode.size, target.len() as u64);
    assert_eq!(fs.readlink(&link).await.unwrap(), target);

    let reopened = make_fs_for_keyspace(keyspace).await;
    let reopened_inode = reopened.stat(&link).await.unwrap();
    assert!(
        reopened_inode.is_symlink(),
        "reopened filesystem must preserve symlink type"
    );
    assert_eq!(
        reopened.readlink(&link).await.unwrap(),
        target,
        "reopened filesystem must preserve symlink target"
    );

    reopened.rename(&link, &renamed).await.unwrap();
    assert_eq!(
        reopened.readlink(&renamed).await.unwrap(),
        target,
        "rename must move the symlink itself, not rewrite its target"
    );
    let missing = reopened.stat(&link).await.unwrap_err();
    assert!(
        matches!(
            missing.downcast_ref::<EmbeddedFsError>(),
            Some(EmbeddedFsError::NotFound(_))
        ),
        "old path must be gone after rename: {missing}"
    );

    reopened.remove(&renamed).await.unwrap();
    assert_eq!(
        reopened.read_file(&target).await.unwrap(),
        b"payload",
        "unlinking a symlink must not remove the target file"
    );
    let removed = reopened.readlink(&renamed).await.unwrap_err();
    assert!(
        matches!(
            removed.downcast_ref::<EmbeddedFsError>(),
            Some(EmbeddedFsError::NotFound(_))
        ),
        "removed symlink path must be gone: {removed}"
    );

    cleanup(&reopened, base).await;
}

#[tokio::test]
#[ignore]
async fn test_symlink_creation_bumps_superblock_to_v5() {
    let fs = make_fs().await;
    let base = "/test_symlink_version_bump";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    // Before any symlink, superblock should be at DEFAULT (v4).
    let mut txn = fs.begin().await.unwrap();
    let sb_before = load_superblock(&mut txn).await.unwrap();
    let _ = txn.rollback().await;
    assert_eq!(
        sb_before.format_version, FS9_FORMAT_VERSION_DEFAULT,
        "fresh keyspace must have default format version"
    );

    // Create a symlink — this should bump superblock to v5.
    let target = format!("{base}/target.txt");
    let link = format!("{base}/link");
    fs.write_file(&target, b"data", None).await.unwrap();
    fs.symlink(&link, &target).await.unwrap();

    let mut txn = fs.begin().await.unwrap();
    let sb_after = load_superblock(&mut txn).await.unwrap();
    let _ = txn.rollback().await;
    assert_eq!(
        sb_after.format_version, FS9_FORMAT_VERSION_SYMLINK,
        "superblock must be bumped to v5 after first symlink"
    );

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_keyspace_without_symlinks_stays_at_v4() {
    let fs = make_fs().await;
    let base = "/test_no_symlink_stays_v4";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    // Write regular files only — no symlinks.
    fs.write_file(&format!("{base}/file.txt"), b"hello", None)
        .await
        .unwrap();
    fs.mkdir(&format!("{base}/subdir"), false, None)
        .await
        .unwrap();

    let mut txn = fs.begin().await.unwrap();
    let sb = load_superblock(&mut txn).await.unwrap();
    let _ = txn.rollback().await;
    assert_eq!(
        sb.format_version, FS9_FORMAT_VERSION_DEFAULT,
        "keyspace without symlinks must remain at v4"
    );

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_begin_write_stream_without_size_keeps_small_files_mutable() {
    if fs9_config().s3.is_none() || fs9_config().inline_max_bytes == 0 {
        return;
    }

    let fs = make_fs().await;
    let base = "/test_begin_write_stream_unknown_size_small";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/small.bin");
    let data: Vec<u8> = (0..fs9_config().inline_max_bytes.clamp(1, 32))
        .map(|idx| (idx % 251) as u8)
        .collect();

    let mut writer = fs
        .begin_write_stream(
            path,
            FsWriteStreamOptions {
                expected_size: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    for chunk in data.chunks(7) {
        writer.write_chunk(chunk).await.unwrap();
    }
    let written = writer.terminate(Ok(())).await.unwrap();

    assert_eq!(written, data.len());
    let inode = fs.stat(path).await.unwrap();
    assert_eq!(inode.data, DataRef::InlineBlob);

    let mut expected = data.clone();
    expected.extend_from_slice(b"-tail");
    fs.append_file(path, b"-tail").await.unwrap();
    assert_eq!(fs.read_file(path).await.unwrap(), expected);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_begin_write_stream_large_without_s3_rejects_without_touching_existing_file() {
    if fs9_config().s3.is_some() || fs9_config().inline_max_bytes == 0 {
        return;
    }

    let fs = make_fs().await;
    let base = "/test_begin_write_stream_large_without_s3";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/existing.bin");
    let original = b"hello-inline";
    fs.write_file(path, original, None).await.unwrap();

    let err = fs
        .begin_write_stream(
            path,
            FsWriteStreamOptions {
                expected_size: Some((fs9_config().inline_max_bytes + 1) as u64),
                ..Default::default()
            },
        )
        .await
        .err()
        .expect("large streaming write without S3 must fail at init");
    assert!(
        err.to_string().contains("require S3-backed object storage"),
        "unexpected error: {err}"
    );

    let inode = fs.stat(path).await.unwrap();
    assert_eq!(inode.data, DataRef::InlineBlob);
    assert_eq!(inode.size, original.len() as u64);
    assert_eq!(fs.read_file(path).await.unwrap(), original);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_begin_write_stream_without_size_routes_large_files_after_spool() {
    if fs9_config().s3.is_none() || fs9_config().object_min_bytes == 0 {
        return;
    }

    let fs = make_fs().await;
    let base = "/test_begin_write_stream_unknown_size_large";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/large.bin");
    let data_len = fs9_config()
        .object_min_bytes
        .max(WRITE_STREAM_FLUSH_BYTES)
        .saturating_add(17);
    let data: Vec<u8> = (0..data_len).map(|idx| (idx % 251) as u8).collect();

    let mut writer = fs
        .begin_write_stream(
            path,
            FsWriteStreamOptions {
                expected_size: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    for chunk in data.chunks(19_337) {
        writer.write_chunk(chunk).await.unwrap();
    }
    let written = writer.terminate(Ok(())).await.unwrap();

    assert_eq!(written, data.len());
    let inode = fs.stat(path).await.unwrap();
    assert!(matches!(inode.data, DataRef::Object { .. }));
    assert_eq!(fs.read_file(path).await.unwrap(), data);
    // Append on Object-backed files creates an immutable delta block.
    assert_eq!(fs.append_file(path, b"!").await.unwrap(), 1);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_append_new_large_file_rolls_back_creation() {
    let fs = make_fs().await;
    let base = "/test_append_new_large_file_rolls_back_creation";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let inline_max = fs9_config().inline_max_bytes;
    if inline_max == 0 {
        return;
    }

    let path = &format!("{base}/large-append.bin");
    let data = vec![b'x'; inline_max.saturating_add(1)];
    let err = fs.append_file(path, &data).await.expect_err(
        "append_file on a missing path must reject non-inline growth without creating the file",
    );
    assert!(
        err.to_string()
            .contains("append is only supported for inline files"),
        "unexpected error: {err}"
    );

    let stat_err = fs
        .stat(path)
        .await
        .expect_err("failed append on a missing path must not leave an empty file behind");
    assert!(
        is_not_found_error(&stat_err),
        "unexpected stat error after failed append: {stat_err}"
    );

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_begin_write_stream_abort_preserves_existing_file() {
    let fs = make_fs().await;
    let base = "/test_begin_write_stream_abort";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/stable.bin");
    let original = b"stable-before-abort".to_vec();
    fs.write_file(path, &original, None).await.unwrap();

    let mut writer = fs
        .begin_write_stream(
            path,
            FsWriteStreamOptions {
                expected_size: Some(32),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    writer
        .write_chunk(b"new-data-that-must-not-commit")
        .await
        .unwrap();
    // Explicit abort via terminate(Err). Exercises the ordered/observable
    // cleanup path so regressions to Drop-only cleanup surface here.
    writer
        .terminate(Err(anyhow::anyhow!("test: abandon staged write")))
        .await
        .unwrap_err();

    assert_eq!(fs.read_file(path).await.unwrap(), original);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_abort_upload_cleans_staging_state() {
    if fs9_config().s3.is_none() || fs9_config().object_min_bytes == 0 {
        return;
    }

    let fs = make_fs().await;
    let base = "/test_abort_upload_cleanup";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/object.bin");
    let upload = fs
        .create_upload(path, fs9_config().object_min_bytes as u64, None, None)
        .await
        .unwrap();
    let claims = verify_upload_token(&upload.upload_token).unwrap();

    fs.abort_upload(&upload.upload_token)
        .await
        .expect("abort_upload must clear staging state");

    let mut txn = fs.begin_internal().await.unwrap();
    assert!(
        load_inode(&mut txn, claims.staging_inode_id)
            .await
            .unwrap()
            .is_none(),
        "staging inode must be removed by abort cleanup"
    );
    assert!(
        lifecycle::load_lifecycle(&mut txn, claims.staging_inode_id)
            .await
            .unwrap()
            .is_none(),
        "lifecycle marker must be removed by abort cleanup"
    );
    let _ = txn.rollback().await;

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_remove_recursive_object_file_stamps_runtime_lifecycle_owner() {
    if fs9_config().s3.is_none() || fs9_config().inline_max_bytes == 0 {
        return;
    }

    let fs = make_fs().await;
    let base = "/test_remove_recursive_object_lifecycle_owner";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/large.bin");
    let data: Vec<u8> = (0..(fs9_config().inline_max_bytes + 1))
        .map(|idx| (idx % 251) as u8)
        .collect();
    fs.write_file(path, &data, None).await.unwrap();

    let inode = fs.stat(path).await.unwrap();
    assert!(matches!(inode.data, DataRef::Object { .. }));

    let removed = fs.remove_recursive(base).await.unwrap();
    assert_eq!(removed, 2, "directory + child file must both be removed");

    let mut txn = fs.begin_internal().await.unwrap();
    assert!(
        load_inode(&mut txn, inode.id).await.unwrap().is_none(),
        "recursive delete must remove the file inode"
    );
    assert!(
        load_inode(&mut txn, ROOT_INODE).await.unwrap().is_some(),
        "sanity check: filesystem root must remain"
    );
    assert_eq!(
        lifecycle::load_lifecycle(&mut txn, inode.id).await.unwrap(),
        Some(FileLifecycle::Deleting {
            fs_instance_id: fs.runtime_state().fs_instance_id,
            data_ref: inode.data.clone(),
        }),
        "recursive delete must stamp lifecycle ownership with the active fs instance"
    );
    let _ = txn.rollback().await;

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_begin_write_stream_replaces_existing_file() {
    let fs = make_fs().await;
    let base = "/test_begin_write_stream_replace";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/replace.bin");
    fs.write_file(path, b"old-data", None).await.unwrap();

    // Size kept below fs9_config().inline_max_bytes (64 KiB); see
    // test_begin_write_stream_behavioral_matches_write_file for rationale.
    let new_data: Vec<u8> = (0..(PAGE_SIZE * 3 + 19))
        .map(|idx| (idx % 251) as u8)
        .collect();
    let mut writer = fs
        .begin_write_stream(
            path,
            FsWriteStreamOptions {
                expected_size: Some(new_data.len() as u64),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    for chunk in new_data.chunks(8192) {
        writer.write_chunk(chunk).await.unwrap();
    }
    let written = writer.terminate(Ok(())).await.unwrap();

    assert_eq!(written, new_data.len());
    assert_eq!(fs.read_file(path).await.unwrap(), new_data);

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_directory() {
    let fs = make_fs().await;
    let base = "/test_rename_dir";
    cleanup(&fs, base).await;
    ensure_dir(&fs, &format!("{base}/old_dir")).await;
    fs.write_file(&format!("{base}/old_dir/child.txt"), b"nested", None)
        .await
        .unwrap();

    fs.rename(&format!("{base}/old_dir"), &format!("{base}/new_dir"))
        .await
        .unwrap();

    assert!(fs.stat(&format!("{base}/old_dir")).await.is_err());
    let children = fs.readdir(&format!("{base}/new_dir")).await.unwrap();
    assert!(
        children.iter().any(|(name, _)| name == "child.txt"),
        "children must follow renamed directory"
    );

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_missing_source_enoent() {
    let fs = make_fs().await;
    let err = fs
        .rename("/nonexistent_path_xyz", "/somewhere")
        .await
        .unwrap_err();
    let fs_err = err
        .downcast_ref::<EmbeddedFsError>()
        .expect("expected EmbeddedFsError");
    assert!(
        matches!(fs_err, EmbeddedFsError::NotFound(_)),
        "missing source should produce ENOENT-equivalent, got: {fs_err}"
    );
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_missing_dest_parent_enoent_before_einval() {
    let fs = make_fs().await;
    let base = "/test_rename_parent_precedence";
    cleanup(&fs, base).await;
    ensure_dir(&fs, &format!("{base}/a")).await;

    let err = fs
        .rename(&format!("{base}/a"), &format!("{base}/a/missing/x"))
        .await
        .unwrap_err();
    let fs_err = err
        .downcast_ref::<EmbeddedFsError>()
        .expect("expected EmbeddedFsError");
    assert!(
            matches!(fs_err, EmbeddedFsError::NotFound(_)),
            "missing destination parent should produce ENOENT-equivalent before cycle EINVAL, got: {fs_err}"
        );

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_cycle_einval() {
    let fs = make_fs().await;
    let base = "/test_rename_cycle";
    cleanup(&fs, base).await;
    ensure_dir(&fs, &format!("{base}/a/b/c")).await;

    let err = fs
        .rename(&format!("{base}/a"), &format!("{base}//a/b/c/moved"))
        .await
        .unwrap_err();
    let fs_err = err
        .downcast_ref::<EmbeddedFsError>()
        .expect("expected EmbeddedFsError");
    assert!(
        matches!(fs_err, EmbeddedFsError::InvalidInput(_)),
        "cycle should produce EINVAL-equivalent, got: {fs_err}"
    );

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_root_rejected() {
    let fs = make_fs().await;
    let err = fs.rename("/", "/newroot").await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cannot rename root"),
        "root rename must be rejected: {msg}"
    );
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_same_path_missing_source_enoent() {
    let fs = make_fs().await;
    let err = fs
        .rename("/nonexistent_same", "/nonexistent_same")
        .await
        .unwrap_err();
    let fs_err = err
        .downcast_ref::<EmbeddedFsError>()
        .expect("expected EmbeddedFsError");
    assert!(
        matches!(fs_err, EmbeddedFsError::NotFound(_)),
        "rename(missing, missing) with identical paths must return ENOENT, got: {fs_err}"
    );
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_same_path_trailing_slash_enotdir() {
    let fs = make_fs().await;
    let base = "/test_rename_noop";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;
    let path = &format!("{base}/f.txt");
    fs.write_file(path, b"stable", None).await.unwrap();

    // trailing slash on destination requires a directory target
    let err = fs.rename(path, &format!("{path}/")).await.unwrap_err();
    let fs_err = err
        .downcast_ref::<EmbeddedFsError>()
        .expect("expected EmbeddedFsError");
    assert!(
        matches!(fs_err, EmbeddedFsError::NotDirectory(_)),
        "trailing-slash destination on file should produce ENOTDIR-equivalent, got: {fs_err}"
    );
    let data = fs.read_file(path).await.unwrap();
    assert_eq!(data, b"stable", "failed rename must not corrupt data");

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_missing_dest_leaf_trailing_slash_enotdir() {
    let fs = make_fs().await;
    let base = "/test_rename_missing_leaf_trailing_slash";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;
    let src = &format!("{base}/f.txt");
    let dst = &format!("{base}/missing/");
    fs.write_file(src, b"stable", None).await.unwrap();

    let err = fs.rename(src, dst).await.unwrap_err();
    let fs_err = err
        .downcast_ref::<EmbeddedFsError>()
        .expect("expected EmbeddedFsError");
    assert!(
            matches!(fs_err, EmbeddedFsError::NotDirectory(_)),
            "trailing-slash destination on file with missing leaf should produce ENOTDIR-equivalent, got: {fs_err}"
        );
    assert_eq!(
        fs.read_file(src).await.unwrap(),
        b"stable",
        "failed rename must not move source file"
    );
    assert!(fs.stat(&format!("{base}/missing")).await.is_err());

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_rename_behavioral_existing_dir_dest_trailing_slash_enotdir() {
    let fs = make_fs().await;
    let base = "/test_rename_existing_dir_trailing_slash";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;
    ensure_dir(&fs, &format!("{base}/existing_dir")).await;
    let src = &format!("{base}/f.txt");
    let dst = &format!("{base}/existing_dir/");
    fs.write_file(src, b"stable", None).await.unwrap();

    let err = fs.rename(src, dst).await.unwrap_err();
    let fs_err = err
        .downcast_ref::<EmbeddedFsError>()
        .expect("expected EmbeddedFsError");
    assert!(
            matches!(fs_err, EmbeddedFsError::NotDirectory(_)),
            "trailing-slash destination on file with existing directory destination should produce ENOTDIR-equivalent, got: {fs_err}"
        );
    assert_eq!(
        fs.read_file(src).await.unwrap(),
        b"stable",
        "failed rename must not move source file"
    );

    cleanup(&fs, base).await;
}

/// Regression test for P0 data-loss bug (#1680):
/// If publish_staged_write commits at TiKV Raft level but the client
/// observes a timeout, finish() calls abort_staged_write on the
/// now-published inode. Without the nlink guard, this deletes the
/// live file's pages and inode.
#[tokio::test]
#[ignore]
async fn test_cleanup_staging_inode_skips_published_file() {
    let fs = make_fs().await;
    let base = "/test_nlink_guard";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/published.bin");
    let data = b"important-data-must-survive";

    // 1. Allocate a staging inode (nlink=0) and write data into it.
    let mut txn = fs.begin().await.unwrap();
    let inode_id = fs.alloc_inode_id().await.unwrap();
    let mut inode = Inode::new_file(inode_id, 0o644);
    inode.nlink = 0;
    save_inode(&mut txn, &inode).await.unwrap();
    mark_staging_write(&mut txn, inode_id, current_unix_timestamp())
        .await
        .unwrap();
    txn.commit().await.unwrap();

    fs.flush_staged_write_chunk(inode_id, 0, data)
        .await
        .unwrap();

    // 2. Publish the staging inode to the target path (sets nlink=1).
    let written = fs.publish_staged_write(path, inode_id).await.unwrap();
    assert_eq!(written, data.len());

    // 3. Simulate abort-after-ambiguous-commit: call cleanup_staging_inode
    //    on the now-published inode. The nlink guard must prevent deletion.
    fs.cleanup_staging_inode(inode_id).await.unwrap();

    // 4. The published file must still be fully readable.
    let readback = fs.read_file(path).await.unwrap();
    assert_eq!(
        readback, data,
        "cleanup_staging_inode must not delete a published file (nlink > 0)"
    );

    cleanup(&fs, base).await;
}

#[test]
fn test_normalize_completed_parts_sorts_and_rejects_duplicates() {
    let sorted = normalize_completed_parts(vec![
        FsMultipartCompletedPart {
            part_number: 2,
            etag: "etag-2".to_string(),
            checksum_crc32c: None,
        },
        FsMultipartCompletedPart {
            part_number: 1,
            etag: "etag-1".to_string(),
            checksum_crc32c: None,
        },
    ])
    .expect("parts should normalize");
    assert_eq!(sorted[0].part_number, 1);
    assert_eq!(sorted[1].part_number, 2);

    let err = normalize_completed_parts(vec![
        FsMultipartCompletedPart {
            part_number: 1,
            etag: "etag-1".to_string(),
            checksum_crc32c: None,
        },
        FsMultipartCompletedPart {
            part_number: 1,
            etag: "etag-1b".to_string(),
            checksum_crc32c: None,
        },
    ])
    .expect_err("duplicate part numbers must fail");
    assert!(err.to_string().contains("duplicate multipart part number"));
}

#[test]
fn test_stream_routing_requires_explicit_size_for_direct_object() {
    let object_min = u64::try_from(fs9_config().object_min_bytes).unwrap_or(u64::MAX);
    let has_object_storage = fs9_config().s3.is_some();

    assert!(!should_use_direct_object_stream(None, has_object_storage));
    if object_min > 0 {
        assert!(!should_use_direct_object_stream(
            Some(object_min - 1),
            has_object_storage
        ));
    }
    assert_eq!(
        should_use_direct_object_stream(Some(object_min), has_object_storage),
        has_object_storage
    );
}

#[test]
fn test_stream_spool_routes_non_inline_sizes_to_object() {
    let inline_max = fs9_config().inline_max_bytes;

    assert!(!should_route_stream_spool_to_object(0));
    if inline_max > 0 {
        assert!(should_route_stream_spool_to_inline(inline_max as u64));
        assert!(!should_route_stream_spool_to_object(inline_max as u64));
        assert!(should_route_stream_spool_to_object((inline_max as u64) + 1));
    }
}

// ── Real backend subgroup contract tests ───────────────────────────
//
// These tests require a running TiKV instance and are #[ignore]d by
// default. Run with:
//   PD_ENDPOINTS=127.0.0.1:2379 cargo test -p db9-server grouped_write_contract -- --ignored --nocapture

/// Verifies that same-dir files exceeding subgroup_size are split into
/// multiple subgroups, and actual_subgroup_count reflects the real txn count.
#[tokio::test]
#[ignore]
async fn grouped_write_contract_chunked_subgroup_count() {
    let fs = make_fs().await;
    fs.mkdir("/gw_contract_chunk", false, None)
        .await
        .expect("mkdir");

    // Write 5 files into one directory. With subgroup_size=2 (overridden
    // via env), we'd get 3 subgroups. Since we can't override config in
    // tests, use the default (32) and write 65 files -> ceil(65/32) = 3.
    let subgroup_size = crate::extensions::fs::config::fs9_config().grouped_write_subgroup_size;
    let file_count = subgroup_size * 2 + 1; // guarantees 3 subgroups
    let files: Vec<FsBatchWriteFile> = (0..file_count)
        .map(|i| FsBatchWriteFile {
            path: format!("/gw_contract_chunk/f_{:04}.dat", i),
            data: vec![0x41u8; 64],
            mode: None,
        })
        .collect();

    let result = fs
        .batch_write_grouped(files.clone())
        .await
        .expect("batch_write_grouped must succeed");

    // Verify subgroup count = ceil(file_count / subgroup_size)
    let expected_subgroups = file_count.div_ceil(subgroup_size);
    assert_eq!(
        result.actual_subgroup_count, expected_subgroups,
        "actual_subgroup_count must equal ceil({file_count}/{subgroup_size}) = {expected_subgroups}"
    );

    // All entries must succeed
    assert_eq!(result.entries.len(), file_count);
    for entry in &result.entries {
        assert!(
            entry.result.is_ok(),
            "entry {} must succeed: {:?}",
            entry.path,
            entry.result
        );
    }

    // Verify files are actually readable (durability)
    for i in 0..file_count {
        let path = format!("/gw_contract_chunk/f_{:04}.dat", i);
        let data = fs.read_file_capped(&path, 1024).await.expect("read back");
        assert_eq!(data.len(), 64, "file {path} must have correct size");
    }
}

/// Verifies cross-directory partial success: if files span multiple
/// directories, each directory's subgroup is independent. One dir can
/// succeed even if another fails.
#[tokio::test]
#[ignore]
async fn grouped_write_contract_cross_dir_multi_subgroup() {
    let fs = make_fs().await;
    fs.mkdir("/gw_cross_a", false, None).await.expect("mkdir a");
    fs.mkdir("/gw_cross_b", false, None).await.expect("mkdir b");

    let files = vec![
        FsBatchWriteFile {
            path: "/gw_cross_a/x.txt".to_string(),
            data: b"alpha".to_vec(),
            mode: None,
        },
        FsBatchWriteFile {
            path: "/gw_cross_b/y.txt".to_string(),
            data: b"bravo".to_vec(),
            mode: None,
        },
    ];

    let result = fs
        .batch_write_grouped(files)
        .await
        .expect("batch_write_grouped must succeed");

    // 2 directories -> 2 subgroups
    assert_eq!(
        result.actual_subgroup_count, 2,
        "2 distinct parent dirs must produce 2 subgroups"
    );
    assert_eq!(result.entries.len(), 2);
    for entry in &result.entries {
        assert!(entry.result.is_ok(), "entry {} must succeed", entry.path);
    }

    // Verify each file is readable in its own directory
    let a = fs
        .read_file_capped("/gw_cross_a/x.txt", 1024)
        .await
        .expect("read a");
    assert_eq!(a, b"alpha");
    let b = fs
        .read_file_capped("/gw_cross_b/y.txt", 1024)
        .await
        .expect("read b");
    assert_eq!(b, b"bravo");
}

/// Verifies single-dir batch with files <= subgroup_size produces
/// exactly 1 subgroup.
#[tokio::test]
#[ignore]
async fn grouped_write_contract_single_subgroup() {
    let fs = make_fs().await;
    fs.mkdir("/gw_single", false, None).await.expect("mkdir");

    let files = vec![
        FsBatchWriteFile {
            path: "/gw_single/a.txt".to_string(),
            data: b"one".to_vec(),
            mode: None,
        },
        FsBatchWriteFile {
            path: "/gw_single/b.txt".to_string(),
            data: b"two".to_vec(),
            mode: None,
        },
    ];

    let result = fs
        .batch_write_grouped(files)
        .await
        .expect("batch_write_grouped must succeed");

    assert_eq!(
        result.actual_subgroup_count, 1,
        "files within subgroup_size in one dir must produce exactly 1 subgroup"
    );
    assert_eq!(result.entries.len(), 2);
    for entry in &result.entries {
        assert!(entry.result.is_ok(), "entry {} must succeed", entry.path);
    }
}

/// Regression test for #2121: single-file `write_file` must produce an event
/// visible through the Redis-backed `fs9_events()` TVF — the shipped surface
/// that `db9 fs watch` polls.
///
/// Before #2121, `emit_event()` only pushed to the in-memory EventRing but did
/// NOT call `persist_events_async()`, so `db9 fs watch` never saw single-file
/// mutation events.
///
/// This test drives the real shipped path end-to-end:
///   `write_file()` → `emit_event()` → `persist_events_async()` →
///   Redis Streams XADD → `execute_fs9_events_from_redis()` → event visible
///
/// Requires both TiKV (for write_file) and Redis (for event persistence).
#[tokio::test]
#[ignore = "requires TiKV + Redis (REDIS_URL env var)"]
async fn test_write_file_event_visible_through_fs9_events() {
    use crate::extensions::fs::notify::execute_fs9_events_from_redis;

    // Ensure Redis client and event loop are initialized.
    // OnceLock-guarded — safe to call multiple times, only first succeeds.
    let _ = crate::extensions::fs::redis_events::init_redis_client().await;
    crate::extensions::fs::redis_events::spawn_event_loop();

    let fs = make_fs().await;
    let base = "/test_write_file_fs9_events_regression";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    // Snapshot: query fs9_events before our write to get a cursor.
    let before_rows = execute_fs9_events_from_redis(&fs.keyspace, "0", Some(base), 10_000)
        .await
        .unwrap_or_default();
    let since_id = before_rows
        .last()
        .and_then(|r| match &r.values[0] {
            crate::model::Value::Text(id) => Some(id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "0".to_string());

    // Perform a single-file write — the exact path broken before #2121.
    let path = format!("{base}/watch_regression.txt");
    fs.write_file(&path, b"hello from regression test", None)
        .await
        .expect("write_file must succeed");

    // Poll fs9_events() until the event appears (background flush is async,
    // FLUSH_INTERVAL = 50ms, so we retry for up to 2s).
    let mut found = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rows = execute_fs9_events_from_redis(&fs.keyspace, &since_id, Some(base), 100)
            .await
            .unwrap_or_default();
        let matching = rows
            .iter()
            .any(|r| matches!(&r.values[2], crate::model::Value::Text(p) if p == &path));
        if matching {
            // Verify event type is CREATE or WRITE.
            let event_row = rows
                .iter()
                .rfind(|r| matches!(&r.values[2], crate::model::Value::Text(p) if p == &path))
                .unwrap();
            let event_type = match &event_row.values[1] {
                crate::model::Value::Text(t) => t.as_str(),
                _ => "",
            };
            assert!(
                event_type == "CREATE" || event_type == "WRITE",
                "expected CREATE or WRITE event, got '{}'",
                event_type
            );
            found = true;
            break;
        }
    }

    assert!(
        found,
        "write_file event for '{}' not found in fs9_events() after 2s — \
         this is the exact regression that #2121 fixed: single-file mutations \
         were not persisted to Redis Streams",
        path
    );

    cleanup(&fs, base).await;
}

// ── Storage stats probe regression tests (require TiKV) ─────────────
//
// Verifies that probe_superblock_readonly() and aggregate_storage_stats()
// behave correctly on both fresh (uninitialized) and initialized keyspaces.
//
//   cargo test -p db9-server storage_stats_behavioral -- --ignored

/// Fresh keyspace: probe_superblock_readonly returns None without creating
/// any filesystem state. This is a regression guard — the first version of
/// fs9_storage_stats() accidentally walked through load_runtime_superblock
/// which could initialize the filesystem as a side effect.
#[tokio::test]
#[ignore]
async fn test_storage_stats_fresh_keyspace_returns_none_without_init() {
    // Use a unique keyspace that has never been initialized.
    let keyspace = format!("fs9_stats_noinit_{}_{}", std::process::id(), 0);
    let pd_raw = std::env::var("PD_ENDPOINTS").unwrap_or("127.0.0.1:2379".into());
    let pd_addrs: Vec<String> = pd_raw.split(',').map(|s| s.trim().to_string()).collect();
    ensure_behavioral_test_keyspace(&pd_addrs, &keyspace).await;

    let mut config = tikv_client::Config::default().with_keyspace(&keyspace);
    if let (Ok(ca), Ok(cert), Ok(key)) = (
        std::env::var("TIKV_CA_PATH"),
        std::env::var("TIKV_CERT_PATH"),
        std::env::var("TIKV_KEY_PATH"),
    ) {
        config = config.with_security(ca, cert, key);
    }
    let client = TransactionClient::new_with_config(pd_addrs, config)
        .await
        .expect("TiKV connection required for behavioral tests");
    let client = Arc::new(client);

    // Probe should return None (not initialized).
    let result = probe_superblock_readonly(&client).await.unwrap();
    assert!(
        result.is_none(),
        "fresh keyspace must return None from probe_superblock_readonly"
    );

    // Aggregate stats should also work (returns zeros).
    let stats = aggregate_storage_stats(&client).await.unwrap();
    assert_eq!(stats.total_files, 0);
    assert_eq!(stats.total_directories, 0);
    assert_eq!(stats.total_logical_bytes, 0);

    // Verify no side effects: superblock key must still be absent.
    let result_after = probe_superblock_readonly(&client).await.unwrap();
    assert!(
        result_after.is_none(),
        "probe_superblock_readonly must not create fs state as a side effect"
    );
}

/// Initialized keyspace: aggregate_storage_stats returns correct counts.
#[tokio::test]
#[ignore]
async fn test_storage_stats_initialized_keyspace_returns_counts() {
    let fs = make_fs().await;

    // Write some test files.
    let base = "/tmp/storage_stats_test";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;
    fs.write_file(&format!("{base}/a.txt"), b"hello", None)
        .await
        .unwrap();
    fs.write_file(&format!("{base}/b.txt"), b"world!", None)
        .await
        .unwrap();

    // Probe should return Some.
    let result = probe_superblock_readonly(&fs.client).await.unwrap();
    assert!(
        result.is_some(),
        "initialized keyspace must have superblock"
    );

    // Aggregate stats should reflect our files.
    let stats = aggregate_storage_stats(&fs.client).await.unwrap();
    assert!(
        stats.total_files >= 2,
        "expected at least 2 files, got {}",
        stats.total_files
    );
    assert!(
        stats.total_logical_bytes >= 11,
        "expected at least 11 bytes (hello + world!), got {}",
        stats.total_logical_bytes
    );
    assert!(
        stats.total_directories >= 1,
        "expected at least 1 directory, got {}",
        stats.total_directories
    );

    cleanup(&fs, base).await;
}

#[tokio::test]
#[ignore]
async fn test_presign_url_ttl_is_fixed_not_decaying_with_real_s3() {
    // Verifies that presigned URLs get a fixed TTL from config, not a
    // decaying TTL derived from token expires_at. Two presign calls at
    // different times must produce URLs with the same X-Amz-Expires.
    if fs9_config().s3.is_none() || fs9_config().object_min_bytes == 0 {
        return;
    }

    let fs = make_fs().await;
    let base = "/test_presign_fixed_ttl";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/large.bin");
    let upload = fs
        .create_upload(path, fs9_config().object_min_bytes as u64 * 10, None, None)
        .await
        .unwrap();

    // Verify token has extended TTL (>> presign_ttl_secs)
    let claims = verify_upload_token(&upload.upload_token).unwrap();
    let now = current_unix_timestamp();
    let token_ttl = claims.expires_at - now;
    assert!(
        token_ttl > i64::try_from(fs9_config().presign_ttl_secs).unwrap(),
        "token TTL ({token_ttl}s) must exceed presign_ttl_secs ({}s)",
        fs9_config().presign_ttl_secs,
    );

    // Presign part 1 immediately
    let presigned1 = fs
        .presign_upload_part(&upload.upload_token, 1, None)
        .await
        .unwrap();

    // Sleep briefly and presign part 2
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let presigned2 = fs
        .presign_upload_part(&upload.upload_token, 2, None)
        .await
        .unwrap();

    // Extract X-Amz-Expires from both URLs
    fn extract_amz_expires(url: &str) -> Option<u64> {
        url.find("X-Amz-Expires=").and_then(|pos| {
            url[pos + 14..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
    }

    let ttl1 =
        extract_amz_expires(&presigned1.url).expect("presigned URL 1 must contain X-Amz-Expires");
    let ttl2 =
        extract_amz_expires(&presigned2.url).expect("presigned URL 2 must contain X-Amz-Expires");

    assert_eq!(
        ttl1, ttl2,
        "presigned URL TTL must be fixed (no decay): part1={ttl1}s, part2={ttl2}s"
    );
    assert_eq!(
        ttl1,
        fs9_config().presign_ttl_secs,
        "presigned URL TTL must equal presign_ttl_secs config"
    );

    fs.abort_upload(&upload.upload_token).await.ok();
    cleanup(&fs, base).await;
}

/// Regression test for #2287: two concurrent presign_upload_part calls on a
/// stale lifecycle must both succeed thanks to the WriteConflict retry loop.
///
/// Uses a `Barrier(2)` injected via `test_lifecycle_commit_barrier` to force
/// both calls to read the stale lifecycle before either commits the refresh.
/// This guarantees a deterministic WriteConflict — without the retry in
/// `presign_upload_part`, one call would fail with EIO.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn test_presign_upload_part_concurrent_stale_lifecycle_retry() {
    if fs9_config().s3.is_none() || fs9_config().object_min_bytes == 0 {
        return;
    }

    let mut fs = make_fs().await;
    let base = "/test_presign_stale_lifecycle";
    cleanup(&fs, base).await;
    ensure_dir(&fs, base).await;

    let path = &format!("{base}/large.bin");
    let upload = fs
        .create_upload(path, fs9_config().object_min_bytes as u64 * 10, None, None)
        .await
        .unwrap();

    // Backdate the lifecycle's updated_at so both presign calls see
    // needs_refresh=true and race to write _fs_L{inode}.
    let claims = verify_upload_token(&upload.upload_token).unwrap();
    {
        let mut txn = fs.begin_unchecked().await.unwrap();
        let lc = lifecycle::load_lifecycle(&mut txn, claims.staging_inode_id)
            .await
            .unwrap()
            .expect("lifecycle must exist after create_upload");
        if let lifecycle::FileLifecycle::Uploading {
            fs_instance_id,
            upload_id,
            reservation,
            ..
        } = lc
        {
            lifecycle::save_lifecycle(
                &mut txn,
                claims.staging_inode_id,
                &lifecycle::FileLifecycle::Uploading {
                    fs_instance_id,
                    upload_id,
                    // Set updated_at far in the past to trigger refresh on next presign
                    updated_at: 0,
                    reservation,
                },
            )
            .await
            .unwrap();
        } else {
            panic!("expected Uploading lifecycle after create_upload");
        }
        txn.commit().await.unwrap();
    }

    // Install a barrier so both presign calls pause after reading the stale
    // lifecycle but before committing the refresh.  When both arrive at the
    // barrier they proceed simultaneously, guaranteeing a WriteConflict.
    fs.test_lifecycle_commit_barrier = Some(Arc::new(tokio::sync::Barrier::new(2)));

    // Fire two presign requests concurrently on a multi_thread runtime.
    // Both read stale updated_at → both write refresh → barrier → both
    // commit → one wins, one gets WriteConflict → retry → success.
    let token = upload.upload_token.clone();
    let (r1, r2) = tokio::join!(
        fs.presign_upload_part(&token, 1, None),
        fs.presign_upload_part(&token, 2, None),
    );

    assert!(
        r1.is_ok(),
        "presign part 1 must succeed (got: {:?})",
        r1.err()
    );
    assert!(
        r2.is_ok(),
        "presign part 2 must succeed (got: {:?})",
        r2.err()
    );

    fs.abort_upload(&upload.upload_token).await.ok();
    cleanup(&fs, base).await;
}
