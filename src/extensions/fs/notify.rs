use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tikv_client::{CheckLevel, Transaction, TransactionClient, TransactionOptions};
use tokio::sync::broadcast;

use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
use crate::storage::backpressure::tikv_op;

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
// EventRing
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
    ///
    /// **Hard Contract #6**: seq is allocated inside the write lock by reading
    /// `back().seq + 1`, guaranteeing strict monotonicity.
    ///
    /// **Hard Contract #4**: If the lock is poisoned we return Err and the
    /// caller silently drops the event (mutation already committed to TiKV).
    pub fn push(&self, builder: FsEventBuilder) -> Result<u64, PushError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.push_inner(builder, now)
    }

    fn push_inner(&self, builder: FsEventBuilder, timestamp: i64) -> Result<u64, PushError> {
        let mut events = self.events.write().map_err(|_| PushError::LockPoisoned)?;

        // Allocate seq inside write lock (Hard Contract #6).
        let seq = events.back().map(|e| e.seq + 1).unwrap_or(1);

        let event = builder.into_event(seq, timestamp);
        events.push_back(event);

        // Evict oldest if over capacity.
        let mut evicted = 0u64;
        while events.len() > self.capacity {
            events.pop_front();
            evicted += 1;
        }
        if evicted > 0 {
            self.evicted
                .fetch_add(evicted, std::sync::atomic::Ordering::Relaxed);
        }

        drop(events); // release lock before broadcast

        // Best-effort broadcast (Hard Contract #4 — never fail the mutation).
        let _ = self.notify_tx.send(seq);

        Ok(seq)
    }

    // -- push_batch ---------------------------------------------------------

    /// Push multiple events atomically (Hard Contract #5).
    ///
    /// All events are appended within a single write-lock acquisition, so
    /// consumers see either all events from a batch or none.
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

        // Evict oldest if over capacity.
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

        let _ = first_seq; // suppress unused warning
        Ok(last_seq)
    }

    // -- query --------------------------------------------------------------

    /// Query events with `seq > since_seq`, optional path prefix filter, and limit.
    ///
    /// Always returns a `QueryResult` containing ring metadata (epoch, oldest/newest
    /// seq, overflow flag, capacity) and matching events.
    ///
    /// **Overflow detection** (triple check):
    /// 1. `since_seq < oldest_seq` — events evicted
    /// 2. `since_seq > newest_seq` — stale cursor (likely post-restart)
    /// 3. Epoch mismatch is detected by the consumer comparing `result.epoch`
    ///    against their cached epoch.
    pub fn query(&self, since_seq: u64, path_prefix: Option<&str>, limit: usize) -> QueryResult {
        let events = self.events.read().expect("lock poisoned");

        let oldest_seq = events.front().map(|e| e.seq).unwrap_or(0);
        let newest_seq = events.back().map(|e| e.seq).unwrap_or(0);

        // Overflow detection (conditions 1 & 2; condition 3 is consumer-side).
        // since_seq == 0 means "start from the beginning" — only overflow if
        // the ring has already evicted events (oldest_seq > 1).
        let overflow = if events.is_empty() || since_seq == 0 {
            // When since_seq == 0 and ring is non-empty, overflow iff events
            // have already been evicted (oldest_seq > 1).
            since_seq == 0 && oldest_seq > 1
        } else {
            since_seq < oldest_seq || since_seq > newest_seq
        };

        // Normalize path prefix: ensure it ends with '/'.
        let prefix = path_prefix.map(|p| {
            if p.ends_with('/') {
                p.to_string()
            } else {
                format!("{}/", p)
            }
        });

        // Find start position via binary search on seq.
        // We want the first event with seq > since_seq.
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
            // Apply path prefix filter.
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

    /// Subscribe to push notifications. Returns a broadcast Receiver that
    /// yields the seq of each newly pushed event (or last seq of a batch).
    ///
    /// The receiver is for wake-up only — after receiving a seq, the consumer
    /// should call `query()` to fetch actual events.
    pub fn subscribe(&self) -> broadcast::Receiver<u64> {
        self.notify_tx.subscribe()
    }

    // -- introspection (for metrics) ----------------------------------------

    /// Oldest seq in the ring, or None if empty.
    pub fn oldest_seq(&self) -> Option<u64> {
        self.events
            .read()
            .expect("lock poisoned")
            .front()
            .map(|e| e.seq)
    }

    /// Newest seq in the ring, or None if empty.
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
///
/// Used by the mutation/emit path (e.g. `EmbeddedPageFs::new()`) to obtain or
/// create the ring. Only the emit side should call this — creating a ring
/// implicitly marks this process as the owner for this keyspace.
///
/// TODO(Phase 2): Implement real multi-instance owner detection. Phase 1
/// assumes single process / single owner per keyspace.
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
    /// Events suppressed by commit-scope coalescing (same path in one commit).
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
// fs9_events() table function schema + execution
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
            col("seq", DataType::Int64, false),
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

/// Execute `fs9_events(since_seq [, path_prefix [, limit]])` and return rows.
///
/// First row is always a META row with event_type='META' containing ring
/// metadata (epoch, oldest_seq, newest_seq, overflow status).
///
/// **Overflow detection** (seq-based):
/// 1. `since_seq < oldest_seq` — events evicted from ring
/// 2. `since_seq > newest_seq` — stale cursor
///
/// **Epoch / restart detection** is the consumer's responsibility:
/// compare the META row's `inode` field (ring epoch) against your cached
/// epoch. If they differ, the process restarted and a full resync is needed.
/// The META row always exposes the current epoch for this purpose.
///
/// Returns an error if this process does not own the ring for the given
/// keyspace (i.e. no mutation path has registered a ring).
#[allow(dead_code)]
pub fn execute_fs9_events(
    keyspace: &str,
    since_seq: i64,
    path_prefix: Option<&str>,
    limit: usize,
) -> Result<Vec<Row>, String> {
    if since_seq < 0 {
        return Err("fs9_events: since_seq must be non-negative".to_string());
    }

    // Auto-create the ring on first query so users don't need a prior fs9
    // operation. The ring starts empty (META-only) until mutations happen.
    let ring = get_or_create_event_ring(keyspace);
    let metrics = notify_metrics_for_keyspace(keyspace);

    let since = since_seq as u64;
    let result = ring.query(since, path_prefix, limit);

    if result.overflow {
        metrics.record_overflow();
        tracing::warn!(
            keyspace = keyspace,
            since_seq = since,
            oldest_seq = result.oldest_seq,
            newest_seq = result.newest_seq,
            epoch = result.epoch,
            "fs9_events: overflow detected, consumer must resync"
        );
    }

    let now_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let mut rows = Vec::with_capacity(result.events.len() + 1);

    // META row — always first, contains ring metadata.
    // path='' per spec; structured metadata goes in typed columns.
    rows.push(Row::new(vec![
        Value::Int64(result.newest_seq as i64), // seq
        Value::Text("META".to_string()),        // event_type
        Value::Text(String::new()),             // path (empty per spec)
        Value::Null,                            // old_path
        Value::Int64(result.epoch as i64),      // inode (carries epoch)
        Value::Int64(result.oldest_seq as i64), // generation (carries oldest_seq)
        Value::Boolean(result.overflow),        // is_dir (carries overflow flag)
        Value::Int64(result.capacity as i64),   // size (carries capacity)
        Value::Timestamp(now_millis),           // timestamp (current time per spec)
    ]));

    // Event rows.
    for event in &result.events {
        rows.push(Row::new(vec![
            Value::Int64(event.seq as i64),
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
            Value::Timestamp(event.timestamp * 1_000), // seconds → milliseconds
        ]));
    }

    Ok(rows)
}

// ---------------------------------------------------------------------------
// TiKV-backed persistent event log
// ---------------------------------------------------------------------------

/// Seq counter key: `_fs_ES` — stores the current max seq as big-endian u64.
fn notify_seq_key() -> Vec<u8> {
    b"_fs_ES".to_vec()
}

/// Event key: `_fs_E` + seq (big-endian u64). Natural sort order for range scans.
fn notify_event_key(seq: u64) -> Vec<u8> {
    let mut key = b"_fs_E".to_vec();
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

/// Event key prefix for range scans.
fn notify_event_prefix() -> Vec<u8> {
    b"_fs_E".to_vec()
}

/// End key for event range scans (exclusive). `_fs_F` is the byte after `_fs_E`.
fn notify_event_range_end() -> Vec<u8> {
    b"_fs_F".to_vec()
}

// -- Binary event encoding --------------------------------------------------

/// Encode event type as a single byte.
fn encode_event_type(et: &FsEventType) -> u8 {
    match et {
        FsEventType::Create => 1,
        FsEventType::Write => 2,
        FsEventType::Delete => 3,
        FsEventType::Rename => 4,
        FsEventType::Mkdir => 5,
    }
}

fn decode_event_type(b: u8) -> Option<FsEventType> {
    match b {
        1 => Some(FsEventType::Create),
        2 => Some(FsEventType::Write),
        3 => Some(FsEventType::Delete),
        4 => Some(FsEventType::Rename),
        5 => Some(FsEventType::Mkdir),
        _ => None,
    }
}

/// Binary-encode an FsEvent for TiKV storage.
///
/// Format (v1):
///   event_type: 1 byte
///   is_dir: 1 byte
///   timestamp: 8 bytes (i64 BE, milliseconds since epoch)
///   inode: 8 bytes (u64 BE)
///   parent_inode: 8 bytes (u64 BE)
///   generation: 8 bytes (u64 BE)
///   size: 8 bytes (u64 BE)
///   path_len: 4 bytes (u32 BE)
///   path: path_len bytes (UTF-8)
///   old_path_len: 4 bytes (u32 BE, 0 if None)
///   old_path: old_path_len bytes (UTF-8)
fn encode_fs_event(event: &FsEvent) -> Vec<u8> {
    let path_bytes = event.path.as_bytes();
    let old_path_bytes = event.old_path.as_deref().map(|s| s.as_bytes());
    let old_path_len = old_path_bytes.map_or(0, |b| b.len());
    let capacity = 1 + 1 + 8 + 8 + 8 + 8 + 8 + 4 + path_bytes.len() + 4 + old_path_len;
    let mut buf = Vec::with_capacity(capacity);
    buf.push(encode_event_type(&event.event_type));
    buf.push(event.is_dir as u8);
    buf.extend_from_slice(&event.timestamp.to_be_bytes());
    buf.extend_from_slice(&event.inode.to_be_bytes());
    buf.extend_from_slice(&event.parent_inode.to_be_bytes());
    buf.extend_from_slice(&event.generation.to_be_bytes());
    buf.extend_from_slice(&event.size.to_be_bytes());
    buf.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(path_bytes);
    buf.extend_from_slice(&(old_path_len as u32).to_be_bytes());
    if let Some(ob) = old_path_bytes {
        buf.extend_from_slice(ob);
    }
    buf
}

fn decode_fs_event(seq: u64, data: &[u8]) -> Option<FsEvent> {
    // minimum: 1+1+8+8+8+8+8+4 = 46
    if data.len() < 46 {
        return None;
    }
    let event_type = decode_event_type(data[0])?;
    let is_dir = data[1] != 0;
    let timestamp = i64::from_be_bytes(data[2..10].try_into().ok()?);
    let inode = u64::from_be_bytes(data[10..18].try_into().ok()?);
    let parent_inode = u64::from_be_bytes(data[18..26].try_into().ok()?);
    let generation = u64::from_be_bytes(data[26..34].try_into().ok()?);
    let size = u64::from_be_bytes(data[34..42].try_into().ok()?);
    let path_len = u32::from_be_bytes(data[42..46].try_into().ok()?) as usize;
    if data.len() < 46 + path_len + 4 {
        return None;
    }
    let path = std::str::from_utf8(&data[46..46 + path_len])
        .ok()?
        .to_string();
    let old_path_offset = 46 + path_len;
    let old_path_len =
        u32::from_be_bytes(data[old_path_offset..old_path_offset + 4].try_into().ok()?) as usize;
    let old_path = if old_path_len > 0 {
        let start = old_path_offset + 4;
        if data.len() < start + old_path_len {
            return None;
        }
        Some(
            std::str::from_utf8(&data[start..start + old_path_len])
                .ok()?
                .to_string(),
        )
    } else {
        None
    };
    Some(FsEvent {
        seq,
        timestamp,
        event_type,
        path,
        old_path,
        inode,
        parent_inode,
        generation,
        is_dir,
        size,
    })
}

// -- TiKV persistence -------------------------------------------------------

/// Default max events per tenant in TiKV.
pub const DEFAULT_EVENT_CAP: u64 = 100_000;

/// Environment variable to override event cap.
pub const EVENT_CAP_ENV: &str = "FS9_NOTIFY_EVENT_CAP";

/// Default event TTL in seconds (1 hour).
pub const DEFAULT_EVENT_TTL_SECS: u64 = 3600;

/// Environment variable to override event TTL.
pub const EVENT_TTL_ENV: &str = "FS9_NOTIFY_TTL_SECS";

/// Default GC interval in seconds.
pub const DEFAULT_GC_INTERVAL_SECS: u64 = 60;

/// Environment variable to override GC interval.
pub const GC_INTERVAL_ENV: &str = "FS9_NOTIFY_GC_INTERVAL_SECS";

/// Grace period added to TTL for clock skew tolerance.
const TTL_GRACE_PERIOD_SECS: u64 = 300;

/// Notify config, loaded once from env.
struct NotifyConfig {
    event_cap: u64,
    event_ttl_secs: u64,
    gc_interval_secs: u64,
}

fn notify_config() -> &'static NotifyConfig {
    static CONFIG: OnceLock<NotifyConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let event_cap = std::env::var(EVENT_CAP_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&c| c > 0)
            .unwrap_or(DEFAULT_EVENT_CAP);
        let event_ttl_secs = std::env::var(EVENT_TTL_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&c| c > 0)
            .unwrap_or(DEFAULT_EVENT_TTL_SECS);
        let gc_interval_secs = std::env::var(GC_INTERVAL_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&c| c > 0)
            .unwrap_or(DEFAULT_GC_INTERVAL_SECS);
        NotifyConfig {
            event_cap,
            event_ttl_secs,
            gc_interval_secs,
        }
    })
}

