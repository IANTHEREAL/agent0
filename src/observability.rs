// TODO(#2335): migrate to parking_lot — phase 2/3
#![allow(clippy::disallowed_types)]

use std::collections::{HashMap, VecDeque};
use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const WINDOW_SECS: u64 = 60 * 60;
const BUCKET_SECS: u64 = 60;
const NUM_BUCKETS: usize = (WINDOW_SECS / BUCKET_SECS) as usize;

const SUB_BINS: u32 = 8;
const MAX_EXP: u32 = 32; // 2^32us ~= 71 minutes
const NUM_BINS: usize = (MAX_EXP as usize + 1) * (SUB_BINS as usize);

const DEFAULT_SAMPLE_EVERY: u64 = 1000; // 0.1%
const DEFAULT_SLOW_MS: u64 = 200;
const DEFAULT_MAX_SAMPLE_EVENTS: usize = 20_000;
const DEFAULT_MAX_SAMPLE_GROUPS: usize = 50;
const DEFAULT_MAX_SQL_LEN: usize = 512;
// Expensive-query (memory) log. Threshold matches TiDB's `tidb_mem_quota_query`
// default (1 GiB). SQL truncation is a separate, larger limit than the
// slow-query log (which is high-volume) so rare forensic lines keep full
// GROUP BY / WHERE context. See #2557.
const DEFAULT_EXPENSIVE_MEM_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_EXPENSIVE_SQL_LEN: usize = 4096;

#[derive(Debug, Clone)]
pub struct ObservabilityConfig {
    pub enabled: bool,
    pub sample_every: u64,
    pub slow_query_threshold_us: u64,
    pub max_sample_events: usize,
    pub max_sample_groups: usize,
    pub max_sql_len: usize,
    /// Peak per-statement memory (bytes) that triggers the `expensive_query`
    /// log. `0` disables it. Independent of `enabled` (see
    /// [`expensive_mem_threshold_bytes`]).
    pub expensive_mem_threshold_bytes: u64,
    /// SQL truncation length for the `expensive_query` log only.
    pub expensive_sql_max_len: usize,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_every: DEFAULT_SAMPLE_EVERY,
            slow_query_threshold_us: DEFAULT_SLOW_MS * 1000,
            max_sample_events: DEFAULT_MAX_SAMPLE_EVENTS,
            max_sample_groups: DEFAULT_MAX_SAMPLE_GROUPS,
            max_sql_len: DEFAULT_MAX_SQL_LEN,
            expensive_mem_threshold_bytes: DEFAULT_EXPENSIVE_MEM_BYTES,
            expensive_sql_max_len: DEFAULT_EXPENSIVE_SQL_LEN,
        }
    }
}

impl ObservabilityConfig {
    fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(v) = env::var("DB9_OBS_ENABLED") {
            cfg.enabled = parse_bool(&v).unwrap_or(cfg.enabled);
        }
        if let Ok(v) = env::var("DB9_OBS_SAMPLE_EVERY") {
            cfg.sample_every = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.sample_every);
        }
        if let Ok(v) = env::var("DB9_OBS_SLOW_MS") {
            cfg.slow_query_threshold_us = v
                .parse::<u64>()
                .ok()
                .map(|ms| ms.saturating_mul(1000))
                .unwrap_or(cfg.slow_query_threshold_us);
        }
        if let Ok(v) = env::var("DB9_OBS_MAX_SAMPLE_EVENTS") {
            cfg.max_sample_events = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.max_sample_events);
        }
        if let Ok(v) = env::var("DB9_OBS_MAX_SAMPLE_GROUPS") {
            cfg.max_sample_groups = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.max_sample_groups);
        }
        if let Ok(v) = env::var("DB9_OBS_MAX_SQL_LEN") {
            cfg.max_sql_len = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.max_sql_len);
        }
        if let Ok(v) = env::var("DB9_OBS_EXPENSIVE_MEM_MB") {
            // `0` is valid and disables the log, so do not filter it out.
            if let Ok(mb) = v.parse::<u64>() {
                cfg.expensive_mem_threshold_bytes = mb.saturating_mul(1024 * 1024);
            }
        }
        if let Ok(v) = env::var("DB9_OBS_EXPENSIVE_SQL_LEN") {
            cfg.expensive_sql_max_len = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.expensive_sql_max_len);
        }

        cfg
    }
}

pub struct ObservabilityRegistry {
    config: ObservabilityConfig,
    tenants: Mutex<HashMap<String, Arc<TenantObservability>>>,
}

