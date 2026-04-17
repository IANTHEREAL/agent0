//! Prometheus metrics exporter for db9-server.
//!
//! Follows the same pattern as db9-backend: global `metrics` recorder with
//! `metrics-exporter-prometheus`, exposed via `/internal/metrics` HTTP endpoint.
//!
//! The recorder is installed once at startup via [`install_recorder`].  All
//! subsequent calls to `metrics::counter!()`, `gauge!()`, `histogram!()`
//! anywhere in the codebase are captured automatically.

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use std::time::SystemTime;
use tracing::{info, warn};

/// Histogram buckets for query/request latencies (seconds).
/// Matches db9-backend's FAST_BUCKETS.
const FAST_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Histogram buckets for slow background operations (seconds).
#[allow(dead_code)]
const SLOW_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0];

/// Histogram buckets for upload hot-path timers (seconds).
/// Covers two overlapping signals whose healthy ranges differ by ~4 orders
/// of magnitude:
///
/// * SHA-256 update() on a 64 KiB chunk: healthy ~100–300 µs, pathological
///   ~tens of ms.
/// * mpsc send into the fs9 WriteParts channel: healthy <1 µs (unblocked),
///   backpressured up to multiple seconds when fs9 itself is stalled —
///   exactly the failure mode `mpsc_bound` exists to surface.
///
/// A single bucket set sized only for the healthy end (≤ 100 ms) would
/// saturate p99 at the ceiling during the backpressure scenario we need
/// to diagnose. Buckets span 100 µs → 10 s.
const UPLOAD_BUCKETS: &[f64] = &[0.0001, 0.0005, 0.001, 0.005, 0.025, 0.1, 0.5, 2.5, 10.0];

/// Install the global Prometheus metrics recorder.
///
/// Returns a [`PrometheusHandle`] used to render metrics at the HTTP endpoint.
/// Must be called exactly once, early in `main()`, before any metric
/// emission or description. Startup metric values (`db9_server_build_info`,
/// `db9_server_start_time_seconds`, `db9_tokio_worker_threads`) are emitted
/// from `main` *after* this returns and after descriptions are registered,
/// so their first scrape carries HELP/TYPE metadata.
pub fn install_recorder() -> PrometheusHandle {
    PrometheusBuilder::new()
        .set_buckets(FAST_BUCKETS)
        .expect("failed to set default buckets")
        .set_buckets_for_metric(
            Matcher::Full("db9_upload_sha256_seconds".to_string()),
            UPLOAD_BUCKETS,
        )
        .expect("failed to set upload buckets for sha256 timer")
        .set_buckets_for_metric(
            Matcher::Full("db9_upload_mpsc_send_seconds".to_string()),
            UPLOAD_BUCKETS,
        )
        .expect("failed to set upload buckets for mpsc send timer")
        .install_recorder()
        .expect("failed to install Prometheus recorder")
}

/// Current time in seconds since the Unix epoch, for the
/// `db9_server_start_time_seconds` startup gauge.
pub fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Start a lightweight HTTP server that serves `/internal/metrics`.
///
/// Listens on `0.0.0.0:{port}`. Disabled when `port == 0`.
/// Uses raw TCP + manual HTTP parsing (no framework dependency) to stay
/// consistent with the break-glass admin server pattern.
/// Maximum concurrent connections to the metrics HTTP server.
const MAX_METRICS_CONNECTIONS: usize = 8;

