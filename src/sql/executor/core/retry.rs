//! Retry helpers

pub(super) fn is_retryable_tikv_error(err: &anyhow::Error) -> bool {
    fn contains_write_conflict(err: &tikv_client::Error) -> bool {
        // Retry on ANY WriteConflict, not just PessimisticRetry.
        // This emulates PostgreSQL's row-lock wait behavior: when concurrent
        // transactions UPDATE the same row, the second should wait and retry
        // rather than immediately failing.
        //
        // WriteConflict reasons (from kvrpcpb.proto):
        //   0 = Unknown
        //   1 = Optimistic (optimistic txn conflict)
        //   2 = PessimisticRetry (lock wait wakeup or newer version)
        //   3 = SelfRolledBack (txn rolled back during prewrite)
        //   4 = RcCheckTs (RC isolation check failure)
        //   5 = LazyUniquenessCheck (pessimistic unique constraint)
        //
        // We retry all of these to maximize compatibility with PostgreSQL
        // semantics where concurrent UPDATEs on the same row succeed
        // (second waits for first to commit).
        match err {
            tikv_client::Error::PessimisticLockError { inner, .. } => {
                contains_write_conflict(inner)
            }
            tikv_client::Error::UndeterminedError(inner) => contains_write_conflict(inner),
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => {
                errors.iter().any(contains_write_conflict)
            }
            tikv_client::Error::KeyError(key_error) => key_error.conflict.is_some(),
            _ => false,
        }
    }

    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(contains_write_conflict)
    })
}
