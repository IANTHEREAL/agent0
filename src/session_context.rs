use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, OnceLock};

use crate::sql::DEFAULT_MAX_SORT_BYTES;

/// Extension transaction delta snapshot: (created_set, dropped_set).
pub type ExtensionTxnDelta = (HashSet<String>, HashSet<String>);

tokio::task_local! {
    static TIMEZONE: Arc<str>;
}

tokio::task_local! {
    static MAX_SORT_BYTES: usize;
}

tokio::task_local! {
    static CURRENT_SEARCH_PATH: Arc<Vec<String>>;
}

tokio::task_local! {
    static TEXT_SEARCH_CONFIG: Arc<str>;
}

tokio::task_local! {
    static CURRENT_KEYSPACE: Arc<str>;
}

tokio::task_local! {
    static CURRENT_DATABASE_ID: u64;
}

tokio::task_local! {
    static DML_LIMIT_CAP: usize;
}

tokio::task_local! {
    static CURRENT_TXN_SNAPSHOT_TS_VERSION: Option<u64>;
}

// Extension transaction delta: (created, dropped).
//
// Semantics:
// - DDL executed in the current transaction is tracked in-memory.
// - When no in-transaction override exists, extension gate checks may use the
//   transaction snapshot timestamp (explicit transaction) or latest committed
//   state (autocommit).
tokio::task_local! {
    static EXTENSION_TXN_DELTA: Arc<ExtensionTxnDelta>;
}

fn utc_arc() -> Arc<str> {
    static UTC: OnceLock<Arc<str>> = OnceLock::new();
    UTC.get_or_init(|| Arc::from("UTC")).clone()
}

pub fn current_timezone() -> Arc<str> {
    TIMEZONE
        .try_with(|tz| tz.clone())
        .unwrap_or_else(|_| utc_arc())
}

pub fn current_max_sort_bytes() -> usize {
    MAX_SORT_BYTES
        .try_with(|bytes| *bytes)
        .unwrap_or(DEFAULT_MAX_SORT_BYTES)
}

pub async fn with_timezone<R, Fut>(timezone: Arc<str>, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    TIMEZONE.scope(timezone, fut).await
}

pub async fn with_max_sort_bytes<R, Fut>(max_sort_bytes: usize, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    MAX_SORT_BYTES.scope(max_sort_bytes, fut).await
}

pub fn current_search_path_first_schema() -> String {
    CURRENT_SEARCH_PATH
        .try_with(|sp| {
            // Skip "$user" (PostgreSQL's search_path default) since we don't
            // create per-user schemas — return the first real schema.
            sp.iter()
                .find(|s| *s != "$user")
                .cloned()
                .unwrap_or_else(|| "public".to_string())
        })
        .unwrap_or_else(|_| "public".to_string())
}

