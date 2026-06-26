use std::future::Future;

/// Maximum retries for transient TiKV errors (RegionNotFound, EpochNotMatch,
/// gRPC Unavailable, etc.) that can occur after region split/merge,
/// leader-transfer, or store endpoint refresh operations.
pub(crate) const REGION_ERROR_MAX_RETRIES: u32 = 3;

fn is_retryable_region(err: &tikv_client::Error) -> bool {
    match err {
        tikv_client::Error::RegionError(re) => {
            re.server_is_busy.is_none()
                && re.raft_entry_too_large.is_none()
                && re.max_timestamp_not_synced.is_none()
                && re.disk_full.is_none()
        }
        tikv_client::Error::ExtractedErrors(errors)
        | tikv_client::Error::MultipleKeyErrors(errors) => {
            !errors.is_empty() && errors.iter().all(is_retryable_region)
        }
        _ => false,
    }
}

fn is_retryable_grpc_unavailable(err: &tikv_client::Error) -> bool {
    match err {
        tikv_client::Error::Grpc(_) => true,
        tikv_client::Error::GrpcAPI(status) => status.code() as i32 == 14,
        tikv_client::Error::PessimisticLockError { inner, .. } => {
            is_retryable_tikv_transient(inner)
        }
        tikv_client::Error::ExtractedErrors(errors)
        | tikv_client::Error::MultipleKeyErrors(errors) => {
            !errors.is_empty() && errors.iter().all(is_retryable_tikv_transient)
        }
        _ => false,
    }
}

pub(crate) fn is_retryable_tikv_transient(err: &tikv_client::Error) -> bool {
    is_retryable_region(err) || is_retryable_grpc_unavailable(err)
}

pub(crate) async fn retry_tikv_transient_operation<T, F, Fut>(
    operation: &'static str,
    mut op: F,
) -> Result<T, tikv_client::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, tikv_client::Error>>,
{
    for attempt in 0..=REGION_ERROR_MAX_RETRIES {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                let retryable = is_retryable_tikv_transient(&err);
                if attempt < REGION_ERROR_MAX_RETRIES && retryable {
                    tracing::info!(
                        attempt = attempt + 1,
                        max_attempts = REGION_ERROR_MAX_RETRIES + 1,
                        operation,
                        "retrying TiKV operation after transient error"
                    );
                    region_error_backoff(attempt).await;
                    continue;
                }
                return Err(err);
            }
        }
    }

    unreachable!("TiKV transient retry loop must return")
}

/// Returns `true` if the error originated from a TiKV region routing issue that
/// is expected to resolve on retry with a fresh transaction whose region cache
/// has been refreshed.
///
/// Excludes non-routing `RegionError` variants that tikv-client surfaces
/// without internal retry: `server_is_busy` (handled by AIMD backpressure),
/// `raft_entry_too_large` (deterministic), `max_timestamp_not_synced`, and
/// `disk_full`.
#[cfg(test)]
pub(crate) fn is_retryable_region_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(is_retryable_region)
    })
}

/// Returns `true` for bounded, safe-to-retry TiKV transient failures that should
/// be retried with a fresh transaction before surfacing to SQL/API callers.
pub(crate) fn is_retryable_tikv_transient_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(is_retryable_tikv_transient)
    })
}

/// Backoff sleep for transient TiKV retries: 500ms, 1s, 2s, ...
pub(crate) async fn region_error_backoff(attempt: u32) {
    let ms = 500_u64 * (1_u64 << attempt.min(4));
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn retry_tikv_transient_operation_retries_grpc_unavailable_until_success() {
        let attempts = Arc::new(AtomicU32::new(0));
        let result = retry_tikv_transient_operation("test begin", {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt < 2 {
                        Err(tikv_client::Error::GrpcAPI(tonic::Status::unavailable(
                            "connection refused",
                        )))
                    } else {
                        Ok(42)
                    }
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn undetermined_transient_commit_outcome_is_not_retryable() {
        let tikv_err = tikv_client::Error::UndeterminedError(Box::new(
            tikv_client::Error::GrpcAPI(tonic::Status::unavailable("commit response lost")),
        ));
        assert!(!is_retryable_tikv_transient(&tikv_err));

        let anyhow_err = anyhow::Error::new(tikv_client::Error::UndeterminedError(Box::new(
            tikv_client::Error::GrpcAPI(tonic::Status::unavailable("commit response lost")),
        )));
        assert!(!is_retryable_tikv_transient_error(&anyhow_err));
    }

    #[tokio::test]
    async fn retry_tikv_transient_operation_does_not_retry_undetermined_outcome() {
        let attempts = Arc::new(AtomicU32::new(0));
        let result = retry_tikv_transient_operation("test commit", {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(tikv_client::Error::UndeterminedError(Box::new(
                        tikv_client::Error::GrpcAPI(tonic::Status::unavailable(
                            "commit response lost",
                        )),
                    )))
                }
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_tikv_transient_operation_does_not_retry_non_transient_error() {
        let attempts = Arc::new(AtomicU32::new(0));
        let result = retry_tikv_transient_operation("test begin", {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(tikv_client::Error::GrpcAPI(
                        tonic::Status::invalid_argument("bad request"),
                    ))
                }
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
