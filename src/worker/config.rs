use std::env;

const DEFAULT_POLL_MS: u64 = 60_000;
const MIN_POLL_MS: u64 = 100;
const DEFAULT_MAX_CONCURRENT_JOBS: usize = 32;
const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_CRON_JOB_TIMEOUT_MS: u64 = 1_800_000;
const DEFAULT_ORPHAN_TIMEOUT_SEC: u64 = 300;
// Claim lease span. The owning worker renews at ~lease/3 while it executes, so
// a long-running BgSql/BgDdl task keeps its claim alive instead of being reaped
// mid-flight and double-executed by a second worker. Must be large relative to
// plausible clock skew; sub-second leases are forbidden (design §K4).
const DEFAULT_CLAIM_LEASE_MS: u64 = 60_000;
const MIN_CLAIM_LEASE_MS: u64 = 5_000;
const DEFAULT_EXECUTOR_LEASE_MS: u64 = 30_000;
const MIN_EXECUTOR_LEASE_MS: u64 = 10_000;
const DEFAULT_GC_BATCH_SIZE: usize = 100;
const DEFAULT_AUTO_ANALYZE_THRESHOLD: u64 = 50;
const DEFAULT_GC_INTERVAL_SEC: u64 = 600;
const MIN_GC_INTERVAL_SEC: u64 = 30;
const DEFAULT_HNSW_SWEEP_INTERVAL_SEC: u64 = 600;
const MIN_HNSW_SWEEP_INTERVAL_SEC: u64 = 30;
const DEFAULT_REGISTRY_SWEEP_INTERVAL_SEC: u64 = 60;
const MIN_REGISTRY_SWEEP_INTERVAL_SEC: u64 = 1;
const DEFAULT_SWEEP_PAGE_INTERVAL_SEC: u64 = 30;
const MIN_SWEEP_PAGE_INTERVAL_SEC: u64 = 1;
// Storage-size accounting (StorageSizeScan) refreshes a PD Region/MiB estimate
// for each database. With the storage dirty-marker producer removed, this
// interval is the only automatic refresh cadence; keep it much slower than the
// old 30-min all-tenant scan while still self-healing within the same day.
const DEFAULT_STORAGE_SCAN_INTERVAL_SEC: u64 = 21_600;
const MIN_STORAGE_SCAN_INTERVAL_SEC: u64 = 60;
const DEFAULT_STORAGE_SCAN_JITTER_SEC: u64 = 300;
const DEFAULT_STORAGE_SCAN_PD_RATE_LIMIT_MS: u64 = 100;
const DEFAULT_REGISTRY_RECONCILE_BATCH_SIZE: usize = DEFAULT_MAX_CONCURRENT_JOBS;
const DEFAULT_SYSTEM_KEYSPACE: &str = "_sys_worker";

