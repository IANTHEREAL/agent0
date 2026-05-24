use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
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
    /// PostgreSQL role of the authenticated user. Used to derive the
    /// fs-plane `scp` claim (`fs:volume:jfs_t_<tid>:r|rw`).
    pub(crate) authenticated_role: Option<String>,
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
            authenticated_role: None,
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
            // Cron sessions don't carry an authenticated role, so they
            // can't reach JuiceFS tenants. fs9 access from cron fails at
            // backend init with a clear error pending a
            // service-principal cron path.
            authenticated_role: None,
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

    pub(crate) fn with_authenticated_role(mut self, role: Option<String>) -> Self {
        self.authenticated_role = role;
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
    authenticated_role: Option<String>,
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
        authenticated_role: opts.authenticated_role,
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

/// The principal (role + fs-plane access) fs9 should see for token
/// minting on this call. Honours SECURITY DEFINER: when an SD-superuser
/// function is executing, the call carries the owner's superuser
/// authority, not the caller's login privilege. Symmetric with
/// [`is_superuser`], which already factors in SD elevation.
///
/// `role` is identity, used for the `usr` claim and the cache key —
/// it stays as the *actual* authenticated role even under SD elevation
/// (the previous implementation fabricated the literal string
/// `"admin"`, which broke audit trails and conflicted with custom
/// `DB9_BOOTSTRAP_ADMIN_USER` deployments, PR #2547 review #1).
///
/// `access` is capability, derived from privilege facts via
/// [`crate::auth::fs_plane_token::fs_plane_access_for`] — never from
/// the role name. A superuser regardless of their name gets
/// `ReadWrite`; `_db9_sys_readonly` gets `ReadOnly`; everything else
/// fails closed (`None`), and the JuiceFS backend init surfaces that
/// as a hard error rather than silently downgrading.
pub(crate) fn effective_fs_plane_principal() -> Option<crate::auth::fs_plane_token::Fs9Principal> {
    use crate::auth::fs_plane_token::{fs_plane_access_for, Fs9Principal};

    CTX.try_with(|ctx| {
        let role = ctx.authenticated_role.clone()?;
        let is_superuser = ctx.is_superuser || ctx.security_definer_superuser.get();
        let access = fs_plane_access_for(is_superuser, &role)?;
        Some(Fs9Principal { role, access })
    })
    .ok()
    .flatten()
}

pub(crate) fn cached_fs_backend() -> Option<Arc<dyn SharedFsBackend>> {
    CTX.try_with(|ctx| ctx.statement_state.fs_backend.lock().clone())
        .ok()
        .flatten()
}

pub(crate) fn cache_fs_backend(backend: Arc<dyn SharedFsBackend>) -> Result<()> {
    CTX.try_with(|ctx| {
        *ctx.statement_state.fs_backend.lock() = Some(backend);
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
    CTX.try_with(|ctx| ctx.statement_state.embedding_cache.lock().get(key).cloned())
        .map_err(|_| anyhow!("embedding: extension context not available"))
}

pub(crate) fn cache_embedding(key: EmbeddingCacheKey, vector: Vec<f64>) -> Result<()> {
    CTX.try_with(|ctx| {
        ctx.statement_state
            .embedding_cache
            .lock()
            .insert(key, vector);
    })
    .map_err(|_| anyhow!("embedding: extension context not available"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::auth::fs_plane_token::Fs9Access;

    #[tokio::test]
    async fn principal_falls_through_authenticated_role_as_readonly() {
        let opts = ExtensionContextOpts::statement(false, false, "db9_tenant_x")
            .with_authenticated_role(Some("_db9_sys_readonly".to_string()));
        with_context_opts(opts, async {
            let p = effective_fs_plane_principal().unwrap();
            assert_eq!(p.role, "_db9_sys_readonly");
            assert_eq!(p.access, Fs9Access::ReadOnly);
        })
        .await;
    }

    #[tokio::test]
    async fn principal_returns_none_without_authenticated_role() {
        let opts = ExtensionContextOpts::statement(false, false, "db9_tenant_x");
        with_context_opts(opts, async {
            assert!(effective_fs_plane_principal().is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn principal_returns_none_for_non_superuser_non_readonly_role() {
        // Fail-closed: a non-superuser regular role has no fs-plane
        // capability, regardless of name. JuiceFS backend init surfaces
        // this as a hard error.
        let opts = ExtensionContextOpts::statement(false, false, "db9_tenant_x")
            .with_authenticated_role(Some("alice".to_string()));
        with_context_opts(opts, async {
            assert!(effective_fs_plane_principal().is_none());
        })
        .await;
    }

    /// PR #2547 review #1 regression: a deployment bootstrapped with
    /// `DB9_BOOTSTRAP_ADMIN_USER=postgres` (or `svc_admin`, or any
    /// custom `CREATE ROLE ... SUPERUSER`) must reach `ReadWrite`
    /// fs-plane access without name-matching `"admin"`.
    #[tokio::test]
    async fn principal_grants_rw_to_any_superuser_regardless_of_name() {
        for name in ["admin", "postgres", "svc_admin", "alice"] {
            let opts = ExtensionContextOpts::statement(true, false, "db9_tenant_x")
                .with_authenticated_role(Some(name.to_string()));
            with_context_opts(opts, async move {
                let p = effective_fs_plane_principal()
                    .unwrap_or_else(|| panic!("superuser {name:?} should have a principal"));
                assert_eq!(p.role, name);
                assert_eq!(p.access, Fs9Access::ReadWrite);
            })
            .await;
        }
    }

    #[tokio::test]
    async fn principal_under_sd_elevates_to_rw_keeping_caller_role_as_identity() {
        // Caller logged in as a non-superuser role. Inside a SECURITY
        // DEFINER function owned by a superuser, the call carries
        // superuser authority — so fs-plane access must be ReadWrite.
        // The role string stays as the *actual* caller identity
        // (`alice`), not the fabricated `"admin"` the old code used:
        // identity belongs in `usr`/cache-key, capability in `scp`.
        let opts = ExtensionContextOpts::statement(false, false, "db9_tenant_x")
            .with_authenticated_role(Some("alice".to_string()));
        with_context_opts(opts, async {
            assert!(effective_fs_plane_principal().is_none(), "alice has no rw");
            let _guard = enter_security_definer_superuser();
            let p = effective_fs_plane_principal().unwrap();
            assert_eq!(p.role, "alice", "identity preserved under SD");
            assert_eq!(p.access, Fs9Access::ReadWrite, "SD grants rw capability");
        })
        .await;
    }

    #[tokio::test]
    async fn principal_restores_after_security_definer_drops() {
        let opts = ExtensionContextOpts::statement(false, false, "db9_tenant_x")
            .with_authenticated_role(Some("alice".to_string()));
        with_context_opts(opts, async {
            {
                let _guard = enter_security_definer_superuser();
                let p = effective_fs_plane_principal().unwrap();
                assert_eq!(p.access, Fs9Access::ReadWrite);
            }
            // After SD drops, alice is back to non-superuser — no
            // fs-plane capability.
            assert!(effective_fs_plane_principal().is_none());
        })
        .await;
    }
}
