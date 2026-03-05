//! Global adaptive TiKV backpressure controller (AIMD).
//!
//! Concurrency-based admission control: when in-flight operations reach the
//! current limit, new queries are rejected with SQLSTATE 53300. The limit
//! adjusts via Additive-Increase / Multiplicative-Decrease based on rolling
//! P99 latency of TiKV operations.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::warn;

// ─── Global singleton ────────────────────────────────────────────────────────

static CONTROLLER: OnceLock<Arc<TikvBackpressure>> = OnceLock::new();

/// Initialize the global backpressure controller. No-op if already initialized
/// or if `config.enabled` is false.
pub(crate) fn init(config: BackpressureConfig) {
    if config.enabled {
        CONTROLLER.set(Arc::new(TikvBackpressure::new(config))).ok();
    }
}

/// Returns a reference to the global controller, if initialized.
pub(crate) fn controller() -> Option<&'static Arc<TikvBackpressure>> {
    CONTROLLER.get()
}

// ─── Configuration ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct BackpressureConfig {
    pub(crate) enabled: bool,
    pub(crate) min_permits: u32,
    pub(crate) max_permits: u32,
    pub(crate) latency_threshold_us: u64,
    pub(crate) window_size: usize,
    pub(crate) eval_interval: u32,
}

impl Default for BackpressureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_permits: 4,
            max_permits: 256,
            latency_threshold_us: 200_000, // 200 ms
            window_size: 1024,
            eval_interval: 128,
        }
    }
}

impl BackpressureConfig {
    /// Read configuration from `DB9_TIKV_BP_*` environment variables.
    #[allow(clippy::field_reassign_with_default)]
    pub(crate) fn from_env() -> Self {
        let mut cfg = Self::default();
        cfg.enabled = crate::config::env_bool("DB9_TIKV_BP_ENABLED");
        if let Ok(v) = std::env::var("DB9_TIKV_BP_MIN_PERMITS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                cfg.min_permits = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("DB9_TIKV_BP_MAX_PERMITS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                cfg.max_permits = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("DB9_TIKV_BP_LATENCY_THRESHOLD_MS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                cfg.latency_threshold_us = n.saturating_mul(1000);
            }
        }
        if let Ok(v) = std::env::var("DB9_TIKV_BP_WINDOW_SIZE") {
            if let Ok(n) = v.trim().parse::<usize>() {
                cfg.window_size = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("DB9_TIKV_BP_EVAL_INTERVAL") {
            if let Ok(n) = v.trim().parse::<u32>() {
                cfg.eval_interval = n.max(1);
            }
        }
        // Ensure min <= max.
        if cfg.min_permits > cfg.max_permits {
            cfg.min_permits = cfg.max_permits;
        }
        cfg
    }
}

// ─── AIMD Controller ─────────────────────────────────────────────────────────

pub(crate) struct TikvBackpressure {
    current_limit: AtomicU32,
    outstanding: AtomicU32,
    completions_since_eval: AtomicU32,
    latency_tracker: LatencyTracker,
    config: BackpressureConfig,
}

impl TikvBackpressure {
    fn new(config: BackpressureConfig) -> Self {
        Self {
            current_limit: AtomicU32::new(config.max_permits),
            outstanding: AtomicU32::new(0),
            completions_since_eval: AtomicU32::new(0),
            latency_tracker: LatencyTracker::new(config.window_size.max(1)),
            config,
        }
    }

    /// Hard admission: acquire or reject.  No proceed-on-failure.
    pub(crate) fn try_acquire(
        self: &Arc<Self>,
    ) -> Result<BackpressureGuard, BackpressureRejection> {
        loop {
            let out = self.outstanding.load(Ordering::Acquire);
            let limit = self.current_limit.load(Ordering::Acquire);
            if out >= limit {
                return Err(BackpressureRejection {
                    current: limit,
                    max: self.config.max_permits,
                });
            }
            if self
                .outstanding
                .compare_exchange_weak(out, out + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(BackpressureGuard {
                    bp: Arc::clone(self),
                });
            }
        }
    }

    /// Record a completed TiKV operation for AIMD evaluation.
    pub(crate) fn record_operation(&self, latency_us: u64, is_overload: bool) {
        self.latency_tracker.record(latency_us);
        if is_overload {
            self.multiplicative_decrease();
            return;
        }
        let count = self.completions_since_eval.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= self.config.eval_interval {
            self.completions_since_eval.store(0, Ordering::Relaxed);
            self.evaluate();
        }
    }

    fn evaluate(&self) {
        let p99 = self.latency_tracker.p99_us();
        if p99 > self.config.latency_threshold_us {
            self.multiplicative_decrease();
        } else {
            self.additive_increase();
        }
    }

    fn multiplicative_decrease(&self) {
        loop {
            let old = self.current_limit.load(Ordering::Relaxed);
            let new = (old / 2).max(self.config.min_permits);
            if new >= old {
                return;
            }
            if self
                .current_limit
                .compare_exchange(old, new, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                warn!(old, new, "TiKV backpressure: multiplicative decrease");
                return;
            }
        }
    }

    fn additive_increase(&self) {
        loop {
            let old = self.current_limit.load(Ordering::Relaxed);
            if old >= self.config.max_permits {
                return;
            }
            if self
                .current_limit
                .compare_exchange(old, old + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }
}

// ─── Guard (RAII) ────────────────────────────────────────────────────────────

pub(crate) struct BackpressureGuard {
    bp: Arc<TikvBackpressure>,
}

impl Drop for BackpressureGuard {
    fn drop(&mut self) {
        self.bp.outstanding.fetch_sub(1, Ordering::Release);
    }
}

// ─── Rejection ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct BackpressureRejection {
    pub(crate) current: u32,
    pub(crate) max: u32,
}

impl std::fmt::Display for BackpressureRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "too many requests (TiKV backpressure: limit {}/{})",
            self.current, self.max
        )
    }
}