impl ObservabilityRegistry {
    fn new() -> Self {
        Self {
            config: ObservabilityConfig::from_env(),
            tenants: Mutex::new(HashMap::new()),
        }
    }

    pub fn tenant(&self, keyspace: &str) -> Arc<TenantObservability> {
        let key = if keyspace.is_empty() {
            "default".to_string()
        } else {
            keyspace.to_string()
        };

        let mut guard = self.tenants.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.get(&key) {
            return existing.clone();
        }

        let tenant = Arc::new(TenantObservability::new(self.config.clone()));
        guard.insert(key, tenant.clone());
        tenant
    }
}

static REGISTRY: OnceLock<ObservabilityRegistry> = OnceLock::new();

pub fn registry() -> &'static ObservabilityRegistry {
    REGISTRY.get_or_init(ObservabilityRegistry::new)
}

pub struct ConnectionGuard {
    tenant: Arc<TenantObservability>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.tenant
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
pub struct SummarySnapshot {
    pub window_seconds: u64,
    pub statement_count: u64,
    pub txn_commit_count: u64,
    pub error_count: u64,
    pub rate_limited_count: u64,
    pub retry_attempts: u64,
    pub retry_budget_exhausted: u64,
    pub retry_timeout_aborts: u64,
    /// Per-reason retry counts: [Unknown, Optimistic, PessimisticRetry,
    /// SelfRolledBack, RcCheckTs, LazyUniquenessCheck].
    pub retry_conflict_by_reason: [u64; 6],
    pub hnsw_graph_bytes_written: u64,
    pub hnsw_serialize_duration_us: u64,
    pub qps: f64,
    pub tps: f64,
    pub latency_avg_ms: f64,
    pub latency_p99_ms: f64,
    pub active_connections: u64,
}

#[derive(Debug, Clone)]
pub struct QuerySampleGroup {
    pub query: String,
    pub sample_count: u64,
    pub error_count: u64,
    pub latency_avg_ms: f64,
    pub latency_p99_ms: f64,
    pub latency_max_ms: f64,
    pub last_seen_ms_ago: u64,
}

pub struct TenantObservability {
    config: ObservabilityConfig,
    active_connections: AtomicU64,
    rate_limited_count: AtomicU64,
    /// Total individual retry attempts across all statements (write-conflict retries).
    retry_attempts: AtomicU64,
    /// Statements that exhausted the retry count budget without succeeding.
    retry_budget_exhausted: AtomicU64,
    /// Statements aborted by the `db9.retry_timeout` wall-time ceiling.
    retry_timeout_aborts: AtomicU64,
    /// Retry attempts by WriteConflict reason code (indices 0..=5 map to
    /// kvrpcpb::write_conflict::Reason: Unknown, Optimistic, PessimisticRetry,
    /// SelfRolledBack, RcCheckTs, LazyUniquenessCheck).
    retry_conflict_by_reason: [AtomicU64; 6],
    /// Cumulative HNSW graph bytes written to TiKV.
    hnsw_graph_bytes_written: AtomicU64,
    /// Cumulative HNSW serialization time in microseconds.
    hnsw_serialize_duration_us: AtomicU64,
    // Box to avoid stack overflow: RollingWindow is ~130KB (60 buckets × 264 AtomicU64 bins each)
    window: Box<RollingWindow>,
    samples: Mutex<VecDeque<SampleEvent>>,
}

impl TenantObservability {
    fn new(config: ObservabilityConfig) -> Self {
        Self {
            config,
            active_connections: AtomicU64::new(0),
            rate_limited_count: AtomicU64::new(0),
            retry_attempts: AtomicU64::new(0),
            retry_budget_exhausted: AtomicU64::new(0),
            retry_timeout_aborts: AtomicU64::new(0),
            retry_conflict_by_reason: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            hnsw_graph_bytes_written: AtomicU64::new(0),
            hnsw_serialize_duration_us: AtomicU64::new(0),
            window: Box::new(RollingWindow::new()),
            samples: Mutex::new(VecDeque::new()),
        }
    }

    pub fn connection_open(self: &Arc<Self>) -> ConnectionGuard {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard {
            tenant: self.clone(),
        }
    }

