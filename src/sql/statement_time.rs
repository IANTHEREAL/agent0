//! Statement-scoped timestamp context.
//!
//! PostgreSQL's `NOW()` / `CURRENT_TIMESTAMP` are stable within a statement (and
//! typically within a transaction). To avoid flaky comparisons like
//! `CURRENT_TIMESTAMP(0) = DATE_TRUNC('second', CURRENT_TIMESTAMP)` we capture a
//! single "statement now" timestamp (epoch millis) at the start of execution
//! and reuse it for all evaluations within that async task.

use std::future::Future;

tokio::task_local! {
    static STATEMENT_TIMESTAMP_MILLIS: i64;
}

pub(super) fn statement_timestamp_millis() -> Option<i64> {
    STATEMENT_TIMESTAMP_MILLIS.try_with(|v| *v).ok()
}

pub(crate) fn statement_timestamp_millis_or_now() -> i64 {
    statement_timestamp_millis().unwrap_or_else(now_timestamp_millis)
}

pub(super) fn now_timestamp_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time must be after unix epoch")
        .as_millis() as i64
}

pub(super) async fn with_statement_timestamp_millis<R, Fut>(ts_millis: i64, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    // See `sql::expr::with_query_context` for rationale.
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
}
