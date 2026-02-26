//! PostgreSQL advisory locks, implemented as process-local in-memory state.
//!
//! These locks are node-local: sessions connected to different db9-server
//! processes do not coordinate advisory lock state.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::Notify;

const DEFAULT_MAX_ADVISORY_LOCKS_PER_CONNECTION: usize = 4096;
const ENV_MAX_ADVISORY_LOCKS_PER_CONNECTION: &str = "DB9_MAX_ADVISORY_LOCKS_PER_CONNECTION";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AdvisoryLockMode {
    Exclusive,
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AdvisoryLockScope {
    Session,
    Transaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcquireError {
    Timeout,
    LockLimitExceeded { limit: usize },
    CounterOverflow,
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "canceling statement due to lock timeout"),
            Self::LockLimitExceeded { limit } => write!(
                f,
                "too many advisory locks held by this session (limit: {})",
                limit
            ),
            Self::CounterOverflow => {
                write!(f, "advisory lock reentrant acquisition count overflow")
            }
        }
    }
}

impl std::error::Error for AcquireError {}

#[derive(Default)]
struct HolderInfo {
    session_count: u32,
    xact_count: u32,
}

impl HolderInfo {
    fn total(&self) -> u32 {
        self.session_count.saturating_add(self.xact_count)
    }

    fn is_empty(&self) -> bool {
        self.session_count == 0 && self.xact_count == 0
    }
}

fn parse_max_advisory_locks_from_env() -> Option<usize> {
    let Some(raw) = std::env::var_os(ENV_MAX_ADVISORY_LOCKS_PER_CONNECTION) else {
        return Some(DEFAULT_MAX_ADVISORY_LOCKS_PER_CONNECTION);
    };
    let Some(text) = raw.to_str() else {
        tracing::warn!(
            env = ENV_MAX_ADVISORY_LOCKS_PER_CONNECTION,
            "ignoring non-utf8 advisory lock limit env value"
        );
        return Some(DEFAULT_MAX_ADVISORY_LOCKS_PER_CONNECTION);
    };
    match text.trim().parse::<usize>() {
        Ok(0) => None, // 0 disables the cap.
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                env = ENV_MAX_ADVISORY_LOCKS_PER_CONNECTION,
                value = text,
                error = %e,
                "ignoring invalid advisory lock limit env value"
            );
            Some(DEFAULT_MAX_ADVISORY_LOCKS_PER_CONNECTION)
        }
    }
}

struct LockState {
    exclusive_holders: HashMap<i64, HolderInfo>,
    shared_holders: HashMap<i64, HolderInfo>,
}

impl LockState {
    fn new() -> Self {
        Self {
            exclusive_holders: HashMap::new(),
            shared_holders: HashMap::new(),
        }
    }

    fn can_grant(&self, conn_id: i64, mode: AdvisoryLockMode) -> bool {
        match mode {
            AdvisoryLockMode::Exclusive => {
                let other_excl = self
                    .exclusive_holders
                    .iter()
                    .any(|(&id, info)| id != conn_id && info.total() > 0);
                let other_shared = self
                    .shared_holders
                    .iter()
                    .any(|(&id, info)| id != conn_id && info.total() > 0);
                !other_excl && !other_shared
            }
            AdvisoryLockMode::Shared => {
                let other_excl = self
                    .exclusive_holders
                    .iter()
                    .any(|(&id, info)| id != conn_id && info.total() > 0);
                !other_excl
            }
        }
    }

    fn grant(
        &mut self,
        conn_id: i64,
        mode: AdvisoryLockMode,
        scope: AdvisoryLockScope,
    ) -> Result<(), AcquireError> {
        let holders = match mode {
            AdvisoryLockMode::Exclusive => &mut self.exclusive_holders,
            AdvisoryLockMode::Shared => &mut self.shared_holders,
        };
        let info = holders.entry(conn_id).or_default();
        match scope {
            AdvisoryLockScope::Session => {
                info.session_count = info
                    .session_count
                    .checked_add(1)
                    .ok_or(AcquireError::CounterOverflow)?;
            }
            AdvisoryLockScope::Transaction => {
                info.xact_count = info
                    .xact_count
                    .checked_add(1)
                    .ok_or(AcquireError::CounterOverflow)?;
            }
        }
        Ok(())
    }

    fn release_one_session(&mut self, conn_id: i64, mode: AdvisoryLockMode) -> bool {
        let holders = match mode {
            AdvisoryLockMode::Exclusive => &mut self.exclusive_holders,
            AdvisoryLockMode::Shared => &mut self.shared_holders,
        };
        if let Some(info) = holders.get_mut(&conn_id) {
            if info.session_count > 0 {
                info.session_count -= 1;
                if info.is_empty() {
                    holders.remove(&conn_id);
                }
                return true;
            }
        }
        false
    }

