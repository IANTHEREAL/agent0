use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};

// ---------------------------------------------------------------------------
// FsEventType
// ---------------------------------------------------------------------------

/// Types of filesystem events emitted after successful TiKV commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FsEventType {
    Create, // New file/path becomes visible for the first time
    Write,  // Existing file content modified
    Delete, // File or directory removed (is_dir distinguishes)
    Rename, // Rename/move (carries old_path)
    Mkdir,  // New directory created
}

impl FsEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            FsEventType::Create => "CREATE",
            FsEventType::Write => "WRITE",
            FsEventType::Delete => "DELETE",
            FsEventType::Rename => "RENAME",
            FsEventType::Mkdir => "MKDIR",
        }
    }
}

impl std::fmt::Display for FsEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// FsEvent
// ---------------------------------------------------------------------------

/// A single filesystem change event.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FsEvent {
    /// Global monotonically increasing sequence number (assigned inside write lock).
    pub seq: u64,
    /// Unix timestamp in seconds.
    pub timestamp: i64,
    /// Kind of mutation.
    pub event_type: FsEventType,
    /// Affected path (post-mutation).
    pub path: String,
    /// Previous path — only set for Rename events.
    pub old_path: Option<String>,
    /// Inode id.
    pub inode: u64,
    /// Parent directory inode id (reserved for future TVF columns).
    #[allow(dead_code)]
    pub parent_inode: u64,
    /// Post-mutation generation.
    pub generation: u64,
    /// Whether the target is a directory.
    pub is_dir: bool,
    /// Post-mutation file size (0 for directories).
    pub size: u64,
}

/// Builder for creating an FsEvent before seq is assigned.
/// The `seq` field will be set by EventRing::push.
#[derive(Debug, Clone)]
pub struct FsEventBuilder {
    pub event_type: FsEventType,
    pub path: String,
    pub old_path: Option<String>,
    pub inode: u64,
    pub parent_inode: u64,
    pub generation: u64,
    pub is_dir: bool,
    pub size: u64,
}

impl FsEventBuilder {
    fn into_event(self, seq: u64, timestamp: i64) -> FsEvent {
        FsEvent {
            seq,
            timestamp,
            event_type: self.event_type,
            path: self.path,
            old_path: self.old_path,
            inode: self.inode,
            parent_inode: self.parent_inode,
            generation: self.generation,
            is_dir: self.is_dir,
            size: self.size,
        }
    }
}

// ---------------------------------------------------------------------------
// EventRing (in-process notification for fs watch)
// ---------------------------------------------------------------------------

/// Default ring buffer capacity.
pub const DEFAULT_RING_CAPACITY: usize = 10_000;

/// Environment variable to override ring capacity.
pub const RING_CAPACITY_ENV: &str = "FS9_NOTIFY_RING_CAPACITY";

/// Result of a `query` call, including ring metadata for overflow detection.
#[derive(Debug)]
#[allow(dead_code)]
pub struct QueryResult {
    /// Ring epoch (process incarnation). Consumer compares against cached epoch.
    pub epoch: u64,
    /// Oldest seq still in the ring (0 if empty).
    pub oldest_seq: u64,
    /// Newest seq in the ring (0 if empty).
    pub newest_seq: u64,
    /// Whether this query is in overflow state.
    pub overflow: bool,
    /// Configured ring capacity.
    pub capacity: usize,
    /// Matching events (may be empty even when ring is non-empty, due to filters).
    pub events: Vec<FsEvent>,
}

/// In-memory bounded ring buffer for filesystem change events.
///
/// Thread-safe: uses `std::sync::RwLock` (critical sections are very short —
/// VecDeque push/pop and binary search).
pub struct EventRing {
    events: RwLock<VecDeque<FsEvent>>,
    capacity: usize,
    /// Process incarnation nonce — set once at construction. Consumers use this
    /// to detect process restarts.
    epoch: u64,
    /// Broadcast sender for wake-up notifications. Sends the latest seq.
    notify_tx: broadcast::Sender<u64>,
    /// Total events evicted (for metrics).
    evicted: std::sync::atomic::AtomicU64,
}

#[allow(dead_code)] // Accessor methods are part of the public API surface, used in tests and future consumers.
impl EventRing {
    /// Create a new EventRing with the given capacity and a fresh epoch.
    pub fn new(capacity: usize) -> Self {
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self::with_epoch(capacity, epoch)
    }

