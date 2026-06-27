//! Prometheus metrics exporter for db9-server.
//!
//! Follows the same pattern as db9-backend: global `metrics` recorder with
//! `metrics-exporter-prometheus`, exposed via `/internal/metrics` HTTP endpoint.
//!
//! The recorder is installed once at startup via [`install_recorder`].  All
//! subsequent calls to `metrics::counter!()`, `gauge!()`, `histogram!()`
//! anywhere in the codebase are captured automatically.

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use std::net::SocketAddr;
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

/// Record bounded-channel send latency for the fs9 gRPC upload path.
pub(crate) fn record_upload_mpsc_send_latency(duration: std::time::Duration) {
    metrics::histogram!("db9_upload_mpsc_send_seconds").record(duration.as_secs_f64());
}

/// Register HELP/TYPE metadata for db9-server observability metric families.
pub fn describe_observability_metrics() {
    use ::metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_counter!(
        "db9_fs9_glob_truncations_total",
        Unit::Count,
        "fs9 glob scans stopped early by byte budget"
    );
    describe_counter!(
        "db9_fs9_read_budget_rejections_total",
        Unit::Count,
        "Scalar SQL fs9 read calls rejected by the process-global read budget"
    );
    describe_counter!(
        "db9_fs9_juicefs_lifecycle_total",
        Unit::Count,
        "fs-plane JuiceFS lifecycle materialization outcomes"
    );
    describe_gauge!(
        "db9_fs9_redis_event_queue_depth",
        Unit::Count,
        "Pending fs9 Redis persistence events"
    );
    describe_gauge!(
        "db9_fs9_gc_backoff_failures",
        Unit::Count,
        "Consecutive fs9 background maintenance failures"
    );
    describe_gauge!(
        "db9_fs9_gc_backoff_seconds",
        Unit::Seconds,
        "Current fs9 background maintenance backoff delay"
    );
    describe_gauge!(
        "db9_fs9_orphan_inodes",
        Unit::Count,
        "fs9 orphan inode count when a runtime source is available"
    );
    describe_counter!(
        "db9_fs9_stats_worker_scans_total",
        Unit::Count,
        "fs9 stats worker scan outcomes"
    );
    describe_histogram!(
        "db9_fs9_stats_worker_scan_duration_seconds",
        Unit::Seconds,
        "fs9 stats worker scan latency"
    );
    describe_gauge!(
        "db9_fs9_stats_worker_staleness_seconds",
        Unit::Seconds,
        "Cached fs9 stats age after each scan attempt"
    );
    describe_counter!(
        "db9_fs9_pd_lifecycle_probe_total",
        Unit::Count,
        "Final PD lifecycle guard outcomes before opening a JuiceFS backend"
    );
    describe_histogram!(
        "db9_fs9_operation_duration_seconds",
        Unit::Seconds,
        "fs9 backend operation latency by tenant, backend kind, operation, and outcome"
    );
    describe_gauge!(
        "db9_trigger_queue_depth",
        Unit::Count,
        "Pending plus processing async trigger tasks for a tenant"
    );
    describe_counter!(
        "db9_trigger_events_total",
        Unit::Count,
        "Async trigger worker task completion outcomes"
    );
    describe_histogram!(
        "db9_trigger_execution_duration_seconds",
        Unit::Seconds,
        "Per-trigger body execution latency when a runtime source is available"
    );
    describe_counter!(
        "db9_trigger_gc_runs_total",
        Unit::Count,
        "Trigger queue GC runs when a runtime source is available"
    );
    describe_counter!(
        "db9_batch_write_atomic_requests_total",
        Unit::Count,
        "batch_write_atomic request outcomes"
    );
    describe_counter!(
        "db9_batch_write_atomic_files_total",
        Unit::Count,
        "Files accepted into validated batch_write_atomic requests"
    );
    describe_histogram!(
        "db9_batch_write_atomic_subgroup_duration_seconds",
        Unit::Seconds,
        "Per-subgroup atomic batch write commit latency"
    );
    describe_counter!(
        "db9_batch_write_atomic_errors_total",
        Unit::Count,
        "Request-level batch_write_atomic error categories"
    );
    describe_counter!(
        "db9_batch_write_atomic_entry_errors_total",
        Unit::Count,
        "Per-entry batch_write_atomic execution failure categories"
    );
    describe_counter!(
        "db9_hnsw_s3_operations_total",
        Unit::Count,
        "HNSW S3 GET/PUT outcomes"
    );
    describe_histogram!(
        "db9_hnsw_s3_operation_duration_seconds",
        Unit::Seconds,
        "HNSW S3 GET/PUT latency"
    );
    describe_gauge!(
        "db9_server_worker_executor_active",
        Unit::Count,
        "Whether this db9-server process currently holds the active worker executor lease"
    );
    describe_counter!(
        "db9_server_worker_executor_lease_acquisitions_total",
        Unit::Count,
        "Worker executor lease acquisitions by this process"
    );
    describe_counter!(
        "db9_server_worker_executor_lease_errors_total",
        Unit::Count,
        "Worker executor lease acquire, renew, or release errors"
    );
    describe_counter!("db9_engine_rows_total", Unit::Count, "Engine row counters");
    describe_histogram!(
        "db9_first_row_latency_seconds",
        Unit::Seconds,
        "First-row latency when a runtime source is available"
    );
    describe_gauge!(
        "db9_statement_memory_bytes",
        Unit::Bytes,
        "Statement memory usage gauges when a runtime source is available"
    );
    describe_gauge!(
        "db9_operator_memory_bytes",
        Unit::Bytes,
        "Operator memory usage gauges when a runtime source is available"
    );
    describe_counter!(
        "db9_spill_bytes_total",
        Unit::Bytes,
        "Bytes spilled to temporary storage"
    );
    describe_counter!(
        "db9_plan_cache_events_total",
        Unit::Count,
        "Prepared plan cache events"
    );
    describe_gauge!(
        "db9_plan_cache_memory_bytes",
        Unit::Bytes,
        "Prepared plan cache memory usage when available"
    );
    describe_counter!(
        "db9_remote_rejects_total",
        Unit::Count,
        "Remote execution rejection reasons"
    );
    describe_counter!(
        "db9_streaming_mode_total",
        Unit::Count,
        "DB9 streaming mode selections"
    );
    describe_counter!("db9_spill_runs_total", Unit::Count, "Spill run count");
    describe_counter!(
        "db9_spill_passes_total",
        Unit::Count,
        "Spill merge/pass count"
    );
    describe_counter!(
        "db9_spill_cleanup_failures_total",
        Unit::Count,
        "Spill cleanup failures"
    );
}

