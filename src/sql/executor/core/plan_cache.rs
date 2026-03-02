//! Session-local prepared plan cache.
//!
//! Caches optimized `PhysicalPlan` for prepared statements after N identical
//! executions. Each entry tracks schema dependencies `(table_id, schema_version)`
//! for runtime invalidation.

use crate::sql::optimizer::physical_plan::PhysicalPlan;
use std::collections::{HashMap, HashSet};

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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PlanCacheKey {
    /// Normalized SQL text (from PreparedStatement.sql).
    pub sql: String,
    /// Resolved parameter types.
    pub param_types: Vec<crate::model::DataType>,
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
            param_types: param_types.to_vec(),
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

/// Result of recording a prepared execution for promotion tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromotionCounterOutcome {
    /// Miss-path execution count for a key that is not yet cached.
    MissCount(u64),
    /// Execution was intentionally not tracked for promotion.
    ///
    /// This is returned when:
    /// - cache capacity is zero, or
    /// - key is already promoted in `entries`.
    ///
    /// Callers must never use this to drive promotion decisions.
    NotTracked,
}

#[derive(Debug)]
struct LruState {
    head: Option<usize>,
    tail: Option<usize>,
    nodes: Vec<Option<LruNode>>,
    free: Vec<usize>,
    index: HashMap<PlanCacheKey, usize>,
}

#[derive(Debug)]
struct LruNode {
    key: PlanCacheKey,
    prev: Option<usize>,
    next: Option<usize>,
}

impl LruState {
    fn new() -> Self {
        Self {
            head: None,
            tail: None,
            nodes: Vec::new(),
            free: Vec::new(),
            index: HashMap::new(),
        }
    }

    fn clear(&mut self) {
        self.head = None;
        self.tail = None;
        self.nodes.clear();
        self.free.clear();
        self.index.clear();
    }

    fn push_back(&mut self, key: PlanCacheKey) {
        if self.index.contains_key(&key) {
            self.move_to_back(&key);
            return;
        }

        let node_id = self.alloc_node(LruNode {
            key: key.clone(),
            prev: self.tail,
            next: None,
        });
        if let Some(tail_id) = self.tail {
            if let Some(slot) = self.nodes.get_mut(tail_id) {
                if let Some(node) = slot.as_mut() {
                    node.next = Some(node_id);
                }
            }
        } else {
            self.head = Some(node_id);
        }
        self.tail = Some(node_id);
        self.index.insert(key, node_id);
    }

    fn move_to_back(&mut self, key: &PlanCacheKey) {
        let Some(&node_id) = self.index.get(key) else {
            return;
        };
        self.move_node_to_back(node_id);
    }

    fn pop_front(&mut self) -> Option<PlanCacheKey> {
        let head_id = self.head?;
        self.remove_node(head_id).map(|node| node.key)
    }

    fn remove(&mut self, key: &PlanCacheKey) -> Option<PlanCacheKey> {
        let node_id = self.index.get(key).copied()?;
        self.remove_node(node_id).map(|node| node.key)
    }

    fn alloc_node(&mut self, node: LruNode) -> usize {
        if let Some(node_id) = self.free.pop() {
            if let Some(slot) = self.nodes.get_mut(node_id) {
                *slot = Some(node);
            }
            node_id
        } else {
            self.nodes.push(Some(node));
            self.nodes.len() - 1
        }
    }

    fn move_node_to_back(&mut self, node_id: usize) {
        if self.tail == Some(node_id) {
            return;
        }

        let Some((prev, next)) = self
            .nodes
            .get(node_id)
            .and_then(|slot| slot.as_ref())
            .map(|node| (node.prev, node.next))
        else {
            return;
        };

        if let Some(prev_id) = prev {
            if let Some(slot) = self.nodes.get_mut(prev_id) {
                if let Some(node) = slot.as_mut() {
                    node.next = next;
                }
            }
        } else {
            self.head = next;
        }

        if let Some(next_id) = next {
            if let Some(slot) = self.nodes.get_mut(next_id) {
                if let Some(node) = slot.as_mut() {
                    node.prev = prev;
                }
            }
        } else {
            self.tail = prev;
        }

        let old_tail = self.tail;
        if let Some(slot) = self.nodes.get_mut(node_id) {
            if let Some(node) = slot.as_mut() {
                node.prev = old_tail;
                node.next = None;
            }
        }

        if let Some(tail_id) = old_tail {
            if let Some(slot) = self.nodes.get_mut(tail_id) {
                if let Some(node) = slot.as_mut() {
                    node.next = Some(node_id);
                }
            }
        } else {
            self.head = Some(node_id);
        }
        self.tail = Some(node_id);
    }

