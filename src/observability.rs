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

#[derive(Debug, Clone)]
pub struct ObservabilityConfig {
    pub enabled: bool,
    pub sample_every: u64,
    pub slow_query_threshold_us: u64,
    pub max_sample_events: usize,
    pub max_sample_groups: usize,
    pub max_sql_len: usize,
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
        }
    }
}

impl ObservabilityConfig {
    fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(v) = env::var("PGTIKV_OBS_ENABLED") {
            cfg.enabled = parse_bool(&v).unwrap_or(cfg.enabled);
        }
        if let Ok(v) = env::var("PGTIKV_OBS_SAMPLE_EVERY") {
            cfg.sample_every = v.parse::<u64>().ok().filter(|n| *n > 0).unwrap_or(cfg.sample_every);
        }
        if let Ok(v) = env::var("PGTIKV_OBS_SLOW_MS") {
            cfg.slow_query_threshold_us = v
                .parse::<u64>()
                .ok()
                .map(|ms| ms.saturating_mul(1000))
                .unwrap_or(cfg.slow_query_threshold_us);
        }
        if let Ok(v) = env::var("PGTIKV_OBS_MAX_SAMPLE_EVENTS") {
            cfg.max_sample_events = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.max_sample_events);
        }
        if let Ok(v) = env::var("PGTIKV_OBS_MAX_SAMPLE_GROUPS") {
            cfg.max_sample_groups = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.max_sample_groups);
        }
        if let Ok(v) = env::var("PGTIKV_OBS_MAX_SQL_LEN") {
            cfg.max_sql_len = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.max_sql_len);
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

    #[allow(dead_code)]
    pub fn config(&self) -> &ObservabilityConfig {
        &self.config
    }

    pub fn tenant(&self, keyspace: &str) -> Arc<TenantObservability> {
        let key = if keyspace.is_empty() {
            "default".to_string()
        } else {
            keyspace.to_string()
        };

        let mut guard = self.tenants.lock().expect("observability tenants lock");
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
    window: RollingWindow,
    samples: Mutex<VecDeque<SampleEvent>>,
}

impl TenantObservability {
    fn new(config: ObservabilityConfig) -> Self {
        Self {
            config,
            active_connections: AtomicU64::new(0),
            window: RollingWindow::new(),
            samples: Mutex::new(VecDeque::new()),
        }
    }

    pub fn connection_open(self: &Arc<Self>) -> ConnectionGuard {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard { tenant: self.clone() }
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

        if self.should_sample(latency_us, ok) {
            let mut sql = normalize_sql(&sql_supplier(), self.config.max_sql_len);
            if sql.is_empty() {
                return;
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

            let mut guard = self.samples.lock().expect("observability samples lock");
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
        let mut guard = self.samples.lock().expect("observability samples lock");
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

        groups.sort_by(|a, b| b.sample_count.cmp(&a.sample_count));
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
        fast_rand_u64() % every == 0
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
        b.latency_sum_us
            .fetch_add(latency_us, Ordering::Relaxed);
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
            statement_count = statement_count.saturating_add(b.statement_count.load(Ordering::Relaxed));
            txn_commit_count = txn_commit_count.saturating_add(b.txn_commit_count.load(Ordering::Relaxed));
            error_count = error_count.saturating_add(b.error_count.load(Ordering::Relaxed));
            latency_sum_us = latency_sum_us.saturating_add(b.latency_sum_us.load(Ordering::Relaxed));
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

fn latency_us_to_bin(latency_us: u64) -> usize {
    if latency_us == 0 {
        return 0;
    }
    let exp = (63 - latency_us.leading_zeros()) as u32;
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
}
