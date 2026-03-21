use std::env;

const DEFAULT_POLL_MS: u64 = 60_000;
const MIN_POLL_MS: u64 = 100;
const DEFAULT_MAX_CONCURRENT_JOBS: usize = 32;
const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_CRON_JOB_TIMEOUT_MS: u64 = 1_800_000;
const DEFAULT_ORPHAN_TIMEOUT_SEC: u64 = 300;
const DEFAULT_GC_BATCH_SIZE: usize = 100;
const DEFAULT_AUTO_ANALYZE_THRESHOLD: u64 = 50;
const DEFAULT_GC_INTERVAL_SEC: u64 = 600;
const MIN_GC_INTERVAL_SEC: u64 = 30;
const DEFAULT_HNSW_SWEEP_INTERVAL_SEC: u64 = 600;
const MIN_HNSW_SWEEP_INTERVAL_SEC: u64 = 30;
const DEFAULT_STORAGE_SCAN_INTERVAL_SEC: u64 = 1800;
const MIN_STORAGE_SCAN_INTERVAL_SEC: u64 = 60;
const DEFAULT_SYSTEM_KEYSPACE: &str = "_sys_worker";

// GC safepoint defaults — controls TiKV MVCC version cleanup.
// Without periodic safepoint advancement, TiKV never GCs old MVCC versions,
// causing unbounded storage growth and eventual compaction failure / worker panic.
const DEFAULT_GC_SAFEPOINT_ENABLED: bool = true;
const DEFAULT_GC_SAFEPOINT_INTERVAL_SEC: u64 = 300; // 5 minutes
const MIN_GC_SAFEPOINT_INTERVAL_SEC: u64 = 30;
// 24 hours — conservative default. Interactive SQL transactions are tracked
// via ActiveTxnRegistry and protected regardless of this value. This window
// covers untracked worker transactions (cron, BgSql) whose max duration is
// bounded by cron_job_timeout (default 30 min) and statement_timeout (default
// 5 min). Can be tightened to match max(cron_timeout, statement_timeout).
const DEFAULT_GC_LIFE_TIME_SEC: u64 = 86400;
const MIN_GC_LIFE_TIME_SEC: u64 = 600; // TiDB enforces minimum 10 minutes

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub enabled: bool,
    pub poll_ms: u64,
    pub max_concurrent_jobs: usize,
    pub worker_id: String,
    pub statement_timeout_ms: u64,
    pub cron_job_timeout_ms: u64,
    pub orphan_timeout_sec: u64,
    pub gc_batch_size: usize,
    pub auto_analyze_enabled: bool,
    pub auto_analyze_threshold: u64,
    pub gc_interval_sec: u64,
    pub hnsw_sweep_interval_sec: u64,
    pub storage_scan_interval_sec: u64,
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

        Self {
            enabled: true,
            poll_ms: DEFAULT_POLL_MS,
            max_concurrent_jobs: DEFAULT_MAX_CONCURRENT_JOBS,
            worker_id,
            statement_timeout_ms: DEFAULT_STATEMENT_TIMEOUT_MS,
            cron_job_timeout_ms: DEFAULT_CRON_JOB_TIMEOUT_MS,
            orphan_timeout_sec: DEFAULT_ORPHAN_TIMEOUT_SEC,
            gc_batch_size: DEFAULT_GC_BATCH_SIZE,
            auto_analyze_enabled: true,
            auto_analyze_threshold: DEFAULT_AUTO_ANALYZE_THRESHOLD,
            gc_interval_sec: DEFAULT_GC_INTERVAL_SEC,
            hnsw_sweep_interval_sec: DEFAULT_HNSW_SWEEP_INTERVAL_SEC,
            storage_scan_interval_sec: DEFAULT_STORAGE_SCAN_INTERVAL_SEC,
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

        if let Ok(v) = env::var("DB9_WORKER_ENABLED") {
            cfg.enabled = parse_bool(&v).unwrap_or(cfg.enabled);
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
            cfg.statement_timeout_ms = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.statement_timeout_ms);
        }
        if let Ok(v) = env::var("DB9_CRON_JOB_TIMEOUT_MS") {
            cfg.cron_job_timeout_ms = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.cron_job_timeout_ms);
        }
        if let Ok(v) = env::var("DB9_WORKER_ORPHAN_TIMEOUT_SEC") {
            cfg.orphan_timeout_sec = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.orphan_timeout_sec);
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

        cfg
    }

    /// Validate GC configuration invariants.
    ///
    /// **Key insight**: BgDdl and HnswMerge bypass statement_timeout but do NOT
    /// hold long-lived TiKV transactions. BgDdl rotates transactions every
    /// `DDL_BACKFILL_COMMIT_SIZE` (5000) writes. HnswMerge begins a new
    /// transaction per merge batch. Each sub-transaction lasts seconds, not hours.
    /// Therefore gc_life_time only needs to cover the longest **single transaction**,
    /// not the total task duration. cron_job_timeout (default 30 min) remains the
    /// binding constraint.
    pub fn validate_gc_config(&self) {
        // Worker timeout checks: only when this node runs worker tasks.
        if self.enabled {
            if self.cron_job_timeout_ms == 0 {
                // Cron transactions are NOT tracked in the active txn registry
                // (they use store.begin() directly, not Session). Without a finite
                // timeout, gc_life_time cannot cover them, and an advancer (on this
                // or any other node) may push safepoint past a running cron job.
                panic!(
                    "UNSAFE CONFIG: DB9_CRON_JOB_TIMEOUT_MS=0 (no timeout) while worker \
                     is enabled. Cron job transactions are not tracked in the GC registry \
                     and require a finite timeout for GC safety. \
                     Set DB9_CRON_JOB_TIMEOUT_MS > 0.",
                );
            } else {
                let cron_timeout_sec = self.cron_job_timeout_ms.saturating_add(999) / 1000;
                if self.gc_life_time_sec < cron_timeout_sec {
                    panic!(
                        "UNSAFE CONFIG: DB9_GC_LIFE_TIME_SEC ({}) < cron_job_timeout ({}s). \
                         GC could reclaim data needed by running cron jobs. \
                         Either increase DB9_GC_LIFE_TIME_SEC or decrease DB9_CRON_JOB_TIMEOUT_MS.",
                        self.gc_life_time_sec, cron_timeout_sec,
                    );
                }
            }

            if self.statement_timeout_ms == 0 {
                panic!(
                    "UNSAFE CONFIG: DB9_WORKER_STATEMENT_TIMEOUT_MS=0 (no timeout) while \
                     worker is enabled. Worker task transactions (BgSql, AutoAnalyze) are \
                     not tracked in the GC registry and require a finite timeout for GC safety. \
                     Set DB9_WORKER_STATEMENT_TIMEOUT_MS > 0.",
                );
            } else {
                let stmt_timeout_sec = self.statement_timeout_ms.saturating_add(999) / 1000;
                if self.gc_life_time_sec < stmt_timeout_sec {
                    panic!(
                        "UNSAFE CONFIG: DB9_GC_LIFE_TIME_SEC ({}) < statement_timeout ({}s). \
                         Worker tasks (BgSql, AutoAnalyze) use statement_timeout and open \
                         TiKV transactions directly without session registry tracking. \
                         Either increase DB9_GC_LIFE_TIME_SEC or decrease DB9_WORKER_STATEMENT_TIMEOUT_MS.",
                        self.gc_life_time_sec, stmt_timeout_sec,
                    );
                }
            }
        }

        // Advancer interval check.
        if self.gc_safepoint_enabled && self.gc_safepoint_interval_sec >= self.gc_life_time_sec {
            panic!(
                "UNSAFE CONFIG: DB9_GC_SAFEPOINT_INTERVAL_SEC ({}) >= DB9_GC_LIFE_TIME_SEC ({}). \
                 The advancement interval must be shorter than the retention window.",
                self.gc_safepoint_interval_sec, self.gc_life_time_sec,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn from_env_uses_defaults_when_vars_not_set() {
        let _guard = test_lock().lock().unwrap();

        let keys = [
            "DB9_WORKER_ENABLED",
            "DB9_WORKER_POLL_MS",
            "DB9_WORKER_MAX_CONCURRENT_JOBS",
            "DB9_WORKER_ID",
            "DB9_WORKER_STATEMENT_TIMEOUT_MS",
            "DB9_CRON_JOB_TIMEOUT_MS",
            "DB9_WORKER_ORPHAN_TIMEOUT_SEC",
            "DB9_WORKER_GC_BATCH_SIZE",
            "DB9_AUTO_ANALYZE_ENABLED",
            "DB9_AUTO_ANALYZE_THRESHOLD",
            "DB9_WORKER_GC_INTERVAL_SEC",
            "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC",
            "DB9_WORKER_STORAGE_SCAN_INTERVAL_SEC",
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
        assert_eq!(cfg.gc_batch_size, DEFAULT_GC_BATCH_SIZE);
        assert!(cfg.auto_analyze_enabled);
        assert_eq!(cfg.auto_analyze_threshold, DEFAULT_AUTO_ANALYZE_THRESHOLD);
        assert_eq!(cfg.gc_interval_sec, DEFAULT_GC_INTERVAL_SEC);
        assert_eq!(cfg.hnsw_sweep_interval_sec, DEFAULT_HNSW_SWEEP_INTERVAL_SEC);
        assert_eq!(
            cfg.storage_scan_interval_sec,
            DEFAULT_STORAGE_SCAN_INTERVAL_SEC
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
        let _guard = test_lock().lock().unwrap();

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
        let _guard = test_lock().lock().unwrap();

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
        let _guard = test_lock().lock().unwrap();

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
        let _lock = test_lock().lock().unwrap();
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
        for input in &["maybe", "", "2", "yep", "nope", "enabled"] {
            assert_eq!(parse_bool(input), None, "input: {}", input);
        }
    }

    #[test]
    fn test_config_default_values() {
        let cfg = WorkerConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.poll_ms, 60_000);
        assert_eq!(cfg.max_concurrent_jobs, 32);
        assert_eq!(cfg.statement_timeout_ms, 300_000);
        assert_eq!(cfg.cron_job_timeout_ms, 1_800_000);
        assert_eq!(cfg.orphan_timeout_sec, 300);
        assert_eq!(cfg.gc_batch_size, 100);
        assert!(cfg.auto_analyze_enabled);
        assert_eq!(cfg.auto_analyze_threshold, 50);
        assert_eq!(cfg.gc_interval_sec, 600);
        assert_eq!(cfg.hnsw_sweep_interval_sec, 600);
        assert_eq!(cfg.storage_scan_interval_sec, 1800);
        assert_eq!(cfg.system_keyspace, "_sys_worker");
    }

    #[test]
    fn from_env_applies_hnsw_sweep_interval_when_at_least_minimum() {
        let _guard = test_lock().lock().unwrap();

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
        let _guard = test_lock().lock().unwrap();

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
        let _guard = test_lock().lock().unwrap();

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
    fn from_env_applies_gc_interval_when_at_least_minimum() {
        let _guard = test_lock().lock().unwrap();

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
        let _guard = test_lock().lock().unwrap();

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
    fn gc_and_hnsw_sweep_intervals_are_independent() {
        let _guard = test_lock().lock().unwrap();

        let gc_key = "DB9_WORKER_GC_INTERVAL_SEC";
        let hnsw_key = "DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC";
        let gc_saved = env::var(gc_key).ok();
        let hnsw_saved = env::var(hnsw_key).ok();

        unsafe {
            env::set_var(gc_key, "60");
            env::set_var(hnsw_key, "300");
        }

        let cfg = WorkerConfig::from_env();
        assert_eq!(cfg.gc_interval_sec, 60);
        assert_eq!(cfg.hnsw_sweep_interval_sec, 300);

        match gc_saved {
            Some(v) => unsafe { env::set_var(gc_key, v) },
            None => unsafe { env::remove_var(gc_key) },
        }
        match hnsw_saved {
            Some(v) => unsafe { env::set_var(hnsw_key, v) },
            None => unsafe { env::remove_var(hnsw_key) },
        }
    }

    // --- GC safepoint config validation tests ---

    #[test]
    fn gc_config_defaults_pass_validation() {
        let cfg = WorkerConfig::default();
        cfg.validate_gc_config(); // should not panic
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_life_time_below_cron_timeout() {
        let cfg = WorkerConfig {
            gc_safepoint_enabled: true,
            gc_life_time_sec: 600,          // 10 min
            cron_job_timeout_ms: 1_800_000, // 30 min — exceeds gc_life_time
            ..Default::default()
        };
        cfg.validate_gc_config();
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_cron_timeout_zero() {
        let cfg = WorkerConfig {
            enabled: true,
            cron_job_timeout_ms: 0,
            ..Default::default()
        };
        cfg.validate_gc_config();
    }

    #[test]
    #[should_panic(expected = "UNSAFE CONFIG")]
    fn gc_config_rejects_statement_timeout_zero() {
        let cfg = WorkerConfig {
            enabled: true,
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
        // Advancer-only nodes (no worker) don't need 24h minimum.
        // BgDdl/HnswMerge rotate transactions per batch — each sub-txn
        // is seconds, not hours. gc_life_time only covers single-txn duration.
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
    fn gc_config_interval_check_skipped_when_advancer_disabled() {
        let cfg = WorkerConfig {
            gc_safepoint_enabled: false,
            gc_safepoint_interval_sec: 86400,
            gc_life_time_sec: 86400,
            ..Default::default()
        };
        cfg.validate_gc_config(); // should not panic
    }

    #[test]
    fn gc_config_default_values() {
        let cfg = WorkerConfig::default();
        assert!(cfg.gc_safepoint_enabled);
        assert_eq!(cfg.gc_safepoint_interval_sec, 300);
        assert_eq!(cfg.gc_life_time_sec, 86400);
    }
}