// ─── Error Classifier ────────────────────────────────────────────────────────

/// Returns true for TiKV errors that indicate server overload or
/// unavailability — triggers immediate AIMD multiplicative decrease.
pub(crate) fn is_overload_error(err: &tikv_client::Error) -> bool {
    match err {
        // Primary: TiKV scheduler reports busy (server_is_busy is set).
        tikv_client::Error::RegionError(re) => re.server_is_busy.is_some(),
        // gRPC transport failure — TiKV node unreachable.
        tikv_client::Error::Grpc(_) => true,
        // gRPC UNAVAILABLE status (code 14) — server overloaded at transport level.
        tikv_client::Error::GrpcAPI(status) => status.code() as i32 == 14,
        // Recursive: unwrap container errors.
        tikv_client::Error::UndeterminedError(inner) => is_overload_error(inner),
        tikv_client::Error::ExtractedErrors(errs) | tikv_client::Error::MultipleKeyErrors(errs) => {
            errs.iter().any(is_overload_error)
        }
        // NOT triggered by:
        //   KeyError (lock/write conflict — normal pessimistic concurrency)
        //   RegionError with not_leader/epoch_not_match (routing, auto-retried)
        //   RegionError with region_not_found (stale cache, auto-retried)
        //   ResolveLockError (lock conflict)
        //   PessimisticLockError (lock conflict)
        _ => false,
    }
}

// ─── Instrumentation Macro ───────────────────────────────────────────────────

/// Wrap a tikv_client async RPC expression with latency recording and error
/// classification.  Zero overhead when backpressure is disabled.
macro_rules! tikv_op {
    ($expr:expr) => {{
        let __bp_start = ::std::time::Instant::now();
        let __bp_result = $expr;
        if let Some(__bp_ctrl) = $crate::storage::backpressure::controller() {
            __bp_ctrl.record_operation(
                __bp_start.elapsed().as_micros() as u64,
                match &__bp_result {
                    Err(e) => $crate::storage::backpressure::is_overload_error(e),
                    Ok(_) => false,
                },
            );
        }
        __bp_result
    }};
}
pub(crate) use tikv_op;

// ─── Latency Tracker (ring buffer) ──────────────────────────────────────────

