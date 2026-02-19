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
