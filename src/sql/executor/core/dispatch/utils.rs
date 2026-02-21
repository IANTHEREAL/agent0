//! Transaction mode validation, schema drift detection, notice collection,
//! and shared runtime helpers for the statement dispatch layer.

use super::super::*;
use std::future::Future;
use std::pin::Pin;

/// Validate and apply transaction modes (isolation level, access mode) from
/// `BEGIN ISOLATION LEVEL ...` or `START TRANSACTION ...` statements.
///
/// Rejects SERIALIZABLE (TiKV cannot provide true serializable guarantees)
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
                    TransactionIsolationLevel::Serializable => {
                        return Err(SqlError::Unsupported(
                            "SERIALIZABLE isolation level is not supported".into(),
                        )
                        .into());
                    }
                };
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
    pub(in crate::sql::executor::core) async fn first_schema_drift_on_txn(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_versions: &[(String, u64)],
    ) -> Result<Option<(String, u64, Option<u64>)>> {
        for (table_name, expected_version) in table_versions {
            let current_schema = self.store().get_schema(txn, db_id, table_name).await?;
            let current_version = current_schema.map(|s| s.version);
            if current_version != Some(*expected_version) {
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
                crate::extensions::context::with_context(su, tenant_keyspace, fut),
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

/// Exponential backoff with jitter for autocommit retry loops.
pub(in crate::sql::executor::core) async fn autocommit_backoff(attempt: usize) {
    let base_ms = 5u64.saturating_mul(1u64 << attempt.min(6));
    let jitter_ms = rand::random::<u64>() % (base_ms + 1);
    let backoff_ms = base_ms + jitter_ms;
    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
}
