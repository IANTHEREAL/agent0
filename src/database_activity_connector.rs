//! Control-plane connector for per-database activity (#2638 connector / Phase 2).
//!
//! Bridges the SQL executor's [`crate::database_activity`] emission seam to the
//! backend control plane: it implements [`DatabaseActivitySink`] by coalescing
//! events per keyspace in memory (non-blocking on the SQL hot path) and a
//! background task batch-POSTs them to the backend
//! `POST /internal/v1/database-activity` endpoint, which advances
//! `tenants.last_active_at` / `last_modified_at`.
//!
//! Best-effort by contract: a rejected or failed flush is logged + metered and
//! the events are dropped (the next SQL event re-marks the keyspace). It never
//! blocks or fails the user's SQL query.
//!
//! Known limitation — timestamp lag, never incorrectness: under buffer pressure
//! (more than `max_keyspaces` distinct keyspaces active within one flush window)
//! or while the backend is unavailable, some activity events are dropped, so a
//! tenant's `last_active_at` / `last_modified_at` may lag until its next event
//! re-marks it. The timestamps are eventually-consistent and only ever move
//! forward monotonically (the backend's guard), so a lag is never a wrong value.
//!
//! Source mapping (V1, documented limitation): the seam currently carries a
//! single coarse [`DatabaseActivitySource::Sql`], which collapses pgwire SQL,
//! HTTP SQL, and cron-worker SQL into one origin. This connector maps it to the
//! `pgwire_sql` source value of the backend contract. Distinguishing
//! `http_sql` (or a cron origin) requires the seam to carry the origin and is
//! deferred to a follow-up; per #2638 review, connector acceptance is
//! persistence correctness, not diagnostic-source parity.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;

use crate::database_activity::{
    install_database_activity_sink, DatabaseActivityEvent, DatabaseActivityKind,
    DatabaseActivitySink,
};

/// Default debounce interval between backend flushes.
const DEFAULT_FLUSH_SECS: u64 = 5;
/// Default bound on distinct keyspaces buffered between flushes.
const DEFAULT_MAX_KEYSPACES: usize = 4096;
/// Maximum updates per POST — must not exceed the backend's `MAX_ACTIVITY_BATCH`
/// (1024), or the backend rejects the whole batch with `activity_batch_too_large`.
const BACKEND_MAX_BATCH: usize = 1024;
/// Backend source value this connector emits (see module docs on the V1 mapping).
const SOURCE_PGWIRE_SQL: &str = "pgwire_sql";

/// Coalesced activity for one keyspace between flushes.
#[derive(Debug, Clone, Copy)]
struct Pending {
    /// Latest db9-server numeric database id (diagnostic only).
    database_id: u64,
    active: bool,
    modified: bool,
}

/// In-memory coalescing sink that batch-forwards activity to the backend.
pub(crate) struct HttpActivitySink {
    pending: Mutex<HashMap<Arc<str>, Pending>>,
    max_keyspaces: usize,
}

impl HttpActivitySink {
    fn new(max_keyspaces: usize) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            max_keyspaces: max_keyspaces.max(1),
        }
    }

    /// Atomically take all pending entries, leaving the buffer empty.
    fn drain(&self) -> Vec<(Arc<str>, Pending)> {
        let mut map = self.pending.lock();
        map.drain().collect()
    }

    /// Drain the buffer and forward it to the backend in batches (chunked to the
    /// backend's per-request bound). Best-effort: each batch's failure is
    /// handled inside [`flush_batch`] (log + metric + drop). Drained entries are
    /// never re-queued, so a persistent backend failure cannot grow the buffer.
    async fn flush_pending(&self, client: &reqwest::Client, url: &str, secret: &str) {
        let entries = self.drain();
        for chunk in entries.chunks(BACKEND_MAX_BATCH) {
            let updates = chunk
                .iter()
                .map(|(ks, p)| build_update_json(ks, p))
                .collect();
            flush_batch(client, url, secret, updates).await;
        }
    }
}

