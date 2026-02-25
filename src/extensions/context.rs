use anyhow::{anyhow, Result};
use std::cell::Cell;
use std::future::Future;
use std::sync::Arc;
use tikv_client::TransactionClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionKind {
    Interactive,
    Cron,
}

#[derive(Clone)]
pub(crate) struct ExtensionContextOpts {
    pub(crate) is_superuser: bool,
    pub(crate) allow_local_fs: bool,
    pub(crate) tenant_keyspace: String,
    pub(crate) execution_kind: ExecutionKind,
    pub(crate) tikv_client: Option<Arc<TransactionClient>>,
}

impl ExtensionContextOpts {
    pub(crate) fn statement(is_superuser: bool, tenant_keyspace: &str) -> Self {
        Self {
            is_superuser,
            allow_local_fs: is_superuser,
            tenant_keyspace: tenant_keyspace.to_string(),
            execution_kind: ExecutionKind::Interactive,
            tikv_client: None,
        }
    }

    pub(crate) fn cron(tenant_keyspace: &str) -> Self {
        Self {
            is_superuser: true,
            allow_local_fs: false,
            tenant_keyspace: tenant_keyspace.to_string(),
            execution_kind: ExecutionKind::Cron,
            tikv_client: None,
        }
    }

    pub(crate) fn with_tikv_client(mut self, client: Option<Arc<TransactionClient>>) -> Self {
        self.tikv_client = client;
        self
    }
}

pub(crate) struct ExtensionContext {
    pub(crate) is_superuser: bool,
    pub(crate) allow_local_fs: bool,
    pub(crate) tenant_keyspace: String,
    execution_kind: ExecutionKind,
    http_requests: Cell<u32>,
    tikv_client: Option<Arc<TransactionClient>>,
}

tokio::task_local! {
    static CTX: ExtensionContext;
}

/// Run `future` with a per-statement extension execution context.
///
/// This is task-local to avoid threading session state through all executor layers.
pub(crate) async fn with_context<R>(
    is_superuser: bool,
    tenant_keyspace: &str,
    future: impl Future<Output = R>,
) -> R {
    with_context_opts(
        ExtensionContextOpts::statement(is_superuser, tenant_keyspace),
        future,
    )
    .await
}

pub(crate) async fn with_context_opts<R>(
    opts: ExtensionContextOpts,
    future: impl Future<Output = R>,
) -> R {
    let ctx = ExtensionContext {
        is_superuser: opts.is_superuser,
        allow_local_fs: opts.allow_local_fs,
        tenant_keyspace: opts.tenant_keyspace,
        execution_kind: opts.execution_kind,
        http_requests: Cell::new(0),
        tikv_client: opts.tikv_client,
    };

    // See `sql::query_context::with_query_context` for rationale.
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

pub(crate) fn allow_local_fs() -> bool {
    CTX.try_with(|ctx| ctx.allow_local_fs).unwrap_or(false)
}

pub(crate) fn tenant_keyspace() -> Option<String> {
    CTX.try_with(|ctx| ctx.tenant_keyspace.clone()).ok()
}

pub(crate) fn execution_kind() -> ExecutionKind {
    CTX.try_with(|ctx| ctx.execution_kind)
        .unwrap_or(ExecutionKind::Interactive)
}

pub(crate) fn tikv_client() -> Option<Arc<TransactionClient>> {
    CTX.try_with(|ctx| ctx.tikv_client.clone()).ok().flatten()
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
