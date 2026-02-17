use crate::sql::stats::TableStatsCache;
use crate::sql::triggers::TriggerBodyCache;
use crate::storage::TikvStore;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as TokioMutex, RwLock};
use tracing::{debug, info};

/// How long an idle tenant (zero connections) stays cached before eviction.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the reaper scans for idle tenants.
const DEFAULT_REAPER_INTERVAL: Duration = Duration::from_secs(30);

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Per-tenant metadata inside the pool.
#[allow(dead_code)] // fields accessed by pool lifecycle tests
pub(crate) struct TenantEntry {
    store: Arc<TikvStore>,
    /// Number of active connection-scoped handles (TenantHandle instances).
    active_connections: AtomicU32,
    /// Epoch millis when the last handle was dropped (connections went to zero).
    /// Zero means there are active connections or it was never idle.
    last_idle_at: AtomicU64,
    keyspace: String,
    trigger_cache: Arc<TriggerBodyCache>,
    stats_cache: Arc<TableStatsCache>,
}

impl TenantEntry {
    fn new(store: Arc<TikvStore>, keyspace: String) -> Self {
        Self {
            store,
            active_connections: AtomicU32::new(0),
            last_idle_at: AtomicU64::new(0),
            keyspace,
            trigger_cache: Arc::new(TriggerBodyCache::new()),
            stats_cache: Arc::new(TableStatsCache::new()),
        }
    }

    #[allow(dead_code)] // used in pool tests
    pub(crate) fn store(&self) -> &Arc<TikvStore> {
        &self.store
    }

    #[allow(dead_code)] // used in pool tests
    pub(crate) fn active_connections(&self) -> u32 {
        self.active_connections.load(Ordering::Relaxed)
    }
}

/// RAII handle representing one connection's hold on a tenant's TikvStore.
///
/// When dropped, decrements the tenant's active connection count. If the count
/// reaches zero, the tenant becomes eligible for eviction after the idle timeout.
pub struct TenantHandle {
    entry: Arc<TenantEntry>,
}

impl TenantHandle {
    pub fn store(&self) -> &Arc<TikvStore> {
        &self.entry.store
    }

    pub fn trigger_cache(&self) -> &Arc<TriggerBodyCache> {
        &self.entry.trigger_cache
    }

    pub fn stats_cache(&self) -> &Arc<TableStatsCache> {
        &self.entry.stats_cache
    }
}

impl Clone for TenantHandle {
    fn clone(&self) -> Self {
        self.entry
            .active_connections
            .fetch_add(1, Ordering::Relaxed);
        self.entry.last_idle_at.store(0, Ordering::Relaxed);
        Self {
            entry: self.entry.clone(),
        }
    }
}

impl Drop for TenantHandle {
    fn drop(&mut self) {
        let prev = self
            .entry
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            // This was the last handle — record idle start time.
            self.entry
                .last_idle_at
                .store(now_epoch_ms(), Ordering::Relaxed);
        }
    }
}

pub struct TikvClientPool {
    pd_endpoints: Vec<String>,
    tenants: RwLock<HashMap<String, Arc<TenantEntry>>>,
    /// Per-keyspace creation locks to avoid holding the global write lock during
    /// the slow bootstrap (TiKV connect + default DB creation).
    creation_locks: RwLock<HashMap<String, Arc<TokioMutex<()>>>>,
    idle_timeout: Duration,
    reaper_interval: Duration,
}

