//! Process-global active session registry for admin session management.
//!
//! Provides transport-agnostic primitives for listing, cancelling, and
//! terminating active sessions. Designed for multi-tenant deployments with
//! tenant-scoped operations and audit logging.
//!
//! See `docs/design/32_admin_session_management.md` for the full design.

use dashmap::{DashMap, DashSet};
use pgwire::tokio::CancellationToken;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::OnceLock;

use parking_lot::RwLock;
use std::time::Instant;

// ---------------------------------------------------------------------------
// SessionState
// ---------------------------------------------------------------------------

/// Session lifecycle state, atomically updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SessionState {
    Idle = 0,
    Active = 1,
    IdleInTransaction = 2,
    IdleInFailedTransaction = 3,
}

impl SessionState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Idle,
            1 => Self::Active,
            2 => Self::IdleInTransaction,
            3 => Self::IdleInFailedTransaction,
            _ => Self::Idle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Active => "active",
            Self::IdleInTransaction => "idle_in_transaction",
            Self::IdleInFailedTransaction => "idle_in_failed_transaction",
        }
    }
}

/// Wrapper around AtomicU8 for lock-free state updates.
pub struct AtomicSessionState(AtomicU8);

impl AtomicSessionState {
    pub fn new(state: SessionState) -> Self {
        Self(AtomicU8::new(state as u8))
    }

    pub fn load(&self) -> SessionState {
        SessionState::from_u8(self.0.load(Ordering::Relaxed))
    }

