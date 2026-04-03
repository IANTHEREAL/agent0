//! Retry helpers

pub(super) fn is_retryable_tikv_error(err: &anyhow::Error) -> bool {
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

    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(contains_retryable_error)
    })
}

/// Extract the TiKV WriteConflict reason code from a retryable error.
///
/// Returns the `kvrpcpb::write_conflict::Reason` integer (0..=5) if found,
/// or `None` if the error is not a write conflict.
pub(super) fn extract_write_conflict_reason(err: &anyhow::Error) -> Option<i32> {
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

    err.chain().find_map(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .and_then(first_reason)
    })
}

/// Exponential backoff with jitter for autocommit retry loops.
pub(super) async fn autocommit_backoff(attempt: usize) {
    let base_ms = 5u64.saturating_mul(1u64 << attempt.min(6));
    let jitter_ms = rand::random::<u64>() % (base_ms + 1);
    let backoff_ms = base_ms + jitter_ms;
    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
}
