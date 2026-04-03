use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
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

pub(crate) struct ExtensionStatementState {
    http_requests: AtomicU32,
    embedding_calls: AtomicU32,
    invoke_depth: AtomicU32,
    embedding_cache: Mutex<HashMap<EmbeddingCacheKey, Vec<f64>>>,
    fs_backend: Mutex<Option<Arc<dyn SharedFsBackend>>>,
}

impl Default for ExtensionStatementState {
    fn default() -> Self {
        Self {
            http_requests: AtomicU32::new(0),
            embedding_calls: AtomicU32::new(0),
            invoke_depth: AtomicU32::new(0),
            embedding_cache: Mutex::new(HashMap::new()),
            fs_backend: Mutex::new(None),
        }
    }
}

pub(crate) struct ExtensionContextOpts {
    pub(crate) is_superuser: bool,
    pub(crate) bypass_rls: bool,
    pub(crate) is_in_transaction: bool,
    pub(crate) caller_sub: Option<String>,
    pub(crate) tenant_keyspace: String,
    pub(crate) execution_kind: ExecutionKind,
    pub(crate) tikv_client: Option<Arc<TransactionClient>>,
    pub(crate) statement_state: Arc<ExtensionStatementState>,
}

impl ExtensionContextOpts {
    pub(crate) fn statement(is_superuser: bool, bypass_rls: bool, tenant_keyspace: &str) -> Self {
        Self {
            is_superuser,
            bypass_rls,
            is_in_transaction: false,
            caller_sub: None,
            tenant_keyspace: tenant_keyspace.to_string(),
            execution_kind: ExecutionKind::Interactive,
            tikv_client: None,
            statement_state: Arc::new(ExtensionStatementState::default()),
        }
    }

    pub(crate) fn cron(tenant_keyspace: &str) -> Self {
        Self {
            is_superuser: true,
            bypass_rls: true, // Cron runs as superuser, implicitly bypasses RLS
            is_in_transaction: false,
            caller_sub: None,
            tenant_keyspace: tenant_keyspace.to_string(),
            execution_kind: ExecutionKind::Cron,
            tikv_client: None,
            statement_state: Arc::new(ExtensionStatementState::default()),
        }
    }

    pub(crate) fn with_in_transaction(mut self, in_txn: bool) -> Self {
        self.is_in_transaction = in_txn;
        self
    }

    pub(crate) fn with_caller_sub(mut self, sub: Option<String>) -> Self {
        self.caller_sub = sub;
        self
    }

    pub(crate) fn with_tikv_client(mut self, client: Option<Arc<TransactionClient>>) -> Self {
        self.tikv_client = client;
        self
    }

    pub(crate) fn with_statement_state(
        mut self,
        statement_state: Arc<ExtensionStatementState>,
    ) -> Self {
        // Reuse is only correct for re-entry within the same logical statement.
        self.statement_state = statement_state;
        self
    }
}

pub(crate) struct ExtensionContext {
    pub(crate) is_superuser: bool,
    pub(crate) bypass_rls: bool,
    is_in_transaction: bool,
    caller_sub: Option<String>,
    pub(crate) tenant_keyspace: String,
    execution_kind: ExecutionKind,
    embedding_mode: Cell<EmbeddingExecutionMode>,
    /// When `true`, the current execution is inside a SECURITY DEFINER function
    /// whose owner is a superuser. This overrides `is_superuser` for permission
    /// checks (e.g. fs9) so that SECURITY DEFINER semantics are respected.
    security_definer_superuser: Cell<bool>,
    statement_state: Arc<ExtensionStatementState>,
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
        is_in_transaction: opts.is_in_transaction,
        caller_sub: opts.caller_sub,
        tenant_keyspace: opts.tenant_keyspace,
        execution_kind: opts.execution_kind,
        embedding_mode: Cell::new(EmbeddingExecutionMode::Direct),
        security_definer_superuser: Cell::new(false),
        statement_state: opts.statement_state,
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
    CTX.try_with(|ctx| ctx.is_superuser || ctx.security_definer_superuser.get())
        .unwrap_or(false)
}

/// Temporarily elevate `is_superuser()` to `true` for SECURITY DEFINER
/// functions whose owner is a superuser.  Returns a guard that restores the
/// previous value on drop.
pub(crate) fn enter_security_definer_superuser() -> SecurityDefinerGuard {
    let prev = CTX
        .try_with(|ctx| {
            let old = ctx.security_definer_superuser.get();
            ctx.security_definer_superuser.set(true);
            old
        })
        .unwrap_or(false);
    SecurityDefinerGuard(prev)
}

/// RAII guard that restores `security_definer_superuser` on drop.
pub(crate) struct SecurityDefinerGuard(bool);

