//! Process-local registry of active transactions for GC safepoint protection.
//!
//! Every SQL-serving db9 process unconditionally publishes its `min_start_ts`
//! to the shared `_sys_worker` TiKV keyspace. The GC safepoint advancer reads
//! all instances' published values and clamps the safepoint accordingly.
//!
//! This module provides the **process-local** tracking. Cross-instance
//! coordination is handled by `GcRegistryPublisher` + `GcSafepointAdvancer`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

static GLOBAL_REGISTRY: OnceLock<Arc<ActiveTxnRegistry>> = OnceLock::new();

/// Set the global registry instance. Called once at startup.
pub fn set_global_registry(registry: Arc<ActiveTxnRegistry>) {
    GLOBAL_REGISTRY.set(registry).ok();
}

/// Get the global registry. Returns None if not initialized.
pub fn global_registry() -> Option<&'static Arc<ActiveTxnRegistry>> {
    GLOBAL_REGISTRY.get()
}

/// Tracks active transaction `start_ts` values by connection ID.
///
/// Registered in `Session::begin()`, unregistered on successful
/// `commit()`/`rollback()`. `DynamicPgHandler::Drop` provides a safety-net
/// unregister for disconnected sessions.
pub struct ActiveTxnRegistry {
    /// connection_id → start_ts (TiKV TSO version)
    inner: Mutex<HashMap<i64, u64>>,
}

impl ActiveTxnRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Register an active transaction's start_ts.
    #[inline]
    pub fn register(&self, connection_id: i64, start_ts_version: u64) {
        self.inner
            .lock()
            .expect("ActiveTxnRegistry poisoned")
            .insert(connection_id, start_ts_version);
    }

    /// Unregister a connection's active transaction. Idempotent.
    #[inline]
    pub fn unregister(&self, connection_id: i64) {
        self.inner
            .lock()
            .expect("ActiveTxnRegistry poisoned")
            .remove(&connection_id);
    }

    /// Minimum start_ts across all active transactions, or None if empty.
    pub fn min_start_ts(&self) -> Option<u64> {
        self.inner
            .lock()
            .expect("ActiveTxnRegistry poisoned")
            .values()
            .copied()
            .min()
    }

    /// Number of tracked transactions.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("ActiveTxnRegistry poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_none() {
        let r = ActiveTxnRegistry::new();
        assert_eq!(r.min_start_ts(), None);
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn register_and_min() {
        let r = ActiveTxnRegistry::new();
        r.register(1, 100);
        r.register(2, 50);
        r.register(3, 200);
        assert_eq!(r.min_start_ts(), Some(50));
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn unregister_updates_min() {
        let r = ActiveTxnRegistry::new();
        r.register(1, 100);
        r.register(2, 50);
        r.unregister(2);
        assert_eq!(r.min_start_ts(), Some(100));
    }

    #[test]
    fn unregister_all_returns_none() {
        let r = ActiveTxnRegistry::new();
        r.register(1, 100);
        r.unregister(1);
        assert_eq!(r.min_start_ts(), None);
    }

    #[test]
    fn duplicate_register_overwrites() {
        let r = ActiveTxnRegistry::new();
        r.register(1, 100);
        r.register(1, 200);
        assert_eq!(r.min_start_ts(), Some(200));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn unregister_nonexistent_is_noop() {
        let r = ActiveTxnRegistry::new();
        r.unregister(999);
        assert_eq!(r.min_start_ts(), None);
    }
}