    pub fn record_rate_limited(&self) {
        self.rate_limited_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a single retry attempt (write-conflict retry).
    ///
    /// `conflict_reason` is the `kvrpcpb::write_conflict::Reason` value (0..=5),
    /// or `None` if the reason could not be extracted from the error.
    pub fn record_retry_attempt(&self, conflict_reason: Option<i32>) {
        metrics::counter!("db9_server_write_conflict_retries_total").increment(1);
        self.retry_attempts.fetch_add(1, Ordering::Relaxed);
        if let Some(r) = conflict_reason {
            let idx = r.clamp(0, 5) as usize;
            self.retry_conflict_by_reason[idx].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record that a statement exhausted its retry budget without succeeding.
    pub fn record_retry_budget_exhausted(&self) {
        self.retry_budget_exhausted.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that a statement was aborted by the retry timeout ceiling.
    pub fn record_retry_timeout_abort(&self) {
        self.retry_timeout_aborts.fetch_add(1, Ordering::Relaxed);
    }

    /// Record HNSW graph serialization: bytes written and time spent.
    pub fn record_hnsw_serialize(&self, graph_bytes: u64, duration_us: u64) {
        self.hnsw_graph_bytes_written
            .fetch_add(graph_bytes, Ordering::Relaxed);
        self.hnsw_serialize_duration_us
            .fetch_add(duration_us, Ordering::Relaxed);
    }

    pub fn record_commit(&self) {
        if !self.config.enabled {
            return;
        }
        let now = now_ms();
        let minute = now / 1000 / BUCKET_SECS;
        self.window.record_commit(minute);
    }

    pub fn record_statement<F>(&self, latency: Duration, ok: bool, sql_supplier: F)
    where
        F: FnOnce() -> String,
    {
        // Prometheus metrics: always emitted regardless of DB9_OBS_ENABLED.
        // DB9_OBS_ENABLED controls the in-memory per-tenant sampling/windowing
        // (virtual tables), not infrastructure-level Prometheus counters.
        // When no Prometheus recorder is installed these are no-ops.
        metrics::histogram!("db9_server_query_duration_seconds").record(latency.as_secs_f64());
        metrics::counter!("db9_server_statements_total").increment(1);
        if !ok {
            metrics::counter!("db9_server_query_errors_total").increment(1);
        }

        let latency_us = duration_to_us(latency);
        self.record_statement_us(latency_us, ok, sql_supplier);
    }

    pub fn record_statement_us<F>(&self, latency_us: u64, ok: bool, sql_supplier: F)
    where
        F: FnOnce() -> String,
    {
        if !self.config.enabled {
            return;
        }

        let now = now_ms();
        let minute = now / 1000 / BUCKET_SECS;
        self.window.record_statement(minute, latency_us, ok);

        let is_slow = latency_us >= self.config.slow_query_threshold_us;

        if self.should_sample(latency_us, ok) {
            let mut sql = normalize_sql(&sql_supplier(), self.config.max_sql_len);
            if sql.is_empty() {
                return;
            }
            sql = redact_sensitive_sql(&sql);
            if is_observability_system_sql(&sql) {
                return;
            }

            if is_slow {
                // Bonus: enrich the slow-query log with the statement's peak memory
                // (free, from the same accounting infrastructure as expensive_query).
                let peak_mb = crate::pool::peak_bytes_snapshot().unwrap_or(0) / (1024 * 1024);
                tracing::warn!(
                    latency_ms = latency_us / 1000,
                    peak_mb,
                    ok,
                    "slow query: {sql}"
                );
            }

            if sql.contains('|') {
                sql = sql.replace('|', " ");
            }

            let ev = SampleEvent {
                at_ms: now,
                latency_us,
                ok,
                fingerprint: fnv1a_64(sql.as_bytes()),
                query: sql,
            };

            let mut guard = self.samples.lock().unwrap_or_else(|e| e.into_inner());
            prune_samples(&mut guard, now);
            guard.push_back(ev);
            while guard.len() > self.config.max_sample_events {
                guard.pop_front();
            }
        }
    }

    pub fn snapshot_summary(&self) -> SummarySnapshot {
        let now = now_ms();
        let uptime_secs = (now / 1000).max(1);
        let window_seconds = uptime_secs.min(WINDOW_SECS);

        let minute = now / 1000 / BUCKET_SECS;
        let snap = self.window.snapshot(minute);

        let statement_count = snap.statement_count;
        let txn_commit_count = snap.txn_commit_count;
        let error_count = snap.error_count;

        let qps = statement_count as f64 / window_seconds as f64;
        let tps = txn_commit_count as f64 / window_seconds as f64;

        SummarySnapshot {
            window_seconds,
            statement_count,
            txn_commit_count,
            error_count,
            rate_limited_count: self.rate_limited_count.load(Ordering::Relaxed),
            retry_attempts: self.retry_attempts.load(Ordering::Relaxed),
            retry_budget_exhausted: self.retry_budget_exhausted.load(Ordering::Relaxed),
            retry_timeout_aborts: self.retry_timeout_aborts.load(Ordering::Relaxed),
            retry_conflict_by_reason: [
                self.retry_conflict_by_reason[0].load(Ordering::Relaxed),
                self.retry_conflict_by_reason[1].load(Ordering::Relaxed),
                self.retry_conflict_by_reason[2].load(Ordering::Relaxed),
                self.retry_conflict_by_reason[3].load(Ordering::Relaxed),
                self.retry_conflict_by_reason[4].load(Ordering::Relaxed),
                self.retry_conflict_by_reason[5].load(Ordering::Relaxed),
            ],
            hnsw_graph_bytes_written: self.hnsw_graph_bytes_written.load(Ordering::Relaxed),
            hnsw_serialize_duration_us: self.hnsw_serialize_duration_us.load(Ordering::Relaxed),
            qps,
            tps,
            latency_avg_ms: snap.latency_avg_ms,
            latency_p99_ms: snap.latency_p99_ms,
            active_connections: self.active_connections.load(Ordering::Relaxed),
        }
    }

    pub fn snapshot_query_samples(&self) -> Vec<QuerySampleGroup> {
        if !self.config.enabled {
            return Vec::new();
        }

        let now = now_ms();
        let mut guard = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        prune_samples(&mut guard, now);

        let mut agg: HashMap<u64, SampleAgg> = HashMap::new();
        for ev in guard.iter() {
            let entry = agg.entry(ev.fingerprint).or_insert_with(|| SampleAgg {
                query: ev.query.clone(),
                count: 0,
                error_count: 0,
                sum_us: 0,
                max_us: 0,
                last_seen_ms: 0,
                latencies_us: Vec::new(),
            });
            entry.count += 1;
            if !ev.ok {
                entry.error_count += 1;
            }
            entry.sum_us = entry.sum_us.saturating_add(ev.latency_us);
            entry.max_us = entry.max_us.max(ev.latency_us);
            entry.last_seen_ms = entry.last_seen_ms.max(ev.at_ms);
            entry.latencies_us.push(ev.latency_us);
        }

        let mut groups: Vec<QuerySampleGroup> = agg
            .into_values()
            .map(|mut s| {
                s.latencies_us.sort_unstable();
                let p99_us = quantile_us_sorted(&s.latencies_us, 0.99);
                let avg_ms = if s.count == 0 {
                    0.0
                } else {
                    (s.sum_us as f64 / s.count as f64) / 1000.0
                };
                QuerySampleGroup {
                    query: s.query,
                    sample_count: s.count,
                    error_count: s.error_count,
                    latency_avg_ms: avg_ms,
                    latency_p99_ms: p99_us as f64 / 1000.0,
                    latency_max_ms: s.max_us as f64 / 1000.0,
                    last_seen_ms_ago: now.saturating_sub(s.last_seen_ms),
                }
            })
            .collect();

        groups.sort_by_key(|g| std::cmp::Reverse(g.sample_count));
        groups.truncate(self.config.max_sample_groups);
        groups
    }

    fn should_sample(&self, latency_us: u64, ok: bool) -> bool {
        if !ok {
            return true;
        }
        if latency_us >= self.config.slow_query_threshold_us {
            return true;
        }
        let every = self.config.sample_every;
        if every <= 1 {
            return true;
        }
        fast_rand_u64().is_multiple_of(every)
    }
}

struct SampleEvent {
    at_ms: u64,
    latency_us: u64,
    ok: bool,
    fingerprint: u64,
    query: String,
}

struct SampleAgg {
    query: String,
    count: u64,
    error_count: u64,
    sum_us: u64,
    max_us: u64,
    last_seen_ms: u64,
    latencies_us: Vec<u64>,
}

struct RollingWindow {
    buckets: [Bucket; NUM_BUCKETS],
}

impl RollingWindow {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| Bucket::new()),
        }
    }

    fn record_statement(&self, minute: u64, latency_us: u64, ok: bool) {
        let b = &self.buckets[(minute as usize) % NUM_BUCKETS];
        b.ensure_minute(minute);
        b.statement_count.fetch_add(1, Ordering::Relaxed);
        b.latency_sum_us.fetch_add(latency_us, Ordering::Relaxed);
        if !ok {
            b.error_count.fetch_add(1, Ordering::Relaxed);
        }
        let idx = latency_us_to_bin(latency_us);
        b.latency_bins[idx].fetch_add(1, Ordering::Relaxed);
    }

    fn record_commit(&self, minute: u64) {
        let b = &self.buckets[(minute as usize) % NUM_BUCKETS];
        b.ensure_minute(minute);
        b.txn_commit_count.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self, now_minute: u64) -> WindowSnapshot {
        let oldest = now_minute.saturating_sub((NUM_BUCKETS as u64).saturating_sub(1));

        let mut statement_count = 0u64;
        let mut txn_commit_count = 0u64;
        let mut error_count = 0u64;
        let mut latency_sum_us = 0u64;
        let mut bins = vec![0u64; NUM_BINS];

        for b in &self.buckets {
            let m = b.minute.load(Ordering::Acquire);
            if m < oldest || m > now_minute {
                continue;
            }
            statement_count =
                statement_count.saturating_add(b.statement_count.load(Ordering::Relaxed));
            txn_commit_count =
                txn_commit_count.saturating_add(b.txn_commit_count.load(Ordering::Relaxed));
            error_count = error_count.saturating_add(b.error_count.load(Ordering::Relaxed));
            latency_sum_us =
                latency_sum_us.saturating_add(b.latency_sum_us.load(Ordering::Relaxed));
            for (i, bin) in b.latency_bins.iter().enumerate() {
                bins[i] = bins[i].saturating_add(bin.load(Ordering::Relaxed));
            }
        }

        let (latency_avg_ms, latency_p99_ms) = if statement_count == 0 {
            (0.0, 0.0)
        } else {
            let avg_ms = (latency_sum_us as f64 / statement_count as f64) / 1000.0;
            let p99_us = quantile_us_hist(&bins, statement_count, 0.99);
            (avg_ms, p99_us as f64 / 1000.0)
        };

        WindowSnapshot {
            statement_count,
            txn_commit_count,
            error_count,
            latency_avg_ms,
            latency_p99_ms,
        }
    }
}

struct WindowSnapshot {
    statement_count: u64,
    txn_commit_count: u64,
    error_count: u64,
    latency_avg_ms: f64,
    latency_p99_ms: f64,
}

struct Bucket {
    minute: AtomicU64,
    resetting: AtomicBool,
    statement_count: AtomicU64,
    txn_commit_count: AtomicU64,
    error_count: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_bins: [AtomicU64; NUM_BINS],
}

impl Bucket {
    fn new() -> Self {
        Self {
            minute: AtomicU64::new(0),
            resetting: AtomicBool::new(false),
            statement_count: AtomicU64::new(0),
            txn_commit_count: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            latency_sum_us: AtomicU64::new(0),
            latency_bins: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn ensure_minute(&self, minute: u64) {
        // NOTE: This is intentionally a loop (not recursion). Under high contention,
        // recursion here can overflow the stack on a Tokio worker thread.
        loop {
            let current = self.minute.load(Ordering::Acquire);
            if current == minute {
                return;
            }

            // Observability is approximate: never move a bucket "backwards" in time.
            // This avoids oscillation when some callers computed an older minute around
            // a boundary while other threads already advanced the bucket.
            if current > minute {
                return;
            }

            if self
                .resetting
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let current = self.minute.load(Ordering::Acquire);
                if current >= minute {
                    self.resetting.store(false, Ordering::Release);
                    return;
                }

                self.statement_count.store(0, Ordering::Relaxed);
                self.txn_commit_count.store(0, Ordering::Relaxed);
                self.error_count.store(0, Ordering::Relaxed);
                self.latency_sum_us.store(0, Ordering::Relaxed);
                for b in &self.latency_bins {
                    b.store(0, Ordering::Relaxed);
                }
                self.minute.store(minute, Ordering::Release);
                self.resetting.store(false, Ordering::Release);
                return;
            }

            while self.resetting.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
        }
    }
}

static START: OnceLock<std::time::Instant> = OnceLock::new();
static RNG_SEED: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

thread_local! {
    static RNG_STATE: std::cell::Cell<u64> = {
        let seed = RNG_SEED.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed) ^ now_ms();
        std::cell::Cell::new(seed.max(1))
    };
}

fn now_ms() -> u64 {
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn fast_rand_u64() -> u64 {
    RNG_STATE.with(|state| {
        let mut x = state.get();
        // xorshift64*
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        state.set(x);
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    })
}

fn prune_samples(samples: &mut VecDeque<SampleEvent>, now_ms: u64) {
    let cutoff = now_ms.saturating_sub(WINDOW_SECS.saturating_mul(1000));
    while let Some(front) = samples.front() {
        if front.at_ms < cutoff {
            samples.pop_front();
        } else {
            break;
        }
    }
}

fn duration_to_us(d: Duration) -> u64 {
    let micros: u128 = d.as_micros();
    micros.try_into().unwrap_or(u64::MAX)
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

fn redact_sensitive_sql(s: &str) -> String {
    let upper = s.to_ascii_uppercase();
    let ubytes = upper.as_bytes();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut flush_start = 0;

    while i < bytes.len() {
        if i + 8 <= ubytes.len() && &ubytes[i..i + 8] == b"PASSWORD" {
            out.push_str(&s[flush_start..i]);
            out.push_str(&s[i..i + 8]);
            i += 8;
            while i < bytes.len() && bytes[i] == b' ' {
                out.push(' ');
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'\'' {
                out.push_str("'***'");
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // skip closing quote
                }
            }
            flush_start = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[flush_start..]);
    out
}

fn normalize_sql(s: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_len));
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        prev_space = false;
        out.push(ch);
        if out.len() >= max_len {
            break;
        }
    }
    let out = out.trim().trim_end_matches(';').trim().to_string();
    if out.len() >= max_len {
        format!("{}…", out)
    } else {
        out
    }
}

/// Peak-memory threshold (bytes) that triggers the `expensive_query` log; `0`
/// disables it. Read once at process start.
///
/// Deliberately independent of `DB9_OBS_ENABLED`: the expensive_query log is a
/// rare forensic signal, not sampled observability, so it must fire whenever the
/// memory threshold is crossed regardless of the observability toggle. Only
/// `DB9_OBS_EXPENSIVE_MEM_MB=0` disables it.
pub fn expensive_mem_threshold_bytes() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| ObservabilityConfig::from_env().expensive_mem_threshold_bytes as usize)
}

