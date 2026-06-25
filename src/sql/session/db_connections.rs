//! Per-database connection counter for DROP DATABASE safety.
//!
//! PostgreSQL refuses `DROP DATABASE` when other backends are connected
//! (SQLSTATE 55006). This module provides a global registry that tracks
//! how many sessions are using each `(keyspace, db_id)` and enforces
//! mutual exclusion between new connections and DROP DATABASE.
//!
//! The key invariant: `connect()` and `try_mark_dropping()` both hold
//! the same Mutex, so there is no window where a new connection can
//! register after DROP has checked the count.

use std::collections::HashMap;

use parking_lot::Mutex;

/// Global per-database connection registry.
static DB_CONNECTIONS: std::sync::OnceLock<DbConnectionRegistry> = std::sync::OnceLock::new();

/// Get the global registry (lazily initialized).
pub fn db_connection_registry() -> &'static DbConnectionRegistry {
    DB_CONNECTIONS.get_or_init(DbConnectionRegistry::new)
}

struct DbEntry {
    /// Number of active sessions using this database.
    count: usize,
    /// Number of in-flight non-session operations that can still return data
    /// or commit effects for this database on this node.
    active_ops: usize,
    /// True if DROP DATABASE is in progress. New connections are refused.
    dropping: bool,
}

pub(crate) struct DbConnectionRegistry {
    entries: Mutex<HashMap<(String, u64), DbEntry>>,
}

impl DbConnectionRegistry {
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn key(keyspace: &str, db_id: u64) -> (String, u64) {
        (crate::worker::canonical_registry_keyspace(keyspace), db_id)
    }

    /// Try to register a new connection. Returns Err if the database
    /// is being dropped (DROP DATABASE in progress).
    pub fn try_connect(
        &self,
        keyspace: &str,
        db_id: u64,
    ) -> Result<DbConnectionGuard, &'static str> {
        let key = Self::key(keyspace, db_id);
        let mut map = self.entries.lock();
        let entry = map.entry(key.clone()).or_insert(DbEntry {
            count: 0,
            active_ops: 0,
            dropping: false,
        });
        if entry.dropping {
            return Err("database is being dropped");
        }
        entry.count += 1;
        Ok(DbConnectionGuard {
            keyspace: key.0,
            db_id: key.1,
        })
    }

    /// Atomically check count == 0 and mark as dropping.
    /// Returns Ok(guard) if successful, Err if other connections exist.
    /// The guard clears the dropping flag on drop (if DROP fails/rolls back).
    pub fn try_mark_dropping(&self, keyspace: &str, db_id: u64) -> Result<DroppingGuard, usize> {
        let key = Self::key(keyspace, db_id);
        let mut map = self.entries.lock();
        let entry = map.entry(key.clone()).or_insert(DbEntry {
            count: 0,
            active_ops: 0,
            dropping: false,
        });
        let active = entry.count.saturating_add(entry.active_ops);
        if active > 0 {
            return Err(active);
        }
        if entry.dropping {
            // Another DROP DATABASE is already in progress.
            return Err(0);
        }
        // This registry only tracks in-process connections and FS streams.
        // Cross-node exclusion is handled by the database lifecycle row,
        // node leases, and per-db/epoch drop coordinator in TiKV metadata.
        entry.dropping = true;
        Ok(DroppingGuard {
            keyspace: key.0,
            db_id: key.1,
            committed: false,
        })
    }

    /// Register an in-flight operation that is not represented by a SQL
    /// connection guard, such as an FS/WebSocket read stream.
    pub(crate) fn try_track_operation(
        &self,
        keyspace: &str,
        db_id: u64,
    ) -> Result<DbOperationGuard, &'static str> {
        let key = Self::key(keyspace, db_id);
        let mut map = self.entries.lock();
        let entry = map.entry(key.clone()).or_insert(DbEntry {
            count: 0,
            active_ops: 0,
            dropping: false,
        });
        if entry.dropping {
            return Err("database is being dropped");
        }
        entry.active_ops += 1;
        Ok(DbOperationGuard {
            keyspace: key.0,
            db_id: key.1,
        })
    }

    /// Number of active non-session operations that can still emit old-epoch
    /// data for this database on this node.
    pub(crate) fn active_operation_count(&self, keyspace: &str, db_id: u64) -> usize {
        let key = Self::key(keyspace, db_id);
        let map = self.entries.lock();
        map.get(&key).map(|entry| entry.active_ops).unwrap_or(0)
    }
}

/// RAII guard that decrements the connection count on drop.
pub(crate) struct DbConnectionGuard {
    keyspace: String,
    db_id: u64,
}

