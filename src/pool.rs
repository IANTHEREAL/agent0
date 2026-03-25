use crate::sql::rls::cache::RlsPolicyCache;
use crate::sql::stats::TableStatsCache;
use crate::sql::triggers::TriggerBodyCache;
use crate::storage::TikvStore;
use anyhow::{anyhow, Result};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex as TokioMutex, RwLock};
use tracing::{debug, info};

/// Index of idle tenants ordered by `(idle_at_epoch_ms, keyspace)`.
///
/// Used by `evict_idle()` to find eviction candidates in O(k) time where k
/// is the number of candidates due for eviction, rather than scanning all tenants.
/// Protected by `std::sync::Mutex` (not tokio) so it can be updated from
/// `TenantHandle::Drop` without requiring an async runtime.
type IdleIndex = StdMutex<BTreeSet<(u64, String)>>;

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

/// Read the per-tenant QPS limit from environment once. 0 = disabled.
fn tenant_qps_limit() -> u64 {
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("DB9_TENANT_QPS_LIMIT")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    })
}

/// Read the per-tenant aggregate memory quota from environment once.
/// `0` means unlimited.
fn tenant_memory_quota_bytes() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("DB9_TENANT_MEMORY_QUOTA_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
    })
}

/// Per-tenant aggregate memory accounting domain.
///
/// All interactive/worker statements for the same keyspace share one
/// `TenantMemoryAccountant` so quota is enforced across concurrent sessions.
#[derive(Clone, Debug)]
pub struct TenantMemoryAccountant {
    #[allow(dead_code)] // observability/test helper
    keyspace: Arc<str>,
    used_bytes: Arc<AtomicUsize>,
    quota_bytes: usize,
}

impl TenantMemoryAccountant {
    fn new_with_quota(keyspace: String, quota_bytes: usize) -> Self {
        Self {
            keyspace: Arc::from(keyspace),
            used_bytes: Arc::new(AtomicUsize::new(0)),
            quota_bytes,
        }
    }

    /// Build an unlimited accountant (used by non-pooled startup paths).
    pub fn unlimited(keyspace: String) -> Self {
        Self::new_with_quota(keyspace, 0)
    }

    #[allow(dead_code)] // test helper
    pub fn quota_bytes(&self) -> usize {
        self.quota_bytes
    }