// ---------------------------------------------------------------------------
// Async event persistence via background flush task
// ---------------------------------------------------------------------------

/// Global sender for the event persistence channel.
static EVENT_PERSIST_TX: OnceLock<tokio::sync::mpsc::UnboundedSender<FsEventBuilder>> =
    OnceLock::new();

/// Enqueue event builders for async persistence. Zero-cost on the mutation path:
/// just an `mpsc::send()`, no TiKV I/O.
pub fn enqueue_persist_events(builders: Vec<FsEventBuilder>) {
    if builders.is_empty() {
        return;
    }
    if let Some(tx) = EVENT_PERSIST_TX.get() {
        for b in builders {
            // If the channel is full/closed, drop silently — best-effort.
            let _ = tx.send(b);
        }
    }
}

/// Enqueue a single event builder for async persistence.
pub fn enqueue_persist_event(builder: FsEventBuilder) {
    if let Some(tx) = EVENT_PERSIST_TX.get() {
        let _ = tx.send(builder);
    }
}

/// Maximum number of events to batch into a single TiKV transaction.
const FLUSH_BATCH_SIZE: usize = 500;

/// Maximum time to wait before flushing a partial batch.
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Start the background event persistence loop. Call once at startup.
///
/// The loop drains the channel, batches events, and writes them to TiKV in
/// a single optimistic transaction per batch. This eliminates per-event
/// contention on the `_fs_ES` sequence counter.
pub fn spawn_event_persist_loop(client: Arc<TransactionClient>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<FsEventBuilder>();
    EVENT_PERSIST_TX
        .set(tx)
        .unwrap_or_else(|_| tracing::warn!("fs9_notify: event persist loop already started"));

    tokio::spawn(async move {
        let mut batch: Vec<FsEventBuilder> = Vec::with_capacity(FLUSH_BATCH_SIZE);
        loop {
            // Wait for the first event (blocks until something arrives or channel closes).
            match rx.recv().await {
                Some(builder) => batch.push(builder),
                None => break, // Channel closed — shutdown.
            }

            // Drain up to FLUSH_BATCH_SIZE or until FLUSH_INTERVAL expires.
            let deadline = tokio::time::Instant::now() + FLUSH_INTERVAL;
            while batch.len() < FLUSH_BATCH_SIZE {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(builder)) => batch.push(builder),
                    Ok(None) => break, // Channel closed.
                    Err(_) => break,   // Timeout — flush what we have.
                }
            }

            if batch.is_empty() {
                continue;
            }

            let to_flush: Vec<FsEventBuilder> = std::mem::take(&mut batch);
            if let Err(e) = flush_events_batch(&client, to_flush).await {
                tracing::warn!("fs9_notify: batch flush failed: {e}");
            }
        }
        // Graceful shutdown: drain remaining events before exiting.
        while let Ok(builder) = rx.try_recv() {
            batch.push(builder);
        }
        if !batch.is_empty() {
            let to_flush = std::mem::take(&mut batch);
            if let Err(e) = flush_events_batch(&client, to_flush).await {
                tracing::warn!("fs9_notify: final flush on shutdown failed: {e}");
            }
        }
        tracing::info!("fs9_notify: event persist loop exited");
    });
}