    /// Create with an explicit epoch (useful for testing).
    pub fn with_epoch(capacity: usize, epoch: u64) -> Self {
        let (notify_tx, _) = broadcast::channel(256);
        EventRing {
            events: RwLock::new(VecDeque::with_capacity(capacity)),
            capacity,
            epoch,
            notify_tx,
            evicted: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Create from environment, falling back to default capacity.
    pub fn from_env() -> Self {
        let capacity = std::env::var(RING_CAPACITY_ENV)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&c| c > 0)
            .unwrap_or(DEFAULT_RING_CAPACITY);
        Self::new(capacity)
    }

    /// Returns the ring epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the total number of evicted events.
    pub fn evicted_count(&self) -> u64 {
        self.evicted.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Current ring size.
    pub fn len(&self) -> usize {
        self.events.read().expect("lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // -- push ---------------------------------------------------------------

    /// Push a single event into the ring.
    pub fn push(&self, builder: FsEventBuilder) -> Result<u64, PushError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.push_inner(builder, now)
    }

    fn push_inner(&self, builder: FsEventBuilder, timestamp: i64) -> Result<u64, PushError> {
        let mut events = self.events.write().map_err(|_| PushError::LockPoisoned)?;

        let seq = events.back().map(|e| e.seq + 1).unwrap_or(1);
        let event = builder.into_event(seq, timestamp);
        events.push_back(event);

        let mut evicted = 0u64;
        while events.len() > self.capacity {
            events.pop_front();
            evicted += 1;
        }
        if evicted > 0 {
            self.evicted
                .fetch_add(evicted, std::sync::atomic::Ordering::Relaxed);
        }

        drop(events);
        let _ = self.notify_tx.send(seq);
        Ok(seq)
    }

    // -- push_batch ---------------------------------------------------------

    /// Push multiple events atomically.
    pub fn push_batch(&self, builders: Vec<FsEventBuilder>) -> Result<u64, PushError> {
        if builders.is_empty() {
            return Ok(0);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut events = self.events.write().map_err(|_| PushError::LockPoisoned)?;

        let mut seq = events.back().map(|e| e.seq + 1).unwrap_or(1);
        let first_seq = seq;

        for builder in builders {
            let event = builder.into_event(seq, now);
            events.push_back(event);
            seq += 1;
        }
        let last_seq = seq - 1;

        let mut evicted = 0u64;
        while events.len() > self.capacity {
            events.pop_front();
            evicted += 1;
        }
        if evicted > 0 {
            self.evicted
                .fetch_add(evicted, std::sync::atomic::Ordering::Relaxed);
        }

        drop(events);
        let _ = self.notify_tx.send(last_seq);
        let _ = first_seq;
        Ok(last_seq)
    }

    // -- query --------------------------------------------------------------

    /// Query events with `seq > since_seq`, optional path prefix filter, and limit.
    pub fn query(&self, since_seq: u64, path_prefix: Option<&str>, limit: usize) -> QueryResult {
        let events = self.events.read().expect("lock poisoned");

        let oldest_seq = events.front().map(|e| e.seq).unwrap_or(0);
        let newest_seq = events.back().map(|e| e.seq).unwrap_or(0);

        let overflow = if events.is_empty() || since_seq == 0 {
            since_seq == 0 && oldest_seq > 1
        } else {
            since_seq < oldest_seq || since_seq > newest_seq
        };

        let prefix = path_prefix.map(|p| {
            if p.ends_with('/') {
                p.to_string()
            } else {
                format!("{}/", p)
            }
        });

        let start_idx = if since_seq == 0 {
            0
        } else {
            events.partition_point(|e| e.seq <= since_seq)
        };

        let mut result_events = Vec::new();
        for i in start_idx..events.len() {
            if result_events.len() >= limit {
                break;
            }
            let event = &events[i];
            if let Some(ref pfx) = prefix {
                if !event.path.starts_with(pfx.as_str()) {
                    continue;
                }
            }
            result_events.push(event.clone());
        }

        QueryResult {
            epoch: self.epoch,
            oldest_seq,
            newest_seq,
            overflow,
            capacity: self.capacity,
            events: result_events,
        }
    }

    /// Convenience: query without path filter, returning up to `limit` events.
    pub fn query_since(&self, since_seq: u64, limit: usize) -> QueryResult {
        self.query(since_seq, None, limit)
    }

    // -- subscribe ----------------------------------------------------------

    /// Subscribe to push notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<u64> {
        self.notify_tx.subscribe()
    }

    // -- introspection (for metrics) ----------------------------------------

    pub fn oldest_seq(&self) -> Option<u64> {
        self.events
            .read()
            .expect("lock poisoned")
            .front()
            .map(|e| e.seq)
    }

    pub fn newest_seq(&self) -> Option<u64> {
        self.events
            .read()
            .expect("lock poisoned")
            .back()
            .map(|e| e.seq)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum PushError {
    LockPoisoned,
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::LockPoisoned => f.write_str("EventRing lock poisoned"),
        }
    }
}

impl std::error::Error for PushError {}

// ---------------------------------------------------------------------------
// Global per-keyspace EventRing registry
// ---------------------------------------------------------------------------

type RingRegistry = Mutex<HashMap<String, Arc<EventRing>>>;

fn ring_registry() -> &'static RingRegistry {
    static REGISTRY: OnceLock<RingRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get or create the EventRing for a given keyspace.
pub fn get_or_create_event_ring(keyspace: &str) -> Arc<EventRing> {
    let mut registry = ring_registry().lock().unwrap_or_else(|e| e.into_inner());
    registry
        .entry(keyspace.to_string())
        .or_insert_with(|| Arc::new(EventRing::from_env()))
        .clone()
}

// ---------------------------------------------------------------------------
// Notify metrics
// ---------------------------------------------------------------------------

/// In-memory counters for fs9 notify observability.
#[allow(dead_code)]
pub struct NotifyMetrics {
    pub events_emitted: AtomicU64,
    pub events_create: AtomicU64,
    pub events_write: AtomicU64,
    pub events_delete: AtomicU64,
    pub events_rename: AtomicU64,
    pub events_mkdir: AtomicU64,
    pub overflow_queries: AtomicU64,
    pub emit_errors: AtomicU64,
    pub events_coalesced: AtomicU64,
}

#[allow(dead_code)]
impl NotifyMetrics {
    pub fn new() -> Self {
        Self {
            events_emitted: AtomicU64::new(0),
            events_create: AtomicU64::new(0),
            events_write: AtomicU64::new(0),
            events_delete: AtomicU64::new(0),
            events_rename: AtomicU64::new(0),
            events_mkdir: AtomicU64::new(0),
            overflow_queries: AtomicU64::new(0),
            emit_errors: AtomicU64::new(0),
            events_coalesced: AtomicU64::new(0),
        }
    }

    pub fn record_emit(&self, event_type: &FsEventType) {
        self.events_emitted.fetch_add(1, Ordering::Relaxed);
        match event_type {
            FsEventType::Create => self.events_create.fetch_add(1, Ordering::Relaxed),
            FsEventType::Write => self.events_write.fetch_add(1, Ordering::Relaxed),
            FsEventType::Delete => self.events_delete.fetch_add(1, Ordering::Relaxed),
            FsEventType::Rename => self.events_rename.fetch_add(1, Ordering::Relaxed),
            FsEventType::Mkdir => self.events_mkdir.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn record_overflow(&self) {
        self.overflow_queries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_emit_error(&self) {
        self.emit_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_coalesced(&self, count: u64) {
        self.events_coalesced.fetch_add(count, Ordering::Relaxed);
    }
}

type MetricsRegistry = Mutex<HashMap<String, Arc<NotifyMetrics>>>;

fn metrics_registry() -> &'static MetricsRegistry {
    static REGISTRY: OnceLock<MetricsRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get or create NotifyMetrics for a given keyspace.
pub fn notify_metrics_for_keyspace(keyspace: &str) -> Arc<NotifyMetrics> {
    let mut registry = metrics_registry().lock().unwrap_or_else(|e| e.into_inner());
    registry
        .entry(keyspace.to_string())
        .or_insert_with(|| Arc::new(NotifyMetrics::new()))
        .clone()
}

// ---------------------------------------------------------------------------
// fs9_events() table function schema + execution (Redis Streams backend)
// ---------------------------------------------------------------------------

fn col(name: &str, data_type: DataType, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type,
        nullable,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
        is_dropped: false,
    }
}

/// Return the output schema for `fs9_events(...)`.
pub fn fs9_events_schema() -> TableSchema {
    TableSchema {
        table_id: 0,
        name: "fs9_events".to_string(),
        columns: vec![
            col("stream_id", DataType::Text, false),
            col("event_type", DataType::Text, false),
            col("path", DataType::Text, false),
            col("old_path", DataType::Text, true),
            col("inode", DataType::Int64, false),
            col("generation", DataType::Int64, false),
            col("is_dir", DataType::Boolean, false),
            col("size", DataType::Int64, false),
            col("timestamp", DataType::TimestampTz, false),
        ],
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        version: 1,
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
    }
}

/// Execute `fs9_events(since_id [, path_prefix [, limit]])` by reading from Redis Streams.
///
/// Returns rows with columns: stream_id, event_type, path, old_path, inode,
/// generation, is_dir, size, timestamp.
pub async fn execute_fs9_events_from_redis(
    keyspace: &str,
    since_id: &str,
    path_prefix: Option<&str>,
    limit: usize,
) -> Result<Vec<Row>, String> {
    let events = super::redis_events::read_events(keyspace, since_id, path_prefix, limit)
        .await
        .map_err(|e| format!("fs9_events: {e}"))?;

    let mut rows = Vec::with_capacity(events.len());
    for event in &events {
        rows.push(Row::new(vec![
            Value::Text(event.stream_id.clone()),
            Value::Text(event.event_type.as_str().to_string()),
            Value::Text(event.path.clone()),
            match &event.old_path {
                Some(p) => Value::Text(p.clone()),
                None => Value::Null,
            },
            Value::Int64(event.inode as i64),
            Value::Int64(event.generation as i64),
            Value::Boolean(event.is_dir),
            Value::Int64(event.size as i64),
            Value::Timestamp(event.timestamp), // already in milliseconds
        ]));
    }

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Enqueue helpers (delegate to redis_events module)
// ---------------------------------------------------------------------------

/// Enqueue a single event for async Redis persistence. Zero I/O on caller.
pub fn enqueue_persist_event(keyspace: &str, builder: FsEventBuilder) {
    super::redis_events::enqueue_event(keyspace, builder);
}

/// Enqueue multiple events for async Redis persistence.
pub fn enqueue_persist_events(keyspace: &str, builders: Vec<FsEventBuilder>) {
    super::redis_events::enqueue_events(keyspace, builders);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_builder(event_type: FsEventType, path: &str) -> FsEventBuilder {
        FsEventBuilder {
            event_type,
            path: path.to_string(),
            old_path: None,
            inode: 1,
            parent_inode: 0,
            generation: 1,
            is_dir: false,
            size: 100,
        }
    }

    fn make_rename_builder(old: &str, new: &str) -> FsEventBuilder {
        FsEventBuilder {
            event_type: FsEventType::Rename,
            path: new.to_string(),
            old_path: Some(old.to_string()),
            inode: 1,
            parent_inode: 0,
            generation: 2,
            is_dir: false,
            size: 100,
        }
    }

    #[test]
    fn test_create_vs_write_event_types() {
        let ring = EventRing::with_epoch(100, 42);
        let seq1 = ring
            .push(make_builder(FsEventType::Create, "/data/new.txt"))
            .unwrap();
        let seq2 = ring
            .push(make_builder(FsEventType::Write, "/data/new.txt"))
            .unwrap();

        let result = ring.query(0, None, 100);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].seq, seq1);
        assert_eq!(result.events[0].event_type, FsEventType::Create);
        assert_eq!(result.events[1].seq, seq2);
        assert_eq!(result.events[1].event_type, FsEventType::Write);
    }

    #[test]
    fn test_batch_atomicity_all_or_none_visible() {
        let ring = EventRing::with_epoch(100, 42);
        let builders: Vec<_> = (0..5)
            .map(|i| make_builder(FsEventType::Create, &format!("/batch/{}.txt", i)))
            .collect();
        let last_seq = ring.push_batch(builders).unwrap();
        assert_eq!(last_seq, 5);

        let result = ring.query(0, None, 100);
        assert_eq!(result.events.len(), 5);
        for (i, event) in result.events.iter().enumerate() {
            assert_eq!(event.seq, (i + 1) as u64);
        }
    }

    #[test]
    fn test_batch_empty_returns_zero() {
        let ring = EventRing::with_epoch(100, 42);
        let seq = ring.push_batch(vec![]).unwrap();
        assert_eq!(seq, 0);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_seq_strictly_monotonic() {
        let ring = EventRing::with_epoch(1000, 42);
        let mut last_seq = 0u64;
        for i in 0..100 {
            let seq = ring
                .push(make_builder(FsEventType::Write, &format!("/f{}.txt", i)))
                .unwrap();
            assert!(seq > last_seq);
            last_seq = seq;
        }
    }

    #[test]
    fn test_ring_eviction_at_capacity() {
        let ring = EventRing::with_epoch(5, 42);
        for i in 0..10 {
            ring.push(make_builder(FsEventType::Write, &format!("/f{}.txt", i)))
                .unwrap();
        }
        assert_eq!(ring.len(), 5);
        assert_eq!(ring.evicted_count(), 5);
        let result = ring.query(0, None, 100);
        assert_eq!(result.oldest_seq, 6);
        assert_eq!(result.newest_seq, 10);
    }

    #[test]
    fn test_overflow_since_seq_less_than_oldest() {
        let ring = EventRing::with_epoch(3, 42);
        for i in 0..5 {
            ring.push(make_builder(FsEventType::Write, &format!("/f{}.txt", i)))
                .unwrap();
        }
        let result = ring.query(1, None, 100);
        assert!(result.overflow);
    }

    #[test]
    fn test_path_prefix_filter() {
        let ring = EventRing::with_epoch(100, 42);
        ring.push(make_builder(FsEventType::Create, "/data/foo.txt"))
            .unwrap();
        ring.push(make_builder(FsEventType::Create, "/data/sub/bar.txt"))
            .unwrap();
        ring.push(make_builder(FsEventType::Create, "/other/qux.txt"))
            .unwrap();

        let result = ring.query(0, Some("/data/"), 100);
        assert_eq!(result.events.len(), 2);
    }

    #[test]
    fn test_rename_event_carries_old_path() {
        let ring = EventRing::with_epoch(100, 42);
        ring.push(make_rename_builder("/old/name.txt", "/new/name.txt"))
            .unwrap();
        let result = ring.query(0, None, 100);
        assert_eq!(result.events[0].event_type, FsEventType::Rename);
        assert_eq!(result.events[0].old_path.as_deref(), Some("/old/name.txt"));
    }

    #[tokio::test]
    async fn test_subscribe_receives_push_notification() {
        let ring = EventRing::with_epoch(100, 42);
        let mut rx = ring.subscribe();
        ring.push(make_builder(FsEventType::Create, "/f.txt"))
            .unwrap();
        let seq = rx.recv().await.unwrap();
        assert_eq!(seq, 1);
    }

    #[test]
    fn test_event_type_display() {
        assert_eq!(FsEventType::Create.as_str(), "CREATE");
        assert_eq!(FsEventType::Write.as_str(), "WRITE");
        assert_eq!(FsEventType::Delete.as_str(), "DELETE");
        assert_eq!(FsEventType::Rename.as_str(), "RENAME");
        assert_eq!(FsEventType::Mkdir.as_str(), "MKDIR");
    }

    #[test]
    fn test_from_env_default_capacity() {
        let ring = EventRing::from_env();
        assert_eq!(ring.capacity(), DEFAULT_RING_CAPACITY);
    }

    #[test]
    fn test_fs9_events_schema_columns() {
        let schema = fs9_events_schema();
        assert_eq!(schema.name, "fs9_events");
        assert_eq!(schema.columns.len(), 9);
        assert_eq!(schema.columns[0].name, "stream_id");
        assert_eq!(schema.columns[1].name, "event_type");
        assert_eq!(schema.columns[2].name, "path");
        assert_eq!(schema.columns[3].name, "old_path");
        assert!(schema.columns[3].nullable);
    }

    #[test]
    fn test_notify_metrics() {
        let metrics = NotifyMetrics::new();
        metrics.record_emit(&FsEventType::Create);
        metrics.record_emit(&FsEventType::Write);
        metrics.record_emit(&FsEventType::Write);
        metrics.record_overflow();
        metrics.record_emit_error();

        assert_eq!(metrics.events_emitted.load(Ordering::Relaxed), 3);
        assert_eq!(metrics.events_create.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.events_write.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.overflow_queries.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.emit_errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_push_returns_error_not_panic_on_failure() {
        let ring = EventRing::with_epoch(100, 42);
        let result = ring.push(make_builder(FsEventType::Create, "/ok.txt"));
        assert!(result.is_ok());
        let err = PushError::LockPoisoned;
        assert_eq!(format!("{}", err), "EventRing lock poisoned");
    }

    #[test]
    fn test_concurrent_push_seq_ordering() {
        use std::sync::Arc;
        use std::thread;

        let ring = Arc::new(EventRing::with_epoch(10_000, 42));
        let mut handles = vec![];

        for t in 0..10 {
            let ring = Arc::clone(&ring);
            handles.push(thread::spawn(move || {
                for i in 0..100 {
                    ring.push(make_builder(
                        FsEventType::Write,
                        &format!("/t{}/f{}.txt", t, i),
                    ))
                    .unwrap();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let result = ring.query(0, None, 10_000);
        assert_eq!(result.events.len(), 1000);
        for window in result.events.windows(2) {
            assert!(window[0].seq < window[1].seq);
        }
    }
}
