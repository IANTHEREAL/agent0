use std::future::Future;
use std::sync::{Arc, OnceLock};

use crate::sql::DEFAULT_MAX_SORT_BYTES;

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
