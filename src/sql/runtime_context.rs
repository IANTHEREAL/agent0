use crate::sql::Session;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tikv_client::TransactionClient;

/// Session settings snapshot used to set up per-statement task-local context.
#[derive(Clone)]
pub(crate) struct RuntimeSettings {
    pub is_superuser: bool,
    pub timezone: Arc<str>,
    pub max_sort_bytes: usize,
    pub search_path: Arc<Vec<String>>,
    pub text_search_config: Arc<str>,
}

impl RuntimeSettings {
    pub(crate) fn from_session(session: &Session) -> Self {
        Self {
            is_superuser: session.is_superuser(),
            timezone: Arc::from(
                session
                    .show_setting_value("timezone")
                    .unwrap_or_else(|| "UTC".to_string()),
            ),
            max_sort_bytes: session.max_sort_bytes(),
            search_path: Arc::new(session.search_path().to_vec()),
            text_search_config: Arc::from(
                session
                    .show_setting_value("default_text_search_config")
                    .unwrap_or_else(|| {
                        crate::sql::fts_tokenizers::default_text_search_config().to_string()
                    }),
            ),
        }
    }
}

/// Immutable per-statement runtime scope shared by every execution path.
///
/// This is the single source of truth for statement task-local state outside
/// the query-specific [`QueryContext`] snapshot.
#[derive(Clone)]
pub(crate) struct StatementRuntimeContext {
    pub settings: RuntimeSettings,
    pub tenant_keyspace: Arc<str>,
    pub database_id: u64,
    pub txn_snapshot_ts_version: Option<u64>,
    pub tikv_client: Option<Arc<TransactionClient>>,
    pub extension_txn_delta: Arc<crate::session_context::ExtensionTxnDelta>,
}

impl StatementRuntimeContext {
    pub(crate) fn from_session(
        session: &Session,
        tenant_keyspace: &str,
        tikv_client: Option<Arc<TransactionClient>>,
    ) -> Self {
        Self {
            settings: RuntimeSettings::from_session(session),
            tenant_keyspace: Arc::from(tenant_keyspace),
            database_id: session.current_database_id(),
            txn_snapshot_ts_version: session.active_txn_start_ts_version(),
            tikv_client,
            extension_txn_delta: session.extension_delta_snapshot(),
        }
    }
}

/// Wrap a future in the per-statement task-local runtime context layers:
/// timezone -> max_sort_bytes -> search_path -> text_search_config -> keyspace
/// -> database_id -> txn snapshot ts -> extension txn delta -> extension context.
pub(crate) fn wrap_with_statement_runtime_context<'a, T: Send + 'a>(
    runtime: &StatementRuntimeContext,
    fut: impl Future<Output = T> + Send + 'a,
) -> Pin<Box<dyn Future<Output = T> + Send + 'a>> {
    let fut = Box::pin(fut);

    let settings = runtime.settings.clone();
    let tenant_keyspace = runtime.tenant_keyspace.clone();
    let database_id = runtime.database_id;
    let txn_snapshot_ts_version = runtime.txn_snapshot_ts_version;
    let tikv_client = runtime.tikv_client.clone();
    let extension_txn_delta = runtime.extension_txn_delta.clone();

    Box::pin(crate::session_context::with_timezone(
        settings.timezone,
        crate::session_context::with_max_sort_bytes(
            settings.max_sort_bytes,
            crate::session_context::with_search_path(
                settings.search_path,
                crate::session_context::with_text_search_config(
                    settings.text_search_config,
                    crate::session_context::with_keyspace(
                        tenant_keyspace.clone(),
                        crate::session_context::with_database_id(
                            database_id,
                            crate::session_context::with_txn_snapshot_ts_version(
                                txn_snapshot_ts_version,
                                crate::session_context::with_extension_txn_delta(
                                    extension_txn_delta,
                                    crate::extensions::context::with_context_opts(
                                        crate::extensions::context::ExtensionContextOpts::statement(
                                            settings.is_superuser,
                                            tenant_keyspace.as_ref(),
                                        )
                                        .with_tikv_client(tikv_client),
                                        fut,
                                    ),
                                ),
                            ),
                        ),
                    ),
                ),
            ),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::{wrap_with_statement_runtime_context, RuntimeSettings, StatementRuntimeContext};
    use std::collections::HashSet;
    use std::sync::Arc;

    #[tokio::test]
    async fn wrap_runtime_context_sets_task_locals() {
        let runtime = StatementRuntimeContext {
            settings: RuntimeSettings {
                is_superuser: false,
                timezone: Arc::from("UTC"),
                max_sort_bytes: 1234,
                search_path: Arc::new(vec!["$user".to_string(), "public".to_string()]),
                text_search_config: Arc::from("simple"),
            },
            tenant_keyspace: Arc::from("tenant_a"),
            database_id: 42,
            txn_snapshot_ts_version: Some(999),
            tikv_client: None,
            extension_txn_delta: Arc::new((HashSet::new(), HashSet::new())),
        };

        let out = wrap_with_statement_runtime_context(&runtime, async {
            let tz = crate::session_context::current_timezone();
            let msb = crate::session_context::current_max_sort_bytes();
            let first_schema = crate::session_context::current_search_path_first_schema();
            let tenant = crate::extensions::context::tenant_keyspace().unwrap_or_default();
            let tsc = crate::session_context::current_text_search_config();
            let db_id = crate::session_context::current_database_id();
            let txn_ts = crate::session_context::current_txn_snapshot_ts_version();
            (tz, msb, first_schema, tenant, tsc, db_id, txn_ts)
        })
        .await;

        assert_eq!(out.0.as_ref(), "UTC");
        assert_eq!(out.1, 1234);
        assert_eq!(out.2, "public");
        assert_eq!(out.3, "tenant_a");
        assert_eq!(out.4.as_ref(), "simple");
        assert_eq!(out.5, 42);
        assert_eq!(out.6, Some(999));
    }
}
