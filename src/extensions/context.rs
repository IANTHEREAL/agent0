use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tikv_client::TransactionClient;

use crate::config::EmbeddingProvider;
use crate::extensions::fs::backend::FsBackend as SharedFsBackend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionKind {
    Interactive,
    Cron,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingExecutionMode {
    Direct,
    AuthorizedGenerated,
}

#[derive(Clone)]
pub(crate) struct ExtensionContextOpts {
    pub(crate) is_superuser: bool,
    pub(crate) bypass_rls: bool,
    pub(crate) tenant_keyspace: String,
    pub(crate) execution_kind: ExecutionKind,
    pub(crate) tikv_client: Option<Arc<TransactionClient>>,
}

impl ExtensionContextOpts {
    pub(crate) fn statement(is_superuser: bool, bypass_rls: bool, tenant_keyspace: &str) -> Self {
        Self {
            is_superuser,
            bypass_rls,
            tenant_keyspace: tenant_keyspace.to_string(),
            execution_kind: ExecutionKind::Interactive,
            tikv_client: None,
        }
    }

    pub(crate) fn cron(tenant_keyspace: &str) -> Self {
        Self {
            is_superuser: true,
            bypass_rls: true, // Cron runs as superuser, implicitly bypasses RLS
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
    pub(crate) bypass_rls: bool,
    pub(crate) tenant_keyspace: String,
    execution_kind: ExecutionKind,
    http_requests: Cell<u32>,
    embedding_calls: Cell<u32>,
    embedding_mode: Cell<EmbeddingExecutionMode>,
    embedding_cache: RefCell<HashMap<EmbeddingCacheKey, Vec<f64>>>,
    fs_backend: RefCell<Option<Arc<dyn SharedFsBackend>>>,
    tikv_client: Option<Arc<TransactionClient>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct EmbeddingCacheKey {
    pub(crate) provider: EmbeddingProvider,
    pub(crate) endpoint: String,
    pub(crate) api_key_fingerprint: String,
    pub(crate) model: String,
    pub(crate) dimensions: u32,
    pub(crate) text: String,
}

tokio::task_local! {
    static CTX: ExtensionContext;
}

/// Run `future` with a per-statement extension execution context.
///
/// This is task-local to avoid threading session state through all executor layers.
#[cfg(test)]
pub(crate) async fn with_context<R>(
    is_superuser: bool,
    tenant_keyspace: &str,
    future: impl Future<Output = R>,
) -> R {
    with_context_opts(
        ExtensionContextOpts::statement(is_superuser, false, tenant_keyspace),
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
        bypass_rls: opts.bypass_rls,
        tenant_keyspace: opts.tenant_keyspace,
        execution_kind: opts.execution_kind,
        http_requests: Cell::new(0),
        embedding_calls: Cell::new(0),
        embedding_mode: Cell::new(EmbeddingExecutionMode::Direct),
        embedding_cache: RefCell::new(HashMap::new()),
        fs_backend: RefCell::new(None),
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

pub(crate) fn bypass_rls() -> bool {
    CTX.try_with(|ctx| ctx.bypass_rls).unwrap_or(false)
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

pub(crate) fn cached_fs_backend() -> Option<Arc<dyn SharedFsBackend>> {
    CTX.try_with(|ctx| ctx.fs_backend.borrow().clone())
        .ok()
        .flatten()
}

pub(crate) fn cache_fs_backend(backend: Arc<dyn SharedFsBackend>) -> Result<()> {
    CTX.try_with(|ctx| {
        *ctx.fs_backend.borrow_mut() = Some(backend);
    })
    .map_err(|_| anyhow!("fs9: extension context not available"))?;
    Ok(())
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

const MAX_EMBEDDING_CALLS_PER_STATEMENT: u32 = 100;

pub(crate) fn try_consume_embedding_call() -> Result<()> {
    try_consume_embedding_call_with_limit(MAX_EMBEDDING_CALLS_PER_STATEMENT)
}

pub(crate) fn try_consume_embedding_call_with_limit(max_per_statement: u32) -> Result<()> {
    CTX.try_with(|ctx| {
        let used = ctx.embedding_calls.get();
        if used >= max_per_statement {
            return Err(SqlError::InvalidParameterValue {
                message: format!(
                    "embedding: max calls per statement ({}) exceeded",
                    max_per_statement
                ),
            }
            .into());
        }
        ctx.embedding_calls.set(used + 1);
        Ok(())
    })
    .map_err(|_| anyhow!("embedding: extension context not available"))?
}

pub(crate) fn embedding_execution_mode() -> EmbeddingExecutionMode {
    CTX.try_with(|ctx| ctx.embedding_mode.get())
        .unwrap_or(EmbeddingExecutionMode::Direct)
}

pub(crate) fn is_embedding_authorized() -> bool {
    embedding_execution_mode() == EmbeddingExecutionMode::AuthorizedGenerated
}

pub(crate) async fn with_embedding_authorized<R>(future: impl Future<Output = R>) -> Result<R> {
    let previous = CTX
        .try_with(|ctx| {
            let previous = ctx.embedding_mode.get();
            ctx.embedding_mode
                .set(EmbeddingExecutionMode::AuthorizedGenerated);
            previous
        })
        .map_err(|_| anyhow!("embedding: extension context not available"))?;

    let result = future.await;

    CTX.try_with(|ctx| ctx.embedding_mode.set(previous))
        .map_err(|_| anyhow!("embedding: extension context not available"))?;
    Ok(result)
}

pub(crate) fn cached_embedding(key: &EmbeddingCacheKey) -> Result<Option<Vec<f64>>> {
    CTX.try_with(|ctx| ctx.embedding_cache.borrow().get(key).cloned())
        .map_err(|_| anyhow!("embedding: extension context not available"))
}

pub(crate) fn cache_embedding(key: EmbeddingCacheKey, vector: Vec<f64>) -> Result<()> {
    CTX.try_with(|ctx| {
        ctx.embedding_cache.borrow_mut().insert(key, vector);
    })
    .map_err(|_| anyhow!("embedding: extension context not available"))?;
    Ok(())
}
