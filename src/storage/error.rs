//! Backend-neutral storage error surface shared by the storage facade and
//! the protocol / retry layers.
//!
//! PR-1 (#2524) owns the canonical [`StorageError`] and [`WriteConflictReason`]
//! definitions so PR-1.5 (#19) and PR-3 (#21) can map their backend-specific
//! errors onto a single SQLSTATE / retry contract. Per #2523 Consensus
//! Amendments §D, TiKV and Memory must converge on the same SQLSTATE / retry
//! surface; legacy `tikv_client::Error` matching may stay alongside the new
//! contract during the migration window but must be removed before Stage 4.

// PR-1 introduces these types as the integration surface for PR-1.5 (#19),
// PR-2 (#20), and PR-3 (#21). They have no callers in PR-1 itself; the
// surface is validated by the in-crate tests below. The dead-code allow
// is removed automatically as soon as PR-1.5 / PR-2 wire callers in.
#![allow(dead_code)]

use thiserror::Error;

/// Why a [`StorageError::WriteConflict`] was raised.
///
/// `Optimistic` fires when a transaction in optimistic mode commits and finds
/// a key whose latest committed version is newer than the transaction's
/// `start_ts`. `Pessimistic` fires when a pessimistic transaction reaches
/// commit/write validation for a key whose required lock was not acquired or
/// whose committed version changed in a way that the underlying engine would
/// surface as a pessimistic write conflict. `Other` is reserved for backend
/// quirks that need to be preserved without inventing a new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteConflictReason {
    Optimistic,
    Pessimistic,
    Other(&'static str),
}

/// Backend-neutral storage error surface.
///
/// PR-1.5 (#19) owns the full `StorageError` -> `SQLSTATE` translation contract;
/// see `src/protocol/handler/errors.rs:71` and `src/sql/executor/core/retry.rs:3`
/// for the integration points. The variants below are the contract surface PR-1
/// publishes; concrete classification of `tikv_client::Error` into these
/// variants happens in PR-1.5.
#[derive(Debug, Error)]
pub(crate) enum StorageError {
    #[error("write conflict: {reason:?}")]
    WriteConflict { reason: WriteConflictReason },
    #[error("deadlock detected")]
    Deadlock,
    #[error("lock conflict")]
    LockConflict,
    #[error("lock not available")]
    LockNotAvailable,
    #[error("lock acquisition timed out")]
    LockTimeout,
    #[error("storage capability unavailable: {0}")]
    CapabilityUnavailable(&'static str),
    #[error("key too large: {0}")]
    KeyTooLarge(String),
    #[error("value too large: {0}")]
    ValueTooLarge(String),
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    #[error("internal storage error: {0}")]
    Internal(String),
}

impl StorageError {
    /// Return `true` when the error is the kind that the SQL retry loop should
    /// re-run a single-statement transaction for. PR-1.5 will replace the
    /// in-place `tikv_client::Error` matching in `src/sql/executor/core/retry.rs:3`
    /// with a check against this method on the mapped `StorageError`.
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(
            self,
            StorageError::WriteConflict { .. } | StorageError::Deadlock
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_retryable_variants() {
        assert!(StorageError::WriteConflict {
            reason: WriteConflictReason::Optimistic
        }
        .is_retryable());
        assert!(StorageError::WriteConflict {
            reason: WriteConflictReason::Pessimistic
        }
        .is_retryable());
        assert!(StorageError::Deadlock.is_retryable());
        assert!(!StorageError::LockTimeout.is_retryable());
        assert!(!StorageError::CapabilityUnavailable("db9_cop").is_retryable());
        assert!(!StorageError::Internal("explosion".to_string()).is_retryable());
    }
}
