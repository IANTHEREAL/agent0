use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

static TABLE_STATS: OnceLock<RwLock<HashMap<(u64, u64), usize>>> = OnceLock::new();

fn stats_map() -> &'static RwLock<HashMap<(u64, u64), usize>> {
    TABLE_STATS.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn update_row_count_estimate(db_id: u64, table_id: u64, count: usize) {
    if let Ok(mut map) = stats_map().write() {
        map.insert((db_id, table_id), count);
    }
}

pub fn bump_row_count_estimate(db_id: u64, table_id: u64, delta: isize) {
    if delta == 0 {
        return;
    }

    if let Ok(mut map) = stats_map().write() {
        let key = (db_id, table_id);
        let Some(current) = map.get(&key).copied() else {
            return;
        };

        let next = if delta.is_positive() {
            current.saturating_add(delta.unsigned_abs())
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        map.insert(key, next);
    }
}

pub fn get_row_count_estimate(db_id: u64, table_id: u64) -> Option<usize> {
    stats_map().read().ok()?.get(&(db_id, table_id)).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_update_and_get() {
        update_row_count_estimate(1, 100, 5000);
        assert_eq!(get_row_count_estimate(1, 100), Some(5000));
    }

    #[test]
    fn test_stats_missing_returns_none() {
        assert_eq!(get_row_count_estimate(999, 999), None);
    }

    #[test]
    fn test_stats_overwrite() {
        update_row_count_estimate(2, 200, 5000);
        update_row_count_estimate(2, 200, 3000);
        assert_eq!(get_row_count_estimate(2, 200), Some(3000));
    }

    #[test]
    fn test_stats_bump() {
        update_row_count_estimate(3, 300, 10);
        bump_row_count_estimate(3, 300, 5);
        assert_eq!(get_row_count_estimate(3, 300), Some(15));
        bump_row_count_estimate(3, 300, -1000);
        assert_eq!(get_row_count_estimate(3, 300), Some(0));
    }
}
