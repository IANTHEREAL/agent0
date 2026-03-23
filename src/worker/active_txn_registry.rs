//! Process-local registry of active transactions for GC safepoint protection.
//!
//! Every SQL-serving db9 process unconditionally publishes its `min_start_ts`
//! to the shared `_sys_worker` TiKV keyspace. The GC safepoint advancer reads
//! all instances' published values and clamps the safepoint accordingly.
//!
//! This module provides the **process-local** tracking. Cross-instance
//! coordination is handled by `GcRegistryPublisher` + `GcSafepointAdvancer`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ActiveTxnKey {
    Connection(i64),
    Worker(u64),
}

/// RAII guard for a worker/background TiKV transaction published in the
/// process-local GC registry.
///
/// By default the guard unregisters the transaction when dropped.  Call
/// [`defuse`](Self::defuse) before dropping to **keep** the registration
/// alive — use this when commit or rollback failed and the underlying TiKV
/// transaction may still be live.
pub struct ActiveTxnGuard {
    registry: Arc<ActiveTxnRegistry>,
    handle_id: u64,
    defused: bool,
}

impl ActiveTxnGuard {
    /// Prevent this guard from unregistering the transaction on drop.
    ///
    /// Call this when the transaction's commit or rollback failed — the
    /// registration must stay so the GC safepoint does not advance past a
    /// potentially live transaction.
    pub fn defuse(&mut self) {
        self.defused = true;
    }
}

impl Drop for ActiveTxnGuard {
    fn drop(&mut self) {
        if !self.defused {
            self.registry.unregister_worker(self.handle_id);
        }
    }
}

/// Tracks active transaction `start_ts` values across interactive SQL sessions
/// and worker/background TiKV transactions.
pub struct ActiveTxnRegistry {
    /// tracker key -> start_ts (TiKV TSO version)
    inner: Mutex<HashMap<ActiveTxnKey, u64>>,
    next_worker_handle: AtomicU64,
}

impl ActiveTxnRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            next_worker_handle: AtomicU64::new(1),
        }
    }

    /// Register an interactive session transaction's start_ts.
    #[inline]
    pub fn register_connection(&self, connection_id: i64, start_ts_version: u64) {
        self.inner
            .lock()
            .expect("ActiveTxnRegistry poisoned")
            .insert(ActiveTxnKey::Connection(connection_id), start_ts_version);
    }

    /// Unregister an interactive session transaction. Idempotent.
    #[inline]
    pub fn unregister_connection(&self, connection_id: i64) {
        self.inner
            .lock()
            .expect("ActiveTxnRegistry poisoned")
            .remove(&ActiveTxnKey::Connection(connection_id));
    }

    /// Register a worker/background transaction and return a guard that
    /// unregisters it when dropped.
    pub fn track_worker_txn(self: &Arc<Self>, start_ts_version: u64) -> ActiveTxnGuard {
        let handle_id = self.next_worker_handle.fetch_add(1, Ordering::Relaxed);
        self.inner
            .lock()
            .expect("ActiveTxnRegistry poisoned")
            .insert(ActiveTxnKey::Worker(handle_id), start_ts_version);
        ActiveTxnGuard {
            registry: Arc::clone(self),
            handle_id,
            defused: false,
        }
    }

    #[inline]
    fn unregister_worker(&self, handle_id: u64) {
        self.inner
            .lock()
            .expect("ActiveTxnRegistry poisoned")
            .remove(&ActiveTxnKey::Worker(handle_id));
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
        r.register_connection(1, 100);
        r.register_connection(2, 50);
        r.register_connection(3, 200);
        assert_eq!(r.min_start_ts(), Some(50));
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn unregister_updates_min() {
        let r = ActiveTxnRegistry::new();
        r.register_connection(1, 100);
        r.register_connection(2, 50);
        r.unregister_connection(2);
        assert_eq!(r.min_start_ts(), Some(100));
    }

    #[test]
    fn unregister_all_returns_none() {
        let r = ActiveTxnRegistry::new();
        r.register_connection(1, 100);
        r.unregister_connection(1);
        assert_eq!(r.min_start_ts(), None);
    }

    #[test]
    fn duplicate_register_overwrites() {
        let r = ActiveTxnRegistry::new();
        r.register_connection(1, 100);
        r.register_connection(1, 200);
        assert_eq!(r.min_start_ts(), Some(200));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn unregister_nonexistent_is_noop() {
        let r = ActiveTxnRegistry::new();
        r.unregister_connection(999);
        assert_eq!(r.min_start_ts(), None);
    }

    #[test]
    fn worker_guard_unregisters_on_drop() {
        let r = Arc::new(ActiveTxnRegistry::new());
        {
            let _guard = r.track_worker_txn(123);
            assert_eq!(r.min_start_ts(), Some(123));
            assert_eq!(r.len(), 1);
        }
        assert_eq!(r.min_start_ts(), None);
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn connection_and_worker_transactions_share_same_min() {
        let r = Arc::new(ActiveTxnRegistry::new());
        r.register_connection(1, 200);
        let _guard = r.track_worker_txn(150);
        assert_eq!(r.min_start_ts(), Some(150));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn defused_guard_keeps_registration_on_drop() {
        let r = Arc::new(ActiveTxnRegistry::new());
        {
            let mut guard = r.track_worker_txn(42);
            assert_eq!(r.min_start_ts(), Some(42));
            guard.defuse();
        }
        // Registration survives the drop because guard was defused.
        assert_eq!(r.min_start_ts(), Some(42));
        assert_eq!(r.len(), 1);
    }
}
