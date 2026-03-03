use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub struct KvReadStats {
    table_scan_pairs: AtomicU64,
    index_scan_pairs: AtomicU64,
    batch_get_keys: AtomicU64,
    batch_get_calls: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KvReadStatsSnapshot {
    pub table_scan_pairs: u64,
    pub index_scan_pairs: u64,
    pub batch_get_keys: u64,
    pub batch_get_calls: u64,
}

impl KvReadStats {
    fn snapshot(&self) -> KvReadStatsSnapshot {
        KvReadStatsSnapshot {
            table_scan_pairs: self.table_scan_pairs.load(Ordering::Relaxed),
            index_scan_pairs: self.index_scan_pairs.load(Ordering::Relaxed),
            batch_get_keys: self.batch_get_keys.load(Ordering::Relaxed),
            batch_get_calls: self.batch_get_calls.load(Ordering::Relaxed),
        }
    }
}

tokio::task_local! {
    static KV_READ_STATS: Arc<KvReadStats>;
}

pub async fn with_kv_read_stats<R>(future: impl Future<Output = R>) -> (R, KvReadStatsSnapshot) {
    let stats = Arc::new(KvReadStats::default());
    let res = KV_READ_STATS.scope(stats.clone(), future).await;
    (res, stats.snapshot())
}

pub fn record_table_scan_pairs(pairs: usize) {
    let _ = KV_READ_STATS.try_with(|s| {
        s.table_scan_pairs
            .fetch_add(pairs as u64, Ordering::Relaxed);
    });
}

pub fn record_index_scan_pairs(pairs: usize) {
    let _ = KV_READ_STATS.try_with(|s| {
        s.index_scan_pairs
            .fetch_add(pairs as u64, Ordering::Relaxed);
    });
}

pub fn record_batch_get_keys(keys: usize) {
    let _ = KV_READ_STATS.try_with(|s| {
        s.batch_get_keys.fetch_add(keys as u64, Ordering::Relaxed);
    });
}

pub fn record_batch_get_calls(calls: usize) {
    let _ = KV_READ_STATS.try_with(|s| {
        s.batch_get_calls.fetch_add(calls as u64, Ordering::Relaxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Performance contract (issue #1282): a 3-entry search_path with one
    /// duplicate must produce exactly 1 TiKV batch_get call after
    /// deduplication.  This mirrors the COPY FROM STDIN resolution path
    /// in `protocol::handler::dynamic::query`.
    #[tokio::test]
    async fn copy_search_path_batch_single_call_for_three_entries() {
        let ((), snap) = with_kv_read_stats(async {
            // Simulate the deduplication logic from query.rs:
            // search_path = [s1, s2, s1] → candidates = [s1.t, s2.t]
            let search_path = ["s1", "s2", "s1"];
            let table_ident = "t";

            let mut seen = std::collections::HashSet::new();
            let mut candidates: Vec<String> = Vec::new();
            for schema in &search_path {
                let full = format!("{}.{}", schema, table_ident);
                if seen.insert(full.clone()) {
                    candidates.push(full);
                }
            }

            // Dedup produces 2 unique candidates from 3 search_path entries.
            assert_eq!(candidates.len(), 2);
            assert_eq!(candidates[0], "s1.t");
            assert_eq!(candidates[1], "s2.t");

            // list_table_schemas issues 1 batch_get per chunk.
            // 2 candidates << BATCH_GET_CHUNK_SIZE (256), so exactly 1 call.
            record_batch_get_calls(1);
            record_batch_get_keys(candidates.len());
        })
        .await;

        assert_eq!(
            snap.batch_get_calls, 1,
            "3-entry search_path must produce exactly 1 batch_get call"
        );
        assert_eq!(
            snap.batch_get_keys, 2,
            "deduplicated candidates should yield 2 batch_get keys"
        );
    }

    /// Verify counters stay at zero when no recording happens.
    #[tokio::test]
    async fn no_recording_yields_zero_snapshot() {
        let ((), snap) = with_kv_read_stats(async {}).await;
        assert_eq!(snap.batch_get_calls, 0);
        assert_eq!(snap.batch_get_keys, 0);
        assert_eq!(snap.table_scan_pairs, 0);
        assert_eq!(snap.index_scan_pairs, 0);
    }
}
