//! Statement-scoped timestamp context.
//!
//! PostgreSQL's `NOW()` / `CURRENT_TIMESTAMP` are stable within a statement (and
//! typically within a transaction). To avoid flaky comparisons like
//! `CURRENT_TIMESTAMP(0) = DATE_TRUNC('second', CURRENT_TIMESTAMP)` we capture a
//! single "statement now" timestamp (epoch millis) at the start of execution
//! and reuse it for all evaluations within that async task.
//!
//! `TRANSACTION_TIMESTAMP()` is stable within a transaction block; for implicit
//! (autocommit) statements it equals the statement timestamp.

use std::future::Future;

tokio::task_local! {
    static STATEMENT_TIMESTAMP_MILLIS: i64;
    static TRANSACTION_TIMESTAMP_MILLIS: i64;
}

pub(super) fn statement_timestamp_millis() -> Option<i64> {
    STATEMENT_TIMESTAMP_MILLIS.try_with(|v| *v).ok()
}

pub(crate) fn transaction_timestamp_millis() -> Option<i64> {
    TRANSACTION_TIMESTAMP_MILLIS.try_with(|v| *v).ok()
}

pub(crate) fn statement_timestamp_millis_or_now() -> i64 {
    statement_timestamp_millis().unwrap_or_else(now_timestamp_millis)
}

/// Read the task-local TRANSACTION_TIMESTAMP_MILLIS, falling back to
/// statement time (which is the correct PostgreSQL semantics for implicit
/// autocommit transactions where transaction_ts == statement_ts).
#[cfg(test)]
pub(super) fn transaction_timestamp_millis_or_now() -> i64 {
    transaction_timestamp_millis().unwrap_or_else(statement_timestamp_millis_or_now)
}

pub(super) fn now_timestamp_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time must be after unix epoch")
        .as_millis() as i64
}

/// Set both statement and transaction timestamps as task-locals for the
/// duration of `fut`. Both scopes are nested inside a single async wrapper
/// to avoid adding an extra Future layer to the state machine.
pub(super) async fn with_timestamps<R, Fut>(statement_ts: i64, transaction_ts: i64, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    // In debug builds, box the inner future to keep the state machine small.
    // See `sql::query_context::with_query_context` for rationale.
    #[cfg(debug_assertions)]
    {
        STATEMENT_TIMESTAMP_MILLIS
            .scope(
                statement_ts,
                TRANSACTION_TIMESTAMP_MILLIS.scope(transaction_ts, Box::pin(fut)),
            )
            .await
    }

    #[cfg(not(debug_assertions))]
    {
        STATEMENT_TIMESTAMP_MILLIS
            .scope(
                statement_ts,
                TRANSACTION_TIMESTAMP_MILLIS.scope(transaction_ts, fut),
            )
            .await
    }
}

/// Set only the statement timestamp as a task-local (for tests that don't
/// need transaction timestamp semantics).
#[cfg(test)]
pub(super) async fn with_statement_timestamp_millis<R, Fut>(ts_millis: i64, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    // See `sql::query_context::with_query_context` for rationale.
    #[cfg(debug_assertions)]
    {
        STATEMENT_TIMESTAMP_MILLIS
            .scope(ts_millis, Box::pin(fut))
            .await
    }

    #[cfg(not(debug_assertions))]
    {
        STATEMENT_TIMESTAMP_MILLIS.scope(ts_millis, fut).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn statement_timestamp_scope_is_visible() {
        let fixed = 1_700_000_000_123_i64;
        let got =
            with_statement_timestamp_millis(fixed, async { statement_timestamp_millis().unwrap() })
                .await;
        assert_eq!(got, fixed);
    }

    #[tokio::test]
    async fn both_timestamps_visible_via_with_timestamps() {
        let stmt = 1_700_000_001_000_i64;
        let txn = 1_700_000_000_000_i64;
        let (got_stmt, got_txn) = with_timestamps(stmt, txn, async {
            (
                statement_timestamp_millis().unwrap(),
                transaction_timestamp_millis().unwrap(),
            )
        })
        .await;
        assert_eq!(got_stmt, stmt);
        assert_eq!(got_txn, txn);
    }

    #[tokio::test]
    async fn transaction_timestamp_millis_or_now_reads_task_local() {
        let stmt = 1_700_000_001_000_i64;
        let txn = 1_700_000_000_000_i64;
        let got = with_timestamps(stmt, txn, async { transaction_timestamp_millis_or_now() }).await;
        assert_eq!(
            got, txn,
            "should read TRANSACTION_TIMESTAMP_MILLIS task-local"
        );
    }

    #[tokio::test]
    async fn transaction_timestamp_millis_or_now_falls_back_to_statement() {
        let stmt = 1_700_000_001_000_i64;
        let got =
            with_statement_timestamp_millis(stmt, async { transaction_timestamp_millis_or_now() })
                .await;
        assert_eq!(
            got, stmt,
            "without TRANSACTION_TIMESTAMP_MILLIS, should fall back to statement time"
        );
    }
}
