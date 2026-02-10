use std::future::Future;
use std::sync::{Arc, OnceLock};

use crate::sql::DEFAULT_MAX_SORT_BYTES;

tokio::task_local! {
    static TIMEZONE: Arc<str>;
}

tokio::task_local! {
    static MAX_SORT_BYTES: usize;
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
