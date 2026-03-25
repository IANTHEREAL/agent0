//! PostgreSQL advisory locks, implemented as process-local in-memory state.
//!
//! These locks are node-local: sessions connected to different db9-server
//! processes do not coordinate advisory lock state.

mod manager;
mod state;

#[cfg(test)]
mod tests;

use std::sync::OnceLock;

pub(crate) use manager::AdvisoryLockManager;
use state::{BulkReleaseKind, LockKey, LockState, ManagerState};

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