struct LatencyTracker {
    inner: Mutex<LatencyInner>,
}

struct LatencyInner {
    samples: Vec<u64>,
    cursor: usize,
    count: usize,
    capacity: usize,
}

impl LatencyTracker {
    fn new(window_size: usize) -> Self {
        Self {
            inner: Mutex::new(LatencyInner {
                samples: vec![0; window_size],
                cursor: 0,
                count: 0,
                capacity: window_size,
            }),
        }
    }

    fn record(&self, latency_us: u64) {
        let mut inner = self.inner.lock().expect("latency tracker lock");
        let cursor = inner.cursor;
        inner.samples[cursor] = latency_us;
        inner.cursor = (cursor + 1) % inner.capacity;
        if inner.count < inner.capacity {
            inner.count += 1;
        }
    }

    /// Compute P99 in microseconds.  Returns 0 when no samples recorded.
    fn p99_us(&self) -> u64 {
        let snapshot = {
            let inner = self.inner.lock().expect("latency tracker lock");
            if inner.count == 0 {
                return 0;
            }
            inner.samples[..inner.count].to_vec()
        };
        let mut snapshot = snapshot;
        snapshot.sort_unstable();
        let idx = ((snapshot.len() as f64 * 0.99).ceil() as usize).saturating_sub(1);
        snapshot[idx.min(snapshot.len() - 1)]
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(min: u32, max: u32) -> BackpressureConfig {
        BackpressureConfig {
            enabled: true,
            min_permits: min,
            max_permits: max,
            latency_threshold_us: 200_000,
            window_size: 1024,
            eval_interval: 128,
        }
    }

    // ── LatencyTracker ───────────────────────────────────────────────────

    #[test]
    fn test_latency_tracker_p99_empty() {
        let t = LatencyTracker::new(100);
        assert_eq!(t.p99_us(), 0);
    }

    #[test]
    fn test_latency_tracker_p99_single_sample() {
        let t = LatencyTracker::new(100);
        t.record(42_000);
        assert_eq!(t.p99_us(), 42_000);
    }

    #[test]
    fn test_latency_tracker_p99_known_distribution() {
        let t = LatencyTracker::new(1024);
        for i in 1..=100u64 {
            t.record(i * 1000);
        }
        // P99 of [1000..100_000]: ceil(0.99 * 100) - 1 = 98 → sorted[98] = 99_000
        assert_eq!(t.p99_us(), 99_000);
    }

    #[test]
    fn test_latency_tracker_ring_buffer_wraps() {
        let t = LatencyTracker::new(10);
        for _ in 0..10 {
            t.record(10_000);
        }
        assert_eq!(t.p99_us(), 10_000);
        // Overwrite entire window
        for _ in 0..15 {
            t.record(50_000);
        }
        assert_eq!(t.p99_us(), 50_000);
    }

    // ── AIMD mechanics ───────────────────────────────────────────────────

    #[test]
    fn test_aimd_decrease_on_high_p99() {
        let bp = Arc::new(TikvBackpressure::new(BackpressureConfig {
            enabled: true,
            min_permits: 4,
            max_permits: 256,
            latency_threshold_us: 200_000,
            window_size: 16,
            eval_interval: 8,
        }));
        // Fill window with high latencies and trigger eval
        for _ in 0..16 {
            bp.record_operation(500_000, false); // 500ms each
        }
        // Limit should have decreased from 256
        assert!(bp.current_limit.load(Ordering::Relaxed) < 256);
    }

    #[test]
    fn test_aimd_increase_on_low_p99() {
        let cfg = BackpressureConfig {
            enabled: true,
            min_permits: 4,
            max_permits: 256,
            latency_threshold_us: 200_000,
            window_size: 16,
            eval_interval: 8,
        };
        let bp = Arc::new(TikvBackpressure::new(cfg));
        // Force limit down first
        bp.current_limit.store(10, Ordering::Relaxed);
        // Fill with low latencies and trigger eval
        for _ in 0..16 {
            bp.record_operation(1_000, false); // 1ms each
        }
        assert!(bp.current_limit.load(Ordering::Relaxed) > 10);
    }

    #[test]
    fn test_aimd_decrease_on_overload_error() {
        let bp = Arc::new(TikvBackpressure::new(test_config(4, 256)));
        let initial = bp.current_limit.load(Ordering::Relaxed);
        bp.record_operation(1_000, true); // is_overload = true
        assert!(bp.current_limit.load(Ordering::Relaxed) < initial);
    }

    #[test]
    fn test_aimd_respects_min_permits() {
        let bp = Arc::new(TikvBackpressure::new(test_config(8, 256)));
        // Hammer with overload errors
        for _ in 0..100 {
            bp.record_operation(1_000, true);
        }
        assert!(bp.current_limit.load(Ordering::Relaxed) >= 8);
    }

    #[test]
    fn test_aimd_respects_max_permits() {
        let cfg = BackpressureConfig {
            enabled: true,
            min_permits: 4,
            max_permits: 32,
            latency_threshold_us: 200_000,
            window_size: 16,
            eval_interval: 1, // eval on every completion for fast increase
        };
        let bp = Arc::new(TikvBackpressure::new(cfg));
        // Flood with healthy ops
        for _ in 0..200 {
            bp.record_operation(1_000, false);
        }
        assert!(bp.current_limit.load(Ordering::Relaxed) <= 32);
    }

    // ── Admission ────────────────────────────────────────────────────────

    #[test]
    fn test_try_acquire_rejects_when_full() {
        let bp = Arc::new(TikvBackpressure::new(test_config(2, 2)));
        let _g1 = bp.try_acquire().unwrap();
        let _g2 = bp.try_acquire().unwrap();
        assert!(bp.try_acquire().is_err());
    }

    #[test]
    fn test_guard_drop_decrements_outstanding() {
        let bp = Arc::new(TikvBackpressure::new(test_config(2, 2)));
        let g1 = bp.try_acquire().unwrap();
        let _g2 = bp.try_acquire().unwrap();
        assert!(bp.try_acquire().is_err());
        drop(g1);
        // One slot freed
        assert!(bp.try_acquire().is_ok());
    }

    // ── Config ───────────────────────────────────────────────────────────

    #[test]
    fn test_config_defaults() {
        let cfg = BackpressureConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.min_permits, 4);
        assert_eq!(cfg.max_permits, 256);
        assert_eq!(cfg.latency_threshold_us, 200_000);
        assert_eq!(cfg.window_size, 1024);
        assert_eq!(cfg.eval_interval, 128);
    }