pub async fn start_metrics_server(port: u16, handle: PrometheusHandle) {
    if port == 0 {
        info!("Metrics endpoint disabled (DB9_METRICS_PORT=0)");
        return;
    }

    let addr = format!("0.0.0.0:{}", port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => {
            info!("Prometheus metrics listening on {}", addr);
            l
        }
        Err(e) => {
            warn!("Failed to bind metrics port {}: {}", addr, e);
            return;
        }
    };

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_METRICS_CONNECTIONS));

    loop {
        let (mut stream, _peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => continue,
        };
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => continue, // drop connection silently when at limit
        };
        let handle = handle.clone();
        tokio::spawn(async move {
            let _permit = permit;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let mut buf = [0u8; 4096];
            let n = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stream.read(&mut buf),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => n,
                _ => return,
            };

            let request = String::from_utf8_lossy(&buf[..n]);

            // Only serve GET /internal/metrics
            if request.starts_with("GET /internal/metrics") {
                let body = handle.render();
                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            } else if request.starts_with("GET /health") {
                let response =
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                let _ = stream.write_all(response.as_bytes()).await;
            } else {
                let response =
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
    }
}

// ── RAII gauge guard ────────────────────────────────────────────────────────

/// RAII guard that increments a gauge on creation and decrements on drop.
/// Prevents gauge drift if the owning task panics or is cancelled.
pub struct GaugeGuard {
    gauge: metrics::Gauge,
}

impl GaugeGuard {
    /// Increment the named gauge and return a guard that decrements on drop.
    pub fn increment(name: &'static str) -> Self {
        let gauge = metrics::gauge!(name);
        gauge.increment(1.0);
        Self { gauge }
    }
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.gauge.decrement(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_buckets_are_strictly_increasing() {
        for w in FAST_BUCKETS.windows(2) {
            assert!(w[0] < w[1], "buckets must be strictly increasing");
        }
    }

    #[test]
    fn slow_buckets_are_strictly_increasing() {
        for w in SLOW_BUCKETS.windows(2) {
            assert!(w[0] < w[1], "buckets must be strictly increasing");
        }
    }

    #[test]
    fn upload_buckets_span_micro_to_seconds_and_are_strictly_increasing() {
        for w in UPLOAD_BUCKETS.windows(2) {
            assert!(w[0] < w[1], "buckets must be strictly increasing");
        }
        // Sanity: must reach sub-millisecond resolution (SHA-256 floor)
        // and multi-second ceiling (backpressured mpsc send tail).
        assert!(UPLOAD_BUCKETS.first().copied().unwrap() <= 0.001);
        assert!(UPLOAD_BUCKETS.last().copied().unwrap() >= 1.0);
    }

    /// Create a local recorder + handle for isolated test use.
    fn test_recorder() -> (
        metrics_exporter_prometheus::PrometheusRecorder,
        PrometheusHandle,
    ) {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        (recorder, handle)
    }

    #[test]
    fn gauge_guard_decrements_on_drop() {
        let (recorder, handle) = test_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);

        {
            let _g = GaugeGuard::increment("test_gauge_guard");
            let rendered = handle.render();
            assert!(
                rendered.contains("test_gauge_guard 1"),
                "gauge should be 1 while guard is alive: {rendered}"
            );
        }
        // After drop, gauge should be back to 0
        let rendered = handle.render();
        assert!(
            rendered.contains("test_gauge_guard 0"),
            "gauge should be 0 after guard dropped: {rendered}"
        );
    }

    #[test]
    fn metrics_renders_prometheus_format() {
        let (recorder, handle) = test_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);

        metrics::counter!("test_counter").increment(42);
        metrics::gauge!("test_gauge").set(1.5);

        let rendered = handle.render();
        assert!(rendered.contains("test_counter 42"));
        assert!(rendered.contains("test_gauge 1.5"));
        assert!(rendered.contains("# TYPE test_counter counter"));
        assert!(rendered.contains("# TYPE test_gauge gauge"));
    }

    #[test]
    fn worker_task_metrics_have_labels() {
        let (recorder, handle) = test_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);

        metrics::counter!(
            "db9_server_worker_tasks_total",
            "task_type" => "cron",
            "result" => "ok",
        )
        .increment(5);

        let rendered = handle.render();
        assert!(
            rendered.contains(r#"db9_server_worker_tasks_total{task_type="cron",result="ok"} 5"#),
            "should contain labeled counter: {rendered}"
        );
    }
}