/// SQL truncation length for the `expensive_query` log. Read once at process
/// start, consistent with [`expensive_mem_threshold_bytes`] — avoids re-reading
/// the environment on the per-statement dispatch path.
pub fn expensive_sql_max_len() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| ObservabilityConfig::from_env().expensive_sql_max_len)
}

/// Normalize + redact SQL for the `expensive_query` log, truncated at
/// `DB9_OBS_EXPENSIVE_SQL_LEN` (a separate, larger limit than the slow-query
/// log). Returns an empty string when there is nothing loggable.
pub fn normalize_sql_for_expensive_log(sql: &str) -> String {
    let normalized = normalize_sql(sql, expensive_sql_max_len());
    if normalized.is_empty() {
        return String::new();
    }
    redact_sensitive_sql(&normalized)
}

fn is_observability_system_sql(s: &str) -> bool {
    let upper = s.to_ascii_uppercase();
    upper.contains("_DB9_SYS_OBSERVABILITY") || upper.contains("_DB9_SYS_QUERY_SAMPLES")
}

fn latency_us_to_bin(latency_us: u64) -> usize {
    if latency_us == 0 {
        return 0;
    }
    let exp = 63 - latency_us.leading_zeros();
    let exp = exp.min(MAX_EXP);
    let base = 1u64 << exp;
    let offset = latency_us.saturating_sub(base);
    let sub = ((offset as u128 * SUB_BINS as u128) / base as u128) as u32;
    let sub = sub.min(SUB_BINS - 1);
    (exp as usize * SUB_BINS as usize) + sub as usize
}