impl TikvClientPool {
    pub fn new(pd_endpoints: Vec<String>) -> Self {
        Self {
            pd_endpoints,
            tenants: RwLock::new(HashMap::new()),
            creation_locks: RwLock::new(HashMap::new()),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            reaper_interval: DEFAULT_REAPER_INTERVAL,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_timeouts(
        pd_endpoints: Vec<String>,
        idle_timeout: Duration,
        reaper_interval: Duration,
    ) -> Self {
        Self {
            pd_endpoints,
            tenants: RwLock::new(HashMap::new()),
            creation_locks: RwLock::new(HashMap::new()),
            idle_timeout,
            reaper_interval,
        }
    }

    /// Acquire a connection-scoped handle to a tenant's TikvStore.
    ///
    /// The tenant's TikvStore is lazily created on first access and kept alive
    /// as long as at least one TenantHandle exists. Once all handles are dropped,
    /// the entry becomes eligible for eviction after the idle timeout.
    pub async fn acquire(&self, keyspace: Option<String>) -> Result<TenantHandle> {
        let key = keyspace.clone().unwrap_or_else(|| "default".to_string());

        // Fast path: tenant already exists.
        {
            let tenants = self.tenants.read().await;
            if let Some(entry) = tenants.get(&key) {
                entry.active_connections.fetch_add(1, Ordering::Relaxed);
                entry.last_idle_at.store(0, Ordering::Relaxed);
                return Ok(TenantHandle {
                    entry: entry.clone(),
                });
            }
        }

        // Slow path: need to create. Use a per-keyspace lock so only one
        // creation runs per keyspace, without blocking other keyspaces.
        let creation_lock = {
            let mut locks = self.creation_locks.write().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone()
        };

        let _guard = creation_lock.lock().await;

        // Double-check after acquiring per-keyspace lock.
        {
            let tenants = self.tenants.read().await;
            if let Some(entry) = tenants.get(&key) {
                entry.active_connections.fetch_add(1, Ordering::Relaxed);
                entry.last_idle_at.store(0, Ordering::Relaxed);
                return Ok(TenantHandle {
                    entry: entry.clone(),
                });
            }
        }

        info!("Creating new TiKV client for keyspace: {}", key);
        let store = self.create_store(&key, keyspace).await?;
        let entry = Arc::new(TenantEntry::new(store, key.clone()));
        entry.active_connections.fetch_add(1, Ordering::Relaxed);

        {
            let mut tenants = self.tenants.write().await;
            tenants.insert(key, entry.clone());
        }

        Ok(TenantHandle { entry })
    }

    /// Get a TikvStore without a connection-scoped handle.
    ///
    /// Used by background tasks (trigger worker, startup validation) that need
    /// short-lived access to a tenant's store. The returned Arc<TikvStore>
    /// keeps the store alive even if the pool evicts the entry, but does NOT
    /// prevent eviction.
    pub async fn get_client(&self, keyspace: Option<String>) -> Result<Arc<TikvStore>> {
        let key = keyspace.clone().unwrap_or_else(|| "default".to_string());

        // Fast path: tenant already exists.
        {
            let tenants = self.tenants.read().await;
            if let Some(entry) = tenants.get(&key) {
                return Ok(entry.store.clone());
            }
        }

        // Slow path: create via the same per-keyspace lock mechanism.
        let creation_lock = {
            let mut locks = self.creation_locks.write().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone()
        };

        let _guard = creation_lock.lock().await;

        // Double-check.
        {
            let tenants = self.tenants.read().await;
            if let Some(entry) = tenants.get(&key) {
                return Ok(entry.store.clone());
            }
        }

        info!("Creating new TiKV client for keyspace: {}", key);
        let store = self.create_store(&key, keyspace).await?;
        let entry = Arc::new(TenantEntry::new(store, key.clone()));
        // Mark as idle immediately since no handle is held.
        entry.last_idle_at.store(now_epoch_ms(), Ordering::Relaxed);

        let store_clone = entry.store.clone();
        {
            let mut tenants = self.tenants.write().await;
            tenants.insert(key, entry);
        }

        Ok(store_clone)
    }

    /// Shared TikvStore creation logic. Resolves the "default" keyspace name
    /// and creates the store with bootstrap.
    async fn create_store(&self, key: &str, keyspace: Option<String>) -> Result<Arc<TikvStore>> {
        let actual_keyspace = if key == "default" {
            Some("DEFAULT".to_string())
        } else {
            keyspace
        };

        let result = TikvStore::new_with_keyspace(self.pd_endpoints.clone(), actual_keyspace).await;

        let store = match result {
            Ok(s) => s,
            Err(e) => {
                let err_str = format!("{:?}", e);
                if err_str.contains("keyspace does not exist") {
                    return Err(anyhow!("Tenant '{}' does not exist", key));
                }
                return Err(e);
            }
        };

        Ok(Arc::new(store))
    }

    /// Number of tenants currently cached in the pool.
    #[allow(dead_code)] // used in pool tests
    pub async fn tenant_count(&self) -> usize {
        self.tenants.read().await.len()
    }

    /// Number of tenants with at least one active connection handle.
    #[allow(dead_code)] // used in pool tests
    pub async fn active_tenant_count(&self) -> usize {
        let tenants = self.tenants.read().await;
        tenants
            .values()
            .filter(|e| e.active_connections.load(Ordering::Relaxed) > 0)
            .count()
    }

    /// Snapshot of the active connection count for a specific keyspace.
    /// Returns None if the keyspace is not in the pool.
    #[allow(dead_code)] // used in pool tests
    pub async fn connections_for(&self, keyspace: &str) -> Option<u32> {
        let tenants = self.tenants.read().await;
        tenants
            .get(keyspace)
            .map(|e| e.active_connections.load(Ordering::Relaxed))
    }

    pub async fn list_active_keyspaces(&self) -> Vec<String> {
        let tenants = self.tenants.read().await;
        tenants
            .iter()
            .filter_map(|(keyspace, entry)| {
                if entry.active_connections.load(Ordering::Relaxed) > 0 {
                    Some(keyspace.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Run a single eviction pass. Removes tenants that have zero active
    /// connections and have been idle longer than the configured timeout.
    /// Returns the list of evicted keyspace names.
    pub(crate) async fn evict_idle(&self) -> Vec<String> {
        let now = now_epoch_ms();
        let timeout_ms = self.idle_timeout.as_millis() as u64;

        let mut tenants = self.tenants.write().await;
        let mut evicted = Vec::new();

        tenants.retain(|keyspace, entry| {
            let conns = entry.active_connections.load(Ordering::Relaxed);
            if conns > 0 {
                return true; // Active connections — keep.
            }

            let idle_at = entry.last_idle_at.load(Ordering::Relaxed);
            if idle_at == 0 {
                return true; // Not yet marked idle (shouldn't happen, but be safe).
            }

            if now.saturating_sub(idle_at) >= timeout_ms {
                info!(
                    "Evicting idle tenant '{}' (idle for {}ms)",
                    keyspace,
                    now.saturating_sub(idle_at)
                );
                evicted.push(keyspace.clone());
                false // Remove from map.
            } else {
                true // Not yet expired — keep.
            }
        });

        // Also clean up creation locks for evicted keyspaces.
        if !evicted.is_empty() {
            let mut locks = self.creation_locks.write().await;
            for ks in &evicted {
                locks.remove(ks);
            }
        }

        evicted
    }

    /// Spawn a background reaper task that periodically evicts idle tenants.
    pub fn spawn_reaper(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(pool.reaper_interval);
            loop {
                interval.tick().await;
                let evicted = pool.evict_idle().await;
                if !evicted.is_empty() {
                    debug!("Reaper evicted {} idle tenant(s)", evicted.len());
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_entry(keyspace: &str) -> Arc<TenantEntry> {
        Arc::new(TenantEntry {
            store: TikvStore::new_stub(),
            active_connections: AtomicU32::new(0),
            last_idle_at: AtomicU64::new(0),
            keyspace: keyspace.to_string(),
            trigger_cache: Arc::new(TriggerBodyCache::new()),
            stats_cache: Arc::new(TableStatsCache::new()),
        })
    }

    impl TikvClientPool {
        async fn inject_entry(&self, keyspace: &str) -> Arc<TenantEntry> {
            let entry = make_test_entry(keyspace);
            let mut tenants = self.tenants.write().await;
            tenants.insert(keyspace.to_string(), entry.clone());
            entry
        }
    }

    fn make_handle(entry: &Arc<TenantEntry>) -> TenantHandle {
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        entry.last_idle_at.store(0, Ordering::Relaxed);
        TenantHandle {
            entry: entry.clone(),
        }
    }

    #[tokio::test]
    async fn test_pool_creation() {
        let pool = TikvClientPool::new(vec!["127.0.0.1:2379".to_string()]);
        assert_eq!(pool.tenant_count().await, 0);
    }

    #[tokio::test]
    async fn test_tenant_handle_refcount() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("test_ks").await;

        let h1 = make_handle(&entry);
        let h2 = make_handle(&entry);

        assert_eq!(entry.active_connections(), 2);
        assert_eq!(entry.last_idle_at.load(Ordering::Relaxed), 0);

        drop(h1);
        assert_eq!(entry.active_connections(), 1);
        assert_eq!(entry.last_idle_at.load(Ordering::Relaxed), 0);

        drop(h2);
        assert_eq!(entry.active_connections(), 0);
        assert_ne!(entry.last_idle_at.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_tenant_handle_clone_increments_refcount() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("clone_ks").await;

        let h1 = make_handle(&entry);
        let h2 = h1.clone();
        assert_eq!(entry.active_connections(), 2);

        drop(h1);
        assert_eq!(entry.active_connections(), 1);

        drop(h2);
        assert_eq!(entry.active_connections(), 0);
        assert_ne!(entry.last_idle_at.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_evict_idle_respects_timeout() {
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(100),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("idle_ks").await;

        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);

        entry.last_idle_at.store(now_epoch_ms(), Ordering::Relaxed);

        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);

        tokio::time::sleep(Duration::from_millis(150)).await;

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["idle_ks".to_string()]);
        assert_eq!(pool.tenant_count().await, 0);
    }

    #[tokio::test]
    async fn test_evict_preserves_active_tenants() {
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );

        let active_entry = pool.inject_entry("active_ks").await;
        let idle_entry = pool.inject_entry("idle_ks").await;

        active_entry
            .active_connections
            .fetch_add(1, Ordering::Relaxed);

        idle_entry
            .last_idle_at
            .store(now_epoch_ms(), Ordering::Relaxed);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["idle_ks".to_string()]);
        assert_eq!(pool.tenant_count().await, 1);

        assert!(pool.connections_for("active_ks").await.is_some());
        assert!(pool.connections_for("idle_ks").await.is_none());
    }

    #[tokio::test]
    async fn test_evict_resets_when_handle_reacquired() {
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(100),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("reacquire_ks").await;

        entry.last_idle_at.store(now_epoch_ms(), Ordering::Relaxed);

        tokio::time::sleep(Duration::from_millis(50)).await;
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        entry.last_idle_at.store(0, Ordering::Relaxed);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);
    }

    #[tokio::test]
    async fn test_get_client_marks_idle_immediately() {
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("bg_ks").await;
        entry.last_idle_at.store(now_epoch_ms(), Ordering::Relaxed);

        assert_eq!(entry.active_connections(), 0);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["bg_ks".to_string()]);
    }

    #[tokio::test]
    async fn test_concurrent_handle_drops() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("concurrent_ks").await;

        let num_handles = 100u32;

        let handles: Vec<TenantHandle> = (0..num_handles).map(|_| make_handle(&entry)).collect();

        assert_eq!(entry.active_connections(), num_handles);

        let mut join_handles = Vec::new();
        for h in handles {
            join_handles.push(tokio::spawn(async move {
                drop(h);
            }));
        }

        for jh in join_handles {
            jh.await.unwrap();
        }

        assert_eq!(entry.active_connections(), 0);
        assert_ne!(entry.last_idle_at.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_multiple_tenants_independent_lifecycle() {
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );

        let entry_a = pool.inject_entry("tenant_a").await;
        let entry_b = pool.inject_entry("tenant_b").await;
        let entry_c = pool.inject_entry("tenant_c").await;

        entry_a.active_connections.fetch_add(1, Ordering::Relaxed);

        entry_b
            .last_idle_at
            .store(now_epoch_ms(), Ordering::Relaxed);

        tokio::time::sleep(Duration::from_millis(100)).await;

        entry_c
            .last_idle_at
            .store(now_epoch_ms(), Ordering::Relaxed);

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["tenant_b".to_string()]);
        assert_eq!(pool.tenant_count().await, 2);
    }

    #[tokio::test]
    async fn test_active_tenant_count() {
        let pool = TikvClientPool::new(vec![]);

        let entry_a = pool.inject_entry("a").await;
        let entry_b = pool.inject_entry("b").await;
        let _entry_c = pool.inject_entry("c").await;

        entry_a.active_connections.fetch_add(1, Ordering::Relaxed);
        entry_b.active_connections.fetch_add(3, Ordering::Relaxed);

        assert_eq!(pool.active_tenant_count().await, 2);
        assert_eq!(pool.tenant_count().await, 3);
    }
}
