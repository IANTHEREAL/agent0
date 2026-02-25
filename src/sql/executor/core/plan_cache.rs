//! Session-local prepared plan cache.
//!
//! Caches optimized `PhysicalPlan` for prepared statements after N identical
//! executions. Each entry tracks schema dependencies `(table_id, schema_version)`
//! for runtime invalidation.

use crate::sql::optimizer::physical_plan::PhysicalPlan;
use std::collections::HashMap;

/// Dependency metadata for a cached plan:
/// `(table_name, table_id, schema_version)`.
///
/// The plan is invalidated when the current schema version for any dependent
/// table differs from the version captured at plan creation time. Using
/// `table_id` (not name) ensures correctness across DROP+CREATE of the same
/// table name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanDependency {
    pub table_name: String,
    pub table_id: u64,
    pub schema_version: u64,
}

/// Cache key: normalized SQL + resolved param types + db_id + search_path +
/// resolved table IDs.
///
/// Two executions produce the same key iff they would generate the same plan
/// (same SQL text, same parameter types, same database, same search path,
/// same resolved base-table IDs).
///
/// Parameter types are stored as their `Debug` representation because
/// `DataType` does not implement `Hash`/`Eq`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PlanCacheKey {
    /// Normalized SQL text (from PreparedStatement.sql).
    pub sql: String,
    /// Resolved parameter types as debug-format strings.
    pub param_type_sigs: Vec<String>,
    /// Current database ID.
    pub db_id: u64,
    /// Active search_path at plan time.
    pub search_path: Vec<String>,
    /// Resolved base-table IDs bound by analysis.
    pub resolved_table_ids: Vec<u64>,
}

impl PlanCacheKey {
    /// Build a cache key from the constituent parts.
    pub fn new(
        sql: String,
        param_types: &[crate::model::DataType],
        db_id: u64,
        search_path: &[String],
        resolved_table_ids: &[u64],
    ) -> Self {
        let mut resolved_table_ids = resolved_table_ids.to_vec();
        resolved_table_ids.sort_unstable();
        resolved_table_ids.dedup();
        Self {
            sql,
            param_type_sigs: param_types.iter().map(|t| format!("{:?}", t)).collect(),
            db_id,
            search_path: search_path.to_vec(),
            resolved_table_ids,
        }
    }
}

/// Cached plan entry: frozen PhysicalPlan + dependency metadata.
#[derive(Debug, Clone)]
pub(crate) struct PlanCacheEntry {
    /// The cached physical plan (cloned from optimizer output).
    pub physical_plan: PhysicalPlan,
    /// Schema dependencies captured at plan creation time.
    pub dependencies: Vec<PlanDependency>,
}

/// Per-entry execution counter for adaptive promotion.
#[derive(Debug)]
struct PromotionState {
    /// Number of times this key has been executed.
    exec_count: u64,
}

/// Session-local LRU-bounded plan cache for prepared statements.
///
/// Plans are promoted into the cache after `min_exec` identical executions
/// (adaptive promotion). Entries are evicted LRU when the cache exceeds
/// `capacity`.
#[derive(Debug)]
pub(crate) struct PreparedPlanCache {
    /// Maximum number of cached plans.
    capacity: usize,
    /// Minimum executions before caching a plan.
    min_exec: u64,
    /// Cached plans keyed by PlanCacheKey.
    entries: HashMap<PlanCacheKey, PlanCacheEntry>,
    /// Execution counters for promotion tracking.
    counters: HashMap<PlanCacheKey, PromotionState>,
    /// LRU ordering: most-recently-used keys at the back.
    lru_order: Vec<PlanCacheKey>,
}

impl PreparedPlanCache {
    /// Create a new cache with the given capacity and promotion threshold.
    pub fn new(capacity: usize, min_exec: u64) -> Self {
        Self {
            capacity,
            min_exec,
            entries: HashMap::new(),
            counters: HashMap::new(),
            lru_order: Vec::new(),
        }
    }

    /// Look up a cached plan by key. Returns `None` if not cached.
    ///
    /// On hit, the entry is moved to the back of the LRU list (most recent).
    pub fn get(&mut self, key: &PlanCacheKey) -> Option<&PlanCacheEntry> {
        if self.entries.contains_key(key) {
            self.touch_lru(key);
            self.entries.get(key)
        } else {
            None
        }
    }

    /// Record an execution for the given key and return the current count.
    ///
    /// The caller should check `count >= min_exec` to decide whether to
    /// promote a plan into the cache.
    pub fn record_execution(&mut self, key: &PlanCacheKey) -> u64 {
        if self.capacity == 0 {
            return 0;
        }
        let state = self
            .counters
            .entry(key.clone())
            .or_insert(PromotionState { exec_count: 0 });
        state.exec_count = state.exec_count.saturating_add(1);
        state.exec_count
    }

