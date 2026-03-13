//! RLS policy cache — avoids repeated TiKV scans for policy lists.
//!
//! Keyed by `(db_id, table_id)` with a `schema_version` guard.
//! Policy DDL (CREATE/ALTER/DROP POLICY) bumps `schema.version`,
//! so stale cache entries are naturally invalidated on next access.
//!
//! Owned by `TenantEntry` in the pool — when the reaper drops the entry,
//! the cache is dropped automatically.

use crate::model::RlsPolicy;
use dashmap::DashMap;

/// Per-tenant cache for RLS policies loaded from TiKV.
pub(crate) struct RlsPolicyCache {
    /// Map from (db_id, table_id) → (schema_version, policies).
    inner: DashMap<(u64, u64), (u64, Vec<RlsPolicy>)>,
}

impl RlsPolicyCache {
    pub(crate) fn new() -> Self {
        Self {
            inner: DashMap::new(),
        }
    }

    /// Return cached policies if the schema version matches.
    pub(crate) fn get(
        &self,
        db_id: u64,
        table_id: u64,
        schema_version: u64,
    ) -> Option<Vec<RlsPolicy>> {
        let entry = self.inner.get(&(db_id, table_id))?;
        let (cached_version, policies) = entry.value();
        if *cached_version == schema_version {
            Some(policies.clone())
        } else {
            None
        }
    }

    /// Store policies for a table with its current schema version.
    pub(crate) fn put(
        &self,
        db_id: u64,
        table_id: u64,
        schema_version: u64,
        policies: Vec<RlsPolicy>,
    ) {
        self.inner
            .insert((db_id, table_id), (schema_version, policies));
    }

    /// Remove all cached policies for a database.
    ///
    /// Called on major schema changes (e.g., DROP TABLE).
    /// Normally unnecessary since version-mismatch handles staleness,
    /// but useful as a safety valve.
    #[allow(dead_code)]
    pub(crate) fn invalidate_db(&self, db_id: u64) {
        self.inner.retain(|&(did, _), _| did != db_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RlsCommand;

    fn sample_policy(name: &str) -> RlsPolicy {
        RlsPolicy {
            oid: 1,
            name: name.to_string(),
            table_id: 100,
            command: RlsCommand::Select,
            permissive: true,
            roles: vec![],
            using_expr: Some("true".to_string()),
            with_check_expr: None,
        }
    }

    #[test]
    fn cache_hit_on_same_version() {
        let cache = RlsPolicyCache::new();
        let policies = vec![sample_policy("p1")];
        cache.put(1, 100, 5, policies.clone());

        let result = cache.get(1, 100, 5);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn cache_miss_on_version_bump() {
        let cache = RlsPolicyCache::new();
        cache.put(1, 100, 5, vec![sample_policy("p1")]);

        // Schema version bumped (e.g., CREATE POLICY) → miss
        assert!(cache.get(1, 100, 6).is_none());
    }

    #[test]
    fn cache_miss_on_different_table() {
        let cache = RlsPolicyCache::new();
        cache.put(1, 100, 5, vec![sample_policy("p1")]);

        assert!(cache.get(1, 200, 5).is_none());
    }

    #[test]
    fn put_overwrites_stale_entry() {
        let cache = RlsPolicyCache::new();
        cache.put(1, 100, 5, vec![sample_policy("old")]);
        cache.put(1, 100, 6, vec![sample_policy("new")]);

        assert!(cache.get(1, 100, 5).is_none());
        let result = cache.get(1, 100, 6).unwrap();
        assert_eq!(result[0].name, "new");
    }

    #[test]
    fn invalidate_db_clears_entries() {
        let cache = RlsPolicyCache::new();
        cache.put(1, 100, 5, vec![sample_policy("p1")]);
        cache.put(1, 200, 5, vec![sample_policy("p2")]);
        cache.put(2, 100, 5, vec![sample_policy("p3")]);

        cache.invalidate_db(1);

        assert!(cache.get(1, 100, 5).is_none());
        assert!(cache.get(1, 200, 5).is_none());
        // Other db unaffected
        assert!(cache.get(2, 100, 5).is_some());
    }
}