    #[allow(dead_code)] // test helper
    pub fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Relaxed)
    }

    #[allow(dead_code)] // test helper
    pub fn keyspace(&self) -> &str {
        &self.keyspace
    }

    fn try_charge(
        &self,
        component: &str,
        bytes: usize,
    ) -> std::result::Result<(), crate::sql::error::SqlError> {
        if bytes == 0 {
            return Ok(());
        }

        loop {
            let used = self.used_bytes.load(Ordering::Relaxed);
            let Some(new_used) = used.checked_add(bytes) else {
                return Err(crate::sql::error::SqlError::TenantMemoryQuotaExceeded {
                    component: component.to_string(),
                    requested_bytes: bytes,
                    used_bytes: used,
                    quota_bytes: self.quota_bytes,
                });
            };

            if self.quota_bytes > 0 && new_used > self.quota_bytes {
                return Err(crate::sql::error::SqlError::TenantMemoryQuotaExceeded {
                    component: component.to_string(),
                    requested_bytes: bytes,
                    used_bytes: used,
                    quota_bytes: self.quota_bytes,
                });
            }

            if self
                .used_bytes
                .compare_exchange_weak(used, new_used, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }

        loop {
            let used = self.used_bytes.load(Ordering::Relaxed);
            let new_used = used.saturating_sub(bytes);
            if self
                .used_bytes
                .compare_exchange_weak(used, new_used, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Build a zero-byte reservation owner that can grow over statement/runtime
    /// lifetime and auto-release on drop.
    pub fn reservation(&self) -> TenantMemoryReservation {
        TenantMemoryReservation {
            accountant: self.clone(),
            charged_bytes: 0,
        }
    }
}

/// RAII reservation over tenant aggregate memory budget.
///
/// The reservation can grow/shrink over time and always releases any remaining
/// bytes on drop.
#[derive(Debug)]
pub struct TenantMemoryReservation {
    accountant: TenantMemoryAccountant,
    charged_bytes: usize,
}

impl TenantMemoryReservation {
    #[allow(dead_code)] // test helper
    pub fn charged_bytes(&self) -> usize {
        self.charged_bytes
    }

    pub fn grow(
        &mut self,
        component: &str,
        delta: usize,
    ) -> std::result::Result<(), crate::sql::error::SqlError> {
        if delta == 0 {
            return Ok(());
        }
        self.accountant.try_charge(component, delta)?;
        self.charged_bytes = self.charged_bytes.saturating_add(delta);
        Ok(())
    }

    #[allow(dead_code)] // test helper
    pub fn shrink(&mut self, delta: usize) {
        let to_release = delta.min(self.charged_bytes);
        if to_release == 0 {
            return;
        }
        self.charged_bytes -= to_release;
        self.accountant.release(to_release);
    }

    /// Split `bytes` from this reservation into a new owner.
    ///
    /// Used when ownership transfers from statement scope to suspended portal
    /// buffer lifetime.
    pub fn split(&mut self, bytes: usize) -> Option<TenantMemoryReservation> {
        if bytes > self.charged_bytes {
            return None;
        }
        self.charged_bytes -= bytes;
        Some(TenantMemoryReservation {
            accountant: self.accountant.clone(),
            charged_bytes: bytes,
        })
    }
}

impl Drop for TenantMemoryReservation {
    fn drop(&mut self) {
        if self.charged_bytes > 0 {
            self.accountant.release(self.charged_bytes);
            self.charged_bytes = 0;
        }
    }
}

struct StatementMemoryScope {
    reservation: std::sync::Mutex<TenantMemoryReservation>,
}

tokio::task_local! {
    static STATEMENT_MEMORY_SCOPE: Arc<StatementMemoryScope>;
}

/// Run a future under a statement-scoped memory reservation owner.
///
/// The scope starts with 0 charged bytes and can grow via
/// `try_grow_statement_memory_scope()`. Any remaining bytes are auto-released
/// when the scope future completes.
pub async fn run_with_statement_memory_scope<Fut>(
    accountant: Option<TenantMemoryAccountant>,
    fut: Fut,
) -> Fut::Output
where
    Fut: std::future::Future,
{
    if let Some(accountant) = accountant {
        let scope = Arc::new(StatementMemoryScope {
            reservation: std::sync::Mutex::new(accountant.reservation()),
        });
        STATEMENT_MEMORY_SCOPE.scope(scope, fut).await
    } else {
        fut.await
    }
}

/// Charge bytes against the current statement scope.
///
/// Returns `Ok(())` when no statement scope is active (e.g. tests calling
/// operator helpers directly).
pub fn try_grow_statement_memory_scope(
    component: &str,
    bytes: usize,
) -> std::result::Result<(), crate::sql::error::SqlError> {
    if bytes == 0 {
        return Ok(());
    }
    if let Ok(scope) = STATEMENT_MEMORY_SCOPE.try_with(Arc::clone) {
        let mut guard = scope.reservation.lock().unwrap_or_else(|e| e.into_inner());
        guard.grow(component, bytes)
    } else {
        Ok(())
    }
}

/// Release bytes from the current statement scope.
///
/// No-op when no statement scope is active.
pub fn try_shrink_statement_memory_scope(bytes: usize) {
    if bytes == 0 {
        return;
    }
    if let Ok(scope) = STATEMENT_MEMORY_SCOPE.try_with(Arc::clone) {
        let mut guard = scope.reservation.lock().unwrap_or_else(|e| e.into_inner());
        guard.shrink(bytes);
    }
}

/// Split bytes out of the current statement scope into an independent owner.
///
/// Used to hand off memory ownership from statement lifetime to suspended
/// portal lifetime.
pub fn split_statement_memory_scope(bytes: usize) -> Option<TenantMemoryReservation> {
    if bytes == 0 {
        return None;
    }
    STATEMENT_MEMORY_SCOPE
        .try_with(|scope| {
            let mut guard = scope.reservation.lock().unwrap_or_else(|e| e.into_inner());
            guard.split(bytes)
        })
        .ok()
        .flatten()
}

/// Simple token bucket rate limiter.
///
/// Allows up to `rate` requests per second with burst capacity equal to `rate`.
pub(crate) struct TokenBucket {
    state: std::sync::Mutex<TokenBucketState>,
    rate: u64,
}

struct TokenBucketState {
    tokens: f64,
    last_refill: std::time::Instant,
}

impl TokenBucket {
    fn new(rate: u64) -> Self {
        Self {
            state: std::sync::Mutex::new(TokenBucketState {
                tokens: rate as f64,
                last_refill: std::time::Instant::now(),
            }),
            rate,
        }
    }

    /// Try to consume one token. Returns `true` if the request is allowed.
    pub(crate) fn try_acquire(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(state.last_refill).as_secs_f64();
        let capacity = self.rate as f64;
        state.tokens = (state.tokens + elapsed * capacity).min(capacity);
        state.last_refill = now;
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// The configured rate (queries per second).
    pub(crate) fn rate(&self) -> u64 {
        self.rate
    }

    /// Drain all tokens so the next `try_acquire` returns false.
    #[cfg(test)]
    pub(crate) fn drain(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.tokens = 0.0;
        state.last_refill = std::time::Instant::now();
    }
}

/// Per-tenant metadata inside the pool.
#[allow(dead_code)] // test: fields accessed by pool lifecycle tests
pub(crate) struct TenantEntry {
    store: Arc<TikvStore>,
    /// Number of active connection-scoped handles (TenantHandle instances).
    active_connections: AtomicU32,
    /// Epoch millis when the last handle was dropped (connections went to zero).
    /// Zero means there are active connections or it was never idle.
    last_idle_at: AtomicU64,
    keyspace: String,
    trigger_cache: Arc<TriggerBodyCache>,
    rls_policy_cache: Arc<RlsPolicyCache>,
    stats_cache: Arc<TableStatsCache>,
    /// Per-tenant QPS rate limiter. `None` when rate limiting is disabled (limit = 0).
    rate_limiter: Option<TokenBucket>,
    /// Shared per-tenant aggregate memory ledger/quota.
    memory_accountant: TenantMemoryAccountant,
    /// Per-user active connection counts for `rolconnlimit` enforcement.
    user_connections: std::sync::Mutex<HashMap<String, u32>>,
    /// Back-reference to the pool-level idle index for Drop-time updates.
    /// `None` only in test entries created outside a pool.
    idle_index: Option<Arc<IdleIndex>>,
}

impl TenantEntry {
    fn new(store: Arc<TikvStore>, keyspace: String, idle_index: Arc<IdleIndex>) -> Self {
        let qps_limit = tenant_qps_limit();
        let memory_quota = tenant_memory_quota_bytes();
        Self {
            store,
            active_connections: AtomicU32::new(0),
            last_idle_at: AtomicU64::new(0),
            keyspace: keyspace.clone(),
            trigger_cache: Arc::new(TriggerBodyCache::new()),
            rls_policy_cache: Arc::new(RlsPolicyCache::new()),
            stats_cache: Arc::new(TableStatsCache::new()),
            rate_limiter: if qps_limit > 0 {
                Some(TokenBucket::new(qps_limit))
            } else {
                None
            },
            memory_accountant: TenantMemoryAccountant::new_with_quota(keyspace, memory_quota),
            user_connections: std::sync::Mutex::new(HashMap::new()),
            idle_index: Some(idle_index),
        }
    }

    /// Try to acquire a per-user connection slot.
    /// `limit < 0` means unlimited (PostgreSQL `rolconnlimit = -1`).
    /// Returns `true` if the slot was acquired, `false` if the limit is reached.
    fn try_acquire_user_slot(&self, username: &str, limit: i32) -> bool {
        let mut map = self
            .user_connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let count = map.entry(username.to_string()).or_insert(0);
        if limit >= 0 && (*count as i32) >= limit {
            return false;
        }
        *count += 1;
        true
    }

    /// Release a per-user connection slot.
    fn release_user_slot(&self, username: &str) {
        let mut map = self
            .user_connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = map.get_mut(username) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(username);
            }
        }
    }

    #[allow(dead_code)] // test: used in pool tests
    pub(crate) fn store(&self) -> &Arc<TikvStore> {
        &self.store
    }

    #[allow(dead_code)] // test: used in pool tests
    pub(crate) fn active_connections(&self) -> u32 {
        self.active_connections.load(Ordering::Relaxed)
    }

    /// Per-user connection count snapshot (for tests).
    #[allow(dead_code)] // test: used in pool tests
    pub(crate) fn user_connections_for(&self, username: &str) -> u32 {
        let map = self
            .user_connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.get(username).copied().unwrap_or(0)
    }

    #[allow(dead_code)] // test: used in pool tests
    pub(crate) fn memory_accountant(&self) -> TenantMemoryAccountant {
        self.memory_accountant.clone()
    }
}

/// RAII handle representing one connection's hold on a tenant's TikvStore.
///
/// When dropped, decrements the tenant's active connection count. If the count
/// reaches zero, the tenant becomes eligible for eviction after the idle timeout.
pub struct TenantHandle {
    entry: Arc<TenantEntry>,
    /// If set, this handle owns a per-user connection slot that will be
    /// released on drop. Clones do NOT inherit user slots.
    user_slot: Option<String>,
}

impl TenantHandle {
    pub fn store(&self) -> &Arc<TikvStore> {
        &self.entry.store
    }

    pub fn trigger_cache(&self) -> &Arc<TriggerBodyCache> {
        &self.entry.trigger_cache
    }

    pub fn rls_policy_cache(&self) -> &Arc<RlsPolicyCache> {
        &self.entry.rls_policy_cache
    }

    pub fn stats_cache(&self) -> &Arc<TableStatsCache> {
        &self.entry.stats_cache
    }

    pub fn rate_limiter(&self) -> Option<&TokenBucket> {
        self.entry.rate_limiter.as_ref()
    }

    pub fn keyspace(&self) -> &str {
        &self.entry.keyspace
    }

    pub fn memory_accountant(&self) -> TenantMemoryAccountant {
        self.entry.memory_accountant.clone()
    }

    /// Bind this handle to a user and acquire a per-user connection slot.
    /// Returns an error message if the user's `connection_limit` is exceeded.
    /// `connection_limit < 0` means unlimited (PostgreSQL `rolconnlimit = -1`).
    pub fn try_bind_user(&mut self, username: String, connection_limit: i32) -> Result<(), String> {
        if self.user_slot.is_some() {
            return Ok(()); // Already bound.
        }
        if self
            .entry
            .try_acquire_user_slot(&username, connection_limit)
        {
            self.user_slot = Some(username);
            Ok(())
        } else {
            Err(format!("too many connections for role \"{}\"", username))
        }
    }

    /// Create a TenantHandle with a rate limiter set to the given QPS limit.
    /// The bucket starts full (all tokens available).
    #[cfg(test)]
    pub(crate) fn new_with_rate_limit(qps_limit: u64) -> Self {
        Self::new_with_limits(qps_limit, 0)
    }

    /// Minimal TenantHandle for unit tests that don't need pool/store access.
    #[cfg(test)]
    pub(crate) fn dummy_for_test() -> Self {
        Self::new_with_limits(0, 0)
    }

    /// Create a TenantHandle with explicit QPS+memory limits.
    #[cfg(test)]
    pub(crate) fn new_with_limits(qps_limit: u64, memory_quota_bytes: usize) -> Self {
        let entry = Arc::new(TenantEntry {
            store: TikvStore::new_stub(),
            active_connections: AtomicU32::new(1),
            last_idle_at: AtomicU64::new(0),
            keyspace: "test_ks".to_string(),
            trigger_cache: Arc::new(TriggerBodyCache::new()),
            rls_policy_cache: Arc::new(RlsPolicyCache::new()),
            stats_cache: Arc::new(TableStatsCache::new()),
            rate_limiter: if qps_limit > 0 {
                Some(TokenBucket::new(qps_limit))
            } else {
                None
            },
            memory_accountant: TenantMemoryAccountant::new_with_quota(
                "test_ks".to_string(),
                memory_quota_bytes,
            ),
            user_connections: std::sync::Mutex::new(HashMap::new()),
            idle_index: None,
        });
        Self {
            entry,
            user_slot: None,
        }
    }
}

impl Clone for TenantHandle {
    fn clone(&self) -> Self {
        let prev = self
            .entry
            .active_connections
            .fetch_add(1, Ordering::Relaxed);
        let prev_idle_at = self.entry.last_idle_at.swap(0, Ordering::Relaxed);
        // If transitioning from idle to active, remove from idle index.
        if prev == 0 && prev_idle_at != 0 {
            if let Some(ref idx) = self.entry.idle_index {
                if let Ok(mut set) = idx.lock() {
                    set.remove(&(prev_idle_at, self.entry.keyspace.clone()));
                }
            }
        }
        Self {
            entry: self.entry.clone(),
            // Clones do NOT inherit the user connection slot — only the
            // original handle owns (and releases) the per-user slot.
            user_slot: None,
        }
    }
}

impl Drop for TenantHandle {
    fn drop(&mut self) {
        if let Some(ref username) = self.user_slot {
            self.entry.release_user_slot(username);
        }
        let prev = self
            .entry
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            // This was the last handle — record idle start time.
            let idle_at = now_epoch_ms();
            self.entry.last_idle_at.store(idle_at, Ordering::Relaxed);
            // Register as idle candidate in the pool-level index.
            if let Some(ref idx) = self.entry.idle_index {
                if let Ok(mut set) = idx.lock() {
                    set.insert((idle_at, self.entry.keyspace.clone()));
                }
            }
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
    /// Idle-time index: tenants ordered by `(idle_at_epoch_ms, keyspace)`.
    /// Enables O(k) eviction where k = candidates due, instead of O(n) full scan.
    idle_index: Arc<IdleIndex>,
}

impl TikvClientPool {
    pub fn new(pd_endpoints: Vec<String>) -> Self {
        Self {
            pd_endpoints,
            tenants: RwLock::new(HashMap::new()),
            creation_locks: RwLock::new(HashMap::new()),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            reaper_interval: DEFAULT_REAPER_INTERVAL,
            idle_index: Arc::new(StdMutex::new(BTreeSet::new())),
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
            idle_index: Arc::new(StdMutex::new(BTreeSet::new())),
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
                let prev = entry.active_connections.fetch_add(1, Ordering::Relaxed);
                let prev_idle_at = entry.last_idle_at.swap(0, Ordering::Relaxed);
                // Transitioning from idle to active — remove from idle index.
                if prev == 0 && prev_idle_at != 0 {
                    if let Ok(mut set) = self.idle_index.lock() {
                        set.remove(&(prev_idle_at, key.clone()));
                    }
                }
                return Ok(TenantHandle {
                    entry: entry.clone(),
                    user_slot: None,
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
                let entry = entry.clone();
                let prev = entry.active_connections.fetch_add(1, Ordering::Relaxed);
                let prev_idle_at = entry.last_idle_at.swap(0, Ordering::Relaxed);
                if prev == 0 && prev_idle_at != 0 {
                    if let Ok(mut set) = self.idle_index.lock() {
                        set.remove(&(prev_idle_at, key.clone()));
                    }
                }
                drop(tenants);
                // Construct handle before the `.await` so that async cancellation
                // triggers `TenantHandle::drop`, which decrements `active_connections`.
                let handle = TenantHandle {
                    entry,
                    user_slot: None,
                };
                self.cleanup_creation_lock_if_unused(&key, &creation_lock)
                    .await;
                return Ok(handle);
            }
        }

        info!("Creating new TiKV client for keyspace: {}", key);
        let store = match self.create_store(&key, keyspace).await {
            Ok(s) => s,
            Err(e) => {
                self.cleanup_creation_lock_if_unused(&key, &creation_lock)
                    .await;
                return Err(e);
            }
        };
        let entry = Arc::new(TenantEntry::new(
            store,
            key.clone(),
            self.idle_index.clone(),
        ));
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        // Construct handle before any `.await` so that async cancellation
        // triggers `TenantHandle::drop`, which decrements `active_connections`.
        let handle = TenantHandle {
            entry: entry.clone(),
            user_slot: None,
        };

        {
            let mut tenants = self.tenants.write().await;
            tenants.insert(key.clone(), entry);
        }
        self.cleanup_creation_lock_if_unused(&key, &creation_lock)
            .await;

        Ok(handle)
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
                let store = entry.store.clone();
                drop(tenants);
                self.cleanup_creation_lock_if_unused(&key, &creation_lock)
                    .await;
                return Ok(store);
            }
        }

        info!("Creating new TiKV client for keyspace: {}", key);
        let store = match self.create_store(&key, keyspace).await {
            Ok(s) => s,
            Err(e) => {
                self.cleanup_creation_lock_if_unused(&key, &creation_lock)
                    .await;
                return Err(e);
            }
        };
        let entry = Arc::new(TenantEntry::new(
            store,
            key.clone(),
            self.idle_index.clone(),
        ));
        // Mark as idle immediately since no handle is held.
        let idle_at = now_epoch_ms();
        entry.last_idle_at.store(idle_at, Ordering::Relaxed);
        if let Ok(mut set) = self.idle_index.lock() {
            set.insert((idle_at, key.clone()));
        }

        let store_clone = entry.store.clone();
        {
            let mut tenants = self.tenants.write().await;
            tenants.insert(key.clone(), entry);
        }
        self.cleanup_creation_lock_if_unused(&key, &creation_lock)
            .await;

        Ok(store_clone)
    }

    /// Remove a per-keyspace creation lock after a slow-path operation
    /// when no concurrent waiters remain.
    async fn cleanup_creation_lock_if_unused(
        &self,
        key: &str,
        creation_lock: &Arc<TokioMutex<()>>,
    ) {
        let mut locks = self.creation_locks.write().await;
        let should_remove = locks
            .get(key)
            .is_some_and(|existing| Arc::ptr_eq(existing, creation_lock))
            && Arc::strong_count(creation_lock) == 2;
        if should_remove {
            locks.remove(key);
        }
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
    #[allow(dead_code)] // test: used in pool tests
    pub async fn tenant_count(&self) -> usize {
        self.tenants.read().await.len()
    }

    /// Number of tenants with at least one active connection handle.
    #[allow(dead_code)] // test: used in pool tests
    pub async fn active_tenant_count(&self) -> usize {
        let tenants = self.tenants.read().await;
        tenants
            .values()
            .filter(|e| e.active_connections.load(Ordering::Relaxed) > 0)
            .count()
    }

    /// Snapshot of the active connection count for a specific keyspace.
    /// Returns None if the keyspace is not in the pool.
    #[allow(dead_code)] // test: used in pool tests
    pub async fn connections_for(&self, keyspace: &str) -> Option<u32> {
        let tenants = self.tenants.read().await;
        tenants
            .get(keyspace)
            .map(|e| e.active_connections.load(Ordering::Relaxed))
    }

    /// Run a single eviction pass. Uses the idle-time index to find candidates
    /// in O(k) time where k is the number of due candidates, instead of scanning
    /// all tenants.
    ///
    /// Stale index entries (tenant reacquired or re-idled at a different timestamp)
    /// are validated against the current tenant state before eviction: a candidate
    /// `(idle_at, keyspace)` is only evicted when the tenant still exists, has zero
    /// active connections, and its current `last_idle_at` matches the candidate.
    pub(crate) async fn evict_idle(&self) -> Vec<String> {
        let now = now_epoch_ms();
        let timeout_ms = self.idle_timeout.as_millis() as u64;
        let deadline = now.saturating_sub(timeout_ms);

        // Collect due candidates from the idle index.
        // The index is ordered by (idle_at, keyspace), so we take all entries
        // with idle_at <= deadline.
        let candidates: Vec<(u64, String)> = {
            let mut idx = self.idle_index.lock().expect("idle_index lock");
            let mut due = Vec::new();
            // BTreeSet iteration in ascending order — stop at first entry past deadline.
            while let Some(first) = idx.iter().next().cloned() {
                if first.0 > deadline {
                    break;
                }
                idx.remove(&first);
                due.push(first);
            }
            due
        };

        if candidates.is_empty() {
            return Vec::new();
        }

        let mut tenants = self.tenants.write().await;
        let mut evicted = Vec::new();

        for (candidate_idle_at, keyspace) in candidates {
            let should_evict = tenants.get(&keyspace).is_some_and(|entry| {
                let conns = entry.active_connections.load(Ordering::Relaxed);
                let current_idle_at = entry.last_idle_at.load(Ordering::Relaxed);
                // Only evict if tenant is still idle and the idle timestamp matches
                // the candidate (guards against stale/duplicate index entries).
                conns == 0 && current_idle_at == candidate_idle_at
            });

            if should_evict {
                info!(
                    "Evicting idle tenant '{}' (idle for {}ms)",
                    keyspace,
                    now.saturating_sub(candidate_idle_at)
                );
                tenants.remove(&keyspace);
                evicted.push(keyspace);
            }
        }

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

    fn make_test_entry_with_index(
        keyspace: &str,
        idle_index: Option<Arc<IdleIndex>>,
    ) -> Arc<TenantEntry> {
        Arc::new(TenantEntry {
            store: TikvStore::new_stub(),
            active_connections: AtomicU32::new(0),
            last_idle_at: AtomicU64::new(0),
            keyspace: keyspace.to_string(),
            trigger_cache: Arc::new(TriggerBodyCache::new()),
            rls_policy_cache: Arc::new(RlsPolicyCache::new()),
            stats_cache: Arc::new(TableStatsCache::new()),
            rate_limiter: None,
            memory_accountant: TenantMemoryAccountant::new_with_quota(keyspace.to_string(), 0),
            user_connections: std::sync::Mutex::new(HashMap::new()),
            idle_index,
        })
    }

    impl TikvClientPool {
        async fn inject_entry(&self, keyspace: &str) -> Arc<TenantEntry> {
            let entry = make_test_entry_with_index(keyspace, Some(self.idle_index.clone()));
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
            user_slot: None,
        }
    }

    #[tokio::test]
    async fn test_pool_creation() {
        let pool = TikvClientPool::new(vec!["127.0.0.1:2379".to_string()]);
        assert_eq!(pool.tenant_count().await, 0);
    }

    #[tokio::test]
    async fn test_creation_lock_removed_after_create_store_failure() {
        let pool = TikvClientPool::new(vec!["invalid-pd-endpoint".to_string()]);
        let keyspace = "lock_cleanup_failure";

        let err = match pool.acquire(Some(keyspace.to_string())).await {
            Ok(_) => panic!("acquire should fail when create_store cannot connect"),
            Err(err) => err,
        };
        assert!(
            !err.to_string().is_empty(),
            "failure should return an error message"
        );

        let locks = pool.creation_locks.read().await;
        assert!(
            !locks.contains_key(keyspace),
            "creation lock must be removed after failed create_store"
        );
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

    /// Helper: mark a test entry as idle and register it in the pool's idle index.
    fn mark_idle(entry: &Arc<TenantEntry>, pool: &TikvClientPool) {
        let idle_at = now_epoch_ms();
        entry.last_idle_at.store(idle_at, Ordering::Relaxed);
        if let Ok(mut set) = pool.idle_index.lock() {
            set.insert((idle_at, entry.keyspace.clone()));
        }
    }

    /// Helper: mark a test entry as idle at a specific timestamp.
    fn mark_idle_at(entry: &Arc<TenantEntry>, pool: &TikvClientPool, idle_at: u64) {
        entry.last_idle_at.store(idle_at, Ordering::Relaxed);
        if let Ok(mut set) = pool.idle_index.lock() {
            set.insert((idle_at, entry.keyspace.clone()));
        }
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

        mark_idle(&entry, &pool);

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

        mark_idle(&idle_entry, &pool);

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

        mark_idle(&entry, &pool);

        tokio::time::sleep(Duration::from_millis(50)).await;
        // Simulate reacquire: transition back to active.
        let prev_idle_at = entry.last_idle_at.swap(0, Ordering::Relaxed);
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        if prev_idle_at != 0 {
            if let Ok(mut set) = pool.idle_index.lock() {
                set.remove(&(prev_idle_at, entry.keyspace.clone()));
            }
        }

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
        mark_idle(&entry, &pool);

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

        mark_idle(&entry_b, &pool);

        tokio::time::sleep(Duration::from_millis(100)).await;

        mark_idle(&entry_c, &pool);

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

    #[test]
    fn test_token_bucket_allows_up_to_capacity() {
        let bucket = TokenBucket::new(10);
        for _ in 0..10 {
            assert!(bucket.try_acquire());
        }
        // 11th request within the same instant should be denied
        assert!(!bucket.try_acquire());
    }

    #[test]
    fn test_token_bucket_refills_over_time() {
        let bucket = TokenBucket::new(10);
        // Exhaust all tokens
        for _ in 0..10 {
            assert!(bucket.try_acquire());
        }
        assert!(!bucket.try_acquire());

        // Wait 200ms → ~2 tokens should refill
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(bucket.try_acquire());
    }

    #[test]
    fn test_token_bucket_rate_accessor() {
        let bucket = TokenBucket::new(42);
        assert_eq!(bucket.rate(), 42);
    }

    #[tokio::test]
    async fn test_per_user_connection_limit_enforcement() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("limit_ks").await;

        // user_a with limit=1: first connection succeeds.
        let mut h1 = make_handle(&entry);
        assert!(h1.try_bind_user("user_a".to_string(), 1).is_ok());
        assert_eq!(entry.user_connections_for("user_a"), 1);

        // user_a at limit → second connection rejected.
        let mut h2 = make_handle(&entry);
        assert!(h2.try_bind_user("user_a".to_string(), 1).is_err());
        assert_eq!(entry.user_connections_for("user_a"), 1);
        drop(h2); // Never bound, no user slot to release.

        // user_b with limit=-1 (unlimited) → connects fine on same tenant.
        let mut h3 = make_handle(&entry);
        assert!(h3.try_bind_user("user_b".to_string(), -1).is_ok());
        assert_eq!(entry.user_connections_for("user_b"), 1);
        drop(h3);
        assert_eq!(entry.user_connections_for("user_b"), 0);

        // user_a disconnects → user_a can reconnect.
        drop(h1);
        assert_eq!(entry.user_connections_for("user_a"), 0);

        let mut h4 = make_handle(&entry);
        assert!(h4.try_bind_user("user_a".to_string(), 1).is_ok());
        assert_eq!(entry.user_connections_for("user_a"), 1);
        drop(h4);
    }

    #[tokio::test]
    async fn test_per_user_limit_zero_rejects_all() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("zero_ks").await;

        // connection_limit=0 should reject immediately.
        let mut h = make_handle(&entry);
        assert!(h.try_bind_user("blocked_user".to_string(), 0).is_err());
        assert_eq!(entry.user_connections_for("blocked_user"), 0);
        drop(h);
    }

    #[tokio::test]
    async fn test_per_user_clone_does_not_inherit_slot() {
        let pool = TikvClientPool::new(vec![]);
        let entry = pool.inject_entry("clone_user_ks").await;

        let mut h1 = make_handle(&entry);
        assert!(h1.try_bind_user("user_x".to_string(), 1).is_ok());
        assert_eq!(entry.user_connections_for("user_x"), 1);

        // Clone does NOT inherit the user slot.
        let h2 = h1.clone();
        assert_eq!(entry.user_connections_for("user_x"), 1);
        assert_eq!(entry.active_connections(), 2); // h1 + clone

        // Dropping clone does not release user slot.
        drop(h2);
        assert_eq!(entry.user_connections_for("user_x"), 1);

        // Dropping original releases the user slot.
        drop(h1);
        assert_eq!(entry.user_connections_for("user_x"), 0);
    }

    #[test]
    fn tenant_memory_accountant_unlimited_mode() {
        let accountant = TenantMemoryAccountant::new_with_quota("ks_unlimited".to_string(), 0);
        let mut reservation = accountant.reservation();
        reservation
            .grow("test.unlimited", 10 * 1024 * 1024)
            .expect("unlimited quota must allow reservation");
        assert_eq!(reservation.charged_bytes(), 10 * 1024 * 1024);
        assert_eq!(accountant.used_bytes(), 10 * 1024 * 1024);
        drop(reservation);
        assert_eq!(accountant.used_bytes(), 0);
    }

    #[test]
    fn tenant_memory_reservation_no_underflow_or_overrelease() {
        let accountant = TenantMemoryAccountant::new_with_quota("ks_underflow".to_string(), 1024);
        let mut reservation = accountant.reservation();
        reservation
            .grow("test.underflow", 512)
            .expect("initial grow should pass");
        reservation.shrink(2048); // deliberate over-release attempt
        assert_eq!(reservation.charged_bytes(), 0);
        assert_eq!(accountant.used_bytes(), 0);
    }

    #[test]
    fn tenant_memory_accountant_concurrent_charge_release_correctness() {
        let accountant = TenantMemoryAccountant::new_with_quota("ks_concurrent".to_string(), 0);
        let mut workers = Vec::new();
        for _ in 0..16 {
            let acc = accountant.clone();
            workers.push(std::thread::spawn(move || {
                for _ in 0..2000 {
                    let mut r = acc.reservation();
                    r.grow("test.concurrent", 64)
                        .expect("unlimited quota should allow charge");
                }
            }));
        }
        for w in workers {
            w.join().expect("thread join");
        }
        assert_eq!(accountant.used_bytes(), 0);
    }

    #[test]
    fn tenant_memory_accountant_is_shared_across_concurrent_handles() {
        // Simulate two concurrent sessions on the same tenant handle domain.
        let h1 = TenantHandle::new_with_limits(0, 100);
        let h2 = h1.clone();
        let acc_a = h1.memory_accountant();
        let acc_b = h2.memory_accountant();

        let mut sess_a = acc_a.reservation();
        sess_a
            .grow("session_a", 80)
            .expect("session A reservation should pass");

        let mut sess_b = acc_b.reservation();
        let err = sess_b
            .grow("session_b", 30)
            .expect_err("combined usage should exceed tenant quota");
        assert_eq!(err.sqlstate(), "53200");

        // Once session A releases, session B should be able to proceed.
        drop(sess_a);
        sess_b
            .grow("session_b", 30)
            .expect("reservation should succeed after A release");
    }

    #[tokio::test]
    async fn statement_scope_runtime_shrink_tracks_live_bytes() {
        let accountant = TenantMemoryAccountant::new_with_quota("ks_scope".to_string(), 128);
        run_with_statement_memory_scope(Some(accountant.clone()), async {
            try_grow_statement_memory_scope("scope.grow", 100).expect("grow should succeed");
            assert_eq!(accountant.used_bytes(), 100);
            try_shrink_statement_memory_scope(40);
            assert_eq!(accountant.used_bytes(), 60);
        })
        .await;
        assert_eq!(accountant.used_bytes(), 0);
    }

    // ── idle-index stale-candidate tests ──────────────────────────────

    #[tokio::test]
    async fn test_idle_index_reacquire_before_expiry_prevents_eviction() {
        // A tenant goes idle, gets a candidate in the index, then reacquires
        // before the timeout. The stale candidate must not cause eviction.
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(80),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("stale_ks").await;

        // Go idle.
        mark_idle(&entry, &pool);
        let old_idle_at = entry.last_idle_at.load(Ordering::Relaxed);

        // Reacquire before expiry.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let prev_idle = entry.last_idle_at.swap(0, Ordering::Relaxed);
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        if prev_idle != 0 {
            if let Ok(mut set) = pool.idle_index.lock() {
                set.remove(&(prev_idle, entry.keyspace.clone()));
            }
        }

        // Wait past the original timeout.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Eviction should find nothing — stale candidate was removed.
        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);

        // Verify the old candidate is gone from the index.
        let idx_len = pool.idle_index.lock().unwrap().len();
        assert_eq!(idx_len, 0);

        // Make sure the old idle_at was indeed removed (not left dangling).
        assert_eq!(entry.last_idle_at.load(Ordering::Relaxed), 0);
        let _ = old_idle_at; // used above via mark_idle
    }

    #[tokio::test]
    async fn test_idle_index_duplicate_candidates_are_harmless() {
        // If a tenant goes idle, gets reacquired, goes idle again — the index
        // may briefly have two entries. Only the current one should cause eviction.
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(150),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("dup_ks").await;

        // First idle cycle.
        let first_idle = now_epoch_ms();
        mark_idle_at(&entry, &pool, first_idle);

        // Reacquire.
        entry.active_connections.fetch_add(1, Ordering::Relaxed);
        entry.last_idle_at.store(0, Ordering::Relaxed);
        // Intentionally do NOT remove the old index entry — simulates a race.

        // Second idle cycle with a large gap so the first candidate expires
        // well before the second.
        tokio::time::sleep(Duration::from_millis(100)).await;
        entry.active_connections.fetch_sub(1, Ordering::Relaxed);
        let second_idle = now_epoch_ms();
        mark_idle_at(&entry, &pool, second_idle);

        // Wait past the first candidate's expiry but not the second.
        // First candidate: first_idle + 150ms. Now is ~ first_idle + 100 + 70 = first_idle + 170.
        // Second candidate: second_idle + 150ms = (first_idle + 100) + 150 = first_idle + 250.
        tokio::time::sleep(Duration::from_millis(70)).await;

        // The first candidate is stale (idle_at mismatch) — should NOT evict.
        // The second candidate is not yet due.
        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);

        // Wait for the second candidate to expire.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["dup_ks".to_string()]);
        assert_eq!(pool.tenant_count().await, 0);
    }

    #[tokio::test]
    async fn test_idle_index_evicted_and_recreated_keyspace() {
        // A tenant is evicted, then the same keyspace is recreated.
        // Verify that stale index entries from the old tenant don't interfere.
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );

        // First tenant.
        let entry1 = pool.inject_entry("recycle_ks").await;
        mark_idle(&entry1, &pool);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let evicted = pool.evict_idle().await;
        assert_eq!(evicted, vec!["recycle_ks".to_string()]);
        assert_eq!(pool.tenant_count().await, 0);

        // Recreate tenant with same keyspace.
        let entry2 = pool.inject_entry("recycle_ks").await;
        entry2.active_connections.fetch_add(1, Ordering::Relaxed);

        // No eviction should happen — new tenant is active.
        let evicted = pool.evict_idle().await;
        assert!(evicted.is_empty());
        assert_eq!(pool.tenant_count().await, 1);
    }

    #[tokio::test]
    async fn test_idle_index_drop_registers_candidate() {
        // Verify that TenantHandle::Drop correctly registers an idle candidate.
        let pool = TikvClientPool::new_with_timeouts(
            vec![],
            Duration::from_millis(50),
            Duration::from_secs(60),
        );
        let entry = pool.inject_entry("drop_ks").await;
        let handle = make_handle(&entry);

        assert_eq!(entry.active_connections(), 1);
        assert!(pool.idle_index.lock().unwrap().is_empty());

        drop(handle);

        assert_eq!(entry.active_connections(), 0);
        assert_ne!(entry.last_idle_at.load(Ordering::Relaxed), 0);
        let idx = pool.idle_index.lock().unwrap();
        assert_eq!(idx.len(), 1);
        let (_, ks) = idx.iter().next().unwrap();
        assert_eq!(ks, "drop_ks");
    }
}
