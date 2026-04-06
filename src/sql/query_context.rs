//! Per-statement query context replacing scattered task-local variables.
//!
//! Replaces: `CONNECTION_ID`, `CURRENT_DATABASE_NAME`,
//! `STATEMENT_TIMESTAMP_MILLIS` (statement_time.rs), `TIMEZONE` (session_context.rs).
//! Legacy paths still read task-locals; eval functions require explicit context threading.
// TODO(#2335): migrate to parking_lot — phase 2/3
#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::model::Value;
use crate::sql::advisory_locks::AdvisoryLockMode;
pub(crate) use crate::sql::session::settings::public_setting_value;
use crate::storage::TikvStore;

/// Wrapper around `Arc<TikvStore>` that implements `Debug` so it can live
/// inside `QueryContext` (which derives `Debug`).
#[derive(Clone)]
pub(crate) struct StoreRef(pub(crate) Arc<TikvStore>);

impl std::fmt::Debug for StoreRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<store>")
    }
}

tokio::task_local! {
    static CONNECTION_ID: i64;
    static CURRENT_DATABASE_NAME: Arc<str>;
    static CURRENT_USER_NAME: Arc<str>;
    static CURRENT_TIMEZONE: Arc<str>;
    static QUERY_PARAMS: Vec<Option<Value>>;
    static QUERY_PARAM_TYPES: Vec<Option<crate::model::DataType>>;
    static SETTINGS_SNAPSHOT: Arc<HashMap<String, String>>;
    static EXECUTION_SETTINGS_SNAPSHOT: Arc<HashMap<String, String>>;
    static SETTINGS_RUNTIME_OVERRIDES: Arc<Mutex<HashMap<String, String>>>;
    static PENDING_SET_CONFIG_MUTATIONS: Arc<Mutex<Vec<SetConfigMutation>>>;
    static XACT_ADVISORY_LOCK_USED: Arc<AtomicBool>;
    static XACT_ADVISORY_SAVEPOINT_TRACKER: Arc<tokio::sync::Mutex<XactAdvisorySavepointTracker>>;
    static STORE_REF: Option<StoreRef>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetConfigMutation {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) is_local: bool,
    /// When true, the mutation resets the setting to its boot default
    /// (equivalent to RESET <name>). `value` is ignored by the flush path.
    pub(crate) is_reset: bool,
}

fn lock_ignoring_poison<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone)]
pub(crate) struct XactAdvisoryLockRecord {
    pub(crate) keyspace: Arc<str>,
    pub(crate) key: i64,
    pub(crate) mode: AdvisoryLockMode,
}

#[derive(Debug, Default)]
struct XactAdvisorySavepointFrame {
    name: String,
    acquired_locks: Vec<XactAdvisoryLockRecord>,
}

#[derive(Debug, Default)]
pub(crate) struct XactAdvisorySavepointTracker {
    stack: Vec<XactAdvisorySavepointFrame>,
}

impl XactAdvisorySavepointTracker {
    pub(crate) fn reset(&mut self) {
        self.stack.clear();
    }

    pub(crate) fn create(&mut self, name: String) {
        self.stack.push(XactAdvisorySavepointFrame {
            name,
            acquired_locks: Vec::new(),
        });
    }

    pub(crate) fn release(&mut self, name: &str) -> anyhow::Result<()> {
        let target_index = self
            .stack
            .iter()
            .rposition(|sp| sp.name == name)
            .ok_or_else(|| anyhow::anyhow!("savepoint \"{}\" does not exist", name))?;

        if target_index == 0 {
            self.stack.clear();
            return Ok(());
        }

        let mut released = self.stack.split_off(target_index);
        let parent = self
            .stack
            .last_mut()
            .expect("target_index > 0 implies parent savepoint exists");
        for frame in released.iter_mut() {
            parent.acquired_locks.append(&mut frame.acquired_locks);
        }
        Ok(())
    }

