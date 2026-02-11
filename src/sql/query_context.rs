//! Per-statement query context replacing scattered task-local variables.
//!
//! Replaces: `CONNECTION_ID`, `CURRENT_DATABASE_NAME` (expr/mod.rs),
//! `STATEMENT_TIMESTAMP_MILLIS` (statement_time.rs), `TIMEZONE` (session_context.rs).
//! Legacy paths still set task-locals; eval functions fall back when QueryContext is absent.

use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct QueryContext {
    /// pg_backend_pid()
    pub connection_id: i32,
    /// current_database()
    pub database_name: Arc<str>,
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
        statement_timestamp_ms: i64,
        transaction_timestamp_ms: i64,
        timezone: Arc<str>,
    ) -> Self {
        Self {
            connection_id,
            database_name,
            statement_timestamp_ms,
            transaction_timestamp_ms,
            timezone,
        }
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
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        );
        let ctx2 = ctx.clone();
        assert_eq!(ctx2.connection_id, 42);
        assert_eq!(ctx2.database_name.as_ref(), "mydb");
        assert_eq!(ctx2.statement_timestamp_ms, 1_700_000_000_000);
        assert_eq!(ctx2.transaction_timestamp_ms, 1_700_000_000_000);
        assert_eq!(ctx2.timezone.as_ref(), "UTC");
    }
}
