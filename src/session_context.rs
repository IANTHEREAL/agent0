use std::future::Future;
use std::sync::{Arc, OnceLock};

tokio::task_local! {
    static TIMEZONE: Arc<str>;
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

pub async fn with_timezone<R, Fut>(timezone: Arc<str>, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    TIMEZONE.scope(timezone, fut).await
}