impl DatabaseActivitySink for HttpActivitySink {
    fn try_record(&self, event: DatabaseActivityEvent) -> Result<()> {
        // `Modified` implies activity, so it sets both flags. Keyed by keyspace
        // (not keyspace+kind) so a write can never be split into a
        // `modified`-without-`active` entry.
        let (active, modified) = match event.kind {
            DatabaseActivityKind::Active => (true, false),
            DatabaseActivityKind::Modified => (true, true),
        };

        let mut map = self.pending.lock();
        if let Some(entry) = map.get_mut(event.tenant_keyspace.as_ref()) {
            entry.active |= active;
            entry.modified |= modified;
            entry.database_id = event.database_id;
            Ok(())
        } else if map.len() >= self.max_keyspaces {
            // Bounded: drop a new keyspace over capacity. The seam logs the
            // returned error; the next event re-marks the keyspace.
            ::metrics::counter!("db9_database_activity_dropped_total", "reason" => "buffer_full")
                .increment(1);
            Err(anyhow!("database activity buffer at capacity"))
        } else {
            map.insert(
                event.tenant_keyspace.clone(),
                Pending {
                    database_id: event.database_id,
                    active,
                    modified,
                },
            );
            Ok(())
        }
    }
}

/// Build the JSON `updates[]` element for one keyspace, matching the backend
/// event-intent contract. Pure, for unit testing the payload shape.
fn build_update_json(keyspace: &str, pending: &Pending) -> serde_json::Value {
    serde_json::json!({
        "keyspace": keyspace,
        "active": pending.active,
        "modified": pending.modified,
        "source": SOURCE_PGWIRE_SQL,
        "database_id": pending.database_id,
    })
}

fn backend_activity_url() -> Option<String> {
    let base = std::env::var("DB9_FUNCTIONS_BACKEND_URL")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())?;
    Some(format!("{base}/internal/v1/database-activity"))
}

fn internal_secret() -> Option<String> {
    std::env::var("INTERNAL_CONTROL_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
}

fn flush_interval() -> Duration {
    let secs = std::env::var("DB9_ACTIVITY_CONNECTOR_FLUSH_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_FLUSH_SECS);
    Duration::from_secs(secs)
}

fn max_keyspaces() -> usize {
    std::env::var("DB9_ACTIVITY_CONNECTOR_MAX_KEYSPACES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_MAX_KEYSPACES)
}

/// POST one batch of updates. Best-effort: on rejection or transport error the
/// batch is logged + metered and dropped (never retried in-band, never
/// surfaced to the SQL path).
async fn flush_batch(
    client: &reqwest::Client,
    url: &str,
    secret: &str,
    updates: Vec<serde_json::Value>,
) {
    let count = updates.len() as u64;
    let body = serde_json::json!({ "updates": updates });
    match client
        .post(url)
        .header("Content-Type", "application/json")
        .header("X-Internal-Secret", secret)
        .json(&body)
        .send()
        .await
    {
        // Exact-match the backend's success status: the endpoint returns `202
        // Accepted` only when every update was accepted into the bounded writer.
        // Any other status (including other 2xx) is treated as a non-success
        // best-effort drop, so the contract can't silently widen.
        Ok(resp) if resp.status() == reqwest::StatusCode::ACCEPTED => {
            ::metrics::counter!("db9_database_activity_flushed_total").increment(count);
        }
        Ok(resp) => {
            ::metrics::counter!("db9_database_activity_dropped_total", "reason" => "rejected")
                .increment(count);
            tracing::warn!(
                status = %resp.status(),
                count,
                "database activity flush rejected by backend; dropping"
            );
        }
        Err(err) => {
            ::metrics::counter!("db9_database_activity_dropped_total", "reason" => "transport")
                .increment(count);
            tracing::warn!(
                error = %err,
                count,
                "database activity flush transport error; dropping"
            );
        }
    }
}

/// Install the connector sink and spawn its background flush loop, if the
/// backend endpoint + internal secret are configured. A no-op (leaving the
/// default disabled sink) otherwise, so activity emission stays best-effort and
/// optional.
pub(crate) fn start_database_activity_connector() {
    let Some(url) = backend_activity_url() else {
        tracing::info!("database activity connector disabled (DB9_FUNCTIONS_BACKEND_URL unset)");
        return;
    };
    let Some(secret) = internal_secret() else {
        tracing::info!("database activity connector disabled (INTERNAL_CONTROL_SECRET unset)");
        return;
    };

    let interval = flush_interval();
    let sink = Arc::new(HttpActivitySink::new(max_keyspaces()));
    install_database_activity_sink(sink.clone());

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        // Short cap — this is an off-path best-effort POST, not the SQL path.
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build reqwest client for database activity connector");

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            sink.flush_pending(&client, &url, &secret).await;
        }
    });

    tracing::info!(
        interval_secs = interval.as_secs(),
        "database activity connector started"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        keyspace: &str,
        database_id: u64,
        kind: DatabaseActivityKind,
    ) -> DatabaseActivityEvent {
        DatabaseActivityEvent {
            tenant_keyspace: Arc::from(keyspace),
            database_id,
            source: crate::database_activity::DatabaseActivitySource::Sql,
            kind,
        }
    }

    #[test]
    fn active_sets_active_only() {
        let sink = HttpActivitySink::new(16);
        sink.try_record(event("ks1", 1, DatabaseActivityKind::Active))
            .unwrap();
        let drained = sink.drain();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].1.active);
        assert!(!drained[0].1.modified);
    }

    #[test]
    fn modified_sets_both() {
        let sink = HttpActivitySink::new(16);
        sink.try_record(event("ks1", 1, DatabaseActivityKind::Modified))
            .unwrap();
        let drained = sink.drain();
        assert!(drained[0].1.active);
        assert!(drained[0].1.modified);
    }

    #[test]
    fn coalesces_per_keyspace_modified_never_without_active() {
        let sink = HttpActivitySink::new(16);
        // Active then Modified, and Modified then Active, both end as {true,true}.
        sink.try_record(event("ks1", 1, DatabaseActivityKind::Active))
            .unwrap();
        sink.try_record(event("ks1", 2, DatabaseActivityKind::Modified))
            .unwrap();
        sink.try_record(event("ks2", 3, DatabaseActivityKind::Modified))
            .unwrap();
        sink.try_record(event("ks2", 4, DatabaseActivityKind::Active))
            .unwrap();
        let mut drained = sink.drain();
        drained.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, p) in &drained {
            // Invariant: a modification is never recorded without activity.
            assert!(p.active, "active must be set whenever modified is");
            assert!(p.modified);
        }
        // Latest database_id wins (diagnostic).
        assert_eq!(drained[0].1.database_id, 2);
        assert_eq!(drained[1].1.database_id, 4);
    }

    #[test]
    fn bounded_buffer_rejects_new_keyspace_over_capacity() {
        let sink = HttpActivitySink::new(1);
        sink.try_record(event("ks1", 1, DatabaseActivityKind::Active))
            .unwrap();
        // A new keyspace over capacity is rejected (the seam logs it).
        assert!(sink
            .try_record(event("ks2", 2, DatabaseActivityKind::Active))
            .is_err());
        // An existing keyspace still merges at capacity.
        assert!(sink
            .try_record(event("ks1", 3, DatabaseActivityKind::Modified))
            .is_ok());
    }

    #[test]
    fn update_json_matches_backend_contract() {
        let p = Pending {
            database_id: 7,
            active: true,
            modified: true,
        };
        let v = build_update_json("db9_tenant_example", &p);
        assert_eq!(v["keyspace"], "db9_tenant_example");
        assert_eq!(v["active"], true);
        assert_eq!(v["modified"], true);
        assert_eq!(v["source"], "pgwire_sql");
        assert_eq!(v["database_id"], 7);
    }
}