    /// Returns the promotion threshold.
    pub fn min_exec(&self) -> u64 {
        self.min_exec
    }

    /// Reconfigure cache capacity and promotion threshold at runtime.
    pub fn reconfigure(&mut self, capacity: usize, min_exec: u64) {
        self.capacity = capacity;
        self.min_exec = min_exec;

        if self.capacity == 0 {
            self.clear();
            return;
        }

        // Trim to the new capacity, evicting least-recently-used entries first.
        while self.entries.len() > self.capacity {
            if let Some(evict_key) = self.lru_order.first().cloned() {
                self.entries.remove(&evict_key);
                self.counters.remove(&evict_key);
                self.lru_order.remove(0);
            } else {
                break;
            }
        }

        // Keep LRU order consistent with current entries.
        self.lru_order.retain(|k| self.entries.contains_key(k));
    }

    /// Insert a plan into the cache, evicting LRU if at capacity.
    pub fn insert(&mut self, key: PlanCacheKey, entry: PlanCacheEntry) {
        if self.capacity == 0 {
            return;
        }

        // If key already exists, replace in-place and touch LRU.
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), entry);
            self.touch_lru(&key);
            return;
        }

        // Evict LRU entries if at capacity.
        while self.entries.len() >= self.capacity {
            if let Some(evict_key) = self.lru_order.first().cloned() {
                self.entries.remove(&evict_key);
                self.counters.remove(&evict_key);
                self.lru_order.remove(0);
            } else {
                break;
            }
        }

        self.entries.insert(key.clone(), entry);
        self.lru_order.push(key);
    }

    /// Invalidate (remove) a specific key.
    #[allow(dead_code)]
    pub fn invalidate(&mut self, key: &PlanCacheKey) {
        self.entries.remove(key);
        self.counters.remove(key);
        self.lru_order.retain(|k| k != key);
    }

    /// Invalidate all entries that depend on the given table_id.
    ///
    /// Called after DDL (CREATE INDEX, DROP INDEX, ALTER TABLE, etc.) to
    /// ensure stale plans are not reused.
    #[allow(dead_code)]
    pub fn invalidate_by_table_id(&mut self, table_id: u64) {
        let keys_to_remove: Vec<PlanCacheKey> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.dependencies.iter().any(|d| d.table_id == table_id))
            .map(|(k, _)| k.clone())
            .collect();
        for key in &keys_to_remove {
            self.entries.remove(key);
            self.counters.remove(key);
        }
        self.lru_order.retain(|k| !keys_to_remove.contains(k));
    }

    /// Clear the entire cache. Called on transaction rollback to prevent
    /// stale entries from surviving DDL that was rolled back.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.counters.clear();
        self.lru_order.clear();
    }

    /// Number of cached entries.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Current maximum number of cached entries.
    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Move a key to the back of the LRU list.
    fn touch_lru(&mut self, key: &PlanCacheKey) {
        self.lru_order.retain(|k| k != key);
        self.lru_order.push(key.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::optimizer::logical_plan::PlanSchema;
    use crate::sql::optimizer::physical_plan::{PhysicalCost, PhysicalNode};

    fn make_key(sql: &str) -> PlanCacheKey {
        PlanCacheKey::new(sql.to_string(), &[], 1, &["public".to_string()], &[])
    }

    fn make_entry(deps: Vec<PlanDependency>) -> PlanCacheEntry {
        PlanCacheEntry {
            physical_plan: PhysicalPlan {
                node: PhysicalNode::Empty,
                schema: PlanSchema::empty(),
                cost: PhysicalCost::default(),
            },
            dependencies: deps,
        }
    }

    #[test]
    fn key_equality_and_hash_stability() {
        let k1 = make_key("SELECT 1");
        let k2 = make_key("SELECT 1");
        assert_eq!(k1, k2);

        let k3 = make_key("SELECT 2");
        assert_ne!(k1, k3);
    }

    #[test]
    fn promotion_threshold_tracks_executions() {
        let mut cache = PreparedPlanCache::new(10, 5);
        let key = make_key("SELECT 1");

        for i in 1..=4 {
            let count = cache.record_execution(&key);
            assert_eq!(count, i);
        }
        assert!(cache.get(&key).is_none());

        let count = cache.record_execution(&key);
        assert_eq!(count, 5);
        // After threshold, caller would insert:
        cache.insert(key.clone(), make_entry(vec![]));
        assert!(cache.get(&key).is_some());
    }

    #[test]
    fn lru_eviction_removes_oldest() {
        let mut cache = PreparedPlanCache::new(2, 1);

        let k1 = make_key("SELECT 1");
        let k2 = make_key("SELECT 2");
        let k3 = make_key("SELECT 3");

        cache.insert(k1.clone(), make_entry(vec![]));
        cache.insert(k2.clone(), make_entry(vec![]));
        assert_eq!(cache.len(), 2);

        // Access k1 to make it most-recent.
        cache.get(&k1);

        // Insert k3 — should evict k2 (least recently used).
        cache.insert(k3.clone(), make_entry(vec![]));
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&k1).is_some());
        assert!(cache.get(&k2).is_none());
        assert!(cache.get(&k3).is_some());
    }

    #[test]
    fn dependency_mismatch_invalidates() {
        let mut cache = PreparedPlanCache::new(10, 1);
        let key = make_key("SELECT * FROM t");
        let deps = vec![PlanDependency {
            table_id: 42,
            table_name: "public.t".to_string(),
            schema_version: 1,
        }];
        cache.insert(key.clone(), make_entry(deps));
        assert!(cache.get(&key).is_some());

        // Simulate DDL on table 42.
        cache.invalidate_by_table_id(42);
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn clear_removes_everything() {
        let mut cache = PreparedPlanCache::new(10, 1);
        let k1 = make_key("SELECT 1");
        let k2 = make_key("SELECT 2");
        cache.insert(k1.clone(), make_entry(vec![]));
        cache.insert(k2.clone(), make_entry(vec![]));
        cache.record_execution(&k1);

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.get(&k1).is_none());
    }

    #[test]
    fn zero_capacity_cache_never_stores() {
        let mut cache = PreparedPlanCache::new(0, 1);
        let key = make_key("SELECT 1");
        assert_eq!(cache.record_execution(&key), 0);
        cache.insert(key.clone(), make_entry(vec![]));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn invalidate_by_table_id_only_affects_matching_deps() {
        let mut cache = PreparedPlanCache::new(10, 1);

        let k1 = make_key("SELECT * FROM t1");
        let k2 = make_key("SELECT * FROM t2");
        cache.insert(
            k1.clone(),
            make_entry(vec![PlanDependency {
                table_id: 10,
                table_name: "public.t1".to_string(),
                schema_version: 1,
            }]),
        );
        cache.insert(
            k2.clone(),
            make_entry(vec![PlanDependency {
                table_id: 20,
                table_name: "public.t2".to_string(),
                schema_version: 1,
            }]),
        );

        cache.invalidate_by_table_id(10);
        assert!(cache.get(&k1).is_none());
        assert!(cache.get(&k2).is_some());
    }

    #[test]
    fn reconfigure_updates_threshold_and_trims_lru() {
        let mut cache = PreparedPlanCache::new(3, 5);
        let k1 = make_key("SELECT 1");
        let k2 = make_key("SELECT 2");
        let k3 = make_key("SELECT 3");

        cache.insert(k1.clone(), make_entry(vec![]));
        cache.insert(k2.clone(), make_entry(vec![]));
        cache.insert(k3.clone(), make_entry(vec![]));
        cache.get(&k2); // make k2 most-recent

        cache.reconfigure(2, 1);
        assert_eq!(cache.min_exec(), 1);
        assert!(cache.get(&k1).is_none());
        assert!(cache.get(&k2).is_some());
        assert!(cache.get(&k3).is_some());
    }

    #[test]
    fn reconfigure_zero_capacity_clears_all_state() {
        let mut cache = PreparedPlanCache::new(2, 1);
        let k1 = make_key("SELECT 1");
        cache.insert(k1.clone(), make_entry(vec![]));
        assert_eq!(cache.record_execution(&k1), 1);

        cache.reconfigure(0, 3);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.record_execution(&k1), 0);
    }

    #[test]
    fn key_differs_when_resolved_table_ids_differ() {
        let k1 = PlanCacheKey::new(
            "SELECT * FROM t".to_string(),
            &[],
            1,
            &["public".to_string()],
            &[10],
        );
        let k2 = PlanCacheKey::new(
            "SELECT * FROM t".to_string(),
            &[],
            1,
            &["public".to_string()],
            &[20],
        );
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_normalizes_table_ids_order_and_duplicates() {
        let k1 = PlanCacheKey::new(
            "SELECT * FROM t1 JOIN t2 ON true".to_string(),
            &[],
            1,
            &["public".to_string()],
            &[2, 1, 1],
        );
        let k2 = PlanCacheKey::new(
            "SELECT * FROM t1 JOIN t2 ON true".to_string(),
            &[],
            1,
            &["public".to_string()],
            &[1, 2],
        );
        assert_eq!(k1, k2);
    }
}