    pub(crate) fn record_acquired_lock(&mut self, record: XactAdvisoryLockRecord) {
        if let Some(current) = self.stack.last_mut() {
            current.acquired_locks.push(record);
        }
    }

    pub(crate) fn prepare_rollback_to(
        &mut self,
        name: &str,
    ) -> anyhow::Result<Vec<XactAdvisoryLockRecord>> {
        let target_index = self
            .stack
            .iter()
            .rposition(|sp| sp.name == name)
            .ok_or_else(|| anyhow::anyhow!("savepoint \"{}\" does not exist", name))?;

        let mut popped = self.stack.split_off(target_index + 1);
        let mut to_release = Vec::new();
        for frame in popped.iter_mut().rev() {
            to_release.append(&mut frame.acquired_locks);
        }
        let target = self
            .stack
            .get_mut(target_index)
            .expect("target index must exist");
        to_release.append(&mut target.acquired_locks);
        Ok(to_release)
    }
}

#[derive(Debug, Clone)]
pub struct QueryContext {
    /// Internal 64-bit connection identity.
    /// `pg_backend_pid()` exposes this as int4 for PostgreSQL wire compatibility.
    pub connection_id: i64,
    /// current_database()
    pub database_name: Arc<str>,
    /// current_user / session_user — the authenticated role for this session
    pub current_user: Arc<str>,
    /// NOW() / CURRENT_TIMESTAMP / STATEMENT_TIMESTAMP() — stable within a statement
    pub statement_timestamp_ms: i64,
    /// TRANSACTION_TIMESTAMP() — stable within a transaction block;
    /// equals statement_timestamp_ms for implicit (autocommit) transactions
    pub transaction_timestamp_ms: i64,
    #[allow(dead_code)] // framework: set during init; read path uses session_context fallback
    pub timezone: Arc<str>,
    /// Bound parameter values from extended protocol (Execute).
    /// `None` entries represent SQL NULL. Empty vec for simple-query path.
    pub params: Vec<Option<Value>>,
    /// Finalized parameter types from Parse-time analysis.
    /// Threaded to execute-time Analyzer so re-analysis uses the same type hints.
    /// Empty vec for simple-query path or when no types were finalized.
    pub param_types: Vec<Option<crate::model::DataType>>,
    /// Snapshot of all session settings at statement start.
    /// Used by `current_setting()` in expression contexts.
    pub settings_snapshot: Option<Arc<HashMap<String, String>>>,
    /// Internal execution settings at statement start.
    /// Used by runtime subsystems (e.g. embedding) that need raw values.
    pub execution_settings_snapshot: Option<Arc<HashMap<String, String>>>,
    /// Typed lock timeout snapshot at statement start.
    pub lock_timeout: Option<Duration>,
    /// Shared per-session marker: true when current transaction used
    /// xact-scoped advisory lock functions.
    pub xact_advisory_lock_used: Option<Arc<AtomicBool>>,
    /// Per-session savepoint tracker for xact-scoped advisory locks.
    pub xact_advisory_savepoint_tracker:
        Option<Arc<tokio::sync::Mutex<XactAdvisorySavepointTracker>>>,
    /// Store reference for sync expression evaluation paths that need
    /// async auth lookups (e.g., session_authorization role-existence check).
    pub(crate) store_ref: Option<StoreRef>,
}

impl QueryContext {
    pub fn new(
        connection_id: i64,
        database_name: Arc<str>,
        current_user: Arc<str>,
        statement_timestamp_ms: i64,
        transaction_timestamp_ms: i64,
        timezone: Arc<str>,
    ) -> Self {
        Self {
            connection_id,
            database_name,
            current_user,
            statement_timestamp_ms,
            transaction_timestamp_ms,
            timezone,
            params: vec![],
            param_types: vec![],
            settings_snapshot: None,
            execution_settings_snapshot: None,
            lock_timeout: None,
            xact_advisory_lock_used: None,
            xact_advisory_savepoint_tracker: None,
            store_ref: None,
        }
    }