/// Count fs9 glob scans that stopped early because the byte budget was exhausted.
pub(crate) fn record_fs9_glob_truncated(keyspace: &str, mode: &'static str) {
    metrics::counter!(
        "db9_fs9_glob_truncations_total",
        "keyspace" => keyspace.to_string(),
        "mode" => mode,
    )
    .increment(1);
}

/// Count scalar SQL fs9 read calls rejected by the process-global read budget.
pub(crate) fn record_fs9_read_budget_rejected(keyspace: &str, operation: &'static str) {
    metrics::counter!(
        "db9_fs9_read_budget_rejections_total",
        "keyspace" => keyspace.to_string(),
        "operation" => operation,
    )
    .increment(1);
}

/// Count fs-plane JuiceFS lifecycle materialization outcomes by auth tenant id.
pub(crate) fn record_fs9_juicefs_lifecycle(tenant_id: &str, result: &'static str) {
    metrics::counter!(
        "db9_fs9_juicefs_lifecycle_total",
        "tenant_id" => tenant_id.to_string(),
        "result" => result,
    )
    .increment(1);
}

pub(crate) fn sample_fs9_redis_event_queue_depth(depth: u64) {
    metrics::gauge!("db9_fs9_redis_event_queue_depth").set(depth as f64);
}

pub(crate) fn sample_fs9_gc_backoff(keyspace: &str, consecutive_failures: u32, sleep_secs: u64) {
    metrics::gauge!(
        "db9_fs9_gc_backoff_failures",
        "keyspace" => keyspace.to_string(),
    )
    .set(consecutive_failures as f64);
    metrics::gauge!(
        "db9_fs9_gc_backoff_seconds",
        "keyspace" => keyspace.to_string(),
    )
    .set(sleep_secs as f64);
}

#[allow(dead_code)]
pub(crate) fn sample_fs9_orphan_inode_count(keyspace: &str, count: u64) {
    metrics::gauge!(
        "db9_fs9_orphan_inodes",
        "keyspace" => keyspace.to_string(),
    )
    .set(count as f64);
}

pub(crate) fn record_fs9_stats_worker_scan(
    result: &'static str,
    duration: std::time::Duration,
    staleness_seconds: u64,
) {
    metrics::counter!(
        "db9_fs9_stats_worker_scans_total",
        "result" => result,
    )
    .increment(1);
    metrics::histogram!(
        "db9_fs9_stats_worker_scan_duration_seconds",
        "result" => result,
    )
    .record(duration.as_secs_f64());
    metrics::gauge!(
        "db9_fs9_stats_worker_staleness_seconds",
        "result" => result,
    )
    .set(staleness_seconds as f64);
}

