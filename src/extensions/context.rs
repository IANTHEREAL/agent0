use anyhow::{anyhow, Result};
use std::cell::Cell;
use std::future::Future;

#[derive(Debug)]
pub(crate) struct ExtensionContext {
    pub(crate) is_superuser: bool,
    http_requests: Cell<u32>,
}

tokio::task_local! {
    static CTX: ExtensionContext;
}

/// Run `future` with a per-statement extension execution context.
///
/// This is task-local to avoid threading session state through all executor layers.
pub(crate) async fn with_context<R>(is_superuser: bool, future: impl Future<Output = R>) -> R {
    let ctx = ExtensionContext {
        is_superuser,
        http_requests: Cell::new(0),
    };

    // See `sql::expr::with_query_context` for rationale.
    #[cfg(debug_assertions)]
    {
        CTX.scope(ctx, Box::pin(future)).await
    }

    #[cfg(not(debug_assertions))]
    {
        CTX.scope(ctx, future).await
    }
}

pub(crate) fn is_superuser() -> bool {
    CTX.try_with(|ctx| ctx.is_superuser).unwrap_or(false)
}

pub(crate) fn try_consume_http_request(max_per_statement: u32) -> Result<()> {
    CTX.try_with(|ctx| {
        let used = ctx.http_requests.get();
        if used >= max_per_statement {
            return Err(anyhow!(
                "http: max_requests_per_statement exceeded (max={})",
                max_per_statement
            ));
        }
        ctx.http_requests.set(used + 1);
        Ok(())
    })
    .map_err(|_| anyhow!("http: extension context missing"))?
}