    pub(crate) fn current_connection_id() -> Option<i64> {
        CONNECTION_ID.try_with(|id| *id).ok()
    }

    pub(crate) fn current_database_name() -> Option<Arc<str>> {
        CURRENT_DATABASE_NAME.try_with(|name| name.clone()).ok()
    }

    pub(crate) fn current_user_name() -> Option<Arc<str>> {
        CURRENT_USER_NAME.try_with(|name| name.clone()).ok()
    }

    pub(crate) fn current_timezone() -> Option<Arc<str>> {
        CURRENT_TIMEZONE.try_with(|tz| tz.clone()).ok()
    }

    pub(crate) fn current_query_params() -> Vec<Option<Value>> {
        QUERY_PARAMS.try_with(|p| p.clone()).unwrap_or_default()
    }

    pub(crate) fn current_query_param_types() -> Vec<Option<crate::model::DataType>> {
        QUERY_PARAM_TYPES
            .try_with(|t| t.clone())
            .unwrap_or_default()
    }

    /// Build a QueryContext from task-local storage.
    ///
    /// Use this for code paths that don't receive an explicit QueryContext
    /// (planning phase, triggers, tests, etc.).  During normal query execution
    /// the task-locals are always populated by the session layer, so the
    /// returned context is correct.
    pub fn from_task_locals() -> Self {
        use crate::sql::statement_time::{
            statement_timestamp_millis, transaction_timestamp_millis,
        };

        let stmt_ts = statement_timestamp_millis();
        let conn_id = Self::current_connection_id();
        let db_name = Self::current_database_name();
        let user_name = Self::current_user_name();
        let timezone = Self::current_timezone();
        let params = Self::current_query_params();

        #[cfg(test)]
        if stmt_ts.is_none()
            || conn_id.is_none()
            || db_name.is_none()
            || user_name.is_none()
            || timezone.is_none()
        {
            return Self::for_tests();
        }

        let stmt_ts = stmt_ts.expect(
            "QueryContext::from_task_locals called without statement timestamp; \
             execute through Executor::execute or wrap with statement_time::with_timestamps",
        );
        let txn_ts = transaction_timestamp_millis().unwrap_or(stmt_ts);
        let mut qctx = Self::new(
            conn_id.expect(
                "QueryContext::from_task_locals called without connection_id; \
                 execute through Executor::execute or wrap with query_context::with_query_context",
            ),
            db_name.expect(
                "QueryContext::from_task_locals called without database_name; \
                 execute through Executor::execute or wrap with query_context::with_query_context",
            ),
            user_name.expect(
                "QueryContext::from_task_locals called without current_user; \
                 execute through Executor::execute or wrap with query_context::with_query_context",
            ),
            stmt_ts,
            txn_ts,
            timezone.expect(
                "QueryContext::from_task_locals called without timezone; \
                 execute through Executor::execute or wrap with query_context::with_query_context",
            ),
        );
        qctx.params = params;
        qctx.param_types = Self::current_query_param_types();
        qctx.settings_snapshot = SETTINGS_SNAPSHOT.try_with(|s| s.clone()).ok();
        qctx.lock_timeout = qctx.settings_snapshot.as_deref().and_then(|snapshot| {
            snapshot.get("lock_timeout").and_then(|raw| {
                match crate::sql::session::SessionSettings::parse_timeout_value(raw) {
                    Ok(0) => None,
                    Ok(ms) => Some(Duration::from_millis(ms)),
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            value = raw,
                            "invalid lock_timeout in query context settings snapshot"
                        );
                        None
                    }
                }
            })
        });
        qctx.xact_advisory_lock_used = XACT_ADVISORY_LOCK_USED.try_with(|f| f.clone()).ok();
        qctx.xact_advisory_savepoint_tracker =
            XACT_ADVISORY_SAVEPOINT_TRACKER.try_with(|t| t.clone()).ok();
        qctx.execution_settings_snapshot = EXECUTION_SETTINGS_SNAPSHOT.try_with(|s| s.clone()).ok();
        qctx.store_ref = STORE_REF.try_with(|s| s.clone()).ok().flatten();
        qctx
    }

    /// Read the current `lock_timeout` from the task-local settings snapshot.
    ///
    /// Returns `None` when no timeout is configured (value 0 or missing).
    /// This is a lightweight alternative to `from_task_locals()` when only
    /// the lock timeout is needed.
    pub(crate) fn current_lock_timeout() -> Option<Duration> {
        SETTINGS_SNAPSHOT
            .try_with(|s| {
                s.get("lock_timeout").and_then(|raw| {
                    match crate::sql::session::SessionSettings::parse_timeout_value(raw) {
                        Ok(0) => None,
                        Ok(ms) => Some(Duration::from_millis(ms)),
                        Err(_) => None,
                    }
                })
            })
            .ok()
            .flatten()
    }

    /// Read a setting from the base snapshot only (bypassing runtime overrides).
    ///
    /// Used for NULL-reset resolution where we need the authoritative session
    /// value (e.g. `session_authorization` must resolve to the login role, not
    /// a value mutated by a prior `set_config` in the same statement).
    pub(crate) fn base_setting_snapshot(name: &str) -> Option<String> {
        let canonical =
            crate::sql::session::settings::SessionSettings::canonical_setting_name(name);
        SETTINGS_SNAPSHOT
            .try_with(|s| s.get(canonical).cloned())
            .ok()
            .flatten()
    }

    /// Read a setting from the current statement's settings snapshot.
    ///
    /// Returns `None` when there is no scoped snapshot or the key is absent.
    pub(crate) fn current_setting_snapshot(name: &str) -> Option<String> {
        let canonical =
            crate::sql::session::settings::SessionSettings::canonical_setting_name(name);
        if let Some(override_value) = SETTINGS_RUNTIME_OVERRIDES
            .try_with(|overrides| lock_ignoring_poison(overrides).get(canonical).cloned())
            .ok()
            .flatten()
        {
            return Some(public_setting_value(canonical, override_value));
        }
        SETTINGS_SNAPSHOT
            .try_with(|s| s.get(canonical).cloned())
            .ok()
            .flatten()
            .map(|value| public_setting_value(canonical, value))
    }

    /// Resolve a public SQL-facing current_setting() lookup for an explicit
    /// QueryContext, preferring the scoped task-local snapshot when present and
    /// falling back to the snapshot carried on the QueryContext itself.
    pub(crate) fn current_setting_lookup(qctx: &QueryContext, name: &str) -> CurrentSettingLookup {
        let canonical =
            crate::sql::session::settings::SessionSettings::canonical_setting_name(name);
        if let Some(value) = Self::current_setting_snapshot(canonical) {
            return CurrentSettingLookup::Found(value);
        }
        match qctx.settings_snapshot.as_deref() {
            Some(snapshot) => match snapshot.get(canonical).cloned() {
                Some(value) => CurrentSettingLookup::Found(public_setting_value(canonical, value)),
                None => CurrentSettingLookup::Missing,
            },
            None => CurrentSettingLookup::NoSnapshot,
        }
    }

    /// Read a raw execution setting from the current statement's internal snapshot.
    ///
    /// Returns `None` when there is no scoped snapshot or the key is absent.
    pub(crate) fn current_execution_setting_snapshot(name: &str) -> Option<String> {
        let canonical =
            crate::sql::session::settings::SessionSettings::canonical_setting_name(name);
        if let Some(override_value) = SETTINGS_RUNTIME_OVERRIDES
            .try_with(|overrides| lock_ignoring_poison(overrides).get(canonical).cloned())
            .ok()
            .flatten()
        {
            return Some(override_value);
        }
        EXECUTION_SETTINGS_SNAPSHOT
            .try_with(|s| s.get(canonical).cloned())
            .ok()
            .flatten()
    }

    /// Read a reset-default value from the execution settings snapshot.
    ///
    /// These are internal `_reset_default.<name>` entries carrying the effective
    /// post-reset value for tenant-configurable GUCs. They live only in the
    /// execution snapshot (never in the public snapshot), so they are invisible
    /// to `current_setting()` while still available for NULL-reset resolution.
    pub(crate) fn reset_default_from_snapshot(guc_name: &str) -> Option<String> {
        let key = format!("_reset_default.{}", guc_name);
        EXECUTION_SETTINGS_SNAPSHOT
            .try_with(|s| s.get(&key).cloned())
            .ok()
            .flatten()
    }

    pub(crate) fn set_runtime_setting_override(name: &str, value: &str) {
        let canonical =
            crate::sql::session::settings::SessionSettings::canonical_setting_name(name)
                .to_string();
        let value = value.to_string();
        let _ = SETTINGS_RUNTIME_OVERRIDES.try_with(|overrides| {
            lock_ignoring_poison(overrides).insert(canonical, value);
        });
    }

    pub(crate) fn remove_runtime_setting_override(name: &str) {
        let canonical =
            crate::sql::session::settings::SessionSettings::canonical_setting_name(name);
        let _ = SETTINGS_RUNTIME_OVERRIDES.try_with(|overrides| {
            lock_ignoring_poison(overrides).remove(canonical);
        });
    }

    #[cfg(test)]
    pub(crate) fn clear_runtime_setting_overrides() {
        let _ = SETTINGS_RUNTIME_OVERRIDES.try_with(|overrides| {
            lock_ignoring_poison(overrides).clear();
        });
    }

    pub(crate) fn record_set_config_mutation(name: &str, value: &str, is_local: bool) {
        let canonical =
            crate::sql::session::settings::SessionSettings::canonical_setting_name(name)
                .to_string();
        let value = value.to_string();

        Self::set_runtime_setting_override(&canonical, &value);

        let _ = PENDING_SET_CONFIG_MUTATIONS.try_with(|pending| {
            lock_ignoring_poison(pending).push(SetConfigMutation {
                name: canonical,
                value,
                is_local,
                is_reset: false,
            });
        });
    }

    /// Record a RESET mutation (PG parity: set_config(name, NULL, is_local)).
    /// Sets the runtime override to the boot default so that current_setting()
    /// in the same statement sees the post-reset value, and records an is_reset
    /// mutation for the flush path.
    pub(crate) fn record_set_config_reset(name: &str, is_local: bool, boot_default: &str) {
        let canonical =
            crate::sql::session::settings::SessionSettings::canonical_setting_name(name)
                .to_string();

        Self::set_runtime_setting_override(&canonical, boot_default);

        let _ = PENDING_SET_CONFIG_MUTATIONS.try_with(|pending| {
            lock_ignoring_poison(pending).push(SetConfigMutation {
                name: canonical,
                value: String::new(),
                is_local,
                is_reset: true,
            });
        });
    }

    pub(crate) fn take_set_config_mutations() -> Vec<SetConfigMutation> {
        PENDING_SET_CONFIG_MUTATIONS
            .try_with(|pending| std::mem::take(&mut *lock_ignoring_poison(pending)))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::new(
            0,
            Arc::from("postgres"),
            Arc::from("postgres"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CurrentSettingLookup {
    Found(String),
    Missing,
    NoSnapshot,
}

pub(crate) async fn with_query_context<R, Fut>(
    connection_id: i64,
    database_name: Arc<str>,
    current_user: Arc<str>,
    timezone: Arc<str>,
    fut: Fut,
) -> R
where
    Fut: Future<Output = R>,
{
    #[cfg(debug_assertions)]
    {
        let fut = Box::pin(fut);
        CONNECTION_ID
            .scope(
                connection_id,
                CURRENT_DATABASE_NAME.scope(
                    database_name,
                    CURRENT_USER_NAME.scope(current_user, CURRENT_TIMEZONE.scope(timezone, fut)),
                ),
            )
            .await
    }

    #[cfg(not(debug_assertions))]
    {
        CONNECTION_ID
            .scope(
                connection_id,
                CURRENT_DATABASE_NAME.scope(
                    database_name,
                    CURRENT_USER_NAME.scope(current_user, CURRENT_TIMEZONE.scope(timezone, fut)),
                ),
            )
            .await
    }
}

/// Scope all query task-locals (identity + statement/transaction timestamps)
/// from an explicit `QueryContext`.
pub(crate) async fn with_scoped_query_context<R, Fut>(qctx: &QueryContext, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    // Always scope SETTINGS_SNAPSHOT to prevent stale inheritance from outer async scopes.
    let snapshot = qctx
        .settings_snapshot
        .clone()
        .unwrap_or_else(|| Arc::new(HashMap::new()));
    let execution_snapshot = qctx
        .execution_settings_snapshot
        .clone()
        .unwrap_or_else(|| Arc::new(HashMap::new()));
    // Always scope the xact advisory marker so from_task_locals reads
    // the same per-session flag used by commit/rollback cleanup.
    let xact_advisory_lock_used = qctx
        .xact_advisory_lock_used
        .clone()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let xact_advisory_savepoint_tracker = qctx
        .xact_advisory_savepoint_tracker
        .clone()
        .unwrap_or_else(|| {
            Arc::new(tokio::sync::Mutex::new(
                XactAdvisorySavepointTracker::default(),
            ))
        });
    with_query_context(
        qctx.connection_id,
        qctx.database_name.clone(),
        qctx.current_user.clone(),
        qctx.timezone.clone(),
        STORE_REF.scope(
            qctx.store_ref.clone(),
            XACT_ADVISORY_SAVEPOINT_TRACKER.scope(
                xact_advisory_savepoint_tracker,
                XACT_ADVISORY_LOCK_USED.scope(
                    xact_advisory_lock_used,
                    SETTINGS_RUNTIME_OVERRIDES.scope(
                        Arc::new(Mutex::new(HashMap::new())),
                        PENDING_SET_CONFIG_MUTATIONS.scope(
                            Arc::new(Mutex::new(Vec::new())),
                            EXECUTION_SETTINGS_SNAPSHOT.scope(
                                execution_snapshot,
                                SETTINGS_SNAPSHOT.scope(
                                    snapshot,
                                    QUERY_PARAM_TYPES.scope(
                                        qctx.param_types.clone(),
                                        QUERY_PARAMS.scope(
                                            qctx.params.clone(),
                                            crate::sql::statement_time::with_timestamps(
                                                qctx.statement_timestamp_ms,
                                                qctx.transaction_timestamp_ms,
                                                fut,
                                            ),
                                        ),
                                    ),
                                ),
                            ),
                        ),
                    ),
                ),
            ),
        ),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn query_context_clone() {
        let ctx = QueryContext::new(
            42,
            Arc::from("mydb"),
            Arc::from("admin"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        );
        let ctx2 = ctx.clone();
        assert_eq!(ctx2.connection_id, 42);
        assert_eq!(ctx2.database_name.as_ref(), "mydb");
        assert_eq!(ctx2.current_user.as_ref(), "admin");
        assert_eq!(ctx2.statement_timestamp_ms, 1_700_000_000_000);
        assert_eq!(ctx2.transaction_timestamp_ms, 1_700_000_000_000);
        assert_eq!(ctx2.timezone.as_ref(), "UTC");
    }

    #[tokio::test]
    async fn from_task_locals_reads_query_identity() {
        let ctx = with_query_context(
            77,
            Arc::from("tenant_db"),
            Arc::from("testuser"),
            Arc::from("UTC"),
            crate::sql::statement_time::with_timestamps(
                1_700_000_123_000,
                1_700_000_122_000,
                async { QueryContext::from_task_locals() },
            ),
        )
        .await;
        assert_eq!(ctx.connection_id, 77);
        assert_eq!(ctx.database_name.as_ref(), "tenant_db");
        assert_eq!(ctx.current_user.as_ref(), "testuser");
    }

    #[tokio::test]
    async fn with_scoped_query_context_propagates_xact_advisory_lock_marker() {
        let mut qctx = QueryContext::for_tests();
        let marker = Arc::new(AtomicBool::new(false));
        let tracker = Arc::new(tokio::sync::Mutex::new(
            XactAdvisorySavepointTracker::default(),
        ));
        qctx.xact_advisory_lock_used = Some(marker.clone());
        qctx.xact_advisory_savepoint_tracker = Some(tracker.clone());

        with_scoped_query_context(&qctx, async {
            let from_locals = QueryContext::from_task_locals();
            let scoped_marker = from_locals
                .xact_advisory_lock_used
                .expect("xact advisory marker should be scoped");
            scoped_marker.store(true, Ordering::Release);

            let scoped_tracker = from_locals
                .xact_advisory_savepoint_tracker
                .expect("xact advisory savepoint tracker should be scoped");
            let mut locked = scoped_tracker.lock().await;
            locked.create("sp1".to_string());
        })
        .await;

        assert!(marker.load(Ordering::Acquire));
        let locked = tracker.lock().await;
        assert_eq!(locked.stack.len(), 1);
    }

    #[tokio::test]
    async fn current_lock_timeout_reads_from_task_local() {
        // No snapshot → None
        assert_eq!(QueryContext::current_lock_timeout(), None);

        // With lock_timeout = "500"
        let mut snapshot = HashMap::new();
        snapshot.insert("lock_timeout".to_string(), "500".to_string());
        let result = SETTINGS_SNAPSHOT
            .scope(Arc::new(snapshot), async {
                QueryContext::current_lock_timeout()
            })
            .await;
        assert_eq!(result, Some(Duration::from_millis(500)));

        // With lock_timeout = "0" → None (disabled)
        let mut snapshot = HashMap::new();
        snapshot.insert("lock_timeout".to_string(), "0".to_string());
        let result = SETTINGS_SNAPSHOT
            .scope(Arc::new(snapshot), async {
                QueryContext::current_lock_timeout()
            })
            .await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn set_config_mutations_are_scoped_and_drained() {
        let qctx = QueryContext::for_tests();

        let out = with_scoped_query_context(&qctx, async {
            QueryContext::record_set_config_mutation("statement_timeout", "200ms", false);
            QueryContext::record_set_config_mutation("timezone", "UTC", true);
            QueryContext::take_set_config_mutations()
        })
        .await;

        assert_eq!(
            out,
            vec![
                SetConfigMutation {
                    name: "statement_timeout".to_string(),
                    value: "200ms".to_string(),
                    is_local: false,
                    is_reset: false,
                },
                SetConfigMutation {
                    name: "timezone".to_string(),
                    value: "UTC".to_string(),
                    is_local: true,
                    is_reset: false,
                },
            ]
        );
        assert!(QueryContext::take_set_config_mutations().is_empty());
    }

    #[tokio::test]
    async fn runtime_setting_overrides_can_be_removed_and_cleared() {
        let mut qctx = QueryContext::for_tests();
        let mut snapshot = HashMap::new();
        snapshot.insert("statement_timeout".to_string(), "0".to_string());
        snapshot.insert("timezone".to_string(), "UTC".to_string());
        qctx.settings_snapshot = Some(Arc::new(snapshot));

        with_scoped_query_context(&qctx, async {
            QueryContext::record_set_config_mutation("statement_timeout", "450ms", false);
            assert_eq!(
                QueryContext::current_setting_snapshot("statement_timeout").as_deref(),
                Some("450ms")
            );

            QueryContext::remove_runtime_setting_override("statement_timeout");
            assert_eq!(
                QueryContext::current_setting_snapshot("statement_timeout").as_deref(),
                Some("0")
            );

            QueryContext::set_runtime_setting_override("timezone", "Asia/Shanghai");
            assert_eq!(
                QueryContext::current_setting_snapshot("timezone").as_deref(),
                Some("Asia/Shanghai")
            );

            QueryContext::clear_runtime_setting_overrides();
            assert_eq!(
                QueryContext::current_setting_snapshot("timezone").as_deref(),
                Some("UTC")
            );
        })
        .await;
    }

    #[tokio::test]
    async fn public_and_execution_setting_snapshots_are_separated() {
        let mut public_snapshot = HashMap::new();
        public_snapshot.insert("embedding.api_key".to_string(), "****".to_string());
        let mut execution_snapshot = HashMap::new();
        execution_snapshot.insert("embedding.api_key".to_string(), "secret-key".to_string());

        let result = SETTINGS_RUNTIME_OVERRIDES
            .scope(
                Arc::new(Mutex::new(HashMap::new())),
                EXECUTION_SETTINGS_SNAPSHOT.scope(
                    Arc::new(execution_snapshot),
                    SETTINGS_SNAPSHOT.scope(Arc::new(public_snapshot), async {
                        QueryContext::set_runtime_setting_override(
                            "embedding.api_key",
                            "session-secret",
                        );
                        (
                            QueryContext::current_setting_snapshot("embedding.api_key"),
                            QueryContext::current_execution_setting_snapshot("embedding.api_key"),
                        )
                    }),
                ),
            )
            .await;

        assert_eq!(result.0.as_deref(), Some("****"));
        assert_eq!(result.1.as_deref(), Some("session-secret"));
    }

    #[test]
    fn public_setting_value_masks_embedding_api_key() {
        // Raw secret → masked
        assert_eq!(
            public_setting_value("embedding.api_key", "sk-secret-1234".to_string()),
            "****"
        );
        // Empty → passthrough (no masking needed)
        assert_eq!(
            public_setting_value("embedding.api_key", "".to_string()),
            ""
        );
        // Non-sensitive setting → passthrough
        assert_eq!(public_setting_value("timezone", "UTC".to_string()), "UTC");
    }

    #[test]
    fn xact_advisory_savepoint_tracker_rollback_collects_target_and_nested_locks() {
        let keyspace: Arc<str> = Arc::from("tenant_tracker_rollback");
        let mut tracker = XactAdvisorySavepointTracker::default();
        tracker.create("sp1".to_string());
        tracker.record_acquired_lock(XactAdvisoryLockRecord {
            keyspace: keyspace.clone(),
            key: 11,
            mode: AdvisoryLockMode::Exclusive,
        });
        tracker.create("sp2".to_string());
        tracker.record_acquired_lock(XactAdvisoryLockRecord {
            keyspace: keyspace.clone(),
            key: 22,
            mode: AdvisoryLockMode::Shared,
        });

        let released = tracker
            .prepare_rollback_to("sp1")
            .expect("savepoint should exist");

        assert_eq!(released.len(), 2);
        assert_eq!(released[0].key, 22);
        assert_eq!(released[0].mode, AdvisoryLockMode::Shared);
        assert_eq!(released[1].key, 11);
        assert_eq!(released[1].mode, AdvisoryLockMode::Exclusive);
        assert_eq!(tracker.stack.len(), 1);
        assert_eq!(tracker.stack[0].name, "sp1");
        assert!(tracker.stack[0].acquired_locks.is_empty());
    }
}
