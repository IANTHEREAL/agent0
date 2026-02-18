use crate::sql::optimizer::statistics::TableStatistics;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Per-tenant cache for table statistics used by the query planner.
///
/// Owned by `TenantEntry` in the pool — when the reaper drops the entry,
/// the cache is dropped automatically (no manual eviction needed).
pub(crate) struct TableStatsCache {
    /// Quick row-count estimates (populated by full scans and DML bumps).
    inner: DashMap<(u64, u64), usize>, // (db_id, table_id) → estimated row count
    /// Full column statistics collected by ANALYZE.
    full_stats: DashMap<(u64, u64), Arc<TableStatistics>>,
    /// Per-table modification count since last ANALYZE.
    /// Key: (db_id, table_id), Value: modification count (atomic for lock-free DML path)
    mod_counts: DashMap<(u64, u64), AtomicU64>,
}

impl TableStatsCache {
    pub(crate) fn new() -> Self {
        Self {
            inner: DashMap::new(),
            full_stats: DashMap::new(),
            mod_counts: DashMap::new(),
        }
    }

    pub(crate) fn update_estimate(&self, db_id: u64, table_id: u64, count: usize) {
        self.inner.insert((db_id, table_id), count);
    }

    pub(crate) fn bump_estimate(&self, db_id: u64, table_id: u64, delta: isize) {
        if delta == 0 {
            return;
        }

        let key = (db_id, table_id);
        let Some(current) = self.inner.get(&key).map(|r| *r) else {
            return;
        };

        let next = if delta.is_positive() {
            current.saturating_add(delta.unsigned_abs())
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.inner.insert(key, next);
    }

    pub(crate) fn get_estimate(&self, db_id: u64, table_id: u64) -> Option<usize> {
        self.inner.get(&(db_id, table_id)).map(|r| *r)
    }

    /// Cache full column statistics collected by ANALYZE.
    ///
    /// Also syncs the row-count estimate from `stats.row_count`.
    pub(crate) fn update_full_stats(&self, db_id: u64, table_id: u64, stats: Arc<TableStatistics>) {
        self.inner.insert((db_id, table_id), stats.row_count);
        self.full_stats.insert((db_id, table_id), stats);
    }

    /// Retrieve cached full column statistics, if available.
    pub(crate) fn get_full_stats(&self, db_id: u64, table_id: u64) -> Option<Arc<TableStatistics>> {
        self.full_stats
            .get(&(db_id, table_id))
            .map(|r| Arc::clone(&r))
    }

    /// Remove all cached data for a table (row-count estimate + full stats).
    ///
    /// Called on DROP TABLE and structural ALTER TABLE operations
    /// (AddColumn, DropColumn, RenameColumn, AlterColumn SET DATA TYPE)
    /// to prevent stale statistics from influencing the planner.
    pub(crate) fn invalidate(&self, db_id: u64, table_id: u64) {
        self.inner.remove(&(db_id, table_id));
        self.full_stats.remove(&(db_id, table_id));
    }

    /// Increment modification count. Called on INSERT/UPDATE/DELETE.
    pub(crate) fn bump_mod_count(&self, db_id: u64, table_id: u64, delta: u64) {
        self.mod_counts
            .entry((db_id, table_id))
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(delta, Ordering::Relaxed);
    }

    /// Get current modification count since last ANALYZE.
    pub(crate) fn get_mod_count(&self, db_id: u64, table_id: u64) -> u64 {
        self.mod_counts
            .get(&(db_id, table_id))
            .map(|v| v.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Reset modification count (called after ANALYZE completes).
    pub(crate) fn reset_mod_count(&self, db_id: u64, table_id: u64) {
        if let Some(v) = self.mod_counts.get(&(db_id, table_id)) {
            v.store(0, Ordering::Relaxed);
        }
    }

    /// Check if table needs auto-ANALYZE based on threshold formula.
    /// Formula: mod_count > threshold_base + 0.1 * estimated_rows
    pub(crate) fn needs_auto_analyze(
        &self,
        db_id: u64,
        table_id: u64,
        threshold_base: u64,
    ) -> bool {
        let mod_count = self.get_mod_count(db_id, table_id);
        let estimated_rows = self.get_estimate(db_id, table_id).unwrap_or(0) as u64;
        mod_count > threshold_base + estimated_rows / 10
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::optimizer::statistics::ColumnStatistics;
    use std::collections::HashMap;

    #[test]
    fn test_stats_update_and_get() {
        let cache = TableStatsCache::new();
        cache.update_estimate(1, 100, 5000);
        assert_eq!(cache.get_estimate(1, 100), Some(5000));
    }

    #[test]
    fn test_stats_missing_returns_none() {
        let cache = TableStatsCache::new();
        assert_eq!(cache.get_estimate(999, 999), None);
    }

    #[test]
    fn test_stats_overwrite() {
        let cache = TableStatsCache::new();
        cache.update_estimate(2, 200, 5000);
        cache.update_estimate(2, 200, 3000);
        assert_eq!(cache.get_estimate(2, 200), Some(3000));
    }

    #[test]
    fn test_stats_bump() {
        let cache = TableStatsCache::new();
        cache.update_estimate(3, 300, 10);
        cache.bump_estimate(3, 300, 5);
        assert_eq!(cache.get_estimate(3, 300), Some(15));
        cache.bump_estimate(3, 300, -1000);
        assert_eq!(cache.get_estimate(3, 300), Some(0));
    }

    #[test]
    fn test_table_stats_instance_isolation() {
        let cache_a = TableStatsCache::new();
        let cache_b = TableStatsCache::new();

        cache_a.update_estimate(1, 1, 1000);
        cache_b.update_estimate(1, 1, 5);

        assert_eq!(cache_a.get_estimate(1, 1), Some(1000));
        assert_eq!(cache_b.get_estimate(1, 1), Some(5));
    }

    #[test]
    fn test_drop_clears_cache() {
        let cache = TableStatsCache::new();
        cache.update_estimate(1, 1, 100);
        assert_eq!(cache.get_estimate(1, 1), Some(100));

        drop(cache);
        // After drop, a new cache has no entries
        let cache2 = TableStatsCache::new();
        assert_eq!(cache2.get_estimate(1, 1), None);
    }

    #[test]
    fn test_full_stats_update_and_get() {
        let cache = TableStatsCache::new();
        let stats = Arc::new(TableStatistics {
            table_id: 42,
            row_count: 5000,
            last_analyzed: 1708100000000,
            columns: HashMap::new(),
        });

        cache.update_full_stats(1, 42, stats.clone());

        // Full stats should be retrievable
        let retrieved = cache.get_full_stats(1, 42);
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.table_id, 42);
        assert_eq!(retrieved.row_count, 5000);

        // Row-count estimate should be synced
        assert_eq!(cache.get_estimate(1, 42), Some(5000));
    }

    #[test]
    fn test_full_stats_missing_returns_none() {
        let cache = TableStatsCache::new();
        assert!(cache.get_full_stats(1, 999).is_none());
    }

    #[test]
    fn test_full_stats_overwrite() {
        let cache = TableStatsCache::new();
        let stats1 = Arc::new(TableStatistics {
            table_id: 10,
            row_count: 100,
            last_analyzed: 1000,
            columns: HashMap::new(),
        });
        let stats2 = Arc::new(TableStatistics {
            table_id: 10,
            row_count: 200,
            last_analyzed: 2000,
            columns: HashMap::new(),
        });

        cache.update_full_stats(1, 10, stats1);
        cache.update_full_stats(1, 10, stats2);

        let retrieved = cache.get_full_stats(1, 10).unwrap();
        assert_eq!(retrieved.row_count, 200);
        assert_eq!(retrieved.last_analyzed, 2000);
        assert_eq!(cache.get_estimate(1, 10), Some(200));
    }

    #[test]
    fn test_invalidate_removes_both() {
        let cache = TableStatsCache::new();
        let stats = Arc::new(TableStatistics {
            table_id: 42,
            row_count: 5000,
            last_analyzed: 1000,
            columns: HashMap::new(),
        });

        cache.update_full_stats(1, 42, stats);
        assert!(cache.get_estimate(1, 42).is_some());
        assert!(cache.get_full_stats(1, 42).is_some());

        cache.invalidate(1, 42);

        assert!(cache.get_estimate(1, 42).is_none());
        assert!(cache.get_full_stats(1, 42).is_none());
    }

    #[test]
    fn test_invalidate_does_not_affect_other_tables() {
        let cache = TableStatsCache::new();
        cache.update_estimate(1, 10, 100);
        cache.update_estimate(1, 20, 200);

        let stats = Arc::new(TableStatistics {
            table_id: 10,
            row_count: 100,
            last_analyzed: 1000,
            columns: HashMap::new(),
        });
        cache.update_full_stats(1, 10, stats);

        cache.invalidate(1, 10);

        assert!(cache.get_estimate(1, 10).is_none());
        assert_eq!(cache.get_estimate(1, 20), Some(200));
    }

    #[test]
    fn test_bump_estimate_and_full_stats_independent() {
        let cache = TableStatsCache::new();
        let mut columns = HashMap::new();
        columns.insert("id".to_string(), ColumnStatistics::empty());

        let stats = Arc::new(TableStatistics {
            table_id: 42,
            row_count: 1000,
            last_analyzed: 1000,
            columns,
        });

        cache.update_full_stats(1, 42, stats);
        assert_eq!(cache.get_estimate(1, 42), Some(1000));

        // DML bumps modify the row-count estimate but not full stats
        cache.bump_estimate(1, 42, 50);
        assert_eq!(cache.get_estimate(1, 42), Some(1050));

        // Full stats still reflect the ANALYZE-time value
        let full = cache.get_full_stats(1, 42).unwrap();
        assert_eq!(full.row_count, 1000);
    }

    /// Simulates the get_or_load_stats warm-up pattern:
    /// cache miss → load from storage → populate cache → subsequent cache hit.
    #[test]
    fn test_warmup_miss_then_populate() {
        let cache = TableStatsCache::new();

        // Initial state: cache is empty (simulates post-restart).
        assert!(cache.get_full_stats(1, 42).is_none());

        // Simulate storage load returning stats — caller populates cache.
        let loaded = Arc::new(TableStatistics {
            table_id: 42,
            row_count: 500,
            last_analyzed: 2000,
            columns: HashMap::new(),
        });
        cache.update_full_stats(1, 42, loaded);

        // Subsequent lookups should hit the cache.
        let cached = cache.get_full_stats(1, 42).unwrap();
        assert_eq!(cached.row_count, 500);
        assert_eq!(cache.get_estimate(1, 42), Some(500));
    }

    /// Simulates structural ALTER TABLE → ANALYZE cycle:
    /// stats exist → invalidate (ALTER TABLE) → miss → re-populate (re-ANALYZE).
    #[test]
    fn test_invalidate_then_repopulate() {
        let cache = TableStatsCache::new();

        // Phase 1: ANALYZE populates stats.
        let stats_v1 = Arc::new(TableStatistics {
            table_id: 10,
            row_count: 100,
            last_analyzed: 1000,
            columns: HashMap::new(),
        });
        cache.update_full_stats(1, 10, stats_v1);
        assert!(cache.get_full_stats(1, 10).is_some());

        // Phase 2: Structural ALTER TABLE invalidates.
        cache.invalidate(1, 10);
        assert!(cache.get_full_stats(1, 10).is_none());
        assert!(cache.get_estimate(1, 10).is_none());

        // Phase 3: Re-ANALYZE populates fresh stats.
        let stats_v2 = Arc::new(TableStatistics {
            table_id: 10,
            row_count: 200,
            last_analyzed: 2000,
            columns: HashMap::new(),
        });
        cache.update_full_stats(1, 10, stats_v2);
        let fresh = cache.get_full_stats(1, 10).unwrap();
        assert_eq!(fresh.row_count, 200);
        assert_eq!(fresh.last_analyzed, 2000);
        assert_eq!(cache.get_estimate(1, 10), Some(200));
    }

    /// No-op ALTER TABLE should NOT call invalidate — stats survive.
    /// This test documents the contract: only the caller decides to invalidate.
    #[test]
    fn test_noop_alter_preserves_stats() {
        let cache = TableStatsCache::new();

        let stats = Arc::new(TableStatistics {
            table_id: 42,
            row_count: 300,
            last_analyzed: 1500,
            columns: HashMap::new(),
        });
        cache.update_full_stats(1, 42, stats);

        // Simulate no-op ALTER TABLE (no invalidate call) — stats survive.
        // (The executor skips invalidation when ddl returns None.)
        assert!(cache.get_full_stats(1, 42).is_some());
        assert_eq!(cache.get_full_stats(1, 42).unwrap().row_count, 300);
        assert_eq!(cache.get_estimate(1, 42), Some(300));
    }

    #[test]
    fn test_bump_mod_count_increments() {
        let cache = TableStatsCache::new();
        assert_eq!(cache.get_mod_count(1, 100), 0);

        cache.bump_mod_count(1, 100, 5);
        assert_eq!(cache.get_mod_count(1, 100), 5);

        cache.bump_mod_count(1, 100, 3);
        assert_eq!(cache.get_mod_count(1, 100), 8);
    }

    #[test]
    fn test_reset_mod_count_resets_to_zero() {
        let cache = TableStatsCache::new();
        cache.bump_mod_count(1, 100, 10);
        assert_eq!(cache.get_mod_count(1, 100), 10);

        cache.reset_mod_count(1, 100);
        assert_eq!(cache.get_mod_count(1, 100), 0);
    }

    #[test]
    fn test_reset_mod_count_nonexistent_table() {
        let cache = TableStatsCache::new();
        cache.reset_mod_count(1, 999);
        assert_eq!(cache.get_mod_count(1, 999), 0);
    }

    #[test]
    fn test_needs_auto_analyze_with_1000_rows_threshold_50() {
        let cache = TableStatsCache::new();
        cache.update_estimate(1, 100, 1000);

        let threshold = 50;
        let expected_trigger = 50 + 1000 / 10;

        assert!(!cache.needs_auto_analyze(1, 100, threshold));

        cache.bump_mod_count(1, 100, expected_trigger);
        assert!(!cache.needs_auto_analyze(1, 100, threshold));

        cache.bump_mod_count(1, 100, 1);
        assert!(cache.needs_auto_analyze(1, 100, threshold));
    }

    #[test]
    fn test_needs_auto_analyze_with_zero_rows() {
        let cache = TableStatsCache::new();
        cache.update_estimate(1, 100, 0);

        let threshold = 50;
        assert!(!cache.needs_auto_analyze(1, 100, threshold));

        cache.bump_mod_count(1, 100, 50);
        assert!(!cache.needs_auto_analyze(1, 100, threshold));

        cache.bump_mod_count(1, 100, 1);
        assert!(cache.needs_auto_analyze(1, 100, threshold));
    }

    #[test]
    fn test_needs_auto_analyze_no_estimate() {
        let cache = TableStatsCache::new();

        let threshold = 50;
        assert!(!cache.needs_auto_analyze(1, 100, threshold));

        cache.bump_mod_count(1, 100, 50);
        assert!(!cache.needs_auto_analyze(1, 100, threshold));

        cache.bump_mod_count(1, 100, 1);
        assert!(cache.needs_auto_analyze(1, 100, threshold));
    }

    #[test]
    fn test_mod_count_independent_per_table() {
        let cache = TableStatsCache::new();
        cache.bump_mod_count(1, 100, 10);
        cache.bump_mod_count(1, 200, 20);

        assert_eq!(cache.get_mod_count(1, 100), 10);
        assert_eq!(cache.get_mod_count(1, 200), 20);

        cache.reset_mod_count(1, 100);
        assert_eq!(cache.get_mod_count(1, 100), 0);
        assert_eq!(cache.get_mod_count(1, 200), 20);
    }

    #[test]
    fn test_bump_mod_count_multiple_small_increments() {
        let cache = TableStatsCache::new();
        for _ in 0..5 {
            cache.bump_mod_count(1, 50, 1);
        }
        assert_eq!(cache.get_mod_count(1, 50), 5);
    }

    #[test]
    fn test_needs_auto_analyze_large_table() {
        let cache = TableStatsCache::new();
        cache.update_estimate(1, 100, 10_000_000);
        let threshold = 50;
        let expected_trigger = 50 + 10_000_000 / 10;

        cache.bump_mod_count(1, 100, expected_trigger);
        assert!(!cache.needs_auto_analyze(1, 100, threshold));

        cache.bump_mod_count(1, 100, 1);
        assert!(cache.needs_auto_analyze(1, 100, threshold));
    }

    #[test]
    fn test_mod_count_cross_database_isolation() {
        let cache = TableStatsCache::new();
        cache.bump_mod_count(1, 100, 10);
        cache.bump_mod_count(2, 100, 20);

        assert_eq!(cache.get_mod_count(1, 100), 10);
        assert_eq!(cache.get_mod_count(2, 100), 20);

        cache.reset_mod_count(1, 100);
        assert_eq!(cache.get_mod_count(1, 100), 0);
        assert_eq!(cache.get_mod_count(2, 100), 20);
    }

    #[test]
    fn test_bump_estimate_without_prior_is_noop() {
        let cache = TableStatsCache::new();
        cache.bump_estimate(1, 999, 100);
        assert_eq!(cache.get_estimate(1, 999), None);
    }
}