/// Count final PD lifecycle guard outcomes before opening a JuiceFS backend.
pub(crate) fn record_fs9_pd_lifecycle_probe(keyspace: &str, result: &'static str) {
    metrics::counter!(
        "db9_fs9_pd_lifecycle_probe_total",
        "keyspace" => keyspace.to_string(),
        "result" => result,
    )
    .increment(1);
}

/// Record fs9 backend operation latency by tenant, backend kind, operation, and outcome.
pub(crate) fn record_fs9_operation_latency(
    keyspace: &str,
    backend: &'static str,
    operation: &'static str,
    result: &'static str,
    duration: std::time::Duration,
) {
    metrics::histogram!(
        "db9_fs9_operation_duration_seconds",
        "keyspace" => keyspace.to_string(),
        "backend" => backend,
        "operation" => operation,
        "result" => result,
    )
    .record(duration.as_secs_f64());
}

pub(crate) fn sample_trigger_queue_depth(keyspace: &str, depth: u64) {
    metrics::gauge!(
        "db9_trigger_queue_depth",
        "keyspace" => keyspace.to_string(),
    )
    .set(depth as f64);
}

pub(crate) fn record_trigger_event(keyspace: &str, event: &'static str) {
    metrics::counter!(
        "db9_trigger_events_total",
        "keyspace" => keyspace.to_string(),
        "event" => event,
    )
    .increment(1);
}

#[allow(dead_code)]
pub(crate) fn record_trigger_execution_duration(
    keyspace: &str,
    result: &'static str,
    duration: std::time::Duration,
) {
    metrics::histogram!(
        "db9_trigger_execution_duration_seconds",
        "keyspace" => keyspace.to_string(),
        "result" => result,
    )
    .record(duration.as_secs_f64());
}

#[allow(dead_code)]
pub(crate) fn record_trigger_gc_run(keyspace: &str) {
    metrics::counter!(
        "db9_trigger_gc_runs_total",
        "keyspace" => keyspace.to_string(),
    )
    .increment(1);
}

pub(crate) fn record_batch_write_atomic_request(result: &'static str) {
    metrics::counter!(
        "db9_batch_write_atomic_requests_total",
        "result" => result,
    )
    .increment(1);
}

pub(crate) fn record_batch_write_atomic_files(files: usize) {
    metrics::counter!("db9_batch_write_atomic_files_total").increment(files as u64);
}

pub(crate) fn record_batch_write_atomic_subgroup_latency(duration: std::time::Duration) {
    metrics::histogram!("db9_batch_write_atomic_subgroup_duration_seconds")
        .record(duration.as_secs_f64());
}

pub(crate) fn record_batch_write_atomic_error(code: &'static str) {
    metrics::counter!(
        "db9_batch_write_atomic_errors_total",
        "code" => code,
    )
    .increment(1);
}

/// Count per-entry failures returned inside a validated `batch_write_atomic` response.
pub(crate) fn record_batch_write_atomic_entry_errors(code: &'static str, count: u64) {
    if count == 0 {
        return;
    }
    metrics::counter!(
        "db9_batch_write_atomic_entry_errors_total",
        "code" => code,
    )
    .increment(count);
}

pub(crate) fn record_hnsw_s3_operation(
    operation: &'static str,
    result: &'static str,
    duration: std::time::Duration,
) {
    metrics::counter!(
        "db9_hnsw_s3_operations_total",
        "operation" => operation,
        "result" => result,
    )
    .increment(1);
    metrics::histogram!(
        "db9_hnsw_s3_operation_duration_seconds",
        "operation" => operation,
        "result" => result,
    )
    .record(duration.as_secs_f64());
}

/// Reserved engine row counter vocabulary; call only from operators that can
/// report a complete, well-defined row category.
#[allow(dead_code)]
pub(crate) fn record_engine_rows(kind: &'static str, rows: u64) {
    metrics::counter!(
        "db9_engine_rows_total",
        "kind" => kind,
    )
    .increment(rows);
}

/// Reserved first-row latency metric; call only when a plan boundary can
/// measure time-to-first-row without double-counting.
#[allow(dead_code)]
pub(crate) fn record_first_row_latency(duration: std::time::Duration) {
    metrics::histogram!("db9_first_row_latency_seconds").record(duration.as_secs_f64());
}

