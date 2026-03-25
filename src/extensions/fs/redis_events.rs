//! Redis Streams backend for fs9 event persistence.
//!
//! Architecture:
//! - Write path: mutation commit → channel send (zero I/O) → background loop
//!   batches events and pipelines XADD to Redis (1 RTT per batch).
//! - Read path: `XRANGE` across hourly segments for `fs9_events()` TVF.
//! - Stream key: `fs9_events:{keyspace}:{hour}` per tenant per hour.
//!   Hour segment = UTC `%Y%m%d%H` (e.g., `2026032423`).
//! - Auto-cap: `MAXLEN ~50000` on each XADD per segment.
//! - Segment TTL: 4 hours after last write (consumer catch-up window).
//! - Schema version field `v:1` for future evolution.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Result};
use redis::aio::MultiplexedConnection;
use redis::Client;
use tokio::sync::mpsc;

use super::notify::{FsEventBuilder, FsEventType};

/// Maximum stream length per segment (approximate trim).
/// 50K entries ≈ 15MB per segment, still under Redis big key threshold.
/// Higher than per-tenant cap because each segment only covers 1 hour;
/// high-traffic tenants (e.g., bulk fs cp) may burst within an hour.
const STREAM_MAXLEN: usize = 50_000;

/// Maximum events per pipeline batch.
const BATCH_SIZE: usize = 500;

/// Maximum time to wait before flushing a partial batch.
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// TTL for each hourly segment (4 hours — covers consumer catch-up window).
const SEGMENT_TTL_SECS: u64 = 14400;

/// Global Redis client (set once at startup).
static REDIS_CLIENT: OnceLock<Client> = OnceLock::new();

/// Global sender for the event persistence channel.
static EVENT_TX: OnceLock<mpsc::UnboundedSender<RedisEvent>> = OnceLock::new();

/// An event ready to be written to Redis.
struct RedisEvent {
    keyspace: String,
    builder: FsEventBuilder,
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initialize the global Redis client from `REDIS_URL`.
///
/// Validates connectivity with a PING — fails fast if Redis is unreachable.
pub async fn init_redis_client() -> Result<()> {
    let url = std::env::var("REDIS_URL")
        .map_err(|_| anyhow!("REDIS_URL environment variable is required"))?;
    let client =
        Client::open(url.as_str()).map_err(|e| anyhow!("failed to create Redis client: {e}"))?;

    // Fail fast: verify connectivity with PING before accepting the client.
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .map_err(|e| anyhow!("Redis connection failed: {e}"))?;
    redis::cmd("PING")
        .query_async::<String>(&mut conn)
        .await
        .map_err(|e| anyhow!("Redis PING failed: {e}"))?;

    REDIS_CLIENT
        .set(client)
        .map_err(|_| anyhow!("Redis client already initialized"))?;
    Ok(())
}

/// Get an async multiplexed connection from the global client.
pub async fn get_connection() -> Result<MultiplexedConnection> {
    let client = REDIS_CLIENT
        .get()
        .ok_or_else(|| anyhow!("Redis client not initialized"))?;
    client
        .get_multiplexed_async_connection()
        .await
        .map_err(|e| anyhow!("Redis connection failed: {e}"))
}

/// Stream key for a given keyspace and hour segment.
fn stream_key(keyspace: &str, hour: &str) -> String {
    format!("fs9_events:{keyspace}:{hour}")
}

/// Current hour segment string in UTC (e.g., "2026032423").
fn current_hour_segment() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as YYYYMMDDHH from unix timestamp.
    let secs = now as i64;
    let days = secs / 86400;
    let rem = secs % 86400;
    let hour = rem / 3600;

    // Convert days since epoch to YYYYMMDD.
    // Simple calendar calculation from unix days.
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}{m:02}{d:02}{hour:02}")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    // Algorithm from Howard Hinnant's chrono-compatible date library.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// Extract the hour segment from a Redis Stream ID (ms-timestamp based).
/// Stream ID format: "{ms_timestamp}-{seq}" e.g., "1711324800000-0".
/// Returns the hour segment string, or None if parsing fails.
fn hour_from_stream_id(stream_id: &str) -> Option<String> {
    let ms_str = stream_id.split('-').next()?;
    let ms: u64 = ms_str.parse().ok()?;
    let secs = (ms / 1000) as i64;
    let days = secs / 86400;
    let rem = secs % 86400;
    let hour = rem / 3600;
    let (y, m, d) = days_to_ymd(days);
    Some(format!("{y:04}{m:02}{d:02}{hour:02}"))
}

