//! Best-effort per-database activity emission.
//!
//! The SQL executor reports activity through this nonblocking seam.  The
//! concrete backend writer is installed by the control-plane integration once
//! that endpoint/auth contract exists; until then the default sink is disabled.

use anyhow::Result;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DatabaseActivityKind {
    /// A successful read or write access.
    Active,
    /// A successful committed data or metadata mutation.
    ///
    /// Backends must treat this as both modified and active to preserve
    /// `last_modified_at <= last_active_at` under receive-time stamping.
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DatabaseActivitySource {
    /// SQL execution (pgwire / HTTP SQL / cron-run statements).
    Sql,
    /// fs9 filesystem WS operations observed by db9-server.
    Fs9,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatabaseActivityEvent {
    pub(crate) tenant_keyspace: Arc<str>,
    pub(crate) database_id: u64,
    pub(crate) source: DatabaseActivitySource,
    pub(crate) kind: DatabaseActivityKind,
}

pub(crate) trait DatabaseActivitySink: Send + Sync + 'static {
    /// Enqueue or merge an event without awaiting backend I/O.
    fn try_record(&self, event: DatabaseActivityEvent) -> Result<()>;
}

static ACTIVITY_SINK: Lazy<RwLock<Option<Arc<dyn DatabaseActivitySink>>>> =
    Lazy::new(|| RwLock::new(None));

/// Record a SQL-sourced activity event (pgwire / HTTP SQL / cron-run).
pub(crate) fn record_sql_activity(
    tenant_keyspace: &str,
    database_id: u64,
    kind: DatabaseActivityKind,
) {
    record_activity(
        DatabaseActivitySource::Sql,
        tenant_keyspace,
        database_id,
        kind,
    );
}

/// Record an fs9-sourced activity event (db9-server-observed WS filesystem ops).
///
/// `Active` for served reads/lists; `Modified` for served writes — the caller
/// (the fs9 WS handler) is responsible for emitting only on successful ops and
/// for classifying write-vs-read. This reuses the same installed sink as
/// [`record_sql_activity`]; there is no separate fs9 flush path.
pub(crate) fn record_fs9_activity(
    tenant_keyspace: &str,
    database_id: u64,
    kind: DatabaseActivityKind,
) {
    record_activity(
        DatabaseActivitySource::Fs9,
        tenant_keyspace,
        database_id,
        kind,
    );
}

/// Shared nonblocking emit path for all activity sources.
///
/// `tenant_keyspace` is the backend's row-identity key and is always required.
/// `database_id` is diagnostic-only (the backend keys on keyspace), so it is
/// required for [`DatabaseActivitySource::Sql`] (the pgwire session always has
/// a real id). [`DatabaseActivitySource::Fs9`] accepts `0` for legacy/test
/// callers, but normal WebSocket sessions should pass their bound database id.
fn record_activity(
    source: DatabaseActivitySource,
    tenant_keyspace: &str,
    database_id: u64,
    kind: DatabaseActivityKind,
) {
    if tenant_keyspace.is_empty() {
        return;
    }
    if database_id == 0 && !matches!(source, DatabaseActivitySource::Fs9) {
        return;
    }

    let Some(sink) = ACTIVITY_SINK.read().clone() else {
        return;
    };

    let event = DatabaseActivityEvent {
        tenant_keyspace: Arc::from(tenant_keyspace),
        database_id,
        source,
        kind,
    };

    if let Err(err) = sink.try_record(event) {
        tracing::warn!(
            database_id,
            kind = ?kind,
            source = ?source,
            error = %err,
            "dropping database activity event"
        );
    }
}

/// Install the process-global activity sink (production path).
///
/// Called once at startup by the control-plane connector when the backend
/// endpoint + auth are configured. Replacement is supported (last writer wins);
/// when no sink is installed, [`record_sql_activity`] is a no-op.
pub(crate) fn install_database_activity_sink(sink: Arc<dyn DatabaseActivitySink>) {
    *ACTIVITY_SINK.write() = Some(sink);
}

#[cfg(test)]
pub(crate) struct DatabaseActivitySinkGuard {
    previous: Option<Arc<dyn DatabaseActivitySink>>,
    _lock: parking_lot::MutexGuard<'static, ()>,
}

#[cfg(test)]
static TEST_SINK_LOCK: Lazy<parking_lot::Mutex<()>> = Lazy::new(|| parking_lot::Mutex::new(()));

#[cfg(test)]
impl Drop for DatabaseActivitySinkGuard {
    fn drop(&mut self) {
        *ACTIVITY_SINK.write() = self.previous.take();
    }
}

#[cfg(test)]
pub(crate) fn install_test_database_activity_sink(
    sink: Arc<dyn DatabaseActivitySink>,
) -> DatabaseActivitySinkGuard {
    let lock = TEST_SINK_LOCK.lock();
    let previous = ACTIVITY_SINK.write().replace(sink);
    DatabaseActivitySinkGuard {
        previous,
        _lock: lock,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<DatabaseActivityEvent>>,
    }

    impl DatabaseActivitySink for RecordingSink {
        fn try_record(&self, event: DatabaseActivityEvent) -> Result<()> {
            self.events.lock().push(event);
            Ok(())
        }
    }

    #[test]
    fn record_sql_activity_uses_installed_sink() {
        let sink = Arc::new(RecordingSink::default());
        let _guard = install_test_database_activity_sink(sink.clone());

        record_sql_activity("activity_sink_test", 7, DatabaseActivityKind::Modified);

        let events = sink.events.lock();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            DatabaseActivityEvent {
                tenant_keyspace: Arc::from("activity_sink_test"),
                database_id: 7,
                source: DatabaseActivitySource::Sql,
                kind: DatabaseActivityKind::Modified,
            }
        );
    }

    #[test]
    fn record_sql_activity_ignores_invalid_identity() {
        let sink = Arc::new(RecordingSink::default());
        let _guard = install_test_database_activity_sink(sink.clone());

        record_sql_activity("", 7, DatabaseActivityKind::Active);
        record_sql_activity("activity_sink_test", 0, DatabaseActivityKind::Active);

        assert!(sink.events.lock().is_empty());
    }

    #[test]
    fn record_fs9_activity_emits_with_unknown_database_id() {
        // Legacy/test fs9 callers may not know a numeric db id; the backend
        // keys on keyspace, so db_id == 0 ("diagnostic unknown") still emits.
        let sink = Arc::new(RecordingSink::default());
        let _guard = install_test_database_activity_sink(sink.clone());

        record_fs9_activity("activity_sink_test", 0, DatabaseActivityKind::Active);

        let events = sink.events.lock();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            DatabaseActivityEvent {
                tenant_keyspace: Arc::from("activity_sink_test"),
                database_id: 0,
                source: DatabaseActivitySource::Fs9,
                kind: DatabaseActivityKind::Active,
            }
        );
    }

    #[test]
    fn record_fs9_activity_still_requires_keyspace() {
        let sink = Arc::new(RecordingSink::default());
        let _guard = install_test_database_activity_sink(sink.clone());

        record_fs9_activity("", 0, DatabaseActivityKind::Modified);

        assert!(sink.events.lock().is_empty());
    }

    #[test]
    fn record_sql_activity_still_rejects_zero_database_id() {
        // The db_id == 0 relaxation is fs9-only; SQL must keep passing a real id.
        let sink = Arc::new(RecordingSink::default());
        let _guard = install_test_database_activity_sink(sink.clone());

        record_sql_activity("activity_sink_test", 0, DatabaseActivityKind::Modified);

        assert!(sink.events.lock().is_empty());
    }
}