    fn remove_node(&mut self, node_id: usize) -> Option<LruNode> {
        let node = self.nodes.get_mut(node_id)?.take()?;
        let prev = node.prev;
        let next = node.next;

        if let Some(prev_id) = prev {
            if let Some(slot) = self.nodes.get_mut(prev_id) {
                if let Some(prev_node) = slot.as_mut() {
                    prev_node.next = next;
                }
            }
        } else {
            self.head = next;
        }

        if let Some(next_id) = next {
            if let Some(slot) = self.nodes.get_mut(next_id) {
                if let Some(next_node) = slot.as_mut() {
                    next_node.prev = prev;
                }
            }
        } else {
            self.tail = prev;
        }

        self.index.remove(&node.key);
        self.free.push(node_id);
        Some(node)
    }
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
    /// Entry LRU ordering: head=least-recently-used, tail=most-recently-used.
    lru: LruState,
    /// Counter LRU ordering for miss-path promotion tracking.
    ///
    /// Keys in this list must be a duplicate-free mirror of `counters`.
    counter_lru_order: Vec<PlanCacheKey>,
}

impl PreparedPlanCache {
    /// Create a new cache with the given capacity and promotion threshold.
    pub fn new(capacity: usize, min_exec: u64) -> Self {
        Self {
            capacity,
            min_exec,
            entries: HashMap::new(),
            counters: HashMap::new(),
            lru: LruState::new(),
            counter_lru_order: Vec::new(),
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

    /// Record an execution for a non-cached key and return a typed outcome.
    ///
    /// Promotion decisions must be based only on `MissCount(n)`.
    /// `NotTracked` is never promotable, even when `min_exec = 0`.
    pub fn record_execution(&mut self, key: &PlanCacheKey) -> PromotionCounterOutcome {
        if self.capacity == 0 || self.entries.contains_key(key) {
            return PromotionCounterOutcome::NotTracked;
        }

        if let Some(state) = self.counters.get_mut(key) {
            state.exec_count = state.exec_count.saturating_add(1);
            let count = state.exec_count;
            self.touch_counter_lru(key);
            return PromotionCounterOutcome::MissCount(count);
        }

        while self.counters.len() >= self.capacity {
            if !self.evict_oldest_counter() {
                if let Some(any_key) = self.counters.keys().next().cloned() {
                    self.remove_counter(&any_key);
                } else {
                    break;
                }
            }
        }

        self.counters
            .insert(key.clone(), PromotionState { exec_count: 1 });
        self.touch_counter_lru(key);
        PromotionCounterOutcome::MissCount(1)
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
            if !self.evict_lru_one() {
                break;
            }
        }
        self.trim_counters_to_capacity();
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
            self.remove_counter(&key);
            return;
        }

        // Evict LRU entries if at capacity.
        while self.entries.len() >= self.capacity {
            if !self.evict_lru_one() {
                break;
            }
        }

        self.entries.insert(key.clone(), entry);
        self.remove_counter(&key);
        self.lru.push_back(key);
    }

    /// Invalidate (remove) a specific key.
    #[allow(dead_code)]
    pub fn invalidate(&mut self, key: &PlanCacheKey) {
        self.entries.remove(key);
        self.remove_counter(key);
        self.lru.remove(key);
    }

    /// Invalidate all entries that depend on the given table_id.
    ///
    /// Called after DDL (CREATE INDEX, DROP INDEX, ALTER TABLE, etc.) to
    /// ensure stale plans are not reused.
    ///
    /// Also drops counter-only keys that reference `table_id` via
    /// `PlanCacheKey.resolved_table_ids` so DDL resets miss-path promotion
    /// history for the affected table.
    #[allow(dead_code)]
    pub fn invalidate_by_table_id(&mut self, table_id: u64) {
        let mut keys_to_remove: HashSet<PlanCacheKey> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.dependencies.iter().any(|d| d.table_id == table_id))
            .map(|(k, _)| k.clone())
            .collect();
        for key in self.counters.keys() {
            if key.resolved_table_ids.contains(&table_id) {
                keys_to_remove.insert(key.clone());
            }
        }

