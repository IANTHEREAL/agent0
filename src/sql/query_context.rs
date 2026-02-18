//! Per-statement query context replacing scattered task-local variables.
//!
//! Replaces: `CONNECTION_ID`, `CURRENT_DATABASE_NAME`,
//! `STATEMENT_TIMESTAMP_MILLIS` (statement_time.rs), `TIMEZONE` (session_context.rs).
//! Legacy paths still read task-locals; eval functions fall back when QueryContext is absent.

use std::future::Future;
use std::sync::Arc;

tokio::task_local! {
    static CONNECTION_ID: i32;
    static CURRENT_DATABASE_NAME: Arc<str>;
    static CURRENT_USER_NAME: Arc<str>;
    static USE_OPTIMIZER: bool;
}

#[derive(Debug, Clone)]
pub struct QueryContext {
    /// pg_backend_pid()
    pub connection_id: i32,
    /// current_database()
    pub database_name: Arc<str>,
    /// current_user / session_user — the authenticated role for this session
    pub current_user: Arc<str>,
    /// NOW() / CURRENT_TIMESTAMP / STATEMENT_TIMESTAMP() — stable within a statement
    pub statement_timestamp_ms: i64,
    /// TRANSACTION_TIMESTAMP() — stable within a transaction block;
    /// equals statement_timestamp_ms for implicit (autocommit) transactions
    pub transaction_timestamp_ms: i64,
    #[allow(dead_code)] // set during init; read path uses session_context fallback
    pub timezone: Arc<str>,
}

impl QueryContext {
    pub fn new(
        connection_id: i32,
        database_name: Arc<str>,
        current_user: Arc<str>,
        statement_timestamp_ms: i64,
        transaction_timestamp_ms: i64,
        timezone: Arc<str>,
    ) -> Self {
        Self {
            connection_id,
            database_name,
            current_user,
            statement_timestamp_ms,
            transaction_timestamp_ms,
            timezone,
        }
    }

    pub(crate) fn current_connection_id() -> Option<i32> {
        CONNECTION_ID.try_with(|id| *id).ok()
    }

    pub(crate) fn current_database_name() -> Option<Arc<str>> {
        CURRENT_DATABASE_NAME.try_with(|name| name.clone()).ok()
    }

    pub(crate) fn current_user_name() -> Option<Arc<str>> {
        CURRENT_USER_NAME.try_with(|name| name.clone()).ok()
    }

    /// Whether the CBO optimizer pipeline is enabled for this statement.
    /// Note: always ON in single-path architecture; retained for GUC infrastructure.
    #[allow(dead_code)]
    pub(crate) fn use_optimizer() -> bool {
        USE_OPTIMIZER.try_with(|v| *v).unwrap_or(false)
    }

    /// Build a QueryContext from task-local storage.
    ///
    /// Use this for code paths that don't receive an explicit QueryContext
    /// (planning phase, triggers, tests, etc.).  During normal query execution
    /// the task-locals are always populated by the session layer, so the
    /// returned context is correct.
    pub fn from_task_locals() -> Self {
        use crate::sql::statement_time::{
            statement_timestamp_millis_or_now, transaction_timestamp_millis,
        };

        let stmt_ts = statement_timestamp_millis_or_now();
        let txn_ts = transaction_timestamp_millis().unwrap_or(stmt_ts);
        Self::new(
            Self::current_connection_id().unwrap_or(0),
            Self::current_database_name().unwrap_or_else(|| Arc::from("postgres")),
            Self::current_user_name().unwrap_or_else(|| Arc::from("postgres")),
            stmt_ts,
            txn_ts,
            crate::session_context::current_timezone(),
        )
    }
}

pub(crate) async fn with_query_context<R, Fut>(
    connection_id: i32,
    database_name: Arc<str>,
    current_user: Arc<str>,
    use_optimizer: bool,
    fut: Fut,
) -> R
where
    Fut: Future<Output = R>,
{
    #[cfg(debug_assertions)]
    {
        let fut = Box::pin(fut);
        CONNECTION_ID
            .scope(
                connection_id,
                CURRENT_DATABASE_NAME.scope(
                    database_name,
                    CURRENT_USER_NAME.scope(current_user, USE_OPTIMIZER.scope(use_optimizer, fut)),
                ),
            )
            .await
    }

    #[cfg(not(debug_assertions))]
    {
        CONNECTION_ID
            .scope(
                connection_id,
                CURRENT_DATABASE_NAME.scope(
                    database_name,
                    CURRENT_USER_NAME.scope(current_user, USE_OPTIMIZER.scope(use_optimizer, fut)),
                ),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_context_clone() {
        let ctx = QueryContext::new(
            42,
            Arc::from("mydb"),
            Arc::from("admin"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        );
        let ctx2 = ctx.clone();
        assert_eq!(ctx2.connection_id, 42);
        assert_eq!(ctx2.database_name.as_ref(), "mydb");
        assert_eq!(ctx2.current_user.as_ref(), "admin");
        assert_eq!(ctx2.statement_timestamp_ms, 1_700_000_000_000);
        assert_eq!(ctx2.transaction_timestamp_ms, 1_700_000_000_000);
        assert_eq!(ctx2.timezone.as_ref(), "UTC");
    }

    #[tokio::test]
    async fn from_task_locals_reads_query_identity() {
        let ctx = with_query_context(
            77,
            Arc::from("tenant_db"),
            Arc::from("testuser"),
            false,
            async { QueryContext::from_task_locals() },
        )
        .await;
        assert_eq!(ctx.connection_id, 77);
        assert_eq!(ctx.database_name.as_ref(), "tenant_db");
        assert_eq!(ctx.current_user.as_ref(), "testuser");
    }
}
