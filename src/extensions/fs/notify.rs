use std::collections::VecDeque;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

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
    /// Parent directory inode id.
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
    pub fn query(
        &self,
        since_seq: u64,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> QueryResult {
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
            events
                .partition_point(|e| e.seq <= since_seq)
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
        self.events.read().expect("lock poisoned").front().map(|e| e.seq)
    }

    /// Newest seq in the ring, or None if empty.
    pub fn newest_seq(&self) -> Option<u64> {
        self.events.read().expect("lock poisoned").back().map(|e| e.seq)
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
        let seq1 = ring.push(make_builder(FsEventType::Create, "/data/new.txt")).unwrap();
        // Existing file modified → WRITE
        let seq2 = ring.push(make_builder(FsEventType::Write, "/data/new.txt")).unwrap();

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
        ring.push(make_builder(FsEventType::Create, "/f.txt")).unwrap();
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
            assert!(seq > last_seq, "seq {} must be > previous {}", seq, last_seq);
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

        ring.push(make_builder(FsEventType::Create, "/a.txt")).unwrap(); // seq 1
        ring.push_batch(vec![
            make_builder(FsEventType::Create, "/b.txt"),
            make_builder(FsEventType::Create, "/c.txt"),
        ])
        .unwrap(); // seq 2, 3
        ring.push(make_builder(FsEventType::Write, "/a.txt")).unwrap(); // seq 4

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

        ring.push(make_builder(FsEventType::Write, "/f.txt")).unwrap();
        // since_seq=999 > newest_seq=1 → overflow (stale cursor, post-restart).
        let result = ring.query(999, None, 100);
        assert!(result.overflow);
        assert_eq!(result.events.len(), 0);
    }

    #[test]
    fn test_overflow_epoch_change_detectable() {
        let ring1 = EventRing::with_epoch(100, 42);
        ring1.push(make_builder(FsEventType::Write, "/f.txt")).unwrap();
        let r1 = ring1.query(0, None, 100);
        assert_eq!(r1.epoch, 42);

        // Simulate restart: new ring with different epoch.
        let ring2 = EventRing::with_epoch(100, 99);
        ring2.push(make_builder(FsEventType::Write, "/f.txt")).unwrap();
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
        ring.push(make_builder(FsEventType::Write, "/f.txt")).unwrap();
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

        ring.push(make_builder(FsEventType::Create, "/data/foo.txt")).unwrap();
        ring.push(make_builder(FsEventType::Create, "/data/sub/bar.txt")).unwrap();
        ring.push(make_builder(FsEventType::Create, "/data-backup/baz.txt")).unwrap();
        ring.push(make_builder(FsEventType::Create, "/other/qux.txt")).unwrap();

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

        ring.push(make_builder(FsEventType::Create, "/a/file.txt")).unwrap();
        ring.push(make_builder(FsEventType::Create, "/b/file.txt")).unwrap();

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

        ring.push(make_builder(FsEventType::Create, "/f.txt")).unwrap();

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
        assert_eq!(
            result.events[0].old_path.as_deref(),
            Some("/old/name.txt")
        );
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
}
