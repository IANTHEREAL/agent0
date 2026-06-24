//! Background worker for periodic fs9 storage stats computation.
//!
//! Spawns a tokio task at server startup that periodically calls
//! `aggregate_storage_stats()` and caches the result in-memory.
//! The cache provides O(1) reads for `db9 inspect` (via the system TVF
//! `fs9_cached_storage_stats()`). The existing `fs9_storage_stats()` TVF
//! is preserved as an independent live-scan path.
//!
//! Design invariants:
//! - Cache miss returns None — **never** falls back to live scan.
//! - Two strict paths: inspect reads cache (O(1)); TVF does live scan.
//! - Worker period is configurable via `FS9_STATS_REFRESH_INTERVAL_SECS`.
// TODO(#2335): migrate to parking_lot — phase 2/3
#![allow(clippy::disallowed_types)]

use std::sync::{OnceLock, RwLock};
use std::time::Duration;
use tikv_client::TransactionClient;
use tracing::{info, warn};

/// Cached fs9 storage statistics with a computation timestamp.
#[derive(Debug, Clone)]
pub(crate) struct CachedFsStats {
    pub total_files: i64,
    pub total_directories: i64,
    pub total_logical_bytes: i64,
    /// Unix epoch seconds when the stats were computed.
    pub computed_at: i64,
}

// ---------------------------------------------------------------------------
// Global in-memory cache
// ---------------------------------------------------------------------------

static FS_STATS_CACHE: OnceLock<RwLock<Option<CachedFsStats>>> = OnceLock::new();

fn cache() -> &'static RwLock<Option<CachedFsStats>> {
    FS_STATS_CACHE.get_or_init(|| RwLock::new(None))
}

/// Read the cached fs9 storage stats. Returns `None` on cache miss.
/// This is the O(1) path for `db9 inspect`.
pub(crate) fn get_cached_fs_stats() -> Option<CachedFsStats> {
    cache().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Update the cache with fresh stats.
fn set_cached_fs_stats(stats: CachedFsStats) {
    let mut guard = cache().write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(stats);
}

fn cached_stats_staleness_secs() -> u64 {
    get_cached_fs_stats()
        .map(|stats| now_epoch_secs().saturating_sub(stats.computed_at).max(0) as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Background worker loop
// ---------------------------------------------------------------------------

/// Default refresh interval in seconds.
const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 60;

/// Read the configured refresh interval from `FS9_STATS_REFRESH_INTERVAL_SECS`.
fn refresh_interval() -> Duration {
    let secs = std::env::var("FS9_STATS_REFRESH_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_REFRESH_INTERVAL_SECS);
    Duration::from_secs(secs.max(5)) // floor at 5s to avoid tight loops
}

/// The main worker loop. Runs indefinitely, computing stats on each tick.
/// Intended to be wrapped in `supervised_background_loop` for panic recovery.
pub(crate) async fn run_fs9_stats_worker(client: std::sync::Arc<TransactionClient>) {
    let interval = refresh_interval();
    info!(
        "fs9 stats worker started (interval={}s)",
        interval.as_secs()
    );

    let mut ticker = tokio::time::interval(interval);
    // First tick fires immediately — populate cache on startup.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;

        // Check if fs9 is initialized before doing the expensive scan.
        match super::embedded::pagefs::probe_superblock_readonly(&client).await {
            Ok(None) => {
                // No filesystem initialized — cache zeros.
                set_cached_fs_stats(CachedFsStats {
                    total_files: 0,
                    total_directories: 0,
                    total_logical_bytes: 0,
                    computed_at: now_epoch_secs(),
                });
                continue;
            }
            Ok(Some(_)) => {
                // Filesystem exists — proceed to scan.
            }
            Err(e) => {
                warn!("fs9 stats worker: superblock probe failed: {e}");
                continue; // Keep stale cache rather than clearing it.
            }
        }

        let scan_start = std::time::Instant::now();
        match super::embedded::pagefs::aggregate_storage_stats(&client).await {
            Ok(stats) => {
                let elapsed = scan_start.elapsed();
                info!(
                    "fs9 stats worker: scan completed in {:?} — files={} dirs={} bytes={}",
                    elapsed, stats.total_files, stats.total_directories, stats.total_logical_bytes
                );
                set_cached_fs_stats(CachedFsStats {
                    total_files: stats.total_files,
                    total_directories: stats.total_directories,
                    total_logical_bytes: stats.total_logical_bytes,
                    computed_at: now_epoch_secs(),
                });
                crate::metrics::record_fs9_stats_worker_scan("ok", elapsed, 0);
            }
            Err(e) => {
                let elapsed = scan_start.elapsed();
                warn!("fs9 stats worker: aggregate_storage_stats failed: {e}");
                crate::metrics::record_fs9_stats_worker_scan(
                    "err",
                    elapsed,
                    cached_stats_staleness_secs(),
                );
                // Keep stale cache rather than clearing it.
            }
        }
    }
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Cached stats TVF: fs9_cached_storage_stats()
// ---------------------------------------------------------------------------

use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};

/// Schema for the `fs9_cached_storage_stats()` TVF.
pub(crate) fn fs9_cached_storage_stats_schema() -> TableSchema {
    TableSchema::virtual_table(
        "fs9_cached_storage_stats",
        vec![
            ColumnDef::new("total_files", DataType::Int64, false),
            ColumnDef::new("total_directories", DataType::Int64, false),
            ColumnDef::new("total_logical_bytes", DataType::Int64, false),
            ColumnDef::new("computed_at", DataType::Int64, false),
        ],
    )
}

/// Execute the cached stats TVF. Returns cached values or NULL row on cache miss.
pub(crate) fn execute_fs9_cached_storage_stats() -> Vec<Row> {
    match get_cached_fs_stats() {
        Some(stats) => vec![Row::new(vec![
            Value::Int64(stats.total_files),
            Value::Int64(stats.total_directories),
            Value::Int64(stats.total_logical_bytes),
            Value::Int64(stats.computed_at),
        ])],
        None => vec![Row::new(vec![
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ])],
    }
}
