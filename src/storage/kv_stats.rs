use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub struct KvReadStats {
    table_scan_pairs: AtomicU64,
    index_scan_pairs: AtomicU64,
    batch_get_keys: AtomicU64,
    gin_scan_keys: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KvReadStatsSnapshot {
    pub table_scan_pairs: u64,
    pub index_scan_pairs: u64,
    pub batch_get_keys: u64,
    pub gin_scan_keys: u64,
}

impl KvReadStats {
    fn snapshot(&self) -> KvReadStatsSnapshot {
        KvReadStatsSnapshot {
            table_scan_pairs: self.table_scan_pairs.load(Ordering::Relaxed),
            index_scan_pairs: self.index_scan_pairs.load(Ordering::Relaxed),
            batch_get_keys: self.batch_get_keys.load(Ordering::Relaxed),
            gin_scan_keys: self.gin_scan_keys.load(Ordering::Relaxed),
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

pub fn record_gin_scan_keys(keys: usize) {
    let _ = KV_READ_STATS.try_with(|s| {
        s.gin_scan_keys.fetch_add(keys as u64, Ordering::Relaxed);
    });
}