fn bin_upper_bound_us(idx: usize) -> u64 {
    let exp = (idx as u32) / SUB_BINS;
    let sub = (idx as u32) % SUB_BINS;
    let base = 1u64 << exp.min(MAX_EXP);
    let step = base / SUB_BINS as u64;
    base.saturating_add(step.saturating_mul((sub + 1) as u64))
}

fn quantile_us_hist(bins: &[u64], total: u64, q: f64) -> u64 {
    if total == 0 {
        return 0;
    }
    let q = q.clamp(0.0, 1.0);
    let target = ((total as f64) * q).ceil() as u64;
    let target = target.max(1);

    let mut seen = 0u64;
    for (i, c) in bins.iter().enumerate() {
        seen = seen.saturating_add(*c);
        if seen >= target {
            return bin_upper_bound_us(i);
        }
    }
    bin_upper_bound_us(NUM_BINS.saturating_sub(1))
}

fn quantile_us_sorted(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let q = q.clamp(0.0, 1.0);
    let idx = ((sorted.len() as f64) * q).ceil() as usize;
    let idx = idx.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bucket_ensure_minute_never_moves_backwards() {
        let b = Bucket::new();
        b.ensure_minute(10);
        b.statement_count.store(123, Ordering::Relaxed);
        b.ensure_minute(9);
        assert_eq!(b.minute.load(Ordering::Acquire), 10);
        assert_eq!(b.statement_count.load(Ordering::Relaxed), 123);
    }

    #[test]
    fn test_latency_us_to_bin_monotonic() {
        let mut last = 0usize;
        for us in [0u64, 1, 2, 3, 4, 7, 8, 15, 16, 100, 1000, 1_000_000] {
            let idx = latency_us_to_bin(us);
            assert!(idx >= last);
            last = idx;
        }
    }

    #[test]
    fn test_quantile_sorted() {
        let mut v = vec![10u64, 20, 30, 40, 50];
        v.sort_unstable();
        assert_eq!(quantile_us_sorted(&v, 0.0), 10);
        assert_eq!(quantile_us_sorted(&v, 0.5), 30);
        assert_eq!(quantile_us_sorted(&v, 0.99), 50);
    }

    #[test]
    fn test_normalize_sql() {
        let s = "  SELECT   1 \n FROM  t ;  ";
        assert_eq!(normalize_sql(s, 512), "SELECT 1 FROM t");
    }

    #[test]
    fn test_redact_create_role_password() {
        let sql = "CREATE ROLE myuser WITH LOGIN PASSWORD 'secret123'";
        assert_eq!(
            redact_sensitive_sql(sql),
            "CREATE ROLE myuser WITH LOGIN PASSWORD '***'"
        );
    }

    #[test]
    fn test_redact_alter_role_password() {
        let sql = "ALTER ROLE admin WITH PASSWORD 'newpass!@#'";
        assert_eq!(
            redact_sensitive_sql(sql),
            "ALTER ROLE admin WITH PASSWORD '***'"
        );
    }

    #[test]
    fn test_redact_no_password() {
        let sql = "SELECT * FROM users";
        assert_eq!(redact_sensitive_sql(sql), "SELECT * FROM users");
    }

    #[test]
    fn test_redact_case_insensitive() {
        let sql = "CREATE ROLE foo WITH LOGIN password 'hunter2' SUPERUSER";
        assert_eq!(
            redact_sensitive_sql(sql),
            "CREATE ROLE foo WITH LOGIN password '***' SUPERUSER"
        );
    }

    #[test]
    fn test_redact_multibyte_utf8_preserved() {
        // #2306 Bug 3: bytes[i] as char corrupted multibyte UTF-8 into mojibake.
        let sql = "CREATE ROLE 用户 WITH LOGIN PASSWORD '密码测试' SUPERUSER";
        let redacted = redact_sensitive_sql(sql);
        assert!(redacted.contains("用户"));
        assert!(redacted.contains("'***'"));
        assert!(!redacted.contains("密码测试"));
        assert!(redacted.contains("SUPERUSER"));
    }

    #[test]
    fn test_redact_no_password_multibyte_passthrough() {
        // Multibyte SQL without PASSWORD keyword should pass through unchanged.
        let sql = "SELECT '日本語テスト' AS label FROM 表名";
        assert_eq!(redact_sensitive_sql(sql), sql);
    }

    #[test]
    fn test_observability_system_queries_are_not_sampled() {
        let cfg = ObservabilityConfig {
            sample_every: 1,
            ..ObservabilityConfig::default()
        };
        let tenant = TenantObservability::new(cfg);
        tenant.record_statement_us(1_000, true, || {
            "SELECT * FROM _db9_sys_query_samples()".to_string()
        });

        assert!(tenant.snapshot_query_samples().is_empty());
        assert_eq!(tenant.snapshot_summary().statement_count, 1);
    }

    #[test]
    fn test_normal_queries_are_still_sampled() {
        let cfg = ObservabilityConfig {
            sample_every: 1,
            ..ObservabilityConfig::default()
        };
        let tenant = TenantObservability::new(cfg);
        tenant.record_statement_us(1_000, true, || "SELECT 1".to_string());

        let groups = tenant.snapshot_query_samples();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].query, "SELECT 1");
        assert_eq!(groups[0].sample_count, 1);
    }

    #[test]
    fn test_slow_query_is_sampled_and_counted() {
        let threshold_us = 200_000; // 200ms
        let cfg = ObservabilityConfig {
            sample_every: 1,
            slow_query_threshold_us: threshold_us,
            ..ObservabilityConfig::default()
        };
        let tenant = TenantObservability::new(cfg);

        // Query exactly at threshold — should be sampled as slow
        tenant.record_statement_us(threshold_us, true, || "SELECT slow_at_boundary".to_string());
        let groups = tenant.snapshot_query_samples();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].query, "SELECT slow_at_boundary");

        // Statement count should reflect it
        assert_eq!(tenant.snapshot_summary().statement_count, 1);
    }

    #[test]
    fn test_slow_error_query_is_sampled() {
        let threshold_us = 200_000;
        let cfg = ObservabilityConfig {
            sample_every: 1,
            slow_query_threshold_us: threshold_us,
            ..ObservabilityConfig::default()
        };
        let tenant = TenantObservability::new(cfg);

        // Slow AND error — both conditions trigger sampling
        tenant.record_statement_us(threshold_us + 1, false, || "SELECT slow_error".to_string());
        let groups = tenant.snapshot_query_samples();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].error_count, 1);
        assert_eq!(tenant.snapshot_summary().error_count, 1);
    }

    #[test]
    fn test_fast_query_below_threshold_not_always_sampled() {
        let cfg = ObservabilityConfig {
            sample_every: u64::MAX, // effectively never sample randomly
            slow_query_threshold_us: 200_000,
            ..ObservabilityConfig::default()
        };
        let tenant = TenantObservability::new(cfg);

        // Fast successful query with max sample_every — should not be sampled
        tenant.record_statement_us(100, true, || "SELECT fast".to_string());
        assert!(tenant.snapshot_query_samples().is_empty());
        // But statement count should still increment
        assert_eq!(tenant.snapshot_summary().statement_count, 1);
    }

    #[test]
    fn test_disabled_observability_skips_all_work() {
        let cfg = ObservabilityConfig {
            enabled: false,
            sample_every: 1,
            slow_query_threshold_us: 1, // extremely low threshold
            ..ObservabilityConfig::default()
        };
        let tenant = TenantObservability::new(cfg);

        // Even a very slow query should not be recorded
        tenant.record_statement_us(999_999_999, true, || {
            panic!("sql_supplier should not be called when disabled")
        });
        assert!(tenant.snapshot_query_samples().is_empty());
        assert_eq!(tenant.snapshot_summary().statement_count, 0);
    }

    #[test]
    fn test_slow_query_with_password_is_redacted_in_sample() {
        let cfg = ObservabilityConfig {
            sample_every: 1,
            slow_query_threshold_us: 1, // 1us threshold
            ..ObservabilityConfig::default()
        };
        let tenant = TenantObservability::new(cfg);

        tenant.record_statement_us(1_000_000, true, || {
            "CREATE ROLE admin WITH PASSWORD 'supersecret'".to_string()
        });
        let groups = tenant.snapshot_query_samples();
        assert_eq!(groups.len(), 1);
        assert!(
            groups[0].query.contains("'***'"),
            "password should be redacted, got: {}",
            groups[0].query
        );
        assert!(
            !groups[0].query.contains("supersecret"),
            "raw password should not appear in sample"
        );
    }

    #[test]
    fn test_slow_system_query_not_sampled() {
        let cfg = ObservabilityConfig {
            sample_every: 1,
            slow_query_threshold_us: 1,
            ..ObservabilityConfig::default()
        };
        let tenant = TenantObservability::new(cfg);

        // Even slow observability system queries are filtered out
        tenant.record_statement_us(1_000_000, true, || {
            "SELECT * FROM _db9_sys_observability()".to_string()
        });
        assert!(tenant.snapshot_query_samples().is_empty());
    }

    #[test]
    fn test_slow_empty_sql_not_sampled() {
        let cfg = ObservabilityConfig {
            sample_every: 1,
            slow_query_threshold_us: 1,
            ..ObservabilityConfig::default()
        };
        let tenant = TenantObservability::new(cfg);

        // Empty SQL supplier — should not produce a sample even if slow
        tenant.record_statement_us(1_000_000, true, String::new);
        assert!(tenant.snapshot_query_samples().is_empty());
    }
}