/// Return all schemas in the current search_path as a Vec.
///
/// When `include_implicit` is true, `pg_catalog` is prepended (matching
/// PostgreSQL's `current_schemas(true)` behavior). `"$user"` entries are
/// skipped because db9 does not create per-user schemas.
pub fn current_search_path_schemas(include_implicit: bool) -> Vec<String> {
    let mut schemas = CURRENT_SEARCH_PATH
        .try_with(|sp| {
            sp.iter()
                .filter(|s| *s != "$user")
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|_| vec!["public".to_string()]);

    if include_implicit && !schemas.iter().any(|s| s == "pg_catalog") {
        schemas.insert(0, "pg_catalog".to_string());
    }
    schemas
}

pub async fn with_search_path<R, Fut>(search_path: Arc<Vec<String>>, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    CURRENT_SEARCH_PATH.scope(search_path, fut).await
}

/// Return the current session's `default_text_search_config`.
///
/// Falls back to the process-level default from `fts_tokenizers::default_text_search_config()`
/// when called outside a statement execution scope (e.g. during DDL index build).
pub fn current_text_search_config() -> Arc<str> {
    TEXT_SEARCH_CONFIG
        .try_with(|cfg| cfg.clone())
        .unwrap_or_else(|_| Arc::from(crate::sql::fts_tokenizers::default_text_search_config()))
}

pub async fn with_text_search_config<R, Fut>(config: Arc<str>, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    TEXT_SEARCH_CONFIG.scope(config, fut).await
}

/// Return the current statement's tenant keyspace.
///
/// Returns `"default"` when called outside a statement execution scope.
pub fn current_keyspace() -> Arc<str> {
    CURRENT_KEYSPACE
        .try_with(|ks| ks.clone())
        .unwrap_or_else(|_| Arc::from("default"))
}

pub async fn with_keyspace<R, Fut>(keyspace: Arc<str>, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    CURRENT_KEYSPACE.scope(keyspace, fut).await
}

/// Return the current statement's database ID.
///
/// Returns `0` when called outside a statement execution scope.
pub fn current_database_id() -> u64 {
    CURRENT_DATABASE_ID.try_with(|id| *id).unwrap_or(0)
}

pub async fn with_database_id<R, Fut>(db_id: u64, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    CURRENT_DATABASE_ID.scope(db_id, fut).await
}

/// Return the current DML LIMIT cap (0 = unlimited, not set = 0).
///
/// Only set within DML subquery execution scopes so that regular SELECT
/// queries are never capped.
pub fn current_dml_limit_cap() -> usize {
    DML_LIMIT_CAP.try_with(|cap| *cap).unwrap_or(0)
}

pub async fn with_dml_limit_cap<R, Fut>(cap: usize, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    DML_LIMIT_CAP.scope(cap, fut).await
}

/// Return the current statement's transaction snapshot timestamp version.
///
/// Returns `None` outside a transaction-scoped statement execution context.
pub fn current_txn_snapshot_ts_version() -> Option<u64> {
    CURRENT_TXN_SNAPSHOT_TS_VERSION
        .try_with(|ts| *ts)
        .unwrap_or(None)
}

pub async fn with_txn_snapshot_ts_version<R, Fut>(ts_version: Option<u64>, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    CURRENT_TXN_SNAPSHOT_TS_VERSION.scope(ts_version, fut).await
}

pub async fn with_extension_txn_delta<R, Fut>(delta: Arc<ExtensionTxnDelta>, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    EXTENSION_TXN_DELTA.scope(delta, fut).await
}

/// Returns in-transaction extension status from DDL delta.
/// - `Some(true)`: extension was created in current txn.
/// - `Some(false)`: extension was dropped in current transaction scope.
/// - `None`: no in-transaction override recorded.
pub fn extension_txn_status(name: &str) -> Option<bool> {
    let normalized = name.to_ascii_lowercase();
    EXTENSION_TXN_DELTA
        .try_with(|delta| {
            if delta.0.contains(&normalized) {
                Some(true)
            } else if delta.1.contains(&normalized) {
                Some(false)
            } else {
                None
            }
        })
        .unwrap_or(None)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    #[tokio::test]
    async fn txn_snapshot_ts_is_scoped() {
        assert_eq!(super::current_txn_snapshot_ts_version(), None);
        let got = super::with_txn_snapshot_ts_version(Some(123), async {
            super::current_txn_snapshot_ts_version()
        })
        .await;
        assert_eq!(got, Some(123));
        assert_eq!(super::current_txn_snapshot_ts_version(), None);
    }

    #[tokio::test]
    async fn extension_txn_status_is_case_insensitive() {
        let delta = Arc::new((HashSet::from(["embedding".to_string()]), HashSet::new()));
        let got = super::with_extension_txn_delta(delta, async {
            super::extension_txn_status("EMBEDDING")
        })
        .await;
        assert_eq!(got, Some(true));
    }

    #[tokio::test]
    async fn extension_txn_status_prefers_created_when_both_sets_contain_name() {
        let delta = Arc::new((
            HashSet::from(["embedding".to_string()]),
            HashSet::from(["embedding".to_string()]),
        ));
        let got = super::with_extension_txn_delta(delta, async {
            super::extension_txn_status("embedding")
        })
        .await;
        assert_eq!(got, Some(true));
    }

    #[tokio::test]
    async fn extension_txn_status_returns_none_when_absent() {
        let delta = Arc::new((HashSet::new(), HashSet::new()));
        let got = super::with_extension_txn_delta(delta, async {
            super::extension_txn_status("embedding")
        })
        .await;
        assert_eq!(got, None);
    }

    #[test]
    fn extension_txn_status_outside_scope_returns_none() {
        assert_eq!(super::extension_txn_status("embedding"), None);
    }

    #[tokio::test]
    async fn extension_txn_status_returns_false_when_only_dropped() {
        let delta = Arc::new((HashSet::new(), HashSet::from(["embedding".to_string()])));
        let got = super::with_extension_txn_delta(delta, async {
            super::extension_txn_status("embedding")
        })
        .await;
        assert_eq!(got, Some(false));
    }
}