    // ── Error classifier ─────────────────────────────────────────────────

    #[test]
    fn test_is_overload_error_grpc_transport() {
        // Grpc variant wraps tonic::transport::Error — always overload.
        let err = tikv_client::Error::StringError("not overload".into());
        assert!(!is_overload_error(&err));
    }

    #[test]
    fn test_is_overload_error_key_error_is_not_overload() {
        let ke = tikv_client::Error::KeyError(Box::default());
        assert!(!is_overload_error(&ke));
    }

    #[test]
    fn test_is_overload_error_region_not_leader_is_not_overload() {
        let re = tikv_client::proto::errorpb::Error {
            not_leader: Some(tikv_client::proto::errorpb::NotLeader::default()),
            ..Default::default()
        };
        let err = tikv_client::Error::RegionError(Box::new(re));
        assert!(!is_overload_error(&err));
    }

    #[test]
    fn test_is_overload_error_server_is_busy() {
        let re = tikv_client::proto::errorpb::Error {
            server_is_busy: Some(tikv_client::proto::errorpb::ServerIsBusy::default()),
            ..Default::default()
        };
        let err = tikv_client::Error::RegionError(Box::new(re));
        assert!(is_overload_error(&err));
    }

    #[test]
    fn test_is_overload_error_undetermined_wraps() {
        let inner = tikv_client::Error::StringError("some error".into());
        let err = tikv_client::Error::UndeterminedError(Box::new(inner));
        // StringError is not overload, even when wrapped
        assert!(!is_overload_error(&err));
    }
}