/// In-process HTTP-boundary tests: a minimal `tokio` TCP server captures the
/// connector's actual POST so a path/header/payload regression is caught even
/// though the full cross-repo backend E2E lives in the regression gate.
#[cfg(test)]
mod boundary_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn event(
        keyspace: &str,
        database_id: u64,
        kind: DatabaseActivityKind,
    ) -> DatabaseActivityEvent {
        DatabaseActivityEvent {
            tenant_keyspace: Arc::from(keyspace),
            database_id,
            source: crate::database_activity::DatabaseActivitySource::Sql,
            kind,
        }
    }

    struct Captured {
        method: String,
        path: String,
        internal_secret: Option<String>,
        body: serde_json::Value,
    }

    fn find_headers_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    /// Bind an ephemeral listener and serve exactly one request, replying with
    /// `status`. Returns the bound address and a handle yielding the captured
    /// request.
    async fn serve_one(status: u16) -> (std::net::SocketAddr, tokio::task::JoinHandle<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            // Bound the whole serve so the test can never hang the suite if the
            // connector POST never arrives (a connection failure must fail the
            // test fast, not block `cargo test` forever).
            let served = tokio::time::timeout(Duration::from_secs(10), async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                loop {
                    let n = stream.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(end) = find_headers_end(&buf) {
                        let header_txt = String::from_utf8_lossy(&buf[..end]).to_string();
                        let content_len = header_txt
                            .lines()
                            .find_map(|l| {
                                let (name, val) = l.split_once(':')?;
                                name.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| val.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + content_len {
                            let mut req_line =
                                header_txt.lines().next().unwrap_or("").split_whitespace();
                            let method = req_line.next().unwrap_or("").to_string();
                            let path = req_line.next().unwrap_or("").to_string();
                            let internal_secret = header_txt.lines().find_map(|l| {
                                let (name, val) = l.split_once(':')?;
                                name.trim()
                                    .eq_ignore_ascii_case("x-internal-secret")
                                    .then(|| val.trim().to_string())
                            });
                            let body: serde_json::Value =
                                serde_json::from_slice(&buf[end + 4..end + 4 + content_len])
                                    .unwrap_or(serde_json::Value::Null);
                            let reason = if (200..300).contains(&status) {
                                "OK"
                            } else {
                                "ERR"
                            };
                            let resp =
                                format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\n\r\n");
                            let _ = stream.write_all(resp.as_bytes()).await;
                            let _ = stream.flush().await;
                            return Captured {
                                method,
                                path,
                                internal_secret,
                                body,
                            };
                        }
                    }
                }
                panic!("incomplete request");
            })
            .await;
            served.expect("test HTTP server timed out waiting for the connector POST")
        });
        (addr, handle)
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_posts_backend_shaped_request_with_secret() {
        let (addr, server) = serve_one(202).await;
        let url = format!("http://{addr}/internal/v1/database-activity");

        let sink = HttpActivitySink::new(16);
        sink.try_record(event("ks_read", 1, DatabaseActivityKind::Active))
            .unwrap();
        sink.try_record(event("ks_write", 2, DatabaseActivityKind::Modified))
            .unwrap();

        sink.flush_pending(&test_client(), &url, "shh-secret").await;

        let captured = server.await.unwrap();
        assert_eq!(captured.method, "POST");
        assert_eq!(captured.path, "/internal/v1/database-activity");
        assert_eq!(captured.internal_secret.as_deref(), Some("shh-secret"));

        // Body is the backend event-intent contract.
        let updates = captured.body["updates"].as_array().expect("updates array");
        assert_eq!(updates.len(), 2);
        for u in updates {
            assert_eq!(u["source"], "pgwire_sql");
            // modified implies active for every emitted update.
            if u["modified"] == serde_json::json!(true) {
                assert_eq!(u["active"], serde_json::json!(true));
            }
        }
        let by_ks: std::collections::HashMap<&str, &serde_json::Value> = updates
            .iter()
            .map(|u| (u["keyspace"].as_str().unwrap(), u))
            .collect();
        assert_eq!(by_ks["ks_read"]["active"], serde_json::json!(true));
        assert_eq!(by_ks["ks_read"]["modified"], serde_json::json!(false));
        assert_eq!(by_ks["ks_write"]["active"], serde_json::json!(true));
        assert_eq!(by_ks["ks_write"]["modified"], serde_json::json!(true));

        // The buffer was drained.
        assert!(sink.drain().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backend_rejection_is_dropped_not_surfaced() {
        // A non-2xx response must be swallowed (best-effort): flush completes
        // without panicking, and the entry is dropped (not re-queued).
        let (addr, server) = serve_one(503).await;
        let url = format!("http://{addr}/internal/v1/database-activity");

        let sink = HttpActivitySink::new(16);
        sink.try_record(event("ks1", 1, DatabaseActivityKind::Modified))
            .unwrap();

        // Returns `()` — no error path to the caller / SQL path.
        sink.flush_pending(&test_client(), &url, "shh-secret").await;

        let captured = server.await.unwrap();
        assert_eq!(captured.path, "/internal/v1/database-activity");
        // Dropped, not retried in-band.
        assert!(sink.drain().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backend_200_ok_is_treated_as_drop_not_success() {
        // Only `202 Accepted` is success; a different 2xx (here `200 OK`) is a
        // non-success best-effort drop. Guards the exact-status contract so it
        // can't silently widen back to `is_success()`.
        let (addr, server) = serve_one(200).await;
        let url = format!("http://{addr}/internal/v1/database-activity");

        let sink = HttpActivitySink::new(16);
        sink.try_record(event("ks1", 1, DatabaseActivityKind::Modified))
            .unwrap();
        sink.flush_pending(&test_client(), &url, "shh-secret").await;

        let captured = server.await.unwrap();
        assert_eq!(captured.path, "/internal/v1/database-activity");
        assert!(sink.drain().is_empty());
    }
}
