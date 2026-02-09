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
}