    fn release_one_xact(&mut self, conn_id: i64, mode: AdvisoryLockMode) -> bool {
        let holders = match mode {
            AdvisoryLockMode::Exclusive => &mut self.exclusive_holders,
            AdvisoryLockMode::Shared => &mut self.shared_holders,
        };
        if let Some(info) = holders.get_mut(&conn_id) {
            if info.xact_count > 0 {
                info.xact_count -= 1;
                if info.is_empty() {
                    holders.remove(&conn_id);
                }
                return true;
            }
        }
        false
    }

    fn release_all_session_for_connection(&mut self, conn_id: i64) {
        for holders in [&mut self.exclusive_holders, &mut self.shared_holders] {
            if let Some(info) = holders.get_mut(&conn_id) {
                info.session_count = 0;
                if info.is_empty() {
                    holders.remove(&conn_id);
                }
            }
        }
    }

    fn release_xact_for_connection(&mut self, conn_id: i64) {
        for holders in [&mut self.exclusive_holders, &mut self.shared_holders] {
            if let Some(info) = holders.get_mut(&conn_id) {
                info.xact_count = 0;
                if info.is_empty() {
                    holders.remove(&conn_id);
                }
            }
        }
    }

    fn release_all_for_connection(&mut self, conn_id: i64) {
        self.exclusive_holders.remove(&conn_id);
        self.shared_holders.remove(&conn_id);
    }

    fn has_session_for_connection(&self, conn_id: i64) -> bool {
        self.exclusive_holders
            .get(&conn_id)
            .is_some_and(|info| info.session_count > 0)
            || self
                .shared_holders
                .get(&conn_id)
                .is_some_and(|info| info.session_count > 0)
    }

    fn has_xact_for_connection(&self, conn_id: i64) -> bool {
        self.exclusive_holders
            .get(&conn_id)
            .is_some_and(|info| info.xact_count > 0)
            || self
                .shared_holders
                .get(&conn_id)
                .is_some_and(|info| info.xact_count > 0)
    }

    fn is_empty(&self) -> bool {
        self.exclusive_holders.is_empty() && self.shared_holders.is_empty()
    }

    fn holds_connection(&self, conn_id: i64) -> bool {
        self.exclusive_holders
            .get(&conn_id)
            .is_some_and(|info| info.total() > 0)
            || self
                .shared_holders
                .get(&conn_id)
                .is_some_and(|info| info.total() > 0)
    }
}

/// Composite key: (tenant_keyspace, lock_id). Locks from different tenants
/// never conflict even if they use the same numeric lock id.
type LockKey = (Arc<str>, i64);

#[derive(Default)]
struct ManagerState {
    locks: HashMap<LockKey, LockState>,
    // Keep notifiers keyed by lock identity (not lock-state lifetime) so
    // waiters cannot miss wakeups when a lock entry is removed/recreated.
    key_notifiers: HashMap<LockKey, Arc<Notify>>,
    // Reverse index: connection -> distinct lock keys currently held.
    connection_keys: HashMap<i64, HashSet<LockKey>>,
}

#[derive(Clone, Copy)]
enum BulkReleaseKind {
    Session,
    Transaction,
    All,
}

pub(crate) struct AdvisoryLockManager {
    state: Mutex<ManagerState>,
    max_locks_per_connection: Option<usize>,
}

static GLOBAL_LOCK_MANAGER: OnceLock<AdvisoryLockManager> = OnceLock::new();

pub(crate) fn global_lock_manager() -> &'static AdvisoryLockManager {
    GLOBAL_LOCK_MANAGER.get_or_init(AdvisoryLockManager::new_from_env)
}

pub(crate) fn is_advisory_lock_function(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "PG_ADVISORY_LOCK"
            | "PG_ADVISORY_LOCK_SHARED"
            | "PG_ADVISORY_XACT_LOCK"
            | "PG_ADVISORY_XACT_LOCK_SHARED"
            | "PG_TRY_ADVISORY_LOCK"
            | "PG_TRY_ADVISORY_LOCK_SHARED"
            | "PG_TRY_ADVISORY_XACT_LOCK"
            | "PG_TRY_ADVISORY_XACT_LOCK_SHARED"
            | "PG_ADVISORY_UNLOCK"
            | "PG_ADVISORY_UNLOCK_SHARED"
            | "PG_ADVISORY_UNLOCK_ALL"
    )
}