/// Generate hour segments between `start_hour` and `end_hour` (inclusive).
/// Both are in "YYYYMMDDHH" format.
fn hour_range(start_hour: &str, end_hour: &str) -> Vec<String> {
    // Parse start and end as timestamps, iterate by hour.
    let start_ts = hour_to_epoch_secs(start_hour);
    let end_ts = hour_to_epoch_secs(end_hour);
    if start_ts.is_none() || end_ts.is_none() {
        return vec![end_hour.to_string()];
    }
    let start = start_ts.unwrap();
    let end = end_ts.unwrap();

    let mut hours = Vec::new();
    let mut ts = start;
    // Safety: cap at 48 hours to prevent runaway loops.
    while ts <= end && hours.len() < 48 {
        let days = ts / 86400;
        let rem = ts % 86400;
        let h = rem / 3600;
        let (y, m, d) = days_to_ymd(days);
        hours.push(format!("{y:04}{m:02}{d:02}{h:02}"));
        ts += 3600;
    }
    if hours.is_empty() {
        hours.push(end_hour.to_string());
    }
    hours
}

/// Parse "YYYYMMDDHH" to epoch seconds (start of that hour).
fn hour_to_epoch_secs(hour_str: &str) -> Option<i64> {
    if hour_str.len() != 10 {
        return None;
    }
    let y: i32 = hour_str[0..4].parse().ok()?;
    let m: u32 = hour_str[4..6].parse().ok()?;
    let d: u32 = hour_str[6..8].parse().ok()?;
    let h: i64 = hour_str[8..10].parse().ok()?;

    // Convert to days since epoch, then to seconds.
    let days = ymd_to_days(y, m, d)?;
    Some(days * 86400 + h * 3600)
}

/// Convert (year, month, day) to days since Unix epoch.
fn ymd_to_days(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Inverse of days_to_ymd (Howard Hinnant algorithm).
    let y = y as i64;
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;
    Some(days)
}

// ---------------------------------------------------------------------------
// Write path: async channel → background pipeline XADD
// ---------------------------------------------------------------------------

/// Enqueue multiple events for async Redis persistence.
pub fn enqueue_events(keyspace: &str, builders: Vec<FsEventBuilder>) {
    if let Some(tx) = EVENT_TX.get() {
        for b in builders {
            let _ = tx.send(RedisEvent {
                keyspace: keyspace.to_string(),
                builder: b,
            });
        }
    }
}