/// Reserved statement memory gauge; call only with a stable memory category
/// measured from a real runtime source.
#[allow(dead_code)]
pub(crate) fn sample_statement_memory_bytes(kind: &'static str, bytes: u64) {
    metrics::gauge!(
        "db9_statement_memory_bytes",
        "kind" => kind,
    )
    .set(bytes as f64);
}

/// Reserved operator memory gauge; call only with low-cardinality operator
/// names and measured memory, not estimates from unrelated code paths.
#[allow(dead_code)]
pub(crate) fn sample_operator_memory_bytes(operator: &'static str, bytes: u64) {
    metrics::gauge!(
        "db9_operator_memory_bytes",
        "operator" => operator,
    )
    .set(bytes as f64);
}

/// Reserved spill byte counter for the external-spill implementation.
#[allow(dead_code)]
pub(crate) fn record_spill_bytes(bytes: u64) {
    metrics::counter!("db9_spill_bytes_total").increment(bytes);
}

pub(crate) fn record_plan_cache_event(event: &'static str) {
    metrics::counter!(
        "db9_plan_cache_events_total",
        "event" => event,
    )
    .increment(1);
}

/// Reserved plan-cache memory gauge; call only after adding a reliable entry
/// size estimator.
#[allow(dead_code)]
pub(crate) fn sample_plan_cache_memory_bytes(bytes: u64) {
    metrics::gauge!("db9_plan_cache_memory_bytes").set(bytes as f64);
}

/// Reserved remote-execution rejection counter; use stable low-cardinality
/// reason labels only.
#[allow(dead_code)]
pub(crate) fn record_remote_reject(reason: &'static str) {
    metrics::counter!(
        "db9_remote_rejects_total",
        "reason" => reason,
    )
    .increment(1);
}

/// Reserved streaming-mode selection counter; use only for finalized execution
/// mode decisions.
#[allow(dead_code)]
pub(crate) fn record_streaming_mode(mode: &'static str) {
    metrics::counter!(
        "db9_streaming_mode_total",
        "mode" => mode,
    )
    .increment(1);
}

/// Reserved spill run counter for the external-spill implementation.
#[allow(dead_code)]
pub(crate) fn record_spill_run() {
    metrics::counter!("db9_spill_runs_total").increment(1);
}

/// Reserved spill pass counter for the external-spill implementation.
#[allow(dead_code)]
pub(crate) fn record_spill_pass() {
    metrics::counter!("db9_spill_passes_total").increment(1);
}

/// Reserved spill cleanup failure counter for the external-spill implementation.
#[allow(dead_code)]
pub(crate) fn record_spill_cleanup_failure() {
    metrics::counter!("db9_spill_cleanup_failures_total").increment(1);
}

/// Maximum concurrent connections to the metrics HTTP server.
const MAX_METRICS_CONNECTIONS: usize = 8;

