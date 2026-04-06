// TODO(#2335): migrate to parking_lot — phase 2/3
#![allow(clippy::disallowed_types)]

use super::{
    AcquireError, AdvisoryLockMode, AdvisoryLockScope, BulkReleaseKind, LockKey, LockState,
    ManagerState,
};
use crate::sql::advisory_locks::state::parse_max_advisory_locks_from_env;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

pub(crate) struct AdvisoryLockManager {
    pub(super) state: Mutex<ManagerState>,
    max_locks_per_connection: Option<usize>,
}

impl AdvisoryLockManager {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_max_locks_per_connection(Some(super::DEFAULT_MAX_ADVISORY_LOCKS_PER_CONNECTION))
    }

    #[cfg(test)]
    pub fn force_session_count_for_test(
        &self,
        keyspace: &Arc<str>,
        key: i64,
        conn_id: i64,
        mode: AdvisoryLockMode,
        count: u32,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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

    pub(super) fn new_from_env() -> Self {
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
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
                    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
                    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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

    pub fn release_all_session_locks(&self, conn_id: i64) {
        self.release_keys_for_connection(conn_id, BulkReleaseKind::Session);
    }

    pub fn release_xact_locks(&self, conn_id: i64) {
        self.release_keys_for_connection(conn_id, BulkReleaseKind::Transaction);
    }

    pub fn release_all_for_connection(&self, conn_id: i64) {
        self.release_keys_for_connection(conn_id, BulkReleaseKind::All);
    }
}
