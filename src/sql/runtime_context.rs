use crate::sql::Session;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tikv_client::TransactionClient;

/// Session settings snapshot used to set up per-statement task-local context.
#[derive(Clone)]
pub(crate) struct RuntimeSettings {
    pub is_superuser: bool,
    pub bypass_rls: bool,
    pub timezone: Arc<str>,
    pub max_sort_bytes: usize,
    pub search_path: Arc<Vec<String>>,
    pub text_search_config: Arc<str>,
}

impl RuntimeSettings {
    pub(crate) fn from_session(session: &Session) -> Self {
        Self {
            is_superuser: session.is_superuser(),
            bypass_rls: session.bypass_rls(),
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
    pub extension_statement_state: Arc<crate::extensions::context::ExtensionStatementState>,
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
            extension_statement_state: Arc::new(
                crate::extensions::context::ExtensionStatementState::default(),
            ),
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
    let extension_statement_state = runtime.extension_statement_state.clone();

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
                                            settings.bypass_rls,
                                            tenant_keyspace.as_ref(),
                                        )
                                        .with_statement_state(extension_statement_state)
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
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tokio::io::AsyncBufRead;

    use crate::extensions::fs::backend::{
        FsBackend, FsCreateUpload, FsFileInfo, FsMultipartCompletedPart, FsPreparedDownload,
        FsPresignedRequest, FsWriteStream, FsWriteStreamOptions,
    };

    struct RuntimeTestBackend;

    struct RuntimeTestWriteStream;

    #[async_trait]
    impl FsWriteStream for RuntimeTestWriteStream {
        async fn write_chunk(&mut self, _chunk: &[u8]) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn finish(self: Box<Self>) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn abort(self: Box<Self>) -> Result<()> {
            anyhow::bail!("not implemented")
        }
    }

    #[async_trait]
    impl FsBackend for RuntimeTestBackend {
        async fn stat(&self, _path: &str) -> Result<FsFileInfo> {
            anyhow::bail!("not implemented")
        }

        async fn readdir(&self, _path: &str) -> Result<Vec<FsFileInfo>> {
            anyhow::bail!("not implemented")
        }

        async fn read_file(&self, _path: &str, _max_bytes: usize) -> Result<Vec<u8>> {
            anyhow::bail!("not implemented")
        }

        async fn read_file_stream(
            &self,
            _path: &str,
            _max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            anyhow::bail!("not implemented")
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn remove_recursive(&self, _path: &str) -> Result<u64> {
            anyhow::bail!("not implemented")
        }

        async fn mkdir(&self, _path: &str, _recursive: bool, _mode: Option<u32>) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn write_file(&self, _path: &str, _data: &[u8], _mode: Option<u32>) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            Ok(Box::new(RuntimeTestWriteStream))
        }

        async fn read_file_at(&self, _path: &str, _offset: u64, _length: usize) -> Result<Vec<u8>> {
            anyhow::bail!("not implemented")
        }

        async fn write_file_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn append_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn truncate(&self, _path: &str, _size: u64) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn create_upload(
            &self,
            _path: &str,
            _expected_size: u64,
            _mode: Option<u32>,
        ) -> Result<FsCreateUpload> {
            anyhow::bail!("not implemented")
        }

        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
        ) -> Result<FsPresignedRequest> {
            anyhow::bail!("not implemented")
        }

        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
            anyhow::bail!("not implemented")
        }

        async fn symlink(&self, _path: &str, _target: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn readlink(&self, _path: &str) -> Result<String> {
            anyhow::bail!("not implemented")
        }

        async fn chmod(&self, _path: &str, _mode: u32) -> Result<()> {
            unreachable!("chmod is not used in these tests");
        }
    }

    #[tokio::test]
    async fn wrap_runtime_context_sets_task_locals() {
        let runtime = StatementRuntimeContext {
            settings: RuntimeSettings {
                is_superuser: false,
                bypass_rls: false,
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
            extension_statement_state: Arc::new(
                crate::extensions::context::ExtensionStatementState::default(),
            ),
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

    #[tokio::test]
    async fn wrap_runtime_context_reuses_extension_statement_state_across_reentry() {
        let runtime = StatementRuntimeContext {
            settings: RuntimeSettings {
                is_superuser: false,
                bypass_rls: false,
                timezone: Arc::from("UTC"),
                max_sort_bytes: 1234,
                search_path: Arc::new(vec!["public".to_string()]),
                text_search_config: Arc::from("simple"),
            },
            tenant_keyspace: Arc::from("tenant_a"),
            database_id: 42,
            txn_snapshot_ts_version: Some(999),
            tikv_client: None,
            extension_txn_delta: Arc::new((HashSet::new(), HashSet::new())),
            extension_statement_state: Arc::new(
                crate::extensions::context::ExtensionStatementState::default(),
            ),
        };
        let backend: Arc<dyn FsBackend> = Arc::new(RuntimeTestBackend);

        wrap_with_statement_runtime_context(&runtime, async {
            crate::extensions::context::cache_fs_backend(backend.clone()).expect("cache backend");
            crate::extensions::context::try_consume_embedding_call_with_limit(1)
                .expect("first embedding call must succeed");
        })
        .await;

        let cached = wrap_with_statement_runtime_context(&runtime, async {
            crate::extensions::context::try_consume_embedding_call_with_limit(1)
                .expect_err("embedding call count must persist across re-entry");
            crate::extensions::context::cached_fs_backend().expect("cached backend")
        })
        .await;

        assert!(Arc::ptr_eq(&cached, &backend));
    }

    #[tokio::test]
    async fn wrap_runtime_context_does_not_leak_embedding_authorization_across_reentry() {
        let runtime = StatementRuntimeContext {
            settings: RuntimeSettings {
                is_superuser: false,
                bypass_rls: false,
                timezone: Arc::from("UTC"),
                max_sort_bytes: 1234,
                search_path: Arc::new(vec!["public".to_string()]),
                text_search_config: Arc::from("simple"),
            },
            tenant_keyspace: Arc::from("tenant_a"),
            database_id: 42,
            txn_snapshot_ts_version: Some(999),
            tikv_client: None,
            extension_txn_delta: Arc::new((HashSet::new(), HashSet::new())),
            extension_statement_state: Arc::new(
                crate::extensions::context::ExtensionStatementState::default(),
            ),
        };

        let authorized = wrap_with_statement_runtime_context(&runtime, async {
            crate::extensions::context::with_embedding_authorized(async {
                crate::extensions::context::is_embedding_authorized()
            })
            .await
            .expect("authorized embedding scope")
        })
        .await;
        assert!(authorized);

        let leaked = wrap_with_statement_runtime_context(&runtime, async {
            crate::extensions::context::is_embedding_authorized()
        })
        .await;
        assert!(
            !leaked,
            "embedding authorization is dynamic-scope state and must not leak across re-entry"
        );
    }
}
