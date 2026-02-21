//! Per-statement query context replacing scattered task-local variables.
//!
//! Replaces: `CONNECTION_ID`, `CURRENT_DATABASE_NAME`,
//! `STATEMENT_TIMESTAMP_MILLIS` (statement_time.rs), `TIMEZONE` (session_context.rs).
//! Legacy paths still read task-locals; eval functions require explicit context threading.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use crate::types::Value;

tokio::task_local! {
    static CONNECTION_ID: i32;
    static CURRENT_DATABASE_NAME: Arc<str>;
    static CURRENT_USER_NAME: Arc<str>;
    static CURRENT_TIMEZONE: Arc<str>;
    static QUERY_PARAMS: Vec<Option<Value>>;
    static QUERY_PARAM_TYPES: Vec<Option<crate::types::DataType>>;
    static SETTINGS_SNAPSHOT: Arc<HashMap<String, String>>;
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
    /// Bound parameter values from extended protocol (Execute).
    /// `None` entries represent SQL NULL. Empty vec for simple-query path.
    pub params: Vec<Option<Value>>,
    /// Finalized parameter types from Parse-time analysis.
    /// Threaded to execute-time Analyzer so re-analysis uses the same type hints.
    /// Empty vec for simple-query path or when no types were finalized.
    pub param_types: Vec<Option<crate::types::DataType>>,
    /// Snapshot of all session settings at statement start.
    /// Used by `current_setting()` in expression contexts.
    pub settings_snapshot: Option<Arc<HashMap<String, String>>>,
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
            params: vec![],
            param_types: vec![],
            settings_snapshot: None,
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

    pub(crate) fn current_timezone() -> Option<Arc<str>> {
        CURRENT_TIMEZONE.try_with(|tz| tz.clone()).ok()
    }

    pub(crate) fn current_query_params() -> Vec<Option<Value>> {
        QUERY_PARAMS.try_with(|p| p.clone()).unwrap_or_default()
    }

    pub(crate) fn current_query_param_types() -> Vec<Option<crate::types::DataType>> {
        QUERY_PARAM_TYPES
            .try_with(|t| t.clone())
            .unwrap_or_default()
    }

    /// Build a QueryContext from task-local storage.
    ///
    /// Use this for code paths that don't receive an explicit QueryContext
    /// (planning phase, triggers, tests, etc.).  During normal query execution
    /// the task-locals are always populated by the session layer, so the
    /// returned context is correct.
    pub fn from_task_locals() -> Self {
        use crate::sql::statement_time::{
            statement_timestamp_millis, transaction_timestamp_millis,
        };

        let stmt_ts = statement_timestamp_millis();
        let conn_id = Self::current_connection_id();
        let db_name = Self::current_database_name();
        let user_name = Self::current_user_name();
        let timezone = Self::current_timezone();
        let params = Self::current_query_params();

        #[cfg(test)]
        if stmt_ts.is_none()
            || conn_id.is_none()
            || db_name.is_none()
            || user_name.is_none()
            || timezone.is_none()
        {
            return Self::for_tests();
        }

        let stmt_ts = stmt_ts.expect(
            "QueryContext::from_task_locals called without statement timestamp; \
             execute through Executor::execute or wrap with statement_time::with_timestamps",
        );
        let txn_ts = transaction_timestamp_millis().unwrap_or(stmt_ts);
        let mut qctx = Self::new(
            conn_id.expect(
                "QueryContext::from_task_locals called without connection_id; \
                 execute through Executor::execute or wrap with query_context::with_query_context",
            ),
            db_name.expect(
                "QueryContext::from_task_locals called without database_name; \
                 execute through Executor::execute or wrap with query_context::with_query_context",
            ),
            user_name.expect(
                "QueryContext::from_task_locals called without current_user; \
                 execute through Executor::execute or wrap with query_context::with_query_context",
            ),
            stmt_ts,
            txn_ts,
            timezone.expect(
                "QueryContext::from_task_locals called without timezone; \
                 execute through Executor::execute or wrap with query_context::with_query_context",
            ),
        );
        qctx.params = params;
        qctx.param_types = Self::current_query_param_types();
        qctx.settings_snapshot = SETTINGS_SNAPSHOT.try_with(|s| s.clone()).ok();
        qctx
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::new(
            0,
            Arc::from("postgres"),
            Arc::from("postgres"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        )
    }
}

pub(crate) async fn with_query_context<R, Fut>(
    connection_id: i32,
    database_name: Arc<str>,
    current_user: Arc<str>,
    timezone: Arc<str>,
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
                    CURRENT_USER_NAME.scope(current_user, CURRENT_TIMEZONE.scope(timezone, fut)),
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
                    CURRENT_USER_NAME.scope(current_user, CURRENT_TIMEZONE.scope(timezone, fut)),
                ),
            )
            .await
    }
}

/// Scope all query task-locals (identity + statement/transaction timestamps)
/// from an explicit `QueryContext`.
pub(crate) async fn with_scoped_query_context<R, Fut>(qctx: &QueryContext, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    // Always scope SETTINGS_SNAPSHOT to prevent stale inheritance from outer async scopes.
    let snapshot = qctx
        .settings_snapshot
        .clone()
        .unwrap_or_else(|| Arc::new(HashMap::new()));
    with_query_context(
        qctx.connection_id,
        qctx.database_name.clone(),
        qctx.current_user.clone(),
        qctx.timezone.clone(),
        SETTINGS_SNAPSHOT.scope(
            snapshot,
            QUERY_PARAM_TYPES.scope(
                qctx.param_types.clone(),
                QUERY_PARAMS.scope(
                    qctx.params.clone(),
                    crate::sql::statement_time::with_timestamps(
                        qctx.statement_timestamp_ms,
                        qctx.transaction_timestamp_ms,
                        fut,
                    ),
                ),
            ),
        ),
    )
    .await
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
            Arc::from("UTC"),
            crate::sql::statement_time::with_timestamps(
                1_700_000_123_000,
                1_700_000_122_000,
                async { QueryContext::from_task_locals() },
            ),
        )
        .await;
        assert_eq!(ctx.connection_id, 77);
        assert_eq!(ctx.database_name.as_ref(), "tenant_db");
        assert_eq!(ctx.current_user.as_ref(), "testuser");
    }
}
