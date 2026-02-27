//! Transaction mode validation, schema drift detection, notice collection,
//! and shared runtime helpers for the statement dispatch layer.

use super::super::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tikv_client::TransactionClient;

/// Validate and apply transaction modes (isolation level, access mode) from
/// `BEGIN ISOLATION LEVEL ...` or `START TRANSACTION ...` statements.
///
/// Downgrades SERIALIZABLE to REPEATABLE READ (TiKV snapshot isolation)
/// and stores accepted modes in the session for `SHOW` readback.
pub(in crate::sql::executor::core) fn validate_transaction_modes(
    session: &mut Session,
    modes: &[TransactionMode],
) -> Result<()> {
    for mode in modes {
        match mode {
            TransactionMode::IsolationLevel(level) => {
                let level_str = match level {
                    TransactionIsolationLevel::ReadUncommitted
                    | TransactionIsolationLevel::ReadCommitted => "read committed",
                    TransactionIsolationLevel::RepeatableRead => "repeatable read",
                    TransactionIsolationLevel::Serializable => "repeatable read",
                };
                if matches!(level, TransactionIsolationLevel::Serializable) {
                    tracing::warn!(
                        requested = "serializable",
                        actual = "repeatable read",
                        "TiKV cannot provide PostgreSQL SERIALIZABLE semantics; \
                         BEGIN/START TRANSACTION request has been downgraded"
                    );
                }
                session.set_known_setting("transaction_isolation", level_str.to_string())?;
            }
            TransactionMode::AccessMode(access_mode) => {
                let mode_str = match access_mode {
                    TransactionAccessMode::ReadOnly => "on",
                    TransactionAccessMode::ReadWrite => "off",
                };
                session.set_known_setting("default_transaction_read_only", mode_str.to_string())?;
            }
        }
    }
    Ok(())
}

impl Executor {
    /// Detect schema drift by checking `(table_id, schema_version)` pairs.
    ///
    /// Returns the first mismatch as `(table_name, expected_version, current_version)`.
    /// Uses `table_id` for lookup when available (non-zero), falling back to
    /// name-based lookup for backwards compatibility.
    pub(in crate::sql::executor::core) async fn first_schema_drift_on_txn(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_versions: &[(String, u64, u64)],
    ) -> Result<Option<(String, u64, Option<u64>)>> {
        for (table_name, table_id, expected_version) in table_versions {
            let current_schema = self.store().get_schema(txn, db_id, table_name).await?;
            let has_drift = match current_schema.as_ref() {
                // Backward-compat fallback for historical dependencies that did
                // not record `table_id` (0 sentinel). New dependencies always
                // carry `(table_id, schema_version)`.
                Some(schema) if *table_id == 0 => schema.version != *expected_version,
                Some(schema) => schema.table_id != *table_id || schema.version != *expected_version,
                None => true,
            };

            if has_drift {
                let current_version = current_schema.and_then(|schema| {
                    if *table_id == 0 || schema.table_id == *table_id {
                        Some(schema.version)
                    } else {
                        None
                    }
                });
                return Ok(Some((
                    table_name.clone(),
                    *expected_version,
                    current_version,
                )));
            }
        }
        Ok(None)
    }

    pub(in crate::sql::executor::core) async fn collect_notices_before_statement(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        stmt: &Statement,
    ) -> Result<Vec<ExecuteResult>> {
        use sqlparser::ast::ObjectType;

        match stmt {
            Statement::Drop {
                object_type: ObjectType::Table,
                names: drop_names,
                if_exists: true,
                ..
            } => {
                let mut notices = Vec::new();
                for name in drop_names {
                    let exists = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        name,
                        search_path,
                    )
                    .await?
                    .is_some();

                    if exists {
                        continue;
                    }

                    let base = name
                        .0
                        .last()
                        .map(|ident| ident.value.as_str())
                        .unwrap_or("?");
                    notices.push(ExecuteResult::Notice {
                        message: format!("table \"{}\" does not exist, skipping", base),
                        severity: "NOTICE".to_string(),
                    });
                }
                Ok(notices)
            }
            _ => Ok(Vec::new()),
        }
    }
}

// ── Shared runtime helpers ────────────────────────────────────

/// Session settings snapshot used to set up per-statement task-local context.
///
/// Both the simple-query and prepared dispatch paths extract the same four
/// settings before entering the runtime context nesting. This struct avoids
/// repeating that extraction logic.
pub(in crate::sql::executor::core) struct RuntimeSettings {
    pub is_superuser: bool,
    pub timezone: Arc<str>,
    pub max_sort_bytes: usize,
    pub search_path: Arc<Vec<String>>,
}

impl RuntimeSettings {
    pub fn from_session(session: &Session) -> Self {
        Self {
            is_superuser: session.is_superuser(),
            timezone: Arc::from(
                session
                    .show_setting_value("timezone")
                    .unwrap_or_else(|| "UTC".to_string()),
            ),
            max_sort_bytes: session.max_sort_bytes(),
            search_path: Arc::new(session.search_path().to_vec()),
        }
    }
}