impl Drop for SecurityDefinerGuard {
    fn drop(&mut self) {
        let _ = CTX.try_with(|ctx| ctx.security_definer_superuser.set(self.0));
    }
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

pub(crate) fn http_request_ordinal() -> u32 {
    CTX.try_with(|ctx| ctx.statement_state.http_requests.load(Ordering::Relaxed))
        .unwrap_or(0)
}

pub(crate) fn is_in_transaction() -> bool {
    CTX.try_with(|ctx| ctx.is_in_transaction).unwrap_or(false)
}

pub(crate) fn caller_sub() -> Option<String> {
    CTX.try_with(|ctx| ctx.caller_sub.clone()).ok().flatten()
}

pub(crate) fn current_invoke_depth() -> u32 {
    CTX.try_with(|ctx| ctx.statement_state.invoke_depth.load(Ordering::Relaxed))
        .unwrap_or(0)
}

pub(crate) fn try_enter_invoke(max_depth: u32) -> Result<()> {
    CTX.try_with(|ctx| loop {
        let depth = ctx.statement_state.invoke_depth.load(Ordering::Relaxed);
        if depth >= max_depth {
            return Err(anyhow!(
                "serverless_functions.invoke: max recursion depth ({}) exceeded",
                max_depth
            ));
        }
        if ctx
            .statement_state
            .invoke_depth
            .compare_exchange_weak(depth, depth + 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(());
        }
    })
    .map_err(|_| anyhow!("serverless_functions.invoke: extension context not available"))?
}

pub(crate) fn leave_invoke() {
    let _ = CTX.try_with(|ctx| {
        let prev = ctx
            .statement_state
            .invoke_depth
            .fetch_sub(1, Ordering::Relaxed);
        debug_assert!(prev > 0, "invoke_depth underflow");
    });
}

pub(crate) fn tikv_client() -> Option<Arc<TransactionClient>> {
    CTX.try_with(|ctx| ctx.tikv_client.clone()).ok().flatten()
}

pub(crate) fn cached_fs_backend() -> Option<Arc<dyn SharedFsBackend>> {
    CTX.try_with(|ctx| {
        ctx.statement_state
            .fs_backend
            .lock()
            .expect("fs backend mutex poisoned")
            .clone()
    })
    .ok()
    .flatten()
}

pub(crate) fn cache_fs_backend(backend: Arc<dyn SharedFsBackend>) -> Result<()> {
    CTX.try_with(|ctx| {
        *ctx.statement_state
            .fs_backend
            .lock()
            .expect("fs backend mutex poisoned") = Some(backend);
    })
    .map_err(|_| anyhow!("fs9: extension context not available"))?;
    Ok(())
}

pub(crate) fn try_consume_http_request(max_per_statement: u32) -> Result<()> {
    CTX.try_with(|ctx| loop {
        let used = ctx.statement_state.http_requests.load(Ordering::Relaxed);
        if used >= max_per_statement {
            return Err(anyhow!(
                "http: max_requests_per_statement exceeded (max={})",
                max_per_statement
            ));
        }
        if ctx
            .statement_state
            .http_requests
            .compare_exchange_weak(used, used + 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(());
        }
    })
    .map_err(|_| anyhow!("http: extension context missing"))?
}

const MAX_EMBEDDING_CALLS_PER_STATEMENT: u32 = 100;

pub(crate) fn try_consume_embedding_call() -> Result<()> {
    try_consume_embedding_call_with_limit(MAX_EMBEDDING_CALLS_PER_STATEMENT)
}

pub(crate) fn try_consume_embedding_call_with_limit(max_per_statement: u32) -> Result<()> {
    CTX.try_with(|ctx| loop {
        let used = ctx.statement_state.embedding_calls.load(Ordering::Relaxed);
        if used >= max_per_statement {
            return Err(SqlError::InvalidParameterValue {
                message: format!(
                    "embedding: max calls per statement ({}) exceeded",
                    max_per_statement
                ),
            }
            .into());
        }
        if ctx
            .statement_state
            .embedding_calls
            .compare_exchange_weak(used, used + 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(());
        }
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
    CTX.try_with(|ctx| {
        ctx.statement_state
            .embedding_cache
            .lock()
            .expect("embedding cache mutex poisoned")
            .get(key)
            .cloned()
    })
    .map_err(|_| anyhow!("embedding: extension context not available"))
}

pub(crate) fn cache_embedding(key: EmbeddingCacheKey, vector: Vec<f64>) -> Result<()> {
    CTX.try_with(|ctx| {
        ctx.statement_state
            .embedding_cache
            .lock()
            .expect("embedding cache mutex poisoned")
            .insert(key, vector);
    })
    .map_err(|_| anyhow!("embedding: extension context not available"))?;
    Ok(())
}
