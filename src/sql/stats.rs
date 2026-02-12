use dashmap::DashMap;

/// Per-tenant cache for table row-count estimates used by the query planner.
///
/// Owned by `TenantEntry` in the pool — when the reaper drops the entry,
/// the cache is dropped automatically (no manual eviction needed).
pub(crate) struct TableStatsCache {
    inner: DashMap<(u64, u64), usize>, // (db_id, table_id) → estimated row count
}

impl TableStatsCache {
    pub(crate) fn new() -> Self {
        Self {
            inner: DashMap::new(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