/// Wrap a future in the per-statement task-local runtime context layers:
/// timezone → max_sort_bytes → search_path → extension context.
///
/// Returns a boxed future — transparent passthrough (`T`, not `Result<T>`).
pub(in crate::sql::executor::core) fn wrap_with_runtime_context<'a, T: Send + 'a>(
    settings: &RuntimeSettings,
    tenant_keyspace: &'a str,
    tikv_client: Option<Arc<TransactionClient>>,
    fut: impl Future<Output = T> + Send + 'a,
) -> Pin<Box<dyn Future<Output = T> + Send + 'a>> {
    let tz = settings.timezone.clone();
    let msb = settings.max_sort_bytes;
    let sp = settings.search_path.clone();
    let su = settings.is_superuser;
    Box::pin(session_context::with_timezone(
        tz,
        session_context::with_max_sort_bytes(
            msb,
            session_context::with_search_path(
                sp,
                crate::extensions::context::with_context_opts(
                    crate::extensions::context::ExtensionContextOpts::statement(
                        su,
                        tenant_keyspace,
                    )
                    .with_tikv_client(tikv_client),
                    fut,
                ),
            ),
        ),
    ))
}

/// Apply an optional statement timeout to a future.
///
/// If `timeout` is `Some`, wraps the future with `tokio::time::timeout` and
/// converts the elapsed error into `StatementTimeoutError`. If `None`,
/// runs the future directly.
pub(in crate::sql::executor::core) async fn apply_statement_timeout<T>(
    timeout: Option<Duration>,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, fut).await {
            Ok(res) => res,
            Err(_) => Err(anyhow::Error::new(StatementTimeoutError)),
        },
        None => fut.await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session(is_superuser: bool) -> Session {
        let store = crate::storage::TikvStore::new_stub();
        let obs = crate::observability::registry().tenant("ut_dispatch_utils");
        Session::new_with_user_and_database(
            store,
            obs,
            "tester".to_string(),
            is_superuser,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        )
    }

    #[test]
    fn validate_transaction_modes_accepts_supported_modes() {
        let mut session = test_session(true);
        validate_transaction_modes(
            &mut session,
            &[
                TransactionMode::IsolationLevel(TransactionIsolationLevel::ReadCommitted),
                TransactionMode::AccessMode(TransactionAccessMode::ReadOnly),
            ],
        )
        .unwrap();

        assert_eq!(
            session
                .show_setting_value("transaction_isolation")
                .as_deref(),
            Some("repeatable read")
        );
        assert_eq!(
            session
                .show_setting_value("default_transaction_read_only")
                .as_deref(),
            Some("on")
        );
    }

    #[test]
    fn validate_transaction_modes_downgrades_serializable_to_repeatable_read() {
        let mut session = test_session(false);
        validate_transaction_modes(
            &mut session,
            &[TransactionMode::IsolationLevel(
                TransactionIsolationLevel::Serializable,
            )],
        )
        .unwrap();
        assert_eq!(
            session
                .show_setting_value("transaction_isolation")
                .as_deref(),
            Some("repeatable read")
        );
    }

    #[test]
    fn runtime_settings_reads_session_snapshot() {
        let mut session = test_session(true);
        session
            .set_known_setting("timezone", "Asia/Shanghai".to_string())
            .unwrap();
        session
            .set_known_setting("search_path", "public, pg_catalog".to_string())
            .unwrap();
        session
            .set_known_setting("max_sort_bytes", "4096".to_string())
            .unwrap();

        let settings = RuntimeSettings::from_session(&session);
        assert_eq!(settings.timezone.as_ref(), "Asia/Shanghai");
        assert_eq!(settings.max_sort_bytes, session.max_sort_bytes());
        assert!(settings.search_path.iter().any(|s| s == "public"));
        assert!(settings.is_superuser);
    }

    #[tokio::test]
    async fn wrap_runtime_context_sets_task_locals() {
        let settings = RuntimeSettings {
            is_superuser: false,
            timezone: Arc::from("UTC"),
            max_sort_bytes: 1234,
            search_path: Arc::new(vec!["$user".to_string(), "public".to_string()]),
        };
        let out = wrap_with_runtime_context(&settings, "tenant_a", None, async {
            let tz = crate::session_context::current_timezone();
            let msb = crate::session_context::current_max_sort_bytes();
            let first_schema = crate::session_context::current_search_path_first_schema();
            let tenant = crate::extensions::context::tenant_keyspace().unwrap_or_default();
            (tz, msb, first_schema, tenant)
        })
        .await;

        assert_eq!(out.0.as_ref(), "UTC");
        assert_eq!(out.1, 1234);
        assert_eq!(out.2, "public");
        assert_eq!(out.3, "tenant_a");
    }

    #[tokio::test]
    async fn apply_statement_timeout_handles_timeout_and_success() {
        let ok = apply_statement_timeout(Some(Duration::from_millis(50)), async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok::<_, anyhow::Error>(7)
        })
        .await
        .unwrap();
        assert_eq!(ok, 7);

        let err = apply_statement_timeout(Some(Duration::from_millis(1)), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok::<_, anyhow::Error>(1)
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("statement timeout"));
    }

    #[tokio::test]
    async fn autocommit_backoff_returns() {
        crate::sql::executor::core::retry::autocommit_backoff(0).await;
        crate::sql::executor::core::retry::autocommit_backoff(8).await;
    }
}