impl Drop for DbConnectionGuard {
    fn drop(&mut self) {
        let key = (self.keyspace.clone(), self.db_id);
        let registry = db_connection_registry();
        let mut map = registry.entries.lock();
        if let Some(entry) = map.get_mut(&key) {
            entry.count = entry.count.saturating_sub(1);
            if entry.count == 0 && entry.active_ops == 0 && !entry.dropping {
                map.remove(&key);
            }
        }
    }
}

/// RAII guard for one in-flight non-session database operation.
pub(crate) struct DbOperationGuard {
    keyspace: String,
    db_id: u64,
}

impl Drop for DbOperationGuard {
    fn drop(&mut self) {
        let key = (self.keyspace.clone(), self.db_id);
        let registry = db_connection_registry();
        let mut map = registry.entries.lock();
        if let Some(entry) = map.get_mut(&key) {
            entry.active_ops = entry.active_ops.saturating_sub(1);
            if entry.count == 0 && entry.active_ops == 0 && !entry.dropping {
                map.remove(&key);
            }
        }
    }
}

/// RAII guard for DROP DATABASE in-progress state.
/// If DROP succeeds, call `commit()` to clean up the entry.
/// If DROP fails, the guard drops and clears the `dropping` flag,
/// allowing new connections again.
pub(crate) struct DroppingGuard {
    keyspace: String,
    db_id: u64,
    committed: bool,
}

impl DroppingGuard {
    /// Call after DROP DATABASE has fully committed.
    /// Removes the registry entry entirely.
    pub fn commit(&mut self) {
        let key = (self.keyspace.clone(), self.db_id);
        let registry = db_connection_registry();
        let mut map = registry.entries.lock();
        map.remove(&key);
        self.committed = true;
    }
}

impl Drop for DroppingGuard {
    fn drop(&mut self) {
        if !self.committed {
            // DROP failed or was rolled back — allow connections again.
            let key = (self.keyspace.clone(), self.db_id);
            let registry = db_connection_registry();
            let mut map = registry.entries.lock();
            if let Some(entry) = map.get_mut(&key) {
                entry.dropping = false;
                if entry.count == 0 && entry.active_ops == 0 {
                    map.remove(&key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::db_connection_registry;

    static NEXT_DB_ID: AtomicU64 = AtomicU64::new(900_000);

    fn unique_db() -> (String, u64) {
        let id = NEXT_DB_ID.fetch_add(1, Ordering::SeqCst);
        (format!("db_connections_test_{id}"), id)
    }

    #[test]
    fn active_operation_blocks_drop_until_released() {
        let registry = db_connection_registry();
        let (keyspace, db_id) = unique_db();

        let operation = registry
            .try_track_operation(&keyspace, db_id)
            .expect("operation should register before drop");
        assert!(matches!(
            registry.try_mark_dropping(&keyspace, db_id),
            Err(1)
        ));

        drop(operation);
        let mut dropping = registry
            .try_mark_dropping(&keyspace, db_id)
            .expect("drop can start after operation exits");
        dropping.commit();
    }

    #[test]
    fn default_keyspace_casing_is_canonical_for_drop_exclusion() {
        let registry = db_connection_registry();
        let (_, db_id) = unique_db();

        let operation = registry
            .try_track_operation("default", db_id)
            .expect("default-keyed FS operation should register");
        assert!(matches!(
            registry.try_mark_dropping("DEFAULT", db_id),
            Err(1)
        ));
        assert_eq!(registry.active_operation_count("DEFAULT", db_id), 1);

        drop(operation);
        let connection = registry
            .try_connect("DEFAULT", db_id)
            .expect("DEFAULT-keyed SQL session should register");
        assert!(matches!(
            registry.try_mark_dropping("default", db_id),
            Err(1)
        ));

        drop(connection);
        let mut dropping = registry
            .try_mark_dropping("DEFAULT", db_id)
            .expect("drop can start after all canonicalized users exit");
        assert!(registry.try_track_operation("default", db_id).is_err());
        dropping.commit();
    }

    #[test]
    fn dropping_blocks_new_operations_until_rollback() {
        let registry = db_connection_registry();
        let (keyspace, db_id) = unique_db();

        let dropping = registry
            .try_mark_dropping(&keyspace, db_id)
            .expect("drop should start on idle database");
        assert!(registry.try_track_operation(&keyspace, db_id).is_err());

        drop(dropping);
        let operation = registry
            .try_track_operation(&keyspace, db_id)
            .expect("rolled back drop should allow operations again");
        drop(operation);
    }
}