// GC safepoint defaults — controls TiKV MVCC version cleanup.
// Without periodic safepoint advancement, TiKV never GCs old MVCC versions,
// causing unbounded storage growth and eventual compaction failure / worker panic.
const DEFAULT_GC_SAFEPOINT_ENABLED: bool = true;
const DEFAULT_GC_SAFEPOINT_INTERVAL_SEC: u64 = 300; // 5 minutes
const MIN_GC_SAFEPOINT_INTERVAL_SEC: u64 = 30;
// 24 hours — conservative default retention window for historical MVCC
// versions. Active foreground and worker transactions are protected directly
// via ActiveTxnRegistry; gc_life_time is not a timeout surrogate.
const DEFAULT_GC_LIFE_TIME_SEC: u64 = 86400;
const MIN_GC_LIFE_TIME_SEC: u64 = 600; // TiDB enforces minimum 10 minutes

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub enabled: bool,
    pub poll_ms: u64,
    pub max_concurrent_jobs: usize,
    pub worker_id: String,
    pub gc_instance_id: String,
    pub statement_timeout_ms: u64,
    pub cron_job_timeout_ms: u64,
    pub orphan_timeout_sec: u64,
    /// Worker-claim lease span in ms. The executing worker renews its claim at
    /// ~lease/3; GC reaps only leases that have actually expired.
    pub claim_lease_ms: u64,
    /// Cluster-wide executor lease span in ms. Only the holder scans/drains the
    /// worker queues; other db9 processes remain standby SQL-serving nodes.
    pub executor_lease_ms: u64,
    pub gc_batch_size: usize,
    pub auto_analyze_enabled: bool,
    pub auto_analyze_threshold: u64,
    pub gc_interval_sec: u64,
    pub hnsw_sweep_interval_sec: u64,
    pub registry_sweep_interval_sec: u64,
    pub sweep_page_interval_sec: u64,
    pub storage_scan_interval_sec: u64,
    pub storage_scan_jitter_sec: u64,
    pub storage_scan_pd_rate_limit_ms: u64,
    pub registry_reconcile_batch_size: usize,
    pub system_keyspace: String,

    // TiKV MVCC GC safepoint advancement
    pub gc_safepoint_enabled: bool,
    pub gc_safepoint_interval_sec: u64,
    pub gc_life_time_sec: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        let hostname = env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());
        let pid = std::process::id();
        let worker_id = format!("{}:{}", hostname, pid);
        let gc_instance_id = uuid::Uuid::new_v4().to_string();

        Self {
            enabled: true,
            poll_ms: DEFAULT_POLL_MS,
            max_concurrent_jobs: DEFAULT_MAX_CONCURRENT_JOBS,
            worker_id,
            gc_instance_id,
            statement_timeout_ms: DEFAULT_STATEMENT_TIMEOUT_MS,
            cron_job_timeout_ms: DEFAULT_CRON_JOB_TIMEOUT_MS,
            orphan_timeout_sec: DEFAULT_ORPHAN_TIMEOUT_SEC,
            claim_lease_ms: DEFAULT_CLAIM_LEASE_MS,
            executor_lease_ms: DEFAULT_EXECUTOR_LEASE_MS,
            gc_batch_size: DEFAULT_GC_BATCH_SIZE,
            auto_analyze_enabled: true,
            auto_analyze_threshold: DEFAULT_AUTO_ANALYZE_THRESHOLD,
            gc_interval_sec: DEFAULT_GC_INTERVAL_SEC,
            hnsw_sweep_interval_sec: DEFAULT_HNSW_SWEEP_INTERVAL_SEC,
            registry_sweep_interval_sec: DEFAULT_REGISTRY_SWEEP_INTERVAL_SEC,
            sweep_page_interval_sec: DEFAULT_SWEEP_PAGE_INTERVAL_SEC,
            storage_scan_interval_sec: DEFAULT_STORAGE_SCAN_INTERVAL_SEC,
            storage_scan_jitter_sec: DEFAULT_STORAGE_SCAN_JITTER_SEC,
            storage_scan_pd_rate_limit_ms: DEFAULT_STORAGE_SCAN_PD_RATE_LIMIT_MS,
            registry_reconcile_batch_size: DEFAULT_REGISTRY_RECONCILE_BATCH_SIZE,
            system_keyspace: DEFAULT_SYSTEM_KEYSPACE.to_string(),

            gc_safepoint_enabled: DEFAULT_GC_SAFEPOINT_ENABLED,
            gc_safepoint_interval_sec: DEFAULT_GC_SAFEPOINT_INTERVAL_SEC,
            gc_life_time_sec: DEFAULT_GC_LIFE_TIME_SEC,
        }
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

