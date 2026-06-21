//! Retry helpers

use crate::sql::error::SqlError;
use crate::storage::StorageError;

fn storage_error(err: &anyhow::Error) -> Option<&StorageError> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<StorageError>())
}

fn sql_internal_error(err: &anyhow::Error) -> Option<&anyhow::Error> {
    err.chain()
        .find_map(|cause| match cause.downcast_ref::<SqlError>() {
            Some(SqlError::Internal(inner)) => Some(inner),
            _ => None,
        })
}

pub(crate) fn is_retryable_tikv_error(err: &anyhow::Error) -> bool {
    if let Some(storage_err) = storage_error(err) {
        return storage_err.is_retryable();
    }

    fn contains_retryable_error(err: &tikv_client::Error) -> bool {
        // Retry on WriteConflict AND Deadlock errors.
        //
        // WriteConflict: emulates PostgreSQL's row-lock wait behavior where
        // concurrent UPDATEs on the same row succeed (second waits for first).
        //
        // Deadlock: TiKV's pessimistic locking can produce circular waits when
        // concurrent DML touches overlapping rows via different index scans.
        // PostgreSQL detects and resolves these automatically; we do the same
        // by retrying the statement with exponential backoff.
        //
        // WriteConflict reasons (from kvrpcpb.proto):
        //   0 = Unknown
        //   1 = Optimistic (optimistic txn conflict)
        //   2 = PessimisticRetry (lock wait wakeup or newer version)
        //   3 = SelfRolledBack (txn rolled back during prewrite)
        //   4 = RcCheckTs (RC isolation check failure)
        //   5 = LazyUniquenessCheck (pessimistic unique constraint)
        match err {
            tikv_client::Error::PessimisticLockError { inner, .. } => {
                contains_retryable_error(inner)
            }
            tikv_client::Error::UndeterminedError(inner) => contains_retryable_error(inner),
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => {
                errors.iter().any(contains_retryable_error)
            }
            tikv_client::Error::KeyError(key_error) => {
                key_error.conflict.is_some() || key_error.deadlock.is_some()
            }
            _ => false,
        }
    }

    if err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(contains_retryable_error)
    }) {
        return true;
    }

    sql_internal_error(err).is_some_and(is_retryable_tikv_error)
}

/// Extract the write-conflict reason code from a retryable error.
///
/// Returns the existing TiKV `kvrpcpb::write_conflict::Reason` integer (0..=5)
/// for legacy errors, or the staged `StorageError` equivalent when the facade
/// path is in use. Returns `None` if the error is not a write conflict.
pub(super) fn extract_write_conflict_reason(err: &anyhow::Error) -> Option<i32> {
    if let Some(storage_err) = storage_error(err) {
        return storage_err.write_conflict_reason_code();
    }

    fn first_reason(err: &tikv_client::Error) -> Option<i32> {
        match err {
            tikv_client::Error::PessimisticLockError { inner, .. } => first_reason(inner),
            tikv_client::Error::UndeterminedError(inner) => first_reason(inner),
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => errors.iter().find_map(first_reason),
            tikv_client::Error::KeyError(ke) => ke.conflict.as_ref().map(|c| c.reason),
            _ => None,
        }
    }

    if let Some(reason) = err.chain().find_map(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .and_then(first_reason)
    }) {
        return Some(reason);
    }

    sql_internal_error(err).and_then(extract_write_conflict_reason)
}

/// Exponential backoff with jitter for autocommit retry loops.
pub(super) async fn autocommit_backoff(attempt: usize) {
    let base_ms = 5u64.saturating_mul(1u64 << attempt.min(6));
    let jitter_ms = rand::random::<u64>() % (base_ms + 1);
    let backoff_ms = base_ms + jitter_ms;
    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
}