/// Start the background event persistence loop. Call once at startup.
pub fn spawn_event_loop() {
    let (tx, mut rx) = mpsc::unbounded_channel::<RedisEvent>();
    EVENT_TX
        .set(tx)
        .unwrap_or_else(|_| tracing::warn!("fs9_redis: event loop already started"));

    tokio::spawn(async move {
        // Get initial connection (retry on startup failure).
        let mut conn = loop {
            match get_connection().await {
                Ok(c) => break c,
                Err(e) => {
                    tracing::error!("fs9_redis: connection failed, retrying in 1s: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };

        let mut batch: Vec<RedisEvent> = Vec::with_capacity(BATCH_SIZE);
        loop {
            // Wait for the first event.
            match rx.recv().await {
                Some(evt) => batch.push(evt),
                None => break, // Channel closed — shutdown.
            }

            // Drain up to BATCH_SIZE or until FLUSH_INTERVAL expires.
            let deadline = tokio::time::Instant::now() + FLUSH_INTERVAL;
            while batch.len() < BATCH_SIZE {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(evt)) => batch.push(evt),
                    Ok(None) => break,
                    Err(_) => break, // Timeout.
                }
            }

            if batch.is_empty() {
                continue;
            }

            let to_flush = std::mem::take(&mut batch);
            if let Err(e) = flush_batch(&mut conn, to_flush).await {
                tracing::warn!("fs9_redis: batch flush failed: {e}");
                // Reconnect on failure.
                if let Ok(new_conn) = get_connection().await {
                    conn = new_conn;
                }
            }
        }

        // Graceful shutdown: drain remaining events.
        while let Ok(evt) = rx.try_recv() {
            batch.push(evt);
        }
        if !batch.is_empty() {
            let to_flush = std::mem::take(&mut batch);
            let _ = flush_batch(&mut conn, to_flush).await;
        }
        tracing::info!("fs9_redis: event loop exited");
    });
}

/// Flush a batch of events to Redis using pipelined XADD commands.
/// Each event goes to the current hour's segment key with MAXLEN trim + TTL.
async fn flush_batch(conn: &mut MultiplexedConnection, events: Vec<RedisEvent>) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let hour = current_hour_segment();

    let mut pipe = redis::pipe();
    // Track unique keys for TTL refresh.
    let mut seen_keys = std::collections::HashSet::new();

    for evt in &events {
        let key = stream_key(&evt.keyspace, &hour);
        let b = &evt.builder;

        let mut cmd = redis::cmd("XADD");
        cmd.arg(&key)
            .arg("MAXLEN")
            .arg("~")
            .arg(STREAM_MAXLEN)
            .arg("*");

        // Fields
        cmd.arg("v").arg("1");
        cmd.arg("type").arg(b.event_type.as_str());
        cmd.arg("path").arg(&b.path);
        cmd.arg("inode").arg(b.inode);
        cmd.arg("parent_inode").arg(b.parent_inode);
        cmd.arg("generation").arg(b.generation);
        cmd.arg("is_dir").arg(if b.is_dir { 1u8 } else { 0u8 });
        cmd.arg("size").arg(b.size);
        cmd.arg("ts").arg(now_ms);
        if let Some(ref old) = b.old_path {
            cmd.arg("old_path").arg(old);
        }

        pipe.add_command(cmd);
        seen_keys.insert(key);
    }

    // Set/refresh TTL on each segment key touched in this batch.
    for key in &seen_keys {
        let mut cmd = redis::cmd("EXPIRE");
        cmd.arg(key).arg(SEGMENT_TTL_SECS);
        pipe.add_command(cmd);
    }

    pipe.query_async::<()>(conn)
        .await
        .map_err(|e| anyhow!("Redis pipeline XADD failed: {e}"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read path: XRANGE across hourly segments for fs9_events() TVF
// ---------------------------------------------------------------------------

/// A single event read from Redis Stream.
#[derive(Debug)]
#[allow(dead_code)]
pub struct RedisStreamEvent {
    /// Redis Stream ID (e.g., "1711324800000-0").
    pub stream_id: String,
    /// Event type.
    pub event_type: FsEventType,
    /// Affected path.
    pub path: String,
    /// Previous path (Rename only).
    pub old_path: Option<String>,
    /// Inode id.
    pub inode: u64,
    /// Parent directory inode.
    pub parent_inode: u64,
    /// Post-mutation generation.
    pub generation: u64,
    /// Whether target is a directory.
    pub is_dir: bool,
    /// File size.
    pub size: u64,
    /// Timestamp in milliseconds.
    pub timestamp: i64,
}

/// Read events from Redis Stream across hourly segments using XRANGE.
///
/// - `since_id`: exclusive lower bound (use `"0"` for all events).
///   If non-empty, we use `(since_id` exclusive syntax.
/// - `path_prefix`: optional path filter (applied client-side).
/// - `limit`: max events to return.
pub async fn read_events(
    keyspace: &str,
    since_id: &str,
    path_prefix: Option<&str>,
    limit: usize,
) -> Result<Vec<RedisStreamEvent>> {
    let mut conn = get_connection().await?;
    let current_hour = current_hour_segment();

    // Determine which hour segments to scan.
    let start_hour = if since_id.is_empty() || since_id == "0" {
        // No cursor — scan back 4 hours (matches SEGMENT_TTL).
        prev_n_hours(&current_hour, 4)
    } else {
        // Extract hour from stream ID timestamp.
        hour_from_stream_id(since_id).unwrap_or_else(|| current_hour.clone())
    };

    let segments = hour_range(&start_hour, &current_hour);

    // XRANGE start bound.
    let start = if since_id.is_empty() || since_id == "0" {
        "-".to_string()
    } else {
        format!("({since_id}")
    };

    // Normalize path prefix.
    let prefix = path_prefix.map(|p| {
        if p.ends_with('/') {
            p.to_string()
        } else {
            format!("{p}/")
        }
    });

    // We request more than limit to account for client-side path filtering.
    let scan_limit = if path_prefix.is_some() {
        (limit * 10).clamp(1000, 100_000)
    } else {
        limit
    };

    let mut events = Vec::with_capacity(limit);

    for (i, seg) in segments.iter().enumerate() {
        if events.len() >= limit {
            break;
        }

        let key = stream_key(keyspace, seg);
        // First segment uses the exclusive since_id bound; subsequent use "-" (start of stream).
        let seg_start = if i == 0 { start.as_str() } else { "-" };

        let remaining = limit - events.len();
        let seg_scan = if path_prefix.is_some() {
            scan_limit
        } else {
            remaining
        };

        let result: Vec<redis::Value> = redis::cmd("XRANGE")
            .arg(&key)
            .arg(seg_start)
            .arg("+")
            .arg("COUNT")
            .arg(seg_scan)
            .query_async(&mut conn)
            .await
            .map_err(|e| anyhow!("Redis XRANGE failed: {e}"))?;

        for entry in &result {
            if events.len() >= limit {
                break;
            }
            if let Some(evt) = parse_stream_entry(entry) {
                // Apply path prefix filter.
                if let Some(ref pfx) = prefix {
                    if !evt.path.starts_with(pfx.as_str()) {
                        continue;
                    }
                }
                events.push(evt);
            }
        }
    }

    Ok(events)
}

/// Get the hour segment N hours before the given hour.
fn prev_n_hours(hour: &str, n: u64) -> String {
    if let Some(secs) = hour_to_epoch_secs(hour) {
        let prev = secs - (n as i64 * 3600);
        let days = prev / 86400;
        let rem = prev % 86400;
        let h = rem / 3600;
        let (y, m, d) = days_to_ymd(days);
        format!("{y:04}{m:02}{d:02}{h:02}")
    } else {
        hour.to_string()
    }
}

/// Parse a single Redis Stream entry (array of [id, [field, value, ...]]).
fn parse_stream_entry(value: &redis::Value) -> Option<RedisStreamEvent> {
    let arr = match value {
        redis::Value::Array(a) => a,
        _ => return None,
    };
    if arr.len() < 2 {
        return None;
    }

    // Entry ID
    let stream_id = match &arr[0] {
        redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
        _ => return None,
    };

    // Fields array
    let fields = match &arr[1] {
        redis::Value::Array(f) => f,
        _ => return None,
    };

    // Parse field-value pairs into a map.
    let mut map = std::collections::HashMap::new();
    let mut i = 0;
    while i + 1 < fields.len() {
        let k = match &fields[i] {
            redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
            _ => {
                i += 2;
                continue;
            }
        };
        let v = match &fields[i + 1] {
            redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
            _ => String::new(),
        };
        map.insert(k, v);
        i += 2;
    }

    let event_type = match map.get("type")?.as_str() {
        "CREATE" => FsEventType::Create,
        "WRITE" => FsEventType::Write,
        "DELETE" => FsEventType::Delete,
        "RENAME" => FsEventType::Rename,
        "MKDIR" => FsEventType::Mkdir,
        _ => return None,
    };

    Some(RedisStreamEvent {
        stream_id,
        event_type,
        path: map.get("path")?.clone(),
        old_path: map.get("old_path").cloned(),
        inode: map.get("inode")?.parse().unwrap_or(0),
        parent_inode: map.get("parent_inode")?.parse().unwrap_or(0),
        generation: map.get("generation")?.parse().unwrap_or(0),
        is_dir: map.get("is_dir").is_some_and(|v| v == "1"),
        size: map.get("size")?.parse().unwrap_or(0),
        timestamp: map.get("ts")?.parse().unwrap_or(0),
    })
}