impl WorkerConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        let worker_enabled_raw = env::var("DB9_WORKER_ENABLED").ok();
        if let Some(v) = worker_enabled_raw.as_deref() {
            match parse_bool(v) {
                Some(enabled) => cfg.enabled = enabled,
                None => {
                    tracing::warn!(
                        "DB9_WORKER_ENABLED='{}' is not a valid boolean; disabling worker",
                        v
                    );
                    cfg.enabled = false;
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_POLL_MS") {
            match v.parse::<u64>() {
                Ok(parsed) if parsed >= MIN_POLL_MS => {
                    cfg.poll_ms = parsed;
                }
                Ok(parsed) => {
                    tracing::warn!(
                        "DB9_WORKER_POLL_MS={} is below minimum {}ms; using default {}ms",
                        parsed,
                        MIN_POLL_MS,
                        cfg.poll_ms
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_POLL_MS='{}' is not a valid integer; using default {}ms",
                        v,
                        cfg.poll_ms
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_MAX_CONCURRENT_JOBS") {
            cfg.max_concurrent_jobs = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.max_concurrent_jobs);
        }
        if let Ok(v) = env::var("DB9_WORKER_ID") {
            cfg.worker_id = v;
        }
        if let Ok(v) = env::var("DB9_WORKER_STATEMENT_TIMEOUT_MS") {
            match v.parse::<u64>() {
                Ok(parsed) => cfg.statement_timeout_ms = parsed,
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_STATEMENT_TIMEOUT_MS='{}' is not a valid integer; using default {}ms",
                        v,
                        cfg.statement_timeout_ms
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_CRON_JOB_TIMEOUT_MS") {
            match v.parse::<u64>() {
                Ok(parsed) => cfg.cron_job_timeout_ms = parsed,
                Err(_) => {
                    tracing::warn!(
                        "DB9_CRON_JOB_TIMEOUT_MS='{}' is not a valid integer; using default {}ms",
                        v,
                        cfg.cron_job_timeout_ms
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_ORPHAN_TIMEOUT_SEC") {
            cfg.orphan_timeout_sec = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.orphan_timeout_sec);
        }
        if let Ok(v) = env::var("DB9_WORKER_CLAIM_LEASE_MS") {
            match v.parse::<u64>() {
                Ok(parsed) if parsed >= MIN_CLAIM_LEASE_MS => {
                    cfg.claim_lease_ms = parsed;
                }
                Ok(parsed) => {
                    tracing::warn!(
                        "DB9_WORKER_CLAIM_LEASE_MS={} is below minimum {}ms; using default {}ms",
                        parsed,
                        MIN_CLAIM_LEASE_MS,
                        cfg.claim_lease_ms
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_CLAIM_LEASE_MS='{}' is not a valid integer; using default {}ms",
                        v,
                        cfg.claim_lease_ms
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_EXECUTOR_LEASE_MS") {
            match v.parse::<u64>() {
                Ok(parsed) if parsed >= MIN_EXECUTOR_LEASE_MS => {
                    cfg.executor_lease_ms = parsed;
                }
                Ok(parsed) => {
                    tracing::warn!(
                        "DB9_WORKER_EXECUTOR_LEASE_MS={} is below minimum {}ms; using default {}ms",
                        parsed,
                        MIN_EXECUTOR_LEASE_MS,
                        cfg.executor_lease_ms
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_EXECUTOR_LEASE_MS='{}' is not a valid integer; using default {}ms",
                        v,
                        cfg.executor_lease_ms
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_GC_BATCH_SIZE") {
            cfg.gc_batch_size = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .map(|n| n.min(u32::MAX as u64) as usize)
                .unwrap_or(cfg.gc_batch_size);
        }
        if let Ok(v) = env::var("DB9_AUTO_ANALYZE_ENABLED") {
            cfg.auto_analyze_enabled = parse_bool(&v).unwrap_or(cfg.auto_analyze_enabled);
        }
        if let Ok(v) = env::var("DB9_AUTO_ANALYZE_THRESHOLD") {
            cfg.auto_analyze_threshold = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.auto_analyze_threshold);
        }
        if let Ok(v) = env::var("DB9_WORKER_GC_INTERVAL_SEC") {
            match v.parse::<u64>() {
                Ok(parsed) if parsed >= MIN_GC_INTERVAL_SEC => {
                    cfg.gc_interval_sec = parsed;
                }
                Ok(parsed) => {
                    tracing::warn!(
                        "DB9_WORKER_GC_INTERVAL_SEC={} is below minimum {}s; using default {}s",
                        parsed,
                        MIN_GC_INTERVAL_SEC,
                        cfg.gc_interval_sec
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_GC_INTERVAL_SEC='{}' is not a valid integer; using default {}s",
                        v,
                        cfg.gc_interval_sec
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC") {
            match v.parse::<u64>() {
                Ok(parsed) if parsed >= MIN_HNSW_SWEEP_INTERVAL_SEC => {
                    cfg.hnsw_sweep_interval_sec = parsed;
                }
                Ok(parsed) => {
                    tracing::warn!(
                        "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC={} is below minimum {}s; using default {}s",
                        parsed,
                        MIN_HNSW_SWEEP_INTERVAL_SEC,
                        cfg.hnsw_sweep_interval_sec
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC='{}' is not a valid integer; using default {}s",
                        v,
                        cfg.hnsw_sweep_interval_sec
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC") {
            match v.parse::<u64>() {
                Ok(parsed) if parsed >= MIN_REGISTRY_SWEEP_INTERVAL_SEC => {
                    cfg.registry_sweep_interval_sec = parsed;
                }
                Ok(parsed) => {
                    tracing::warn!(
                        "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC={} is below minimum {}s; using default {}s",
                        parsed,
                        MIN_REGISTRY_SWEEP_INTERVAL_SEC,
                        cfg.registry_sweep_interval_sec
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC='{}' is not a valid integer; using default {}s",
                        v,
                        cfg.registry_sweep_interval_sec
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_SWEEP_PAGE_INTERVAL_SEC") {
            match v.parse::<u64>() {
                Ok(parsed) if parsed >= MIN_SWEEP_PAGE_INTERVAL_SEC => {
                    cfg.sweep_page_interval_sec = parsed;
                }
                Ok(parsed) => {
                    tracing::warn!(
                        "DB9_WORKER_SWEEP_PAGE_INTERVAL_SEC={} is below minimum {}s; using default {}s",
                        parsed,
                        MIN_SWEEP_PAGE_INTERVAL_SEC,
                        cfg.sweep_page_interval_sec
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_SWEEP_PAGE_INTERVAL_SEC='{}' is not a valid integer; using default {}s",
                        v,
                        cfg.sweep_page_interval_sec
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_STORAGE_SCAN_INTERVAL_SEC") {
            match v.parse::<u64>() {
                Ok(parsed) if parsed >= MIN_STORAGE_SCAN_INTERVAL_SEC => {
                    cfg.storage_scan_interval_sec = parsed;
                }
                Ok(parsed) => {
                    tracing::warn!(
                        "DB9_WORKER_STORAGE_SCAN_INTERVAL_SEC={} is below minimum {}s; using default {}s",
                        parsed,
                        MIN_STORAGE_SCAN_INTERVAL_SEC,
                        cfg.storage_scan_interval_sec
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_STORAGE_SCAN_INTERVAL_SEC='{}' is not a valid integer; using default {}s",
                        v,
                        cfg.storage_scan_interval_sec
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_STORAGE_SCAN_JITTER_SEC") {
            match v.parse::<u64>() {
                Ok(parsed) => cfg.storage_scan_jitter_sec = parsed,
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_STORAGE_SCAN_JITTER_SEC='{}' is not a valid integer; using default {}s",
                        v,
                        cfg.storage_scan_jitter_sec
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_STORAGE_SCAN_PD_RATE_LIMIT_MS") {
            match v.parse::<u64>() {
                Ok(parsed) => cfg.storage_scan_pd_rate_limit_ms = parsed,
                Err(_) => {
                    tracing::warn!(
                        "DB9_WORKER_STORAGE_SCAN_PD_RATE_LIMIT_MS='{}' is not a valid integer; using default {}ms",
                        v,
                        cfg.storage_scan_pd_rate_limit_ms
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_WORKER_REGISTRY_RECONCILE_BATCH_SIZE") {
            cfg.registry_reconcile_batch_size = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .map(|n| n.min(u32::MAX as u64) as usize)
                .unwrap_or_else(|| {
                    tracing::warn!(
                        "DB9_WORKER_REGISTRY_RECONCILE_BATCH_SIZE='{}' is not a valid positive integer; using default {}",
                        v,
                        cfg.registry_reconcile_batch_size
                    );
                    cfg.registry_reconcile_batch_size
                });
        }
        if let Ok(v) = env::var("DB9_WORKER_SYSTEM_KEYSPACE") {
            cfg.system_keyspace = v;
        }

        // GC safepoint configuration
        if let Ok(v) = env::var("DB9_GC_SAFEPOINT_ENABLED") {
            cfg.gc_safepoint_enabled = parse_bool(&v).unwrap_or(cfg.gc_safepoint_enabled);
        }
        if let Ok(v) = env::var("DB9_GC_SAFEPOINT_INTERVAL_SEC") {
            match v.parse::<u64>() {
                Ok(parsed) if parsed >= MIN_GC_SAFEPOINT_INTERVAL_SEC => {
                    cfg.gc_safepoint_interval_sec = parsed;
                }
                Ok(parsed) => {
                    tracing::warn!(
                        "DB9_GC_SAFEPOINT_INTERVAL_SEC={} is below minimum {}s; using default {}s",
                        parsed,
                        MIN_GC_SAFEPOINT_INTERVAL_SEC,
                        cfg.gc_safepoint_interval_sec
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "DB9_GC_SAFEPOINT_INTERVAL_SEC='{}' is not a valid integer; using default {}s",
                        v,
                        cfg.gc_safepoint_interval_sec
                    );
                }
            }
        }
        if let Ok(v) = env::var("DB9_GC_LIFE_TIME_SEC") {
            match v.parse::<u64>() {
                Ok(parsed) if parsed >= MIN_GC_LIFE_TIME_SEC => {
                    cfg.gc_life_time_sec = parsed;
                }
                Ok(parsed) => {
                    tracing::warn!(
                        "DB9_GC_LIFE_TIME_SEC={} is below minimum {}s; using default {}s",
                        parsed,
                        MIN_GC_LIFE_TIME_SEC,
                        cfg.gc_life_time_sec
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "DB9_GC_LIFE_TIME_SEC='{}' is not a valid integer; using default {}s",
                        v,
                        cfg.gc_life_time_sec
                    );
                }
            }
        }

        tracing::info!(
            "Worker config loaded: DB9_WORKER_ENABLED raw='{}', resolved={}",
            worker_enabled_raw.as_deref().unwrap_or("<unset>"),
            cfg.enabled
        );

        cfg
    }

    /// Minimum ratio of `gc_life_time_sec` to `gc_safepoint_interval_sec`.
    /// At N=3, the system survives 2 consecutive missed heartbeats before
    /// a row is considered stale by the advancer.
    const MIN_LIFE_TIME_TO_INTERVAL_RATIO: u64 = 3;

    /// Validate GC configuration invariants.
    pub fn validate_gc_config(&self) {
        let min_life_time = self
            .gc_safepoint_interval_sec
            .saturating_mul(Self::MIN_LIFE_TIME_TO_INTERVAL_RATIO);
        if self.gc_life_time_sec < min_life_time {
            panic!(
                "UNSAFE CONFIG: DB9_GC_LIFE_TIME_SEC ({}) < DB9_GC_SAFEPOINT_INTERVAL_SEC ({}) * {} = {}. \
                 The GC retention window must be at least {}x the heartbeat interval \
                 so that {} consecutive missed heartbeats do not expose live transactions to GC.",
                self.gc_life_time_sec,
                self.gc_safepoint_interval_sec,
                Self::MIN_LIFE_TIME_TO_INTERVAL_RATIO,
                min_life_time,
                Self::MIN_LIFE_TIME_TO_INTERVAL_RATIO,
                Self::MIN_LIFE_TIME_TO_INTERVAL_RATIO - 1,
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::OnceLock;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn from_env_uses_defaults_when_vars_not_set() {
        let _guard = test_lock().lock();

        let keys = [
            "DB9_WORKER_ENABLED",
            "DB9_WORKER_POLL_MS",
            "DB9_WORKER_MAX_CONCURRENT_JOBS",
            "DB9_WORKER_ID",
            "DB9_WORKER_STATEMENT_TIMEOUT_MS",
            "DB9_CRON_JOB_TIMEOUT_MS",
            "DB9_WORKER_ORPHAN_TIMEOUT_SEC",
            "DB9_WORKER_CLAIM_LEASE_MS",
            "DB9_WORKER_EXECUTOR_LEASE_MS",
            "DB9_WORKER_GC_BATCH_SIZE",
            "DB9_AUTO_ANALYZE_ENABLED",
            "DB9_AUTO_ANALYZE_THRESHOLD",
            "DB9_WORKER_GC_INTERVAL_SEC",
            "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC",
            "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC",
            "DB9_WORKER_SWEEP_PAGE_INTERVAL_SEC",
            "DB9_WORKER_STORAGE_SCAN_INTERVAL_SEC",
            "DB9_WORKER_STORAGE_SCAN_JITTER_SEC",
            "DB9_WORKER_STORAGE_SCAN_PD_RATE_LIMIT_MS",
            "DB9_WORKER_REGISTRY_RECONCILE_BATCH_SIZE",
            "DB9_WORKER_SYSTEM_KEYSPACE",
        ];

        let saved: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|k| (k.to_string(), env::var(k).ok()))
            .collect();

        for key in &keys {
            unsafe {
                env::remove_var(key);
            }
        }

        let cfg = WorkerConfig::from_env();
        assert!(cfg.enabled);
        assert_eq!(cfg.poll_ms, DEFAULT_POLL_MS);
        assert_eq!(cfg.max_concurrent_jobs, DEFAULT_MAX_CONCURRENT_JOBS);
        assert!(cfg.worker_id.contains(':'));
        assert_eq!(cfg.statement_timeout_ms, DEFAULT_STATEMENT_TIMEOUT_MS);
        assert_eq!(cfg.cron_job_timeout_ms, DEFAULT_CRON_JOB_TIMEOUT_MS);
        assert_eq!(cfg.orphan_timeout_sec, DEFAULT_ORPHAN_TIMEOUT_SEC);
        assert_eq!(cfg.claim_lease_ms, DEFAULT_CLAIM_LEASE_MS);
        assert_eq!(cfg.executor_lease_ms, DEFAULT_EXECUTOR_LEASE_MS);
        assert_eq!(cfg.gc_batch_size, DEFAULT_GC_BATCH_SIZE);
        assert!(cfg.auto_analyze_enabled);
        assert_eq!(cfg.auto_analyze_threshold, DEFAULT_AUTO_ANALYZE_THRESHOLD);
        assert_eq!(cfg.gc_interval_sec, DEFAULT_GC_INTERVAL_SEC);
        assert_eq!(cfg.hnsw_sweep_interval_sec, DEFAULT_HNSW_SWEEP_INTERVAL_SEC);
        assert_eq!(
            cfg.registry_sweep_interval_sec,
            DEFAULT_REGISTRY_SWEEP_INTERVAL_SEC
        );
        assert_eq!(cfg.sweep_page_interval_sec, DEFAULT_SWEEP_PAGE_INTERVAL_SEC);
        assert_eq!(
            cfg.storage_scan_interval_sec,
            DEFAULT_STORAGE_SCAN_INTERVAL_SEC
        );
        assert_eq!(cfg.storage_scan_jitter_sec, DEFAULT_STORAGE_SCAN_JITTER_SEC);
        assert_eq!(
            cfg.storage_scan_pd_rate_limit_ms,
            DEFAULT_STORAGE_SCAN_PD_RATE_LIMIT_MS
        );
        assert_eq!(
            cfg.registry_reconcile_batch_size,
            DEFAULT_REGISTRY_RECONCILE_BATCH_SIZE
        );
        assert_eq!(cfg.system_keyspace, DEFAULT_SYSTEM_KEYSPACE);

        for (key, value) in saved {
            match value {
                Some(v) => unsafe { env::set_var(key, v) },
                None => unsafe { env::remove_var(key) },
            }
        }
    }

    #[test]
    fn from_env_clamps_gc_batch_size_to_u32_max() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_GC_BATCH_SIZE";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, (u64::from(u32::MAX) + 1).to_string());
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.gc_batch_size, u32::MAX as usize);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_poll_ms_when_at_least_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_POLL_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "5000");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.poll_ms, 5_000);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_keeps_default_poll_ms_when_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_POLL_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "99");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.poll_ms, DEFAULT_POLL_MS);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_ignores_non_integer_poll_ms() {
        let _lock = test_lock().lock();
        unsafe { env::set_var("DB9_WORKER_POLL_MS", "not_a_number") };
        let cfg = WorkerConfig::from_env();
        unsafe { env::remove_var("DB9_WORKER_POLL_MS") };
        assert_eq!(
            cfg.poll_ms, DEFAULT_POLL_MS,
            "non-integer should fall back to default"
        );
    }
    #[test]
    fn test_parse_bool_true_variants() {
        for input in &["1", "true", "t", "yes", "y", "on"] {
            assert_eq!(parse_bool(input), Some(true), "input: {}", input);
        }
        for input in &[" TRUE ", " Yes ", " ON "] {
            assert_eq!(parse_bool(input), Some(true), "trimmed input: {}", input);
        }
    }

    #[test]
    fn test_parse_bool_false_variants() {
        for input in &["0", "false", "f", "no", "n", "off"] {
            assert_eq!(parse_bool(input), Some(false), "input: {}", input);
        }
        for input in &[" FALSE ", " No ", " OFF "] {
            assert_eq!(parse_bool(input), Some(false), "trimmed input: {}", input);
        }
    }

    #[test]
    fn test_parse_bool_invalid() {
        for input in &["maybe", "", "2", "yep", "nope", "enabled", "live=false"] {
            assert_eq!(parse_bool(input), None, "input: {}", input);
        }
    }

    #[test]
    fn from_env_applies_worker_enabled_false() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_ENABLED";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "false");
        }

        let cfg = WorkerConfig::from_env();
        assert!(!cfg.enabled);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_invalid_worker_enabled_fails_closed() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_ENABLED";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "live=false");
        }

        let cfg = WorkerConfig::from_env();
        assert!(
            !cfg.enabled,
            "invalid DB9_WORKER_ENABLED must not leave the worker enabled"
        );

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn test_config_default_values() {
        let cfg = WorkerConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.poll_ms, 60_000);
        assert_eq!(cfg.max_concurrent_jobs, 32);
        assert!(uuid::Uuid::parse_str(&cfg.gc_instance_id).is_ok());
        assert_eq!(cfg.statement_timeout_ms, 300_000);
        assert_eq!(cfg.cron_job_timeout_ms, 1_800_000);
        assert_eq!(cfg.orphan_timeout_sec, 300);
        assert_eq!(cfg.claim_lease_ms, 60_000);
        assert_eq!(cfg.gc_batch_size, 100);
        assert!(cfg.auto_analyze_enabled);
        assert_eq!(cfg.auto_analyze_threshold, 50);
        assert_eq!(cfg.gc_interval_sec, 600);
        assert_eq!(cfg.hnsw_sweep_interval_sec, 600);
        assert_eq!(cfg.registry_sweep_interval_sec, 60);
        assert_eq!(cfg.sweep_page_interval_sec, 30);
        assert_eq!(cfg.storage_scan_interval_sec, 21_600);
        assert_eq!(cfg.storage_scan_jitter_sec, 300);
        assert_eq!(cfg.storage_scan_pd_rate_limit_ms, 100);
        assert_eq!(cfg.registry_reconcile_batch_size, 32);
        assert_eq!(cfg.system_keyspace, "_sys_worker");
    }

    #[test]
    fn from_env_applies_storage_scan_jitter_and_pd_rate_limit() {
        let _guard = test_lock().lock();

        let jitter_key = "DB9_WORKER_STORAGE_SCAN_JITTER_SEC";
        let rate_key = "DB9_WORKER_STORAGE_SCAN_PD_RATE_LIMIT_MS";
        let jitter_saved = env::var(jitter_key).ok();
        let rate_saved = env::var(rate_key).ok();

        unsafe {
            env::set_var(jitter_key, "17");
            env::set_var(rate_key, "250");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.storage_scan_jitter_sec, 17);
        assert_eq!(cfg.storage_scan_pd_rate_limit_ms, 250);

        match jitter_saved {
            Some(v) => unsafe { env::set_var(jitter_key, v) },
            None => unsafe { env::remove_var(jitter_key) },
        }
        match rate_saved {
            Some(v) => unsafe { env::set_var(rate_key, v) },
            None => unsafe { env::remove_var(rate_key) },
        }
    }

    #[test]
    fn from_env_applies_claim_lease_ms_and_clamps_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_CLAIM_LEASE_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "120000");
        }
        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.claim_lease_ms, 120_000);

        // Below the minimum: keep the default (sub-second/too-small leases are
        // forbidden by design §K4).
        unsafe {
            env::set_var(key, "1000");
        }
        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.claim_lease_ms, DEFAULT_CLAIM_LEASE_MS);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_executor_lease_ms_and_clamps_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_EXECUTOR_LEASE_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "45000");
        }
        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.executor_lease_ms, 45_000);

        unsafe {
            env::set_var(key, "5000");
        }
        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.executor_lease_ms, DEFAULT_EXECUTOR_LEASE_MS);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_registry_reconcile_batch_size() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_REGISTRY_RECONCILE_BATCH_SIZE";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "7");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.registry_reconcile_batch_size, 7);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_ignores_zero_registry_reconcile_batch_size() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_REGISTRY_RECONCILE_BATCH_SIZE";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "0");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(
            cfg.registry_reconcile_batch_size,
            DEFAULT_REGISTRY_RECONCILE_BATCH_SIZE
        );

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_hnsw_sweep_interval_when_at_least_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "120");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.hnsw_sweep_interval_sec, 120);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_keeps_default_hnsw_sweep_interval_when_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "29");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.hnsw_sweep_interval_sec, DEFAULT_HNSW_SWEEP_INTERVAL_SEC);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_ignores_non_integer_hnsw_sweep_interval() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "not_a_number");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(
            cfg.hnsw_sweep_interval_sec, DEFAULT_HNSW_SWEEP_INTERVAL_SEC,
            "non-integer should fall back to default"
        );

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_registry_sweep_interval_when_at_least_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "7");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.registry_sweep_interval_sec, 7);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_keeps_default_registry_sweep_interval_when_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "0");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(
            cfg.registry_sweep_interval_sec,
            DEFAULT_REGISTRY_SWEEP_INTERVAL_SEC
        );

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_applies_gc_interval_when_at_least_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_GC_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "120");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.gc_interval_sec, 120);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_keeps_default_gc_interval_when_below_minimum() {
        let _guard = test_lock().lock();

        let key = "DB9_WORKER_GC_INTERVAL_SEC";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "29");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.gc_interval_sec, DEFAULT_GC_INTERVAL_SEC);

        match saved {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn gc_hnsw_and_registry_sweep_intervals_are_independent() {
        let _guard = test_lock().lock();

        let gc_key = "DB9_WORKER_GC_INTERVAL_SEC";
        let hnsw_key = "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC";
        let registry_key = "DB9_WORKER_REGISTRY_SWEEP_INTERVAL_SEC";
        let gc_saved = env::var(gc_key).ok();
        let hnsw_saved = env::var(hnsw_key).ok();
        let registry_saved = env::var(registry_key).ok();

        unsafe {
            env::set_var(gc_key, "60");
            env::set_var(hnsw_key, "300");
            env::set_var(registry_key, "9");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.gc_interval_sec, 60);
        assert_eq!(cfg.hnsw_sweep_interval_sec, 300);
        assert_eq!(cfg.registry_sweep_interval_sec, 9);

        match gc_saved {
            Some(v) => unsafe { env::set_var(gc_key, v) },
            None => unsafe { env::remove_var(gc_key) },
        }
        match hnsw_saved {
            Some(v) => unsafe { env::set_var(hnsw_key, v) },
            None => unsafe { env::remove_var(hnsw_key) },
        }
        match registry_saved {
            Some(v) => unsafe { env::set_var(registry_key, v) },
            None => unsafe { env::remove_var(registry_key) },
        }
    }

    #[test]
    fn from_env_preserves_zero_worker_timeouts() {
        let _guard = test_lock().lock();

        let stmt_key = "DB9_WORKER_STATEMENT_TIMEOUT_MS";
        let cron_key = "DB9_CRON_JOB_TIMEOUT_MS";
        let stmt_saved = env::var(stmt_key).ok();
        let cron_saved = env::var(cron_key).ok();

        unsafe {
            env::set_var(stmt_key, "0");
            env::set_var(cron_key, "0");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.statement_timeout_ms, 0);
        assert_eq!(cfg.cron_job_timeout_ms, 0);

        match stmt_saved {
            Some(v) => unsafe { env::set_var(stmt_key, v) },
            None => unsafe { env::remove_var(stmt_key) },
        }
        match cron_saved {
            Some(v) => unsafe { env::set_var(cron_key, v) },
            None => unsafe { env::remove_var(cron_key) },
        }
    }

    // --- GC safepoint config validation tests ---

    #[test]
    fn gc_config_defaults_pass_validation() {
        let cfg = WorkerConfig::default();
        cfg.validate_gc_config(); // should not panic
    }

    #[test]
    fn gc_config_allows_zero_worker_timeouts_when_transactions_are_tracked() {
        let cfg = WorkerConfig {
            enabled: true,
            cron_job_timeout_ms: 0,
            statement_timeout_ms: 0,
            ..Default::default()
        };
        cfg.validate_gc_config();
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_interval_ge_life_time() {
        let cfg = WorkerConfig {
            gc_safepoint_enabled: true,
            gc_safepoint_interval_sec: 86400,
            gc_life_time_sec: 86400, // interval == life_time
            ..Default::default()
        };
        cfg.validate_gc_config();
    }

    #[test]
    fn gc_config_advancer_only_node_passes_with_short_life_time() {
        let cfg = WorkerConfig {
            enabled: false,
            gc_safepoint_enabled: true,
            gc_life_time_sec: 3600, // 1h — safe for advancer-only
            gc_safepoint_interval_sec: 300,
            ..Default::default()
        };
        cfg.validate_gc_config(); // should not panic
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_interval_ge_life_time_when_advancer_disabled() {
        let cfg = WorkerConfig {
            gc_safepoint_enabled: false,
            gc_safepoint_interval_sec: 86400,
            gc_life_time_sec: 86400,
            ..Default::default()
        };
        cfg.validate_gc_config();
    }

    #[test]
    fn gc_config_default_values() {
        let cfg = WorkerConfig::default();
        assert!(cfg.gc_safepoint_enabled);
        assert_eq!(cfg.gc_safepoint_interval_sec, 300);
        assert_eq!(cfg.gc_life_time_sec, 86400);
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_single_missed_heartbeat_fatal_edge() {
        // interval=599, life_time=600: a single missed publish makes the
        // row stale after just 1 extra second.
        let cfg = WorkerConfig {
            gc_safepoint_interval_sec: 599,
            gc_life_time_sec: 600,
            ..Default::default()
        };
        cfg.validate_gc_config();
    }

    #[test]
    fn gc_config_accepts_exactly_3x_ratio() {
        let cfg = WorkerConfig {
            gc_safepoint_interval_sec: 200,
            gc_life_time_sec: 600, // exactly 3x
            ..Default::default()
        };
        cfg.validate_gc_config(); // should not panic
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_just_below_3x_ratio() {
        let cfg = WorkerConfig {
            gc_safepoint_interval_sec: 201,
            gc_life_time_sec: 600, // 600 < 201*3 = 603
            ..Default::default()
        };
        cfg.validate_gc_config();
    }
}
