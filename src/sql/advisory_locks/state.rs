use super::{
    AcquireError, AdvisoryLockMode, AdvisoryLockScope, DEFAULT_MAX_ADVISORY_LOCKS_PER_CONNECTION,
    ENV_MAX_ADVISORY_LOCKS_PER_CONNECTION,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Default)]
pub(super) struct HolderInfo {
    pub(super) session_count: u32,
    pub(super) xact_count: u32,
}

impl HolderInfo {
    pub(super) fn total(&self) -> u32 {
        self.session_count.saturating_add(self.xact_count)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.session_count == 0 && self.xact_count == 0
    }
}

pub(super) fn parse_max_advisory_locks_from_env() -> Option<usize> {
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
        Ok(0) => None,
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

pub(super) struct LockState {
    pub(super) exclusive_holders: HashMap<i64, HolderInfo>,
    pub(super) shared_holders: HashMap<i64, HolderInfo>,
}

impl LockState {
    pub(super) fn new() -> Self {
        Self {
            exclusive_holders: HashMap::new(),
            shared_holders: HashMap::new(),
        }
    }

    pub(super) fn can_grant(&self, conn_id: i64, mode: AdvisoryLockMode) -> bool {
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

    pub(super) fn grant(
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

    pub(super) fn release_one_session(&mut self, conn_id: i64, mode: AdvisoryLockMode) -> bool {
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

    pub(super) fn release_one_xact(&mut self, conn_id: i64, mode: AdvisoryLockMode) -> bool {
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

    pub(super) fn release_all_session_for_connection(&mut self, conn_id: i64) {
        for holders in [&mut self.exclusive_holders, &mut self.shared_holders] {
            if let Some(info) = holders.get_mut(&conn_id) {
                info.session_count = 0;
                if info.is_empty() {
                    holders.remove(&conn_id);
                }
            }
        }
    }

    pub(super) fn release_xact_for_connection(&mut self, conn_id: i64) {
        for holders in [&mut self.exclusive_holders, &mut self.shared_holders] {
            if let Some(info) = holders.get_mut(&conn_id) {
                info.xact_count = 0;
                if info.is_empty() {
                    holders.remove(&conn_id);
                }
            }
        }
    }

    pub(super) fn release_all_for_connection(&mut self, conn_id: i64) {
        self.exclusive_holders.remove(&conn_id);
        self.shared_holders.remove(&conn_id);
    }

    pub(super) fn has_session_for_connection(&self, conn_id: i64) -> bool {
        self.exclusive_holders
            .get(&conn_id)
            .is_some_and(|info| info.session_count > 0)
            || self
                .shared_holders
                .get(&conn_id)
                .is_some_and(|info| info.session_count > 0)
    }

    pub(super) fn has_xact_for_connection(&self, conn_id: i64) -> bool {
        self.exclusive_holders
            .get(&conn_id)
            .is_some_and(|info| info.xact_count > 0)
            || self
                .shared_holders
                .get(&conn_id)
                .is_some_and(|info| info.xact_count > 0)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.exclusive_holders.is_empty() && self.shared_holders.is_empty()
    }

    pub(super) fn holds_connection(&self, conn_id: i64) -> bool {
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
pub(super) type LockKey = (Arc<str>, i64);

#[derive(Default)]
pub(super) struct ManagerState {
    pub(super) locks: HashMap<LockKey, LockState>,
    // Keep notifiers keyed by lock identity (not lock-state lifetime) so
    // waiters cannot miss wakeups when a lock entry is removed/recreated.
    pub(super) key_notifiers: HashMap<LockKey, Arc<Notify>>,
    // Reverse index: connection -> distinct lock keys currently held.
    pub(super) connection_keys: HashMap<i64, HashSet<LockKey>>,
}

#[derive(Clone, Copy)]
pub(super) enum BulkReleaseKind {
    Session,
    Transaction,
    All,
}