/// Start a lightweight HTTP server that serves `/internal/metrics`.
///
/// Disabled when the configured address uses port `0`. Uses raw TCP plus
/// manual HTTP parsing to stay consistent with the break-glass admin server
/// pattern without adding another HTTP framework dependency.
pub async fn start_metrics_server(addr: SocketAddr, handle: PrometheusHandle) {
    if addr.port() == 0 {
        info!("Metrics endpoint disabled (DB9_METRICS_ADDR port is 0)");
        return;
    }

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => {
            info!("Prometheus metrics listening on {}", addr);
            l
        }
        Err(e) => {
            warn!("Failed to bind metrics address {}: {}", addr, e);
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

    #[test]
    fn upload_mpsc_send_latency_helper_exports_prometheus_series() {
        let (recorder, handle) = test_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);

        record_upload_mpsc_send_latency(std::time::Duration::from_millis(3));

        let rendered = handle.render();
        assert!(
            rendered.contains("db9_upload_mpsc_send_seconds"),
            "upload mpsc send latency should be exported: {rendered}"
        );
    }

    #[test]
    fn fs9_metric_helpers_export_prometheus_series() {
        let (recorder, handle) = test_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);

        record_fs9_glob_truncated("tenant_a", "stream");
        record_fs9_read_budget_rejected("tenant_a", "fs9_read");
        record_fs9_juicefs_lifecycle("tenant_a", "created");
        sample_fs9_redis_event_queue_depth(7);
        sample_fs9_gc_backoff("tenant_a", 3, 42);
        sample_fs9_orphan_inode_count("tenant_a", 9);
        record_fs9_stats_worker_scan("ok", std::time::Duration::from_millis(25), 11);
        record_fs9_pd_lifecycle_probe("tenant_a", "err");
        record_fs9_operation_latency(
            "tenant_a",
            "embedded",
            "read_file",
            "ok",
            std::time::Duration::from_millis(5),
        );

        let rendered = handle.render();
        assert!(
            rendered
                .contains(r#"db9_fs9_glob_truncations_total{keyspace="tenant_a",mode="stream"} 1"#),
            "{rendered}"
        );
        assert!(rendered.contains(r#"db9_fs9_read_budget_rejections_total{keyspace="tenant_a",operation="fs9_read"} 1"#), "{rendered}");
        assert!(
            rendered.contains(
                r#"db9_fs9_juicefs_lifecycle_total{tenant_id="tenant_a",result="created"} 1"#
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("db9_fs9_redis_event_queue_depth 7"),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_fs9_gc_backoff_failures{keyspace="tenant_a"} 3"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_fs9_orphan_inodes{keyspace="tenant_a"} 9"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_fs9_stats_worker_scans_total{result="ok"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_fs9_stats_worker_staleness_seconds{result="ok"} 11"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                r#"db9_fs9_pd_lifecycle_probe_total{keyspace="tenant_a",result="err"} 1"#
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_fs9_operation_duration_seconds"#),
            "{rendered}"
        );
    }

    #[test]
    fn trigger_batch_hnsw_and_planner_metric_helpers_export_prometheus_series() {
        let (recorder, handle) = test_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);

        sample_trigger_queue_depth("tenant_a", 4);
        record_trigger_event("tenant_a", "completed");
        record_trigger_execution_duration("tenant_a", "ok", std::time::Duration::from_millis(12));
        record_trigger_gc_run("tenant_a");

        record_batch_write_atomic_request("ok");
        record_batch_write_atomic_files(3);
        record_batch_write_atomic_subgroup_latency(std::time::Duration::from_millis(8));
        record_batch_write_atomic_error("execution.txn_conflict");

        record_hnsw_s3_operation("get", "ok", std::time::Duration::from_millis(9));
        record_hnsw_s3_operation("put", "err", std::time::Duration::from_millis(10));

        record_engine_rows("scanned", 13);
        record_engine_rows("decoded", 12);
        record_engine_rows("fetched_base", 3);
        record_first_row_latency(std::time::Duration::from_millis(2));
        sample_statement_memory_bytes("peak", 1024);
        sample_operator_memory_bytes("sort", 2048);
        sample_operator_memory_bytes("aggregate", 4096);
        record_spill_bytes(8192);
        record_plan_cache_event("hit");
        sample_plan_cache_memory_bytes(16384);
        record_remote_reject("capability_mismatch");
        record_streaming_mode("local");
        record_spill_run();
        record_spill_pass();
        record_spill_cleanup_failure();

        let rendered = handle.render();
        assert!(
            rendered.contains(r#"db9_trigger_queue_depth{keyspace="tenant_a"} 4"#),
            "{rendered}"
        );
        assert!(
            rendered
                .contains(r#"db9_trigger_events_total{keyspace="tenant_a",event="completed"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_trigger_gc_runs_total{keyspace="tenant_a"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_batch_write_atomic_requests_total{result="ok"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains("db9_batch_write_atomic_files_total 3"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                r#"db9_batch_write_atomic_errors_total{code="execution.txn_conflict"} 1"#
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_hnsw_s3_operations_total{operation="get",result="ok"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_hnsw_s3_operations_total{operation="put",result="err"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_engine_rows_total{kind="scanned"} 13"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_plan_cache_events_total{event="hit"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains("db9_plan_cache_memory_bytes 16384"),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_remote_rejects_total{reason="capability_mismatch"} 1"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"db9_streaming_mode_total{mode="local"} 1"#),
            "{rendered}"
        );
        assert!(rendered.contains("db9_spill_runs_total 1"), "{rendered}");
        assert!(rendered.contains("db9_spill_passes_total 1"), "{rendered}");
        assert!(
            rendered.contains("db9_spill_cleanup_failures_total 1"),
            "{rendered}"
        );
    }

    #[test]
    fn batch_write_atomic_request_and_entry_errors_are_separate_units() {
        let (recorder, handle) = test_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);

        record_batch_write_atomic_error("validation");
        record_batch_write_atomic_entry_errors("execution.txn_conflict", 3);

        let rendered = handle.render();
        assert!(
            rendered.contains(r#"db9_batch_write_atomic_errors_total{code="validation"} 1"#),
            "request-level errors should stay on errors_total: {rendered}"
        );
        assert!(
            rendered.contains(
                r#"db9_batch_write_atomic_entry_errors_total{code="execution.txn_conflict"} 3"#
            ),
            "entry-level errors should use a separate counter and preserve count: {rendered}"
        );
    }
}