impl AdvisoryLockManager {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_max_locks_per_connection(Some(DEFAULT_MAX_ADVISORY_LOCKS_PER_CONNECTION))
    }

    /// Test helper: force a session counter to a specific value for overflow testing.
    #[cfg(test)]
    pub fn force_session_count_for_test(
        &self,
        keyspace: &Arc<str>,
        key: i64,
        conn_id: i64,
        mode: AdvisoryLockMode,
        count: u32,
    ) {
        let mut state = self.state.lock().unwrap();
        let lk = (keyspace.clone(), key);
        let lock_state = state.locks.entry(lk.clone()).or_insert_with(LockState::new);
        let holders = match mode {
            AdvisoryLockMode::Exclusive => &mut lock_state.exclusive_holders,
            AdvisoryLockMode::Shared => &mut lock_state.shared_holders,
        };
        let info = holders.entry(conn_id).or_default();
        info.session_count = count;
        state.connection_keys.entry(conn_id).or_default().insert(lk);
    }

    pub fn with_max_locks_per_connection(max_locks_per_connection: Option<usize>) -> Self {
        Self {
            state: Mutex::new(ManagerState::default()),
            max_locks_per_connection,
        }
    }

    fn new_from_env() -> Self {
        Self::with_max_locks_per_connection(parse_max_advisory_locks_from_env())
    }

    fn should_reject_new_lock_for_limit(
        &self,
        state: &ManagerState,
        keyspace: &Arc<str>,
        key: i64,
        conn_id: i64,
    ) -> Option<usize> {
        let limit = self.max_locks_per_connection?;
        let lk = (keyspace.clone(), key);
        let already_holds_key = state
            .connection_keys
            .get(&conn_id)
            .is_some_and(|keys| keys.contains(&lk));
        if already_holds_key {
            return None;
        }
        let held = state.connection_keys.get(&conn_id).map_or(0, HashSet::len);
        if held >= limit {
            Some(limit)
        } else {
            None
        }
    }

    fn add_connection_key(state: &mut ManagerState, conn_id: i64, lk: &LockKey) {
        let inserted = state
            .connection_keys
            .entry(conn_id)
            .or_default()
            .insert(lk.clone());
        debug_assert!(
            inserted,
            "advisory lock connection key index duplicate insert: conn_id={}, key={}",
            conn_id, lk.1
        );
    }

    fn remove_connection_key(state: &mut ManagerState, conn_id: i64, lk: &LockKey) {
        let mut should_remove_conn = false;
        if let Some(keys) = state.connection_keys.get_mut(&conn_id) {
            let removed = keys.remove(lk);
            debug_assert!(
                removed,
                "missing advisory lock connection key index removal: conn_id={}, key={}",
                conn_id, lk.1
            );
            should_remove_conn = keys.is_empty();
        } else {
            debug_assert!(
                false,
                "missing advisory lock connection key set: conn_id={}",
                conn_id
            );
        }
        if should_remove_conn {
            state.connection_keys.remove(&conn_id);
        }
    }

    fn try_grant_lock(
        state: &mut ManagerState,
        lk: &LockKey,
        conn_id: i64,
        mode: AdvisoryLockMode,
        scope: AdvisoryLockScope,
    ) -> Result<bool, AcquireError> {
        let held_before = state
            .connection_keys
            .get(&conn_id)
            .is_some_and(|keys| keys.contains(lk));
        let increment_count = {
            let lock_state = state.locks.entry(lk.clone()).or_insert_with(LockState::new);
            debug_assert_eq!(
                held_before,
                lock_state.holds_connection(conn_id),
                "connection index out of sync with lock state: conn_id={}, key={}",
                conn_id,
                lk.1
            );
            if !lock_state.can_grant(conn_id, mode) {
                return Ok(false);
            }
            lock_state.grant(conn_id, mode, scope)?;
            !held_before
        };
        if increment_count {
            Self::add_connection_key(state, conn_id, lk);
        }
        Ok(true)
    }

    fn notifier_for_key(state: &mut ManagerState, lk: &LockKey) -> Arc<Notify> {
        state
            .key_notifiers
            .entry(lk.clone())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    fn maybe_remove_unused_notifier(state: &mut ManagerState, lk: &LockKey) {
        if state.locks.contains_key(lk) {
            return;
        }
        let can_remove = state
            .key_notifiers
            .get(lk)
            .is_some_and(|notify| Arc::strong_count(notify) == 1);
        if can_remove {
            state.key_notifiers.remove(lk);
        }
    }

    fn notifier_for_released_key(state: &mut ManagerState, lk: &LockKey) -> Option<Arc<Notify>> {
        let should_remove = state
            .key_notifiers
            .get(lk)
            .is_some_and(|notify| !state.locks.contains_key(lk) && Arc::strong_count(notify) == 1);
        if should_remove {
            state.key_notifiers.remove(lk);
            return None;
        }
        state.key_notifiers.get(lk).cloned()
    }

    fn release_from_key_for_connection(
        lock_state: &mut LockState,
        conn_id: i64,
        kind: BulkReleaseKind,
    ) -> bool {
        match kind {
            BulkReleaseKind::Session => {
                if lock_state.has_session_for_connection(conn_id) {
                    lock_state.release_all_session_for_connection(conn_id);
                    true
                } else {
                    false
                }
            }
            BulkReleaseKind::Transaction => {
                if lock_state.has_xact_for_connection(conn_id) {
                    lock_state.release_xact_for_connection(conn_id);
                    true
                } else {
                    false
                }
            }
            BulkReleaseKind::All => {
                if lock_state.holds_connection(conn_id) {
                    lock_state.release_all_for_connection(conn_id);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn release_keys_for_connection(&self, conn_id: i64, kind: BulkReleaseKind) {
        let mut state = self.state.lock().unwrap();
        let Some(keys_set) = state.connection_keys.get(&conn_id) else {
            return;
        };
        let keys: Vec<LockKey> = keys_set.iter().cloned().collect();
        if keys.is_empty() {
            return;
        }

        let mut released_keys = Vec::new();
        let mut unheld_keys = Vec::new();
        let mut empty_keys = Vec::new();
        let mut stale_keys = Vec::new();

        for key in keys {
            let Some(lock_state) = state.locks.get_mut(&key) else {
                stale_keys.push(key.clone());
                continue;
            };

            let released = Self::release_from_key_for_connection(lock_state, conn_id, kind);
            let still_holds = lock_state.holds_connection(conn_id);
            let is_empty = lock_state.is_empty();

            if released {
                released_keys.push(key.clone());
            }
            if !still_holds {
                unheld_keys.push(key.clone());
            }
            if is_empty {
                empty_keys.push(key);
            }
        }

        for key in &stale_keys {
            Self::remove_connection_key(&mut state, conn_id, key);
            Self::maybe_remove_unused_notifier(&mut state, key);
        }
        for key in &unheld_keys {
            Self::remove_connection_key(&mut state, conn_id, key);
        }
        for key in &empty_keys {
            state.locks.remove(key);
        }

        let notifies: Vec<Arc<Notify>> = released_keys
            .iter()
            .filter_map(|key| Self::notifier_for_released_key(&mut state, key))
            .collect();
        for key in &empty_keys {
            Self::maybe_remove_unused_notifier(&mut state, key);
        }

        if !notifies.is_empty() {
            drop(state);
            for notify in notifies {
                notify.notify_waiters();
            }
        }
    }

    /// Legacy boolean helper for internal call sites/tests.
    /// SQL-facing callers should use `try_acquire_checked` so lock-cap
    /// violations are reported as errors instead of `false`.
    #[allow(dead_code)]
    pub(crate) fn try_acquire(
        &self,
        keyspace: &Arc<str>,
        key: i64,
        conn_id: i64,
        mode: AdvisoryLockMode,
        scope: AdvisoryLockScope,
    ) -> bool {
        self.try_acquire_checked(keyspace, key, conn_id, mode, scope)
            .unwrap_or(false)
    }

    pub fn try_acquire_checked(
        &self,
        keyspace: &Arc<str>,
        key: i64,
        conn_id: i64,
        mode: AdvisoryLockMode,
        scope: AdvisoryLockScope,
    ) -> Result<bool, AcquireError> {
        let mut state = self.state.lock().unwrap();
        if let Some(limit) = self.should_reject_new_lock_for_limit(&state, keyspace, key, conn_id) {
            return Err(AcquireError::LockLimitExceeded { limit });
        }
        let lk = (keyspace.clone(), key);
        Self::try_grant_lock(&mut state, &lk, conn_id, mode, scope)
    }

    pub async fn acquire(
        &self,
        keyspace: &Arc<str>,
        key: i64,
        conn_id: i64,
        mode: AdvisoryLockMode,
        scope: AdvisoryLockScope,
        timeout: Option<Duration>,
    ) -> Result<(), AcquireError> {
        let lk = (keyspace.clone(), key);
        let wait = async {
            loop {
                let notify = {
                    let mut state = self.state.lock().unwrap();
                    if let Some(limit) =
                        self.should_reject_new_lock_for_limit(&state, keyspace, key, conn_id)
                    {
                        return Err(AcquireError::LockLimitExceeded { limit });
                    }
                    if Self::try_grant_lock(&mut state, &lk, conn_id, mode, scope)? {
                        return Ok(());
                    }
                    Self::notifier_for_key(&mut state, &lk)
                };
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let mut state = self.state.lock().unwrap();
                    if let Some(limit) =
                        self.should_reject_new_lock_for_limit(&state, keyspace, key, conn_id)
                    {
                        return Err(AcquireError::LockLimitExceeded { limit });
                    }
                    if Self::try_grant_lock(&mut state, &lk, conn_id, mode, scope)? {
                        return Ok(());
                    }
                }
                notified.await;
            }
        };
        let result = match timeout {
            Some(dur) => match tokio::time::timeout(dur, wait).await {
                Ok(inner) => inner,
                Err(_) => Err(AcquireError::Timeout),
            },
            None => wait.await,
        };
        if result.is_err() {
            let mut state = self.state.lock().unwrap();
            Self::maybe_remove_unused_notifier(&mut state, &lk);
        }
        result
    }

    pub fn release_session(
        &self,
        keyspace: &Arc<str>,
        key: i64,
        conn_id: i64,
        mode: AdvisoryLockMode,
    ) -> bool {
        let lk = (keyspace.clone(), key);
        let mut state = self.state.lock().unwrap();
        let mut became_unheld = false;
        let mut became_empty = false;
        let released = if let Some(lock_state) = state.locks.get_mut(&lk) {
            let held_before = lock_state.holds_connection(conn_id);
            let released = lock_state.release_one_session(conn_id, mode);
            let held_after = lock_state.holds_connection(conn_id);
            became_unheld = held_before && !held_after;
            became_empty = lock_state.is_empty();
            released
        } else {
            false
        };
        if became_unheld {
            Self::remove_connection_key(&mut state, conn_id, &lk);
        }
        if became_empty {
            state.locks.remove(&lk);
        }
        if released {
            let notify = Self::notifier_for_released_key(&mut state, &lk);
            drop(state);
            if let Some(notify) = notify {
                notify.notify_waiters();
            }
        }
        released
    }

    pub fn release_xact(
        &self,
        keyspace: &Arc<str>,
        key: i64,
        conn_id: i64,
        mode: AdvisoryLockMode,
    ) -> bool {
        let lk = (keyspace.clone(), key);
        let mut state = self.state.lock().unwrap();
        let mut became_unheld = false;
        let mut became_empty = false;
        let released = if let Some(lock_state) = state.locks.get_mut(&lk) {
            let held_before = lock_state.holds_connection(conn_id);
            let released = lock_state.release_one_xact(conn_id, mode);
            let held_after = lock_state.holds_connection(conn_id);
            became_unheld = held_before && !held_after;
            became_empty = lock_state.is_empty();
            released
        } else {
            false
        };
        if became_unheld {
            Self::remove_connection_key(&mut state, conn_id, &lk);
        }
        if became_empty {
            state.locks.remove(&lk);
        }
        if released {
            let notify = Self::notifier_for_released_key(&mut state, &lk);
            drop(state);
            if let Some(notify) = notify {
                notify.notify_waiters();
            }
        }
        released
    }

    /// Release all session-scoped locks for a connection (pg_advisory_unlock_all).
    /// Touches only keys currently held by this connection.
    pub fn release_all_session_locks(&self, conn_id: i64) {
        self.release_keys_for_connection(conn_id, BulkReleaseKind::Session);
    }

    /// Release all xact-scoped locks for a connection (COMMIT/ROLLBACK).
    /// Touches only keys currently held by this connection.
    pub fn release_xact_locks(&self, conn_id: i64) {
        self.release_keys_for_connection(conn_id, BulkReleaseKind::Transaction);
    }

    /// Release everything for a connection (session disconnect).
    /// Touches only keys currently held by this connection.
    pub fn release_all_for_connection(&self, conn_id: i64) {
        self.release_keys_for_connection(conn_id, BulkReleaseKind::All);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(s: &str) -> Arc<str> {
        Arc::from(s)
    }

    #[test]
    fn test_exclusive_blocks_other_exclusive() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive);
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_distinct_large_connection_ids_do_not_collide() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t_conn_id_64");
        let conn_a = i64::from(i32::MAX) + 1;
        let conn_b = conn_a + (1_i64 << 32);

        assert!(mgr.try_acquire(
            &k,
            1,
            conn_a,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(
            !mgr.try_acquire(
                &k,
                1,
                conn_b,
                AdvisoryLockMode::Exclusive,
                AdvisoryLockScope::Session
            ),
            "different i64 connection ids must never be treated as the same holder"
        );

        mgr.release_all_for_connection(conn_a);
        assert!(mgr.try_acquire(
            &k,
            1,
            conn_b,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_exclusive_blocks_other_shared() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_shared_allows_other_shared() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_shared_blocks_other_exclusive() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session
        ));
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_same_session_reentrant_exclusive() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_same_session_reentrant_shared() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_same_session_exclusive_then_shared() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_same_session_shared_then_exclusive() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_stacking_requires_equal_unlocks() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );

        assert!(mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
        assert!(mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));

        assert!(mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_session_scope_survives_xact_release() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        mgr.release_xact_locks(100);
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_xact_scope_released_by_xact_release() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction,
        );
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        mgr.release_xact_locks(100);
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_xact_release_does_not_affect_session() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction,
        );
        mgr.release_xact_locks(100);
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_session_release_does_not_affect_xact() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction,
        );
        assert!(mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(!mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
    }

    #[test]
    fn test_release_xact_releases_single_xact_stack_entry() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t_release_single_xact");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction
        ));
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction
        ));
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));

        assert!(mgr.release_xact(&k, 1, 100, AdvisoryLockMode::Exclusive));
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));

        assert!(mgr.release_xact(&k, 1, 100, AdvisoryLockMode::Exclusive));
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(!mgr.release_xact(&k, 1, 100, AdvisoryLockMode::Exclusive));
    }

    #[test]
    fn test_release_xact_does_not_release_session_entry() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t_release_xact_only");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));

        assert!(!mgr.release_xact(&k, 1, 100, AdvisoryLockMode::Exclusive));
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_release_all_for_connection() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        mgr.try_acquire(
            &k,
            2,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Transaction,
        );
        mgr.try_acquire(
            &k,
            3,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        mgr.release_all_for_connection(100);
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            2,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            3,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_release_all_session_locks() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        mgr.try_acquire(
            &k,
            2,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session,
        );
        mgr.try_acquire(
            &k,
            3,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction,
        );
        mgr.release_all_session_locks(100);
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            2,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(!mgr.try_acquire(
            &k,
            3,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_release_xact_locks() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction,
        );
        mgr.try_acquire(
            &k,
            2,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Transaction,
        );
        mgr.try_acquire(
            &k,
            3,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        mgr.release_xact_locks(100);
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            2,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(!mgr.try_acquire(
            &k,
            3,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_connection_key_index_is_updated_by_xact_release() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t_index");
        let key_session = (k.clone(), 10_i64);
        let key_xact_1 = (k.clone(), 20_i64);
        let key_xact_2 = (k.clone(), 30_i64);

        assert!(mgr.try_acquire(
            &k,
            10,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            20,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction
        ));
        assert!(mgr.try_acquire(
            &k,
            30,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Transaction
        ));

        {
            let state = mgr.state.lock().unwrap();
            let keys = state
                .connection_keys
                .get(&100)
                .expect("connection key index should exist");
            assert_eq!(keys.len(), 3);
            assert!(keys.contains(&key_session));
            assert!(keys.contains(&key_xact_1));
            assert!(keys.contains(&key_xact_2));
        }

        mgr.release_xact_locks(100);

        {
            let state = mgr.state.lock().unwrap();
            let keys = state
                .connection_keys
                .get(&100)
                .expect("session-scoped key should keep index entry");
            assert_eq!(keys.len(), 1);
            assert!(keys.contains(&key_session));
            assert!(!keys.contains(&key_xact_1));
            assert!(!keys.contains(&key_xact_2));
        }
    }

    #[test]
    fn test_two_int_key_encoding() {
        let classid: i32 = 1;
        let objid: i32 = 2;
        let key = ((classid as i64) << 32) | (objid as u32 as i64);
        assert_eq!(key, (1_i64 << 32) | 2);

        let objid_neg: i32 = -1;
        let key_neg = ((classid as i64) << 32) | (objid_neg as u32 as i64);
        assert_eq!(key_neg, (1_i64 << 32) | 0xFFFFFFFF);
    }

    #[test]
    fn test_is_advisory_lock_function() {
        assert!(is_advisory_lock_function("pg_advisory_lock"));
        assert!(is_advisory_lock_function("PG_ADVISORY_LOCK"));
        assert!(is_advisory_lock_function("pg_advisory_lock_shared"));
        assert!(is_advisory_lock_function("pg_advisory_xact_lock"));
        assert!(is_advisory_lock_function("pg_advisory_xact_lock_shared"));
        assert!(is_advisory_lock_function("pg_try_advisory_lock"));
        assert!(is_advisory_lock_function("pg_try_advisory_lock_shared"));
        assert!(is_advisory_lock_function("pg_try_advisory_xact_lock"));
        assert!(is_advisory_lock_function(
            "pg_try_advisory_xact_lock_shared"
        ));
        assert!(is_advisory_lock_function("pg_advisory_unlock"));
        assert!(is_advisory_lock_function("pg_advisory_unlock_shared"));
        assert!(is_advisory_lock_function("pg_advisory_unlock_all"));
        assert!(!is_advisory_lock_function("pg_advisory_lck"));
        assert!(!is_advisory_lock_function("advisory_lock"));
        assert!(!is_advisory_lock_function("pg_sleep"));
    }

    #[tokio::test]
    async fn test_blocking_acquire_wakes_on_release() {
        let mgr = Arc::new(AdvisoryLockManager::new());
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));

        let mgr2 = mgr.clone();
        let k2 = k.clone();
        let handle = tokio::spawn(async move {
            mgr2.acquire(
                &k2,
                1,
                200,
                AdvisoryLockMode::Exclusive,
                AdvisoryLockScope::Session,
                None,
            )
            .await
            .unwrap();
        });

        tokio::task::yield_now().await;
        mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive);

        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("blocking acquire should complete after release")
            .expect("task should not panic");

        assert!(!mgr.try_acquire(
            &k,
            1,
            300,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_multi_session_shared_then_exclusive_conflict() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session
        ));
        assert!(!mgr.try_acquire(
            &k,
            1,
            300,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        mgr.release_session(&k, 1, 100, AdvisoryLockMode::Shared);
        assert!(!mgr.try_acquire(
            &k,
            1,
            300,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        mgr.release_session(&k, 1, 200, AdvisoryLockMode::Shared);
        assert!(mgr.try_acquire(
            &k,
            1,
            300,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_xact_lock_released_on_rollback_simulation() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction,
        );
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        mgr.release_xact_locks(100);
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_unlock_xact_scoped_via_session_unlock_returns_false() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction,
        );
        assert!(!mgr.release_session(&k, 1, 100, AdvisoryLockMode::Exclusive));
        assert!(!mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_tenant_isolation() {
        let mgr = AdvisoryLockManager::new();
        let t1 = ks("tenant_a");
        let t2 = ks("tenant_b");
        // Tenant A acquires exclusive on key 42
        assert!(mgr.try_acquire(
            &t1,
            42,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        // Tenant B can also acquire exclusive on key 42 — different keyspace, no conflict
        assert!(mgr.try_acquire(
            &t2,
            42,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        // But within tenant A, another session is blocked
        assert!(!mgr.try_acquire(
            &t1,
            42,
            300,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_unlock_all_does_not_release_xact_locks() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t1");
        mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        mgr.try_acquire(
            &k,
            2,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction,
        );
        mgr.release_all_session_locks(100);
        // Session lock on key 1 was released
        assert!(mgr.try_acquire(
            &k,
            1,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        // Xact lock on key 2 is still held
        assert!(!mgr.try_acquire(
            &k,
            2,
            200,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[tokio::test]
    async fn test_acquire_timeout() {
        let mgr = Arc::new(AdvisoryLockManager::new());
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));

        let result = mgr
            .acquire(
                &k,
                1,
                200,
                AdvisoryLockMode::Exclusive,
                AdvisoryLockScope::Session,
                Some(Duration::from_millis(50)),
            )
            .await;
        assert!(result.is_err());

        // Lock was NOT granted — conn 200 does not hold it
        assert!(!mgr.release_session(&k, 1, 200, AdvisoryLockMode::Exclusive));
        // Original holder still has it
        assert!(!mgr.try_acquire(
            &k,
            1,
            300,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
    }

    #[test]
    fn test_try_acquire_respects_lock_limit() {
        let mgr = AdvisoryLockManager::with_max_locks_per_connection(Some(2));
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        assert!(mgr.try_acquire(
            &k,
            2,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        // Re-entrant acquire on an already-held key is still allowed.
        assert!(mgr.try_acquire(
            &k,
            2,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        // New distinct key is rejected once the cap is reached.
        assert!(!mgr.try_acquire(
            &k,
            3,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        let err = mgr
            .try_acquire_checked(
                &k,
                4,
                100,
                AdvisoryLockMode::Exclusive,
                AdvisoryLockScope::Session,
            )
            .expect_err("checked try-acquire should surface lock-cap violations");
        assert_eq!(err, AcquireError::LockLimitExceeded { limit: 2 });
    }

    #[tokio::test]
    async fn test_blocking_acquire_returns_lock_limit_error() {
        let mgr = AdvisoryLockManager::with_max_locks_per_connection(Some(1));
        let k = ks("t1");
        assert!(mgr.try_acquire(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));
        let err = mgr
            .acquire(
                &k,
                2,
                100,
                AdvisoryLockMode::Exclusive,
                AdvisoryLockScope::Session,
                Some(Duration::from_millis(500)),
            )
            .await
            .expect_err("second distinct key should hit lock limit");
        assert_eq!(err, AcquireError::LockLimitExceeded { limit: 1 });
    }

    #[tokio::test]
    async fn test_release_cleans_unused_key_notifier_after_waiter_timeout() {
        let mgr = AdvisoryLockManager::new();
        let keyspace = ks("tenant_notifier_cleanup");
        let key = 42_i64;
        let lk = (keyspace.clone(), key);

        assert!(mgr.try_acquire(
            &keyspace,
            key,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));

        let timeout_err = mgr
            .acquire(
                &keyspace,
                key,
                200,
                AdvisoryLockMode::Exclusive,
                AdvisoryLockScope::Session,
                Some(Duration::from_millis(10)),
            )
            .await
            .expect_err("waiter should time out while lock is still held");
        assert_eq!(timeout_err, AcquireError::Timeout);

        // The waiter has timed out and dropped its notifier clone.
        // Releasing the last holder should clean both lock-state and notifier entry.
        assert!(mgr.release_session(&keyspace, key, 100, AdvisoryLockMode::Exclusive));

        let state = mgr.state.lock().unwrap();
        assert!(!state.locks.contains_key(&lk));
        assert!(!state.key_notifiers.contains_key(&lk));
    }

    /// Helper: force a counter to a specific value for overflow testing.
    fn force_session_count(
        mgr: &AdvisoryLockManager,
        keyspace: &Arc<str>,
        key: i64,
        conn_id: i64,
        mode: AdvisoryLockMode,
        count: u32,
    ) {
        let mut state = mgr.state.lock().unwrap();
        let lk = (keyspace.clone(), key);
        let lock_state = state.locks.entry(lk.clone()).or_insert_with(LockState::new);
        let holders = match mode {
            AdvisoryLockMode::Exclusive => &mut lock_state.exclusive_holders,
            AdvisoryLockMode::Shared => &mut lock_state.shared_holders,
        };
        let info = holders.entry(conn_id).or_default();
        info.session_count = count;
        state.connection_keys.entry(conn_id).or_default().insert(lk);
    }

    fn force_xact_count(
        mgr: &AdvisoryLockManager,
        keyspace: &Arc<str>,
        key: i64,
        conn_id: i64,
        mode: AdvisoryLockMode,
        count: u32,
    ) {
        let mut state = mgr.state.lock().unwrap();
        let lk = (keyspace.clone(), key);
        let lock_state = state.locks.entry(lk.clone()).or_insert_with(LockState::new);
        let holders = match mode {
            AdvisoryLockMode::Exclusive => &mut lock_state.exclusive_holders,
            AdvisoryLockMode::Shared => &mut lock_state.shared_holders,
        };
        let info = holders.entry(conn_id).or_default();
        info.xact_count = count;
        state.connection_keys.entry(conn_id).or_default().insert(lk);
    }

    #[test]
    fn test_session_counter_overflow_returns_error() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t_overflow_session");

        // Set session_count to MAX - 1 so the next acquire brings it to MAX.
        force_session_count(&mgr, &k, 1, 100, AdvisoryLockMode::Exclusive, u32::MAX - 1);

        // One more acquire should succeed (counter goes to MAX).
        let result = mgr.try_acquire_checked(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        assert_eq!(result, Ok(true));

        // Next acquire should fail with CounterOverflow.
        let result = mgr.try_acquire_checked(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session,
        );
        assert_eq!(result, Err(AcquireError::CounterOverflow));
    }

    #[test]
    fn test_xact_counter_overflow_returns_error() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t_overflow_xact");

        // Set xact_count to MAX - 1.
        force_xact_count(&mgr, &k, 1, 100, AdvisoryLockMode::Exclusive, u32::MAX - 1);

        // One more acquire should succeed (counter goes to MAX).
        let result = mgr.try_acquire_checked(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction,
        );
        assert_eq!(result, Ok(true));

        // Next acquire should fail with CounterOverflow.
        let result = mgr.try_acquire_checked(
            &k,
            1,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction,
        );
        assert_eq!(result, Err(AcquireError::CounterOverflow));
    }

    #[test]
    fn test_session_counter_overflow_shared_mode() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t_overflow_session_shared");

        force_session_count(&mgr, &k, 1, 100, AdvisoryLockMode::Shared, u32::MAX - 1);

        let result = mgr.try_acquire_checked(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session,
        );
        assert_eq!(result, Ok(true));

        let result = mgr.try_acquire_checked(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Session,
        );
        assert_eq!(result, Err(AcquireError::CounterOverflow));
    }

    #[test]
    fn test_xact_counter_overflow_shared_mode() {
        let mgr = AdvisoryLockManager::new();
        let k = ks("t_overflow_xact_shared");

        force_xact_count(&mgr, &k, 1, 100, AdvisoryLockMode::Shared, u32::MAX - 1);

        let result = mgr.try_acquire_checked(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Transaction,
        );
        assert_eq!(result, Ok(true));

        let result = mgr.try_acquire_checked(
            &k,
            1,
            100,
            AdvisoryLockMode::Shared,
            AdvisoryLockScope::Transaction,
        );
        assert_eq!(result, Err(AcquireError::CounterOverflow));
    }

    #[test]
    fn test_total_does_not_overflow() {
        let info = HolderInfo {
            session_count: u32::MAX,
            xact_count: 1,
        };
        // saturating_add should clamp to MAX instead of wrapping.
        assert_eq!(info.total(), u32::MAX);
    }

    #[tokio::test]
    async fn test_timeout_error_path_cleans_orphan_notifier() {
        use std::time::Instant;

        let mgr = Arc::new(AdvisoryLockManager::new());
        let keyspace = ks("tenant_timeout_orphan_cleanup");
        let key = 314_i64;
        let lk = (keyspace.clone(), key);

        assert!(mgr.try_acquire(
            &keyspace,
            key,
            100,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Session
        ));

        let mgr_waiter = mgr.clone();
        let keyspace_waiter = keyspace.clone();
        let waiter = tokio::spawn(async move {
            mgr_waiter
                .acquire(
                    &keyspace_waiter,
                    key,
                    200,
                    AdvisoryLockMode::Exclusive,
                    AdvisoryLockScope::Session,
                    Some(Duration::from_millis(20)),
                )
                .await
        });

        // Wait until the waiter has created and holds a clone of the key notifier.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let has_waiter_clone = {
                let state = mgr.state.lock().unwrap();
                state
                    .key_notifiers
                    .get(&lk)
                    .is_some_and(|notify| Arc::strong_count(notify) >= 2)
            };
            if has_waiter_clone {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "waiter did not register key notifier clone in time"
            );
            tokio::task::yield_now().await;
        }

        // Force an orphan-like precursor state:
        // remove lock ownership while keeping the notifier entry alive.
        // The waiter is still pending and will time out.
        {
            let mut state = mgr.state.lock().unwrap();
            state.locks.remove(&lk);
            state.connection_keys.remove(&100);
            assert!(state.key_notifiers.contains_key(&lk));
        }

        let err = waiter
            .await
            .expect("waiter task should not panic")
            .expect_err("waiter should time out");
        assert_eq!(err, AcquireError::Timeout);

        let state = mgr.state.lock().unwrap();
        assert!(!state.locks.contains_key(&lk));
        assert!(
            !state.key_notifiers.contains_key(&lk),
            "timeout error path should cleanup orphan notifier entries"
        );
    }
}
