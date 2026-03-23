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
use std::sync::Mutex;

/// Global per-database connection registry.
static DB_CONNECTIONS: std::sync::OnceLock<DbConnectionRegistry> = std::sync::OnceLock::new();

/// Get the global registry (lazily initialized).
pub fn db_connection_registry() -> &'static DbConnectionRegistry {
    DB_CONNECTIONS.get_or_init(DbConnectionRegistry::new)
}

struct DbEntry {
    /// Number of active sessions using this database.
    count: usize,
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

    /// Try to register a new connection. Returns Err if the database
    /// is being dropped (DROP DATABASE in progress).
    pub fn try_connect(
        &self,
        keyspace: &str,
        db_id: u64,
    ) -> Result<DbConnectionGuard, &'static str> {
        let key = (keyspace.to_string(), db_id);
        let mut map = self.entries.lock().unwrap();
        let entry = map.entry(key.clone()).or_insert(DbEntry {
            count: 0,
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
        let key = (keyspace.to_string(), db_id);
        let mut map = self.entries.lock().unwrap();
        let entry = map.entry(key.clone()).or_insert(DbEntry {
            count: 0,
            dropping: false,
        });
        if entry.count > 0 {
            return Err(entry.count);
        }
        if entry.dropping {
            // Another DROP DATABASE is already in progress.
            return Err(0);
        }
        // TODO(#2041): process-local only; multi-node deployments need
        // TiKV-level coordination (persisted dropping flag + pessimistic lock).
        entry.dropping = true;
        Ok(DroppingGuard {
            keyspace: key.0,
            db_id: key.1,
            committed: false,
        })
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
        let mut map = registry.entries.lock().unwrap();
        if let Some(entry) = map.get_mut(&key) {
            entry.count = entry.count.saturating_sub(1);
            if entry.count == 0 && !entry.dropping {
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
        let mut map = registry.entries.lock().unwrap();
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
            let mut map = registry.entries.lock().unwrap();
            if let Some(entry) = map.get_mut(&key) {
                entry.dropping = false;
                if entry.count == 0 {
                    map.remove(&key);
                }
            }
        }
    }
}
