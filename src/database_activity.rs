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
    Sql,
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

pub(crate) fn record_sql_activity(
    tenant_keyspace: &str,
    database_id: u64,
    kind: DatabaseActivityKind,
) {
    if tenant_keyspace.is_empty() || database_id == 0 {
        return;
    }

    let Some(sink) = ACTIVITY_SINK.read().clone() else {
        return;
    };

    let event = DatabaseActivityEvent {
        tenant_keyspace: Arc::from(tenant_keyspace),
        database_id,
        source: DatabaseActivitySource::Sql,
        kind,
    };

    if let Err(err) = sink.try_record(event) {
        tracing::warn!(
            database_id,
            kind = ?kind,
            source = ?DatabaseActivitySource::Sql,
            error = %err,
            "dropping database activity event"
        );
    }
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
}