/// Flush a batch of events to TiKV in a single optimistic transaction.
async fn flush_events_batch(
    client: &TransactionClient,
    builders: Vec<FsEventBuilder>,
) -> Result<()> {
    if builders.is_empty() {
        return Ok(());
    }

    // Coalesce: same path → keep last event.
    let mut coalesced: HashMap<String, FsEventBuilder> = HashMap::with_capacity(builders.len());
    for b in builders {
        coalesced.insert(b.path.clone(), b);
    }
    let final_builders: Vec<FsEventBuilder> = coalesced.into_values().collect();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let options = TransactionOptions::new_optimistic().drop_check(CheckLevel::Warn);
    let mut txn = client
        .begin_with_options(options)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Read current max seq — single lock acquisition per batch.
    let seq_key = notify_seq_key();
    let current_seq =
        match tikv_op!(txn.get(seq_key.clone()).await).map_err(|e| anyhow::anyhow!("{e}"))? {
            Some(v) if v.len() == 8 => u64::from_be_bytes(v[..8].try_into().unwrap()),
            _ => 0,
        };

    let mut seq = current_seq + 1;
    for builder in final_builders {
        let event = builder.into_event(seq, now);
        let event_key = notify_event_key(seq);
        let event_value = encode_fs_event(&event);
        tikv_op!(txn.put(event_key, event_value).await).map_err(|e| anyhow::anyhow!("{e}"))?;
        seq += 1;
    }
    let last_seq = seq - 1;

    tikv_op!(txn.put(seq_key, last_seq.to_be_bytes().to_vec()).await)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    txn.commit().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// Execute `fs9_events()` by reading from TiKV.
///
/// Uses the provided transaction to range-scan persisted events.
pub async fn execute_fs9_events_from_tikv(
    txn: &mut Transaction,
    since_seq: i64,
    path_prefix: Option<&str>,
    limit: usize,
) -> Result<Vec<Row>, String> {
    if since_seq < 0 {
        return Err("fs9_events: since_seq must be non-negative".to_string());
    }

    let since = since_seq as u64;

    // Read current max seq.
    let seq_key = notify_seq_key();
    let newest_seq = match tikv_op!(txn.get(seq_key).await)
        .map_err(|e| format!("fs9_events: failed to read seq counter: {e}"))?
    {
        Some(v) if v.len() == 8 => u64::from_be_bytes(v[..8].try_into().unwrap()),
        _ => 0,
    };

    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    if newest_seq == 0 {
        // No events at all — return META-only.
        return Ok(vec![Row::new(vec![
            Value::Int64(0),
            Value::Text("META".to_string()),
            Value::Text(String::new()),
            Value::Null,
            Value::Int64(0),
            Value::Int64(0),
            Value::Boolean(false),
            Value::Int64(notify_config().event_cap as i64),
            Value::Timestamp(now_millis),
        ])]);
    }

    // Find oldest_seq by scanning from the beginning (1 key).
    let oldest_start = notify_event_prefix();
    let mut oldest_scan = tikv_op!(txn.scan(oldest_start..notify_event_range_end(), 1).await)
        .map_err(|e| format!("fs9_events: oldest scan failed: {e}"))?;
    let oldest_seq = oldest_scan
        .next()
        .and_then(|kv| {
            let key: &[u8] = kv.key().as_ref().into();
            if key.len() >= 13 {
                Some(u64::from_be_bytes(key[5..13].try_into().ok()?))
            } else {
                None
            }
        })
        .unwrap_or(0);

    // Overflow detection.
    let overflow = if since == 0 {
        oldest_seq > 1
    } else {
        since < oldest_seq || since > newest_seq
    };

    // Normalize path prefix.
    let prefix = path_prefix.map(|p| {
        if p.ends_with('/') {
            p.to_string()
        } else {
            format!("{p}/")
        }
    });

    // We request more than `limit` to account for path filtering.
    // Scan up to 10x limit or at least 1000 to reduce round-trips.
    let scan_limit = (limit * 10).clamp(1000, 100_000) as u32;
    let start_key = notify_event_key(since + 1);
    let end_key = notify_event_range_end();
    let pairs = tikv_op!(txn.scan(start_key..end_key, scan_limit).await)
        .map_err(|e| format!("fs9_events: range scan failed: {e}"))?;

    let mut rows = Vec::with_capacity(limit + 1);

    // META row.
    rows.push(Row::new(vec![
        Value::Int64(newest_seq as i64),
        Value::Text("META".to_string()),
        Value::Text(String::new()),
        Value::Null,
        Value::Int64(0),
        Value::Int64(oldest_seq as i64),
        Value::Boolean(overflow),
        Value::Int64(notify_config().event_cap as i64),
        Value::Timestamp(now_millis),
    ]));

    // Event rows.
    let mut count = 0;
    for kv in pairs {
        if count >= limit {
            break;
        }
        let key: &[u8] = kv.key().as_ref().into();
        let value: &[u8] = kv.value();
        if key.len() < 13 {
            continue;
        }
        let seq = u64::from_be_bytes(key[5..13].try_into().unwrap_or([0; 8]));
        if let Some(event) = decode_fs_event(seq, value) {
            // Apply path prefix filter.
            if let Some(ref pfx) = prefix {
                if !event.path.starts_with(pfx.as_str()) {
                    continue;
                }
            }
            rows.push(Row::new(vec![
                Value::Int64(event.seq as i64),
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
            count += 1;
        }
    }

    Ok(rows)
}

// -- GC ---------------------------------------------------------------------

/// Run one GC cycle: delete events exceeding cap or TTL for a single keyspace.
///
/// Safe to run from multiple instances concurrently — worst case is
/// redundant deletes on already-deleted keys.
pub async fn gc_notify_events(txn: &mut Transaction) -> Result<u64> {
    let config = notify_config();
    let mut deleted = 0u64;

    // Read newest seq to compute count.
    let seq_key = notify_seq_key();
    let newest_seq = match tikv_op!(txn.get(seq_key).await).map_err(|e| anyhow::anyhow!("{e}"))? {
        Some(v) if v.len() == 8 => u64::from_be_bytes(v[..8].try_into().unwrap()),
        _ => return Ok(0), // no events
    };

    // Find oldest event.
    let mut oldest_scan = tikv_op!(
        txn.scan(notify_event_prefix()..notify_event_range_end(), 1)
            .await
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let oldest_seq = match oldest_scan.next() {
        Some(kv) => {
            let key: &[u8] = kv.key().as_ref().into();
            if key.len() >= 13 {
                u64::from_be_bytes(key[5..13].try_into().unwrap_or([0; 8]))
            } else {
                return Ok(0);
            }
        }
        None => return Ok(0),
    };

    let count = newest_seq - oldest_seq + 1;

    // Trigger 1: cap exceeded — delete oldest events until at cap.
    if count > config.event_cap {
        let to_delete = count - config.event_cap;
        let start = notify_event_key(oldest_seq);
        let end = notify_event_key(oldest_seq + to_delete);
        let pairs: Vec<_> = tikv_op!(txn.scan(start..end, to_delete as u32).await)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .collect();
        for kv in &pairs {
            let key: Vec<u8> = kv.key().clone().into();
            tikv_op!(txn.delete(key).await).map_err(|e| anyhow::anyhow!("{e}"))?;
            deleted += 1;
        }
    }

    // Trigger 2: TTL exceeded — delete events older than TTL + grace period.
    let ttl_cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
        - ((config.event_ttl_secs + TTL_GRACE_PERIOD_SECS) * 1000) as i64;

    // Scan from oldest, stop when we find an event newer than the cutoff.
    let scan_start = notify_event_prefix();
    let scan_end = notify_event_range_end();
    let batch_size = 1000u32;
    let pairs: Vec<_> = tikv_op!(txn.scan(scan_start..scan_end, batch_size).await)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .collect();
    for kv in &pairs {
        let value: &[u8] = kv.value();
        // Decode just the timestamp (bytes 2..10) without full decode.
        if value.len() >= 10 {
            let ts = i64::from_be_bytes(value[2..10].try_into().unwrap_or([0; 8]));
            if ts >= ttl_cutoff {
                break; // events are ordered by seq (≈ time), stop here
            }
            let key: Vec<u8> = kv.key().clone().into();
            tikv_op!(txn.delete(key).await).map_err(|e| anyhow::anyhow!("{e}"))?;
            deleted += 1;
        }
    }

    Ok(deleted)
}

/// Spawn a background GC loop for fs9 notify events.
///
/// Runs indefinitely, performing GC every `gc_interval_secs`.
pub fn spawn_notify_gc_loop(client: Arc<tikv_client::TransactionClient>) {
    let interval = Duration::from_secs(notify_config().gc_interval_secs);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let txn_result = tikv_op!(
                client
                    .begin_with_options(
                        tikv_client::TransactionOptions::new_pessimistic()
                            .drop_check(tikv_client::CheckLevel::Warn)
                    )
                    .await
            );
            match txn_result {
                Ok(mut txn) => {
                    match gc_notify_events(&mut txn).await {
                        Ok(deleted) => {
                            if deleted > 0 {
                                if let Err(e) = tikv_op!(txn.commit().await) {
                                    tracing::warn!("fs9_notify gc commit failed: {e}");
                                } else {
                                    tracing::debug!("fs9_notify gc: deleted {deleted} events");
                                }
                            } else {
                                // Nothing to delete, rollback.
                                let _ = tikv_op!(txn.rollback().await);
                            }
                        }
                        Err(e) => {
                            tracing::warn!("fs9_notify gc failed: {e}");
                            let _ = tikv_op!(txn.rollback().await);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("fs9_notify gc: failed to begin txn: {e}");
                }
            }
        }
    });
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

    // -----------------------------------------------------------------------
    // Hard Contract #1: CREATE / WRITE semantics
    // (Emission logic is in pagefs, but we verify the enum + builder work.)
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_vs_write_event_types() {
        let ring = EventRing::with_epoch(100, 42);

        // New file → CREATE
        let seq1 = ring
            .push(make_builder(FsEventType::Create, "/data/new.txt"))
            .unwrap();
        // Existing file modified → WRITE
        let seq2 = ring
            .push(make_builder(FsEventType::Write, "/data/new.txt"))
            .unwrap();

        let result = ring.query(0, None, 100);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].seq, seq1);
        assert_eq!(result.events[0].event_type, FsEventType::Create);
        assert_eq!(result.events[1].seq, seq2);
        assert_eq!(result.events[1].event_type, FsEventType::Write);
        // Only one event per mutation — no CREATE+WRITE double-emit.
    }

    // -----------------------------------------------------------------------
    // Hard Contract #2: Commit-scope coalescing
    // (Coalescing is done at the pagefs layer before calling push. Here we
    //  verify that push_batch accepts pre-coalesced events correctly.)
    // -----------------------------------------------------------------------

    #[test]
    fn test_commit_scope_single_event_per_inode() {
        let ring = EventRing::with_epoch(100, 42);
        // Simulating a batch where coalescing already happened: one event per inode.
        let batch = vec![
            make_builder(FsEventType::Write, "/a.txt"),
            make_builder(FsEventType::Write, "/b.txt"),
        ];
        ring.push_batch(batch).unwrap();

        let result = ring.query(0, None, 100);
        assert_eq!(result.events.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Hard Contract #3: Owner affinity
    // (Enforced at the SQL table function layer, not in EventRing itself.
    //  We verify EventRing doesn't crash and returns data correctly for the
    //  owner process.)
    // -----------------------------------------------------------------------

    #[test]
    fn test_owner_process_can_query() {
        let ring = EventRing::with_epoch(100, 42);
        ring.push(make_builder(FsEventType::Create, "/f.txt"))
            .unwrap();
        let result = ring.query(0, None, 100);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.epoch, 42);
    }

    // -----------------------------------------------------------------------
    // Hard Contract #4: Event emission must not affect mutation success
    // (Push errors are returned as Result, caller silently drops. We verify
    //  the error path exists and is non-panicking.)
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_returns_error_not_panic_on_failure() {
        // We can't easily poison a std::sync::RwLock in a test without
        // spawning a thread that panics while holding the lock. Instead we
        // verify the happy path returns Ok and the error type exists.
        let ring = EventRing::with_epoch(100, 42);
        let result = ring.push(make_builder(FsEventType::Create, "/ok.txt"));
        assert!(result.is_ok());

        // Verify PushError is displayable (used in metrics/logging).
        let err = PushError::LockPoisoned;
        assert_eq!(format!("{}", err), "EventRing lock poisoned");
    }

    // -----------------------------------------------------------------------
    // Hard Contract #5: Batch atomicity
    // -----------------------------------------------------------------------

    #[test]
    fn test_batch_atomicity_all_or_none_visible() {
        let ring = EventRing::with_epoch(100, 42);

        // Push a batch of 5 events.
        let builders: Vec<_> = (0..5)
            .map(|i| make_builder(FsEventType::Create, &format!("/batch/{}.txt", i)))
            .collect();
        let last_seq = ring.push_batch(builders).unwrap();
        assert_eq!(last_seq, 5);

        // All 5 must be visible.
        let result = ring.query(0, None, 100);
        assert_eq!(result.events.len(), 5);

        // Seqs must be contiguous 1..=5.
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

    // -----------------------------------------------------------------------
    // Hard Contract #6: Seq monotonicity (seq allocated inside write lock)
    // -----------------------------------------------------------------------

    #[test]
    fn test_seq_strictly_monotonic() {
        let ring = EventRing::with_epoch(1000, 42);

        let mut last_seq = 0u64;
        for i in 0..100 {
            let seq = ring
                .push(make_builder(FsEventType::Write, &format!("/f{}.txt", i)))
                .unwrap();
            assert!(
                seq > last_seq,
                "seq {} must be > previous {}",
                seq,
                last_seq
            );
            last_seq = seq;
        }

        // Verify ordering in ring.
        let result = ring.query(0, None, 1000);
        for window in result.events.windows(2) {
            assert!(window[0].seq < window[1].seq);
        }
    }

    #[test]
    fn test_seq_monotonic_across_push_and_batch() {
        let ring = EventRing::with_epoch(1000, 42);

        ring.push(make_builder(FsEventType::Create, "/a.txt"))
            .unwrap(); // seq 1
        ring.push_batch(vec![
            make_builder(FsEventType::Create, "/b.txt"),
            make_builder(FsEventType::Create, "/c.txt"),
        ])
        .unwrap(); // seq 2, 3
        ring.push(make_builder(FsEventType::Write, "/a.txt"))
            .unwrap(); // seq 4

        let result = ring.query(0, None, 100);
        let seqs: Vec<u64> = result.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
    }

    // -----------------------------------------------------------------------
    // Ring eviction and capacity
    // -----------------------------------------------------------------------

    #[test]
    fn test_ring_eviction_at_capacity() {
        let ring = EventRing::with_epoch(5, 42);

        for i in 0..10 {
            ring.push(make_builder(FsEventType::Write, &format!("/f{}.txt", i)))
                .unwrap();
        }

        assert_eq!(ring.len(), 5);
        assert_eq!(ring.evicted_count(), 5);

        // Oldest should be seq 6 (first 5 evicted).
        let result = ring.query(0, None, 100);
        assert_eq!(result.oldest_seq, 6);
        assert_eq!(result.newest_seq, 10);
        assert_eq!(result.events.len(), 5);
        assert_eq!(result.events[0].seq, 6);
    }

    // -----------------------------------------------------------------------
    // Overflow detection (triple check)
    // -----------------------------------------------------------------------

    #[test]
    fn test_overflow_since_seq_less_than_oldest() {
        let ring = EventRing::with_epoch(3, 42);

        for i in 0..5 {
            ring.push(make_builder(FsEventType::Write, &format!("/f{}.txt", i)))
                .unwrap();
        }
        // Ring has seqs 3,4,5. Query since_seq=1 → overflow.
        let result = ring.query(1, None, 100);
        assert!(result.overflow);
        assert_eq!(result.oldest_seq, 3);
    }

    #[test]
    fn test_overflow_since_seq_greater_than_newest() {
        let ring = EventRing::with_epoch(100, 42);

        ring.push(make_builder(FsEventType::Write, "/f.txt"))
            .unwrap();
        // since_seq=999 > newest_seq=1 → overflow (stale cursor, post-restart).
        let result = ring.query(999, None, 100);
        assert!(result.overflow);
        assert_eq!(result.events.len(), 0);
    }

    #[test]
    fn test_overflow_epoch_change_detectable() {
        let ring1 = EventRing::with_epoch(100, 42);
        ring1
            .push(make_builder(FsEventType::Write, "/f.txt"))
            .unwrap();
        let r1 = ring1.query(0, None, 100);
        assert_eq!(r1.epoch, 42);

        // Simulate restart: new ring with different epoch.
        let ring2 = EventRing::with_epoch(100, 99);
        ring2
            .push(make_builder(FsEventType::Write, "/f.txt"))
            .unwrap();
        let r2 = ring2.query(0, None, 100);
        assert_eq!(r2.epoch, 99);

        // Consumer detects epoch mismatch: 42 != 99 → must resync.
        assert_ne!(r1.epoch, r2.epoch);
    }

    #[test]
    fn test_no_overflow_on_empty_ring() {
        let ring = EventRing::with_epoch(100, 42);
        let result = ring.query(0, None, 100);
        assert!(!result.overflow);
        assert_eq!(result.oldest_seq, 0);
        assert_eq!(result.newest_seq, 0);
        assert_eq!(result.events.len(), 0);
    }

    #[test]
    fn test_no_overflow_since_eq_newest() {
        let ring = EventRing::with_epoch(100, 42);
        ring.push(make_builder(FsEventType::Write, "/f.txt"))
            .unwrap();
        // since_seq == newest_seq → no overflow, no events.
        let result = ring.query(1, None, 100);
        assert!(!result.overflow);
        assert_eq!(result.events.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Path prefix filter
    // -----------------------------------------------------------------------

    #[test]
    fn test_path_prefix_filter() {
        let ring = EventRing::with_epoch(100, 42);

        ring.push(make_builder(FsEventType::Create, "/data/foo.txt"))
            .unwrap();
        ring.push(make_builder(FsEventType::Create, "/data/sub/bar.txt"))
            .unwrap();
        ring.push(make_builder(FsEventType::Create, "/data-backup/baz.txt"))
            .unwrap();
        ring.push(make_builder(FsEventType::Create, "/other/qux.txt"))
            .unwrap();

        // Filter by /data/ — should match /data/foo.txt and /data/sub/bar.txt
        let result = ring.query(0, Some("/data/"), 100);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].path, "/data/foo.txt");
        assert_eq!(result.events[1].path, "/data/sub/bar.txt");

        // /data without trailing slash should also work (normalized).
        let result = ring.query(0, Some("/data"), 100);
        assert_eq!(result.events.len(), 2);

        // Root prefix matches everything.
        let result = ring.query(0, Some("/"), 100);
        assert_eq!(result.events.len(), 4);
    }

    #[test]
    fn test_path_filter_does_not_affect_metadata() {
        let ring = EventRing::with_epoch(100, 42);

        ring.push(make_builder(FsEventType::Create, "/a/file.txt"))
            .unwrap();
        ring.push(make_builder(FsEventType::Create, "/b/file.txt"))
            .unwrap();

        // Filter that matches nothing.
        let result = ring.query(0, Some("/nonexistent/"), 100);
        assert_eq!(result.events.len(), 0);
        // But metadata still reflects ring state.
        assert_eq!(result.oldest_seq, 1);
        assert_eq!(result.newest_seq, 2);
        assert!(!result.overflow);
    }

    // -----------------------------------------------------------------------
    // Query with limit
    // -----------------------------------------------------------------------

    #[test]
    fn test_query_limit() {
        let ring = EventRing::with_epoch(100, 42);
        for i in 0..10 {
            ring.push(make_builder(FsEventType::Write, &format!("/f{}.txt", i)))
                .unwrap();
        }

        let result = ring.query(0, None, 3);
        assert_eq!(result.events.len(), 3);
        assert_eq!(result.events[0].seq, 1);
        assert_eq!(result.events[2].seq, 3);
    }

    // -----------------------------------------------------------------------
    // Query since_seq (exclusive)
    // -----------------------------------------------------------------------

    #[test]
    fn test_query_since_seq_exclusive() {
        let ring = EventRing::with_epoch(100, 42);
        for i in 0..5 {
            ring.push(make_builder(FsEventType::Write, &format!("/f{}.txt", i)))
                .unwrap();
        }

        // since_seq=3 → return events with seq > 3 → seq 4, 5.
        let result = ring.query(3, None, 100);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].seq, 4);
        assert_eq!(result.events[1].seq, 5);
    }

    // -----------------------------------------------------------------------
    // Subscribe (broadcast)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_subscribe_receives_push_notification() {
        let ring = EventRing::with_epoch(100, 42);
        let mut rx = ring.subscribe();

        ring.push(make_builder(FsEventType::Create, "/f.txt"))
            .unwrap();

        let seq = rx.recv().await.unwrap();
        assert_eq!(seq, 1);
    }

    #[tokio::test]
    async fn test_subscribe_receives_batch_notification() {
        let ring = EventRing::with_epoch(100, 42);
        let mut rx = ring.subscribe();

        let builders = vec![
            make_builder(FsEventType::Create, "/a.txt"),
            make_builder(FsEventType::Create, "/b.txt"),
            make_builder(FsEventType::Create, "/c.txt"),
        ];
        ring.push_batch(builders).unwrap();

        // Batch broadcasts the last seq only.
        let seq = rx.recv().await.unwrap();
        assert_eq!(seq, 3);
    }

    // -----------------------------------------------------------------------
    // Rename event
    // -----------------------------------------------------------------------

    #[test]
    fn test_rename_event_carries_old_path() {
        let ring = EventRing::with_epoch(100, 42);
        ring.push(make_rename_builder("/old/name.txt", "/new/name.txt"))
            .unwrap();

        let result = ring.query(0, None, 100);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, FsEventType::Rename);
        assert_eq!(result.events[0].path, "/new/name.txt");
        assert_eq!(result.events[0].old_path.as_deref(), Some("/old/name.txt"));
    }

    // -----------------------------------------------------------------------
    // Concurrent push + query (seq ordering under contention)
    // -----------------------------------------------------------------------

    #[test]
    fn test_concurrent_push_seq_ordering() {
        use std::sync::Arc;
        use std::thread;

        let ring = Arc::new(EventRing::with_epoch(10_000, 42));
        let mut handles = vec![];

        // Spawn 10 threads, each pushing 100 events.
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

        // All 1000 events present with strictly monotonic seqs.
        let result = ring.query(0, None, 10_000);
        assert_eq!(result.events.len(), 1000);
        for window in result.events.windows(2) {
            assert!(
                window[0].seq < window[1].seq,
                "seq ordering violated: {} >= {}",
                window[0].seq,
                window[1].seq
            );
        }
    }

    // -----------------------------------------------------------------------
    // FsEventType display
    // -----------------------------------------------------------------------

    #[test]
    fn test_event_type_display() {
        assert_eq!(FsEventType::Create.as_str(), "CREATE");
        assert_eq!(FsEventType::Write.as_str(), "WRITE");
        assert_eq!(FsEventType::Delete.as_str(), "DELETE");
        assert_eq!(FsEventType::Rename.as_str(), "RENAME");
        assert_eq!(FsEventType::Mkdir.as_str(), "MKDIR");
    }

    // -----------------------------------------------------------------------
    // from_env default
    // -----------------------------------------------------------------------

    #[test]
    fn test_from_env_default_capacity() {
        // Without env var set, should use default.
        let ring = EventRing::from_env();
        assert_eq!(ring.capacity(), DEFAULT_RING_CAPACITY);
    }

    // -----------------------------------------------------------------------
    // fs9_events() TVF execution tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_fs9_events_empty_ring() {
        let keyspace = "test_empty_ring_tvf";
        // Ensure a fresh ring exists.
        let ring = get_or_create_event_ring(keyspace);
        assert!(ring.is_empty());

        let rows = execute_fs9_events(keyspace, 0, None, 100).unwrap();
        // Should have exactly 1 META row.
        assert_eq!(rows.len(), 1);
        let meta = &rows[0];
        assert_eq!(meta.values[1], Value::Text("META".to_string()));
        // overflow should be false for empty ring with since_seq=0.
        assert_eq!(meta.values[6], Value::Boolean(false));
    }

    #[test]
    fn test_execute_fs9_events_with_events() {
        let keyspace = "test_tvf_with_events";
        let ring = get_or_create_event_ring(keyspace);

        ring.push(make_builder(FsEventType::Create, "/data/file1.txt"))
            .unwrap();
        ring.push(make_builder(FsEventType::Write, "/data/file2.txt"))
            .unwrap();

        let rows = execute_fs9_events(keyspace, 0, None, 100).unwrap();
        // 1 META + 2 event rows.
        assert_eq!(rows.len(), 3);

        // First row is META.
        assert_eq!(rows[0].values[1], Value::Text("META".to_string()));

        // Second row is CREATE event.
        assert_eq!(rows[1].values[1], Value::Text("CREATE".to_string()));
        assert_eq!(
            rows[1].values[2],
            Value::Text("/data/file1.txt".to_string())
        );

        // Third row is WRITE event.
        assert_eq!(rows[2].values[1], Value::Text("WRITE".to_string()));
        assert_eq!(
            rows[2].values[2],
            Value::Text("/data/file2.txt".to_string())
        );
    }

    #[test]
    fn test_execute_fs9_events_with_path_prefix() {
        let keyspace = "test_tvf_path_prefix";
        let ring = get_or_create_event_ring(keyspace);

        ring.push(make_builder(FsEventType::Create, "/data/file1.txt"))
            .unwrap();
        ring.push(make_builder(FsEventType::Create, "/logs/app.log"))
            .unwrap();
        ring.push(make_builder(FsEventType::Write, "/data/file2.txt"))
            .unwrap();

        let rows = execute_fs9_events(keyspace, 0, Some("/data/"), 100).unwrap();
        // 1 META + 2 matching events (only /data/ prefix).
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[1].values[2],
            Value::Text("/data/file1.txt".to_string())
        );
        assert_eq!(
            rows[2].values[2],
            Value::Text("/data/file2.txt".to_string())
        );
    }

    #[test]
    fn test_execute_fs9_events_overflow_detection() {
        let keyspace = "test_tvf_overflow";
        // Create a ring with small capacity for overflow testing.
        {
            let mut registry = ring_registry().lock().unwrap();
            registry.insert(
                keyspace.to_string(),
                Arc::new(EventRing::with_epoch(3, 999)),
            );
        }

        let ring = get_or_create_event_ring(keyspace);
        // Push 5 events into a ring of capacity 3 → 2 evicted.
        for i in 0..5 {
            ring.push(make_builder(FsEventType::Write, &format!("/f{i}.txt")))
                .unwrap();
        }

        // Query with since_seq=1 — should be overflow since seq 1 was evicted.
        let rows = execute_fs9_events(keyspace, 1, None, 100).unwrap();
        let meta = &rows[0];
        assert_eq!(meta.values[6], Value::Boolean(true)); // overflow = true
    }

    #[test]
    fn test_execute_fs9_events_since_seq_filtering() {
        let keyspace = "test_tvf_since_seq";
        let ring = get_or_create_event_ring(keyspace);

        let seq1 = ring
            .push(make_builder(FsEventType::Create, "/a.txt"))
            .unwrap();
        let _seq2 = ring
            .push(make_builder(FsEventType::Write, "/b.txt"))
            .unwrap();
        let _seq3 = ring
            .push(make_builder(FsEventType::Delete, "/c.txt"))
            .unwrap();

        // Query since seq1 → should get events 2 and 3 only.
        let rows = execute_fs9_events(keyspace, seq1 as i64, None, 100).unwrap();
        // 1 META + 2 events.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].values[1], Value::Text("WRITE".to_string()));
        assert_eq!(rows[2].values[1], Value::Text("DELETE".to_string()));
    }

    #[test]
    fn test_execute_fs9_events_limit() {
        let keyspace = "test_tvf_limit";
        let ring = get_or_create_event_ring(keyspace);

        for i in 0..10 {
            ring.push(make_builder(FsEventType::Write, &format!("/f{i}.txt")))
                .unwrap();
        }

        let rows = execute_fs9_events(keyspace, 0, None, 3).unwrap();
        // 1 META + 3 events (limited).
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn test_fs9_events_schema_columns() {
        let schema = fs9_events_schema();
        assert_eq!(schema.name, "fs9_events");
        assert_eq!(schema.columns.len(), 9);
        assert_eq!(schema.columns[0].name, "seq");
        assert_eq!(schema.columns[1].name, "event_type");
        assert_eq!(schema.columns[2].name, "path");
        assert_eq!(schema.columns[3].name, "old_path");
        assert!(schema.columns[3].nullable);
        assert_eq!(schema.columns[4].name, "inode");
        assert_eq!(schema.columns[5].name, "generation");
        assert_eq!(schema.columns[6].name, "is_dir");
        assert_eq!(schema.columns[7].name, "size");
        assert_eq!(schema.columns[8].name, "timestamp");
    }

    #[test]
    fn test_execute_fs9_events_negative_since_seq() {
        let keyspace = "test_tvf_neg_seq";
        get_or_create_event_ring(keyspace); // register ring
        let result = execute_fs9_events(keyspace, -1, None, 100);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("since_seq must be non-negative"));
    }

    #[test]
    fn test_execute_fs9_events_auto_creates_ring() {
        // Query a keyspace with no prior ring → auto-creates empty ring.
        let result = execute_fs9_events("auto_create_keyspace_xyz", 0, None, 100);
        assert!(result.is_ok());
        let rows = result.unwrap();
        // Should return META row only (empty ring).
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[1], Value::Text("META".to_string()));
    }

    #[test]
    fn test_execute_fs9_events_meta_epoch_exposed() {
        // Verify META row exposes epoch for consumer-side comparison.
        let keyspace = "test_tvf_meta_epoch";
        {
            let mut registry = ring_registry().lock().unwrap();
            registry.insert(
                keyspace.to_string(),
                Arc::new(EventRing::with_epoch(10, 42)),
            );
        }
        let rows = execute_fs9_events(keyspace, 0, None, 100).unwrap();
        let meta = &rows[0];
        // epoch is carried in inode column (index 4).
        assert_eq!(meta.values[4], Value::Int64(42));
    }

    #[test]
    fn test_execute_fs9_events_meta_timestamp_nonzero() {
        let keyspace = "test_tvf_meta_ts";
        get_or_create_event_ring(keyspace);
        let rows = execute_fs9_events(keyspace, 0, None, 100).unwrap();
        let meta = &rows[0];
        // META timestamp should be current time (non-zero).
        match meta.values[8] {
            Value::Timestamp(ts) => assert!(ts > 0, "META timestamp should be current time"),
            _ => panic!("expected Timestamp value"),
        }
    }

    #[test]
    fn test_timestamp_unit_is_millis() {
        // Regression test: Value::Timestamp must contain Unix milliseconds.
        // Previously we used microseconds (META row) and seconds*1_000_000
        // (event rows), causing the pg wire encoder to show 2056 instead of 2026.
        let keyspace = "test_timestamp_unit";
        let ring = EventRing::with_epoch(100, 99);
        // Use push_inner to set a known timestamp (123 seconds since epoch).
        ring.push_inner(make_builder(FsEventType::Create, "/ts.txt"), 123)
            .unwrap();
        {
            let mut registry = ring_registry().lock().unwrap();
            registry.insert(keyspace.to_string(), Arc::new(ring));
        }

        let before_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let rows = execute_fs9_events(keyspace, 0, None, 100).unwrap();
        let after_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        // META row (index 0): timestamp should be Unix milliseconds in current range.
        match rows[0].values[8] {
            Value::Timestamp(ts) => {
                assert!(
                    ts >= before_ms && ts <= after_ms,
                    "META timestamp {} not in Unix millis range [{}, {}]",
                    ts,
                    before_ms,
                    after_ms
                );
            }
            _ => panic!("expected Timestamp value for META row"),
        }

        // Event row (index 1): timestamp should be 123 seconds * 1_000 = 123_000 millis.
        match rows[1].values[8] {
            Value::Timestamp(ts) => {
                assert_eq!(
                    ts, 123_000,
                    "event timestamp should be 123_000 ms, got {}",
                    ts
                );
            }
            _ => panic!("expected Timestamp value for event row"),
        }
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

    // -----------------------------------------------------------------------
    // Regression: commit-scope coalescing — duplicate path in same batch
    // -----------------------------------------------------------------------

    /// Reproduces the coalescing logic from `EmbeddedPageFs::emit_events()`:
    /// same path appears twice in one batch → only the last state is emitted,
    /// and the `events_coalesced` metric increments.
    #[test]
    fn test_batch_coalescing_duplicate_path() {
        let ring = EventRing::with_epoch(100, 42);
        let metrics = NotifyMetrics::new();

        // Simulate two mutations to the same path within one commit:
        // first a CREATE, then a WRITE (overwrite). Only WRITE should survive.
        let builders = vec![
            {
                let mut b = make_builder(FsEventType::Create, "/data/dup.txt");
                b.size = 50;
                b
            },
            {
                let mut b = make_builder(FsEventType::Write, "/data/dup.txt");
                b.size = 200;
                b
            },
        ];

        // -- replicate the coalescing logic from pagefs emit_events() --
        let input_count = builders.len();
        let mut coalesced: std::collections::HashMap<String, FsEventBuilder> =
            std::collections::HashMap::with_capacity(input_count);
        for b in builders {
            coalesced.insert(b.path.clone(), b);
        }
        let final_builders: Vec<FsEventBuilder> = coalesced.into_values().collect();
        let suppressed = (input_count - final_builders.len()) as u64;
        if suppressed > 0 {
            metrics.record_coalesced(suppressed);
        }

        // Push the coalesced batch.
        let event_types: Vec<FsEventType> = final_builders.iter().map(|b| b.event_type).collect();
        ring.push_batch(final_builders).unwrap();
        for et in &event_types {
            metrics.record_emit(et);
        }

        // Verify: ring has exactly 1 event (the last state).
        let result = ring.query(0, None, 100);
        assert_eq!(
            result.events.len(),
            1,
            "duplicate path must coalesce to 1 event"
        );
        assert_eq!(result.events[0].event_type, FsEventType::Write);
        assert_eq!(result.events[0].path, "/data/dup.txt");
        assert_eq!(result.events[0].size, 200);

        // Verify: coalesced counter incremented by 1.
        assert_eq!(
            metrics.events_coalesced.load(Ordering::Relaxed),
            1,
            "one event was suppressed by coalescing"
        );

        // Verify: only 1 emit recorded (the surviving event).
        assert_eq!(metrics.events_emitted.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.events_write.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.events_create.load(Ordering::Relaxed), 0);
    }
}