        for key in &keys_to_remove {
            self.entries.remove(key);
            self.remove_counter(key);
            self.lru.remove(key);
        }
    }

    /// Clear the entire cache. Called on transaction rollback to prevent
    /// stale entries from surviving DDL that was rolled back.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.counters.clear();
        self.counter_lru_order.clear();
        self.lru.clear();
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

    /// Number of promotion counters currently tracked.
    #[cfg(test)]
    pub fn counter_len(&self) -> usize {
        self.counters.len()
    }

    /// Number of keys in counter LRU order.
    #[cfg(test)]
    pub fn counter_order_len(&self) -> usize {
        self.counter_lru_order.len()
    }

    /// Current counter for a key if tracked.
    #[cfg(test)]
    pub fn counter_count_for(&self, key: &PlanCacheKey) -> Option<u64> {
        self.counters.get(key).map(|state| state.exec_count)
    }

    /// Assert duplicate-free and aligned counter map/order internals.
    #[cfg(test)]
    pub fn assert_counter_internal_consistency(&self) {
        let mut seen = HashSet::new();
        for key in &self.counter_lru_order {
            assert!(seen.insert(key), "counter_lru_order contains duplicate key");
            assert!(
                self.counters.contains_key(key),
                "counter_lru_order contains orphan key"
            );
            assert!(
                !self.entries.contains_key(key),
                "counter map should not track promoted keys"
            );
        }
        assert_eq!(
            self.counters.len(),
            self.counter_lru_order.len(),
            "counter map/order size mismatch"
        );
        assert!(
            self.counters.len() <= self.capacity,
            "counter map exceeds capacity"
        );
    }

    /// Move a key to the back of the LRU list.
    fn touch_lru(&mut self, key: &PlanCacheKey) {
        self.lru.move_to_back(key);
    }

    fn evict_lru_one(&mut self) -> bool {
        let Some(evict_key) = self.lru.pop_front() else {
            return false;
        };
        self.entries.remove(&evict_key);
        self.counters.remove(&evict_key);
        true
    }

    /// Move a counter key to MRU position.
    fn touch_counter_lru(&mut self, key: &PlanCacheKey) {
        self.counter_lru_order.retain(|k| k != key);
        self.counter_lru_order.push(key.clone());
    }

    /// Remove counter state and LRU membership for a key.
    fn remove_counter(&mut self, key: &PlanCacheKey) {
        self.counters.remove(key);
        self.counter_lru_order.retain(|k| k != key);
    }

    /// Evict one oldest counter entry by counter LRU order.
    fn evict_oldest_counter(&mut self) -> bool {
        if let Some(evict_key) = self.counter_lru_order.first().cloned() {
            self.remove_counter(&evict_key);
            true
        } else {
            false
        }
    }

    /// Trim counters to configured capacity and restore order consistency.
    fn trim_counters_to_capacity(&mut self) {
        if self.capacity == 0 {
            self.counters.clear();
            self.counter_lru_order.clear();
            return;
        }

        self.counters
            .retain(|key, _| !self.entries.contains_key(key));
        self.counter_lru_order
            .retain(|key| self.counters.contains_key(key) && !self.entries.contains_key(key));

        let mut seen = HashSet::new();
        let mut deduped_rev = Vec::with_capacity(self.counter_lru_order.len());
        for key in self.counter_lru_order.iter().rev() {
            if seen.insert(key.clone()) {
                deduped_rev.push(key.clone());
            }
        }
        deduped_rev.reverse();
        self.counter_lru_order = deduped_rev;

        while self.counters.len() > self.capacity {
            if !self.evict_oldest_counter() {
                if let Some(any_key) = self.counters.keys().next().cloned() {
                    self.counters.remove(&any_key);
                } else {
                    break;
                }
            }
        }
        self.counter_lru_order
            .retain(|k| self.counters.contains_key(k));
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
            let outcome = cache.record_execution(&key);
            assert_eq!(outcome, PromotionCounterOutcome::MissCount(i));
        }
        assert!(cache.get(&key).is_none());

        let outcome = cache.record_execution(&key);
        assert_eq!(outcome, PromotionCounterOutcome::MissCount(5));
        // After threshold, caller would insert:
        cache.insert(key.clone(), make_entry(vec![]));
        assert!(cache.get(&key).is_some());
        assert_eq!(
            cache.record_execution(&key),
            PromotionCounterOutcome::NotTracked
        );
        assert_eq!(cache.counter_count_for(&key), None);
        cache.assert_counter_internal_consistency();
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
        assert_eq!(
            cache.record_execution(&k1),
            PromotionCounterOutcome::NotTracked
        );

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.counter_len(), 0);
        assert_eq!(cache.counter_order_len(), 0);
        assert!(cache.get(&k1).is_none());
        cache.assert_counter_internal_consistency();
    }

    #[test]
    fn zero_capacity_cache_never_stores() {
        let mut cache = PreparedPlanCache::new(0, 1);
        let key = make_key("SELECT 1");
        assert_eq!(
            cache.record_execution(&key),
            PromotionCounterOutcome::NotTracked
        );
        cache.insert(key.clone(), make_entry(vec![]));
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.counter_len(), 0);
        assert_eq!(cache.counter_order_len(), 0);
    }

    #[test]
    fn min_exec_zero_first_miss_has_count_one() {
        let mut cache = PreparedPlanCache::new(8, 0);
        let key = make_key("SELECT 1");
        assert_eq!(
            cache.record_execution(&key),
            PromotionCounterOutcome::MissCount(1)
        );
        cache.assert_counter_internal_consistency();
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
        cache.assert_counter_internal_consistency();
    }

    #[test]
    fn invalidate_by_table_id_removes_counter_only_keys() {
        let mut cache = PreparedPlanCache::new(10, 3);
        let k1 = PlanCacheKey::new(
            "SELECT * FROM t1 WHERE id = $1".to_string(),
            &[],
            1,
            &["public".to_string()],
            &[10],
        );
        let k2 = PlanCacheKey::new(
            "SELECT * FROM t2 WHERE id = $1".to_string(),
            &[],
            1,
            &["public".to_string()],
            &[20],
        );
        assert_eq!(
            cache.record_execution(&k1),
            PromotionCounterOutcome::MissCount(1)
        );
        assert_eq!(
            cache.record_execution(&k2),
            PromotionCounterOutcome::MissCount(1)
        );
        assert_eq!(cache.counter_len(), 2);

        cache.invalidate_by_table_id(10);
        assert_eq!(cache.counter_count_for(&k1), None);
        assert_eq!(cache.counter_count_for(&k2), Some(1));
        cache.assert_counter_internal_consistency();
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
        cache.assert_counter_internal_consistency();
    }

    #[test]
    fn reconfigure_zero_capacity_clears_all_state() {
        let mut cache = PreparedPlanCache::new(2, 1);
        let k1 = make_key("SELECT 1");
        let k2 = PlanCacheKey::new(
            "SELECT * FROM t WHERE id = $1".to_string(),
            &[],
            1,
            &["public".to_string()],
            &[42],
        );
        cache.insert(k1.clone(), make_entry(vec![]));
        assert_eq!(
            cache.record_execution(&k2),
            PromotionCounterOutcome::MissCount(1)
        );
        assert_eq!(cache.counter_len(), 1);

        cache.reconfigure(0, 3);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.counter_len(), 0);
        assert_eq!(cache.counter_order_len(), 0);
        assert_eq!(
            cache.record_execution(&k1),
            PromotionCounterOutcome::NotTracked
        );
        cache.assert_counter_internal_consistency();
    }

    #[test]
    fn counters_are_bounded_by_capacity_and_evict_lru() {
        let mut cache = PreparedPlanCache::new(2, 5);
        let k1 = make_key("SELECT 1");
        let k2 = make_key("SELECT 2");
        let k3 = make_key("SELECT 3");

        assert_eq!(
            cache.record_execution(&k1),
            PromotionCounterOutcome::MissCount(1)
        );
        assert_eq!(
            cache.record_execution(&k2),
            PromotionCounterOutcome::MissCount(1)
        );
        // Touch k1 so k2 is the oldest tracked counter.
        assert_eq!(
            cache.record_execution(&k1),
            PromotionCounterOutcome::MissCount(2)
        );
        assert_eq!(
            cache.record_execution(&k3),
            PromotionCounterOutcome::MissCount(1)
        );

        assert_eq!(cache.counter_len(), 2);
        assert_eq!(cache.counter_count_for(&k1), Some(2));
        assert_eq!(cache.counter_count_for(&k2), None);
        assert_eq!(cache.counter_count_for(&k3), Some(1));
        cache.assert_counter_internal_consistency();
    }

    #[test]
    fn insert_existing_key_clears_counter_and_keeps_consistency() {
        let mut cache = PreparedPlanCache::new(4, 2);
        let key = make_key("SELECT 1");
        assert_eq!(
            cache.record_execution(&key),
            PromotionCounterOutcome::MissCount(1)
        );
        assert_eq!(cache.counter_count_for(&key), Some(1));

        cache.insert(key.clone(), make_entry(vec![]));
        assert_eq!(cache.counter_count_for(&key), None);

        cache.insert(key.clone(), make_entry(vec![]));
        assert_eq!(cache.counter_count_for(&key), None);
        cache.assert_counter_internal_consistency();
    }

    #[test]
    fn repeated_hits_do_not_duplicate_lru_nodes() {
        let mut cache = PreparedPlanCache::new(2, 1);
        let k1 = make_key("SELECT 1");
        let k2 = make_key("SELECT 2");
        let k3 = make_key("SELECT 3");

        cache.insert(k1.clone(), make_entry(vec![]));
        cache.insert(k2.clone(), make_entry(vec![]));

        for _ in 0..5 {
            assert!(cache.get(&k1).is_some());
        }

        cache.insert(k3.clone(), make_entry(vec![]));
        assert!(cache.get(&k1).is_some());
        assert!(cache.get(&k2).is_none());
        assert!(cache.get(&k3).is_some());
    }

    #[test]
    fn insert_existing_key_keeps_size_and_refreshes_recency() {
        let mut cache = PreparedPlanCache::new(2, 1);
        let k1 = make_key("SELECT 1");
        let k2 = make_key("SELECT 2");
        let k3 = make_key("SELECT 3");

        cache.insert(k1.clone(), make_entry(vec![]));
        cache.insert(k2.clone(), make_entry(vec![]));
        assert_eq!(cache.len(), 2);

        cache.insert(k1.clone(), make_entry(vec![]));
        assert_eq!(cache.len(), 2);

        cache.insert(k3.clone(), make_entry(vec![]));
        assert!(cache.get(&k1).is_some());
        assert!(cache.get(&k2).is_none());
        assert!(cache.get(&k3).is_some());
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

    #[test]
    fn key_differs_when_param_types_differ() {
        use crate::model::DataType;
        let k1 = PlanCacheKey::new(
            "SELECT $1".to_string(),
            &[DataType::Int32],
            1,
            &["public".to_string()],
            &[],
        );
        let k2 = PlanCacheKey::new(
            "SELECT $1".to_string(),
            &[DataType::Text],
            1,
            &["public".to_string()],
            &[],
        );
        let k3 = PlanCacheKey::new(
            "SELECT $1".to_string(),
            &[DataType::Int32],
            1,
            &["public".to_string()],
            &[],
        );
        assert_ne!(k1, k2, "different param types must produce different keys");
        assert_eq!(k1, k3, "same param types must produce equal keys");
    }

    #[test]
    fn cache_hit_miss_by_param_types() {
        use crate::model::DataType;

        let mut cache = PreparedPlanCache::new(10, 1);

        let key_int = PlanCacheKey::new(
            "SELECT $1".to_string(),
            &[DataType::Int32],
            1,
            &["public".to_string()],
            &[],
        );
        let key_text = PlanCacheKey::new(
            "SELECT $1".to_string(),
            &[DataType::Text],
            1,
            &["public".to_string()],
            &[],
        );
        let key_int_dup = PlanCacheKey::new(
            "SELECT $1".to_string(),
            &[DataType::Int32],
            1,
            &["public".to_string()],
            &[],
        );

        // Insert a plan under key_int.
        cache.insert(key_int.clone(), make_entry(vec![]));

        // MISS: same SQL but different param_types.
        assert!(
            cache.get(&key_text).is_none(),
            "different param_types must be a cache miss"
        );

        // HIT: same SQL and same param_types.
        assert!(
            cache.get(&key_int_dup).is_some(),
            "identical param_types must be a cache hit"
        );

        // record_execution reflects the miss/not-tracked distinction.
        assert_eq!(
            cache.record_execution(&key_text),
            PromotionCounterOutcome::MissCount(1),
            "uncached key_text should start at miss count 1"
        );
        assert_eq!(
            cache.record_execution(&key_int),
            PromotionCounterOutcome::NotTracked,
            "already-cached key_int should be NotTracked"
        );
        cache.assert_counter_internal_consistency();
    }
}
