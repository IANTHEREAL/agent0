use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::LazyLock;

static TABLE_STATS: LazyLock<DashMap<String, HashMap<(u64, u64), usize>>> =
    LazyLock::new(DashMap::new);

pub fn update_row_count_estimate(keyspace: &str, db_id: u64, table_id: u64, count: usize) {
    TABLE_STATS
        .entry(keyspace.to_string())
        .or_default()
        .insert((db_id, table_id), count);
}

pub fn bump_row_count_estimate(keyspace: &str, db_id: u64, table_id: u64, delta: isize) {
    if delta == 0 {
        return;
    }

    let Some(mut inner) = TABLE_STATS.get_mut(keyspace) else {
        return;
    };
    let key = (db_id, table_id);
    let Some(current) = inner.get(&key).copied() else {
        return;
    };

    let next = if delta.is_positive() {
        current.saturating_add(delta.unsigned_abs())
    } else {
        current.saturating_sub(delta.unsigned_abs())
    };
    inner.insert(key, next);
}

pub fn get_row_count_estimate(keyspace: &str, db_id: u64, table_id: u64) -> Option<usize> {
    TABLE_STATS.get(keyspace)?.get(&(db_id, table_id)).copied()
}

#[allow(dead_code)] // API ready for pool lifecycle wiring
pub fn evict_keyspace_stats(keyspace: &str) {
    TABLE_STATS.remove(keyspace);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_update_and_get() {
        update_row_count_estimate("test_ks", 1, 100, 5000);
        assert_eq!(get_row_count_estimate("test_ks", 1, 100), Some(5000));
    }

    #[test]
    fn test_stats_missing_returns_none() {
        assert_eq!(get_row_count_estimate("missing_ks", 999, 999), None);
    }

    #[test]
    fn test_stats_overwrite() {
        update_row_count_estimate("overwrite_ks", 2, 200, 5000);
        update_row_count_estimate("overwrite_ks", 2, 200, 3000);
        assert_eq!(get_row_count_estimate("overwrite_ks", 2, 200), Some(3000));
    }

    #[test]
    fn test_stats_bump() {
        update_row_count_estimate("bump_ks", 3, 300, 10);
        bump_row_count_estimate("bump_ks", 3, 300, 5);
        assert_eq!(get_row_count_estimate("bump_ks", 3, 300), Some(15));
        bump_row_count_estimate("bump_ks", 3, 300, -1000);
        assert_eq!(get_row_count_estimate("bump_ks", 3, 300), Some(0));
    }

    #[test]
    fn test_table_stats_tenant_isolation() {
        update_row_count_estimate("ks_iso_a", 1, 1, 1000);
        update_row_count_estimate("ks_iso_b", 1, 1, 5);

        assert_eq!(get_row_count_estimate("ks_iso_a", 1, 1), Some(1000));
        assert_eq!(get_row_count_estimate("ks_iso_b", 1, 1), Some(5));
    }

    #[test]
    fn test_evict_keyspace_stats() {
        update_row_count_estimate("evict_test", 1, 1, 100);
        assert_eq!(get_row_count_estimate("evict_test", 1, 1), Some(100));

        evict_keyspace_stats("evict_test");
        assert_eq!(get_row_count_estimate("evict_test", 1, 1), None);
    }
}
