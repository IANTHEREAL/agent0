//! Two-layer RLS policy cache.
//!
//! **Layer 1 — raw policy list**: Caches `Vec<RlsPolicy>` from TiKV,
//! keyed by `(db_id, table_id)` with a `schema_version` guard.
//! Avoids repeated TiKV prefix scans.
//!
//! **Layer 2 — compiled expressions**: Caches pre-fold `TypedExpr` from
//! parsing + analyzing policy SQL strings, keyed by
//! `(db_id, policy_oid, schema_version)`. The caller still applies
//! `fold_typed_expr` per-query (cheap tree walk), but the expensive
//! parse → analyze pipeline runs only once per policy version.
//!
//! Invalidation: Policy DDL bumps `schema.version`, causing natural
//! cache misses on both layers.
//!
//! Owned by `TenantEntry` in the pool — auto-dropped on tenant eviction.

use crate::model::RlsPolicy;
use crate::sql::analyzer::types::TypedExpr;
use dashmap::DashMap;

/// Per-tenant cache for RLS policies and compiled expressions.
pub(crate) struct RlsPolicyCache {
    /// Layer 1: (db_id, table_id) → (schema_version, policies).
    policies: DashMap<(u64, u64), (u64, Vec<RlsPolicy>)>,
    /// Layer 2: (db_id, policy_oid, schema_version, expr_kind) → pre-fold TypedExpr.
    /// expr_kind: b'u' for USING, b'c' for WITH CHECK.
    exprs: DashMap<(u64, u32, u64, u8), TypedExpr>,
}

/// Expr kind discriminator for cache key.
const USING: u8 = b'u';
const WITH_CHECK: u8 = b'c';

impl RlsPolicyCache {
    pub(crate) fn new() -> Self {
        Self {
            policies: DashMap::new(),
            exprs: DashMap::new(),
        }
    }

    // ── Layer 1: raw policy list cache ───────────────────────

    /// Return cached policies if the schema version matches.
    pub(crate) fn get(
        &self,
        db_id: u64,
        table_id: u64,
        schema_version: u64,
    ) -> Option<Vec<RlsPolicy>> {
        let entry = self.policies.get(&(db_id, table_id))?;
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
        self.policies
            .insert((db_id, table_id), (schema_version, policies));
    }

    // ── Layer 2: compiled expression cache ───────────────────

    /// Get a cached pre-fold USING expression for a policy.
    pub(crate) fn get_using_expr(
        &self,
        db_id: u64,
        policy_oid: u32,
        schema_version: u64,
    ) -> Option<TypedExpr> {
        self.exprs
            .get(&(db_id, policy_oid, schema_version, USING))
            .map(|e| e.value().clone())
    }

    /// Get a cached pre-fold WITH CHECK expression for a policy.
    pub(crate) fn get_with_check_expr(
        &self,
        db_id: u64,
        policy_oid: u32,
        schema_version: u64,
    ) -> Option<TypedExpr> {
        self.exprs
            .get(&(db_id, policy_oid, schema_version, WITH_CHECK))
            .map(|e| e.value().clone())
    }

    /// Cache a pre-fold USING expression.
    pub(crate) fn put_using_expr(
        &self,
        db_id: u64,
        policy_oid: u32,
        schema_version: u64,
        expr: TypedExpr,
    ) {
        self.exprs
            .insert((db_id, policy_oid, schema_version, USING), expr);
    }

    /// Cache a pre-fold WITH CHECK expression.
    pub(crate) fn put_with_check_expr(
        &self,
        db_id: u64,
        policy_oid: u32,
        schema_version: u64,
        expr: TypedExpr,
    ) {
        self.exprs
            .insert((db_id, policy_oid, schema_version, WITH_CHECK), expr);
    }

    // ── Invalidation ─────────────────────────────────────────

    /// Remove all cached data for a database.
    #[cfg(test)]
    pub(crate) fn invalidate_db(&self, db_id: u64) {
        self.policies.retain(|&(did, _), _| did != db_id);
        self.exprs.retain(|&(did, _, _, _), _| did != db_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, RlsCommand, Value};
    use crate::sql::analyzer::types::TypedExprKind;

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

    fn true_expr() -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(true)),
            DataType::Boolean,
        )
    }

    fn false_expr() -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(false)),
            DataType::Boolean,
        )
    }

    // ── Layer 1 tests ────────────────────────────────────────

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
        assert!(cache.get(2, 100, 5).is_some());
    }

    // ── Layer 2 tests ────────────────────────────────────────

    #[test]
    fn expr_cache_hit() {
        let cache = RlsPolicyCache::new();
        cache.put_using_expr(1, 42, 5, true_expr());

        let result = cache.get_using_expr(1, 42, 5);
        assert!(result.is_some());
        match result.unwrap().kind {
            TypedExprKind::Constant(Value::Boolean(true)) => {}
            other => panic!("expected true, got {:?}", other),
        }
    }

    #[test]
    fn expr_cache_miss_on_version() {
        let cache = RlsPolicyCache::new();
        cache.put_using_expr(1, 42, 5, true_expr());
        assert!(cache.get_using_expr(1, 42, 6).is_none());
    }

    #[test]
    fn expr_using_and_with_check_independent() {
        let cache = RlsPolicyCache::new();
        cache.put_using_expr(1, 42, 5, true_expr());
        cache.put_with_check_expr(1, 42, 5, false_expr());

        match cache.get_using_expr(1, 42, 5).unwrap().kind {
            TypedExprKind::Constant(Value::Boolean(true)) => {}
            other => panic!("expected true, got {:?}", other),
        }
        match cache.get_with_check_expr(1, 42, 5).unwrap().kind {
            TypedExprKind::Constant(Value::Boolean(false)) => {}
            other => panic!("expected false, got {:?}", other),
        }
    }

    #[test]
    fn invalidate_db_clears_exprs() {
        let cache = RlsPolicyCache::new();
        cache.put_using_expr(1, 42, 5, true_expr());
        cache.put_using_expr(2, 42, 5, true_expr());

        cache.invalidate_db(1);

        assert!(cache.get_using_expr(1, 42, 5).is_none());
        assert!(cache.get_using_expr(2, 42, 5).is_some());
    }
}