    pub fn store(&self, state: SessionState) {
        self.0.store(state as u8, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// SessionInfo
// ---------------------------------------------------------------------------

/// Metadata for one active session, visible to admin operations.
///
/// All fields are set at registration time (post-authentication) and are
/// immutable except `current_query`, `state`, `query_start`, and
/// `query_cancel`, which are updated during query execution.
pub struct SessionInfo {
    pub connection_id: i64,
    pub tenant_id: String,
    pub principal: String,
    pub database: String,
    pub peer_addr: String,
    /// Wall-clock epoch ms when the session was registered.
    pub connected_at_epoch_ms: i64,
    /// Monotonic timestamp for duration calculations.
    pub connected_at_mono: Instant,
    pub state: AtomicSessionState,
    /// Digest of the currently executing query. Truncated to 1024 chars.
    current_query: RwLock<String>,
    /// Epoch ms when the current query started. 0 = no active query.
    pub query_start: AtomicI64,
    /// Connection-level cancellation token.
    cancel_token: CancellationToken,
    /// Query-level cancellation token (child of cancel_token).
    query_cancel: RwLock<Option<CancellationToken>>,
}

/// Max length for stored query text.
const MAX_QUERY_LEN: usize = 1024;

impl SessionInfo {
    pub fn new(
        connection_id: i64,
        tenant_id: String,
        principal: String,
        database: String,
        peer_addr: String,
        cancel_token: CancellationToken,
    ) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        Self {
            connection_id,
            tenant_id,
            principal,
            database,
            peer_addr,
            connected_at_epoch_ms: now,
            connected_at_mono: Instant::now(),
            state: AtomicSessionState::new(SessionState::Idle),
            current_query: RwLock::new(String::new()),
            query_start: AtomicI64::new(0),
            cancel_token,
            query_cancel: RwLock::new(None),
        }
    }

    /// Set the current query text and create a child cancellation token.
    /// Returns the child token for the caller to pass into the executor.
    pub fn begin_query(&self, query: &str) -> CancellationToken {
        // Use floor_char_boundary to avoid panicking on multi-byte UTF-8.
        let truncated = if query.len() > MAX_QUERY_LEN {
            let end = floor_char_boundary(query, MAX_QUERY_LEN);
            &query[..end]
        } else {
            query
        };
        {
            let mut q = self.current_query.write();
            q.clear();
            q.push_str(truncated);
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        self.query_start.store(now_ms, Ordering::Relaxed);
        self.state.store(SessionState::Active);

        let child = self.cancel_token.child_token();
        *self.query_cancel.write() = Some(child.clone());
        child
    }

    /// Clear the current query text and query cancel token.
    pub fn end_query(&self, next_state: SessionState) {
        self.current_query.write().clear();
        self.query_start.store(0, Ordering::Relaxed);
        self.state.store(next_state);
        *self.query_cancel.write() = None;
    }

    /// Update session state without changing query info.
    pub fn set_state(&self, state: SessionState) {
        self.state.store(state);
    }

    fn snapshot(&self, server_id: &str) -> SessionSnapshot {
        let now_mono = Instant::now();
        let duration_ms = now_mono.duration_since(self.connected_at_mono).as_millis() as i64;
        let query_start_ms = self.query_start.load(Ordering::Relaxed);
        let query_duration_ms = if query_start_ms > 0 {
            let now_wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            Some(now_wall - query_start_ms)
        } else {
            None
        };
        let current_query = self.current_query.read().clone();

        SessionSnapshot {
            connection_id: self.connection_id,
            tenant_id: self.tenant_id.clone(),
            principal: self.principal.clone(),
            database: self.database.clone(),
            peer_addr: self.peer_addr.clone(),
            connected_at_epoch_ms: self.connected_at_epoch_ms,
            state: self.state.load(),
            current_query,
            query_start_epoch_ms: query_start_ms,
            duration_ms,
            query_duration_ms,
            server_id: server_id.to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// SessionSnapshot
// ---------------------------------------------------------------------------

/// Serializable, owned copy of session info returned by list/get operations.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub connection_id: i64,
    pub tenant_id: String,
    pub principal: String,
    pub database: String,
    pub peer_addr: String,
    pub connected_at_epoch_ms: i64,
    pub state: SessionState,
    pub current_query: String,
    pub query_start_epoch_ms: i64,
    pub duration_ms: i64,
    pub query_duration_ms: Option<i64>,
    pub server_id: String,
}

// ---------------------------------------------------------------------------
// SessionFilter
// ---------------------------------------------------------------------------

/// Filter criteria for listing sessions.
pub struct SessionFilter {
    pub tenant_id: Option<String>,
    pub all_tenants: bool,
    pub principal: Option<String>,
    pub state: Option<SessionState>,
    pub min_duration_ms: Option<i64>,
    pub limit: usize,
    pub offset: usize,
}

impl Default for SessionFilter {
    fn default() -> Self {
        Self {
            tenant_id: None,
            all_tenants: false,
            principal: None,
            state: None,
            min_duration_ms: None,
            limit: 100,
            offset: 0,
        }
    }
}

/// Max limit when listing across all tenants.
pub const ALL_TENANTS_MAX_LIMIT: usize = 1000;

// ---------------------------------------------------------------------------
// CancelError / TerminateAllResult
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum CancelError {
    NotFound,
    NoActiveQuery,
}

impl std::fmt::Display for CancelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "connection not found"),
            Self::NoActiveQuery => write!(f, "no active query"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TerminateAllResult {
    pub requested: u32,
    pub terminated: u32,
    pub already_closed: u32,
}

// ---------------------------------------------------------------------------
// SessionRegistry
// ---------------------------------------------------------------------------

/// Process-global active session registry.
///
/// Thread-safe, lock-free reads via DashMap. Memory bounded by active
/// connection count, not total tenant count.
pub struct SessionRegistry {
    sessions: DashMap<i64, std::sync::Arc<SessionInfo>>,
    tenant_index: DashMap<String, DashSet<i64>>,
    server_id: String,
}

impl SessionRegistry {
    pub fn new() -> Self {
        let server_id = hostname();
        Self {
            sessions: DashMap::new(),
            tenant_index: DashMap::new(),
            server_id,
        }
    }

    /// Register a new session after successful authentication.
    pub fn register(&self, info: SessionInfo) {
        let connection_id = info.connection_id;
        let tenant_id = info.tenant_id.clone();
        self.sessions
            .insert(connection_id, std::sync::Arc::new(info));
        self.tenant_index
            .entry(tenant_id)
            .or_default()
            .insert(connection_id);
    }

    /// Unregister a session on connection close.
    pub fn unregister(&self, connection_id: i64) {
        if let Some((_, info)) = self.sessions.remove(&connection_id) {
            if let Some(set) = self.tenant_index.get(&info.tenant_id) {
                set.remove(&connection_id);
                // Clean up empty tenant entries to avoid unbounded growth.
                if set.is_empty() {
                    drop(set);
                    // Re-check under removal to avoid race with concurrent register.
                    self.tenant_index
                        .remove_if(&info.tenant_id, |_, v| v.is_empty());
                }
            }
        }
    }

    /// Get an Arc reference to a session's info for state updates.
    pub fn get_session(&self, connection_id: i64) -> Option<std::sync::Arc<SessionInfo>> {
        self.sessions.get(&connection_id).map(|r| r.clone())
    }

    /// Get a snapshot of a single session.
    pub fn get(&self, connection_id: i64) -> Option<SessionSnapshot> {
        self.sessions
            .get(&connection_id)
            .map(|r| r.snapshot(&self.server_id))
    }

    /// List sessions matching the filter.
    /// List sessions matching the filter.
    ///
    /// The registry returns up to `filter.limit` results. Policy caps
    /// (e.g. `ALL_TENANTS_MAX_LIMIT`) are enforced by the control service
    /// layer, not here — keeping the registry a policy-free primitive.
    pub fn list(&self, filter: &SessionFilter) -> Vec<SessionSnapshot> {
        let now_mono = Instant::now();

        // If filtering by tenant, use the tenant index for efficiency.
        // Sort by connection_id for deterministic pagination order.
        if let Some(ref tid) = filter.tenant_id {
            let mut conn_ids: Vec<i64> = match self.tenant_index.get(tid) {
                Some(set) => set.iter().map(|r| *r).collect(),
                None => return Vec::new(),
            };
            conn_ids.sort_unstable();
            self.collect_snapshots(&conn_ids, filter, filter.limit, now_mono)
        } else if filter.all_tenants {
            let mut conn_ids: Vec<i64> = self.sessions.iter().map(|r| *r.key()).collect();
            conn_ids.sort_unstable();
            self.collect_snapshots(&conn_ids, filter, filter.limit, now_mono)
        } else {
            // Neither tenant_id nor all_tenants — return empty.
            Vec::new()
        }
    }

    fn collect_snapshots(
        &self,
        conn_ids: &[i64],
        filter: &SessionFilter,
        limit: usize,
        now_mono: Instant,
    ) -> Vec<SessionSnapshot> {
        let mut results = Vec::new();
        let mut skipped = 0usize;

        for &cid in conn_ids {
            let Some(entry) = self.sessions.get(&cid) else {
                continue;
            };
            let info = entry.value();

            // Apply filters.
            if let Some(ref principal) = filter.principal {
                if info.principal != *principal {
                    continue;
                }
            }
            if let Some(state) = filter.state {
                if info.state.load() != state {
                    continue;
                }
            }
            if let Some(min_dur_ms) = filter.min_duration_ms {
                let dur = now_mono.duration_since(info.connected_at_mono).as_millis() as i64;
                if dur < min_dur_ms {
                    continue;
                }
            }

            // Pagination offset.
            if skipped < filter.offset {
                skipped += 1;
                continue;
            }

            results.push(info.snapshot(&self.server_id));
            if results.len() >= limit {
                break;
            }
        }
        results
    }

    /// Cancel the current query on a connection. Connection stays open.
    pub fn cancel_query(&self, connection_id: i64) -> Result<(), CancelError> {
        let entry = self
            .sessions
            .get(&connection_id)
            .ok_or(CancelError::NotFound)?;
        let info = entry.value();
        let qc = info.query_cancel.read();
        match qc.as_ref() {
            Some(token) => {
                token.cancel();
                Ok(())
            }
            None => Err(CancelError::NoActiveQuery),
        }
    }

    /// Terminate a connection entirely.
    pub fn terminate(&self, connection_id: i64) -> Result<(), CancelError> {
        let entry = self
            .sessions
            .get(&connection_id)
            .ok_or(CancelError::NotFound)?;
        entry.value().cancel_token.cancel();
        Ok(())
    }

    /// Terminate all connections for a tenant.
    pub fn terminate_all(&self, tenant_id: &str) -> TerminateAllResult {
        let conn_ids: Vec<i64> = match self.tenant_index.get(tenant_id) {
            Some(set) => set.iter().map(|r| *r).collect(),
            None => {
                return TerminateAllResult {
                    requested: 0,
                    terminated: 0,
                    already_closed: 0,
                }
            }
        };

        let requested = conn_ids.len() as u32;
        let mut terminated = 0u32;
        let mut already_closed = 0u32;

        for cid in conn_ids {
            match self.sessions.get(&cid) {
                Some(entry) => {
                    entry.value().cancel_token.cancel();
                    terminated += 1;
                }
                None => {
                    already_closed += 1;
                }
            }
        }

        TerminateAllResult {
            requested,
            terminated,
            already_closed,
        }
    }

    /// List all connection IDs for a tenant (for pre-terminate audit snapshots).
    pub fn list_connection_ids_by_tenant(&self, tenant_id: &str) -> Vec<i64> {
        match self.tenant_index.get(tenant_id) {
            Some(set) => set.iter().map(|r| *r).collect(),
            None => Vec::new(),
        }
    }

    /// Total active session count.
    pub fn count(&self) -> usize {
        self.sessions.len()
    }

    /// Active sessions for one tenant.
    pub fn count_by_tenant(&self, tenant_id: &str) -> usize {
        self.tenant_index
            .get(tenant_id)
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Server identifier.
    pub fn server_id(&self) -> &str {
        &self.server_id
    }
}

// ---------------------------------------------------------------------------
// Global singleton
// ---------------------------------------------------------------------------

static SESSION_REGISTRY: OnceLock<SessionRegistry> = OnceLock::new();

pub fn global_session_registry() -> &'static SessionRegistry {
    SESSION_REGISTRY.get_or_init(SessionRegistry::new)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("POD_NAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// Find the largest byte index <= `index` that is a valid UTF-8 char boundary.
/// Avoids panicking when slicing multi-byte characters.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> SessionRegistry {
        SessionRegistry {
            sessions: DashMap::new(),
            tenant_index: DashMap::new(),
            server_id: "test-server".to_owned(),
        }
    }

    fn make_info(connection_id: i64, tenant_id: &str, principal: &str) -> SessionInfo {
        SessionInfo::new(
            connection_id,
            tenant_id.to_owned(),
            principal.to_owned(),
            "testdb".to_owned(),
            "127.0.0.1:5432".to_owned(),
            CancellationToken::new(),
        )
    }

    #[test]
    fn register_and_unregister() {
        let reg = make_registry();
        reg.register(make_info(1, "tenant_a", "alice"));
        reg.register(make_info(2, "tenant_a", "bob"));
        reg.register(make_info(3, "tenant_b", "carol"));

        assert_eq!(reg.count(), 3);
        assert_eq!(reg.count_by_tenant("tenant_a"), 2);
        assert_eq!(reg.count_by_tenant("tenant_b"), 1);

        reg.unregister(2);
        assert_eq!(reg.count(), 2);
        assert_eq!(reg.count_by_tenant("tenant_a"), 1);

        reg.unregister(3);
        assert_eq!(reg.count_by_tenant("tenant_b"), 0);
        // Tenant entry should be cleaned up.
        assert!(!reg.tenant_index.contains_key("tenant_b"));
    }

    #[test]
    fn unregister_nonexistent_is_noop() {
        let reg = make_registry();
        reg.unregister(999); // Should not panic.
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn get_snapshot() {
        let reg = make_registry();
        reg.register(make_info(1, "tenant_a", "alice"));

        let snap = reg.get(1).unwrap();
        assert_eq!(snap.connection_id, 1);
        assert_eq!(snap.tenant_id, "tenant_a");
        assert_eq!(snap.principal, "alice");
        assert_eq!(snap.database, "testdb");
        assert_eq!(snap.state, SessionState::Idle);
        assert_eq!(snap.server_id, "test-server");
        assert!(snap.current_query.is_empty());

        assert!(reg.get(999).is_none());
    }

    #[test]
    fn list_by_tenant() {
        let reg = make_registry();
        reg.register(make_info(1, "tenant_a", "alice"));
        reg.register(make_info(2, "tenant_a", "bob"));
        reg.register(make_info(3, "tenant_b", "carol"));

        let filter = SessionFilter {
            tenant_id: Some("tenant_a".to_owned()),
            ..Default::default()
        };
        let results = reg.list(&filter);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|s| s.tenant_id == "tenant_a"));

        // Listing without tenant_id or all_tenants returns empty.
        let empty_filter = SessionFilter::default();
        assert!(reg.list(&empty_filter).is_empty());
    }

    #[test]
    fn list_all_tenants_with_limit() {
        let reg = make_registry();
        for i in 0..10 {
            reg.register(make_info(i, &format!("t{}", i), "user"));
        }

        let filter = SessionFilter {
            all_tenants: true,
            limit: 5,
            ..Default::default()
        };
        let results = reg.list(&filter);
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn list_with_principal_filter() {
        let reg = make_registry();
        reg.register(make_info(1, "t", "alice"));
        reg.register(make_info(2, "t", "bob"));
        reg.register(make_info(3, "t", "alice"));

        let filter = SessionFilter {
            tenant_id: Some("t".to_owned()),
            principal: Some("alice".to_owned()),
            ..Default::default()
        };
        let results = reg.list(&filter);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|s| s.principal == "alice"));
    }

    #[test]
    fn list_with_state_filter() {
        let reg = make_registry();
        reg.register(make_info(1, "t", "alice"));
        reg.register(make_info(2, "t", "bob"));

        // Set session 1 to Active.
        if let Some(info) = reg.get_session(1) {
            info.state.store(SessionState::Active);
        }

        let filter = SessionFilter {
            tenant_id: Some("t".to_owned()),
            state: Some(SessionState::Active),
            ..Default::default()
        };
        let results = reg.list(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].connection_id, 1);
    }

    #[test]
    fn list_pagination() {
        let reg = make_registry();
        for i in 0..10 {
            reg.register(make_info(i, "t", "user"));
        }

        let filter = SessionFilter {
            tenant_id: Some("t".to_owned()),
            limit: 3,
            offset: 2,
            ..Default::default()
        };
        let results = reg.list(&filter);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn list_deterministic_order() {
        let reg = make_registry();
        // Register in non-sequential order.
        for &id in &[50, 10, 30, 20, 40] {
            reg.register(make_info(id, "t", "user"));
        }

        let filter = SessionFilter {
            tenant_id: Some("t".to_owned()),
            ..Default::default()
        };
        let results = reg.list(&filter);
        let ids: Vec<i64> = results.iter().map(|s| s.connection_id).collect();
        assert_eq!(
            ids,
            vec![10, 20, 30, 40, 50],
            "results must be sorted by connection_id"
        );

        // Pagination must also be deterministic.
        let page1 = SessionFilter {
            tenant_id: Some("t".to_owned()),
            limit: 2,
            offset: 0,
            ..Default::default()
        };
        let page2 = SessionFilter {
            tenant_id: Some("t".to_owned()),
            limit: 2,
            offset: 2,
            ..Default::default()
        };
        let r1: Vec<i64> = reg.list(&page1).iter().map(|s| s.connection_id).collect();
        let r2: Vec<i64> = reg.list(&page2).iter().map(|s| s.connection_id).collect();
        assert_eq!(r1, vec![10, 20]);
        assert_eq!(r2, vec![30, 40]);
    }

    #[test]
    fn cancel_query_success() {
        let reg = make_registry();
        let cancel_token = CancellationToken::new();
        reg.register(SessionInfo::new(
            1,
            "t".to_owned(),
            "alice".to_owned(),
            "db".to_owned(),
            "127.0.0.1:1".to_owned(),
            cancel_token.clone(),
        ));

        // Begin a query so there's a query_cancel token.
        let info = reg.get_session(1).unwrap();
        let query_cancel = info.begin_query("SELECT 1");

        // Cancel the query.
        assert!(reg.cancel_query(1).is_ok());
        assert!(query_cancel.is_cancelled());
        // Connection-level token should NOT be cancelled.
        assert!(!cancel_token.is_cancelled());
    }

    #[test]
    fn cancel_query_no_active_query() {
        let reg = make_registry();
        reg.register(make_info(1, "t", "alice"));
        assert_eq!(reg.cancel_query(1), Err(CancelError::NoActiveQuery));
    }

    #[test]
    fn cancel_query_not_found() {
        let reg = make_registry();
        assert_eq!(reg.cancel_query(999), Err(CancelError::NotFound));
    }

    #[test]
    fn terminate_success() {
        let reg = make_registry();
        let cancel_token = CancellationToken::new();
        reg.register(SessionInfo::new(
            1,
            "t".to_owned(),
            "alice".to_owned(),
            "db".to_owned(),
            "127.0.0.1:1".to_owned(),
            cancel_token.clone(),
        ));

        assert!(reg.terminate(1).is_ok());
        assert!(cancel_token.is_cancelled());
    }

    #[test]
    fn terminate_not_found() {
        let reg = make_registry();
        assert_eq!(reg.terminate(999), Err(CancelError::NotFound));
    }

    #[test]
    fn terminate_all_success() {
        let reg = make_registry();
        let tokens: Vec<CancellationToken> = (0..3)
            .map(|i| {
                let t = CancellationToken::new();
                reg.register(SessionInfo::new(
                    i,
                    "tenant_x".to_owned(),
                    "user".to_owned(),
                    "db".to_owned(),
                    "127.0.0.1:1".to_owned(),
                    t.clone(),
                ));
                t
            })
            .collect();
        // Add one from different tenant.
        let other = CancellationToken::new();
        reg.register(SessionInfo::new(
            99,
            "tenant_y".to_owned(),
            "user".to_owned(),
            "db".to_owned(),
            "127.0.0.1:1".to_owned(),
            other.clone(),
        ));

        let result = reg.terminate_all("tenant_x");
        assert_eq!(result.requested, 3);
        assert_eq!(result.terminated, 3);
        assert_eq!(result.already_closed, 0);
        assert!(tokens.iter().all(|t| t.is_cancelled()));
        // Other tenant should be unaffected.
        assert!(!other.is_cancelled());
    }

    #[test]
    fn terminate_all_empty_tenant() {
        let reg = make_registry();
        let result = reg.terminate_all("nonexistent");
        assert_eq!(result.requested, 0);
        assert_eq!(result.terminated, 0);
        assert_eq!(result.already_closed, 0);
    }

    #[test]
    fn begin_and_end_query() {
        let info = make_info(1, "t", "alice");
        let child = info.begin_query("SELECT * FROM big_table WHERE id > 100");

        assert_eq!(info.state.load(), SessionState::Active);
        assert!(info.query_start.load(Ordering::Relaxed) > 0);
        assert!(!child.is_cancelled());
        {
            let q = info.current_query.read();
            assert_eq!(*q, "SELECT * FROM big_table WHERE id > 100");
        }

        info.end_query(SessionState::Idle);
        assert_eq!(info.state.load(), SessionState::Idle);
        assert_eq!(info.query_start.load(Ordering::Relaxed), 0);
        {
            let q = info.current_query.read();
            assert!(q.is_empty());
        }
    }

    #[test]
    fn query_text_truncation() {
        let info = make_info(1, "t", "alice");
        let long_query = "x".repeat(2000);
        let _child = info.begin_query(&long_query);

        let q = info.current_query.read();
        assert_eq!(q.len(), MAX_QUERY_LEN);
    }

    #[test]
    fn query_text_truncation_multibyte_safe() {
        let info = make_info(1, "t", "alice");
        // Build a string of multi-byte characters that crosses the MAX_QUERY_LEN boundary.
        // Each '中' is 3 bytes, so 400 chars = 1200 bytes > 1024.
        let multibyte_query = "中".repeat(400);
        assert!(multibyte_query.len() > MAX_QUERY_LEN);
        // This must not panic.
        let _child = info.begin_query(&multibyte_query);

        let q = info.current_query.read();
        assert!(q.len() <= MAX_QUERY_LEN);
        // Must be valid UTF-8 (would fail to read if not).
        assert!(std::str::from_utf8(q.as_bytes()).is_ok());
    }

    #[test]
    fn cancel_idempotent() {
        let reg = make_registry();
        let cancel_token = CancellationToken::new();
        reg.register(SessionInfo::new(
            1,
            "t".to_owned(),
            "alice".to_owned(),
            "db".to_owned(),
            "127.0.0.1:1".to_owned(),
            cancel_token.clone(),
        ));

        // Terminate twice — second call should still succeed (idempotent).
        assert!(reg.terminate(1).is_ok());
        assert!(reg.terminate(1).is_ok());
        assert!(cancel_token.is_cancelled());
    }
}
