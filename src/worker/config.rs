use std::env;

const DEFAULT_POLL_MS: u64 = 60_000;
const DEFAULT_MAX_CONCURRENT_JOBS: usize = 32;
const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_CRON_JOB_TIMEOUT_MS: u64 = 1_800_000;
const DEFAULT_ORPHAN_TIMEOUT_SEC: u64 = 300;
const DEFAULT_GC_BATCH_SIZE: usize = 100;
const DEFAULT_AUTO_ANALYZE_THRESHOLD: u64 = 50;
const DEFAULT_SYSTEM_KEYSPACE: &str = "_sys_worker";

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
    pub system_keyspace: String,
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
            system_keyspace: DEFAULT_SYSTEM_KEYSPACE.to_string(),
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

        if let Ok(v) = env::var("PGTIKV_WORKER_ENABLED") {
            cfg.enabled = parse_bool(&v).unwrap_or(cfg.enabled);
        }
        if let Ok(v) = env::var("PGTIKV_WORKER_POLL_MS") {
            cfg.poll_ms = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.poll_ms);
            cfg.poll_ms = cfg.poll_ms.max(DEFAULT_POLL_MS);
        }
        if let Ok(v) = env::var("PGTIKV_WORKER_MAX_CONCURRENT_JOBS") {
            cfg.max_concurrent_jobs = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.max_concurrent_jobs);
        }
        if let Ok(v) = env::var("PGTIKV_WORKER_ID") {
            cfg.worker_id = v;
        }
        if let Ok(v) = env::var("PGTIKV_WORKER_STATEMENT_TIMEOUT_MS") {
            cfg.statement_timeout_ms = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.statement_timeout_ms);
        }
        if let Ok(v) = env::var("PGTIKV_CRON_JOB_TIMEOUT_MS") {
            cfg.cron_job_timeout_ms = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.cron_job_timeout_ms);
        }
        if let Ok(v) = env::var("PGTIKV_WORKER_ORPHAN_TIMEOUT_SEC") {
            cfg.orphan_timeout_sec = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.orphan_timeout_sec);
        }
        if let Ok(v) = env::var("PGTIKV_WORKER_GC_BATCH_SIZE") {
            cfg.gc_batch_size = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.gc_batch_size);
        }
        if let Ok(v) = env::var("PGTIKV_AUTO_ANALYZE_ENABLED") {
            cfg.auto_analyze_enabled = parse_bool(&v).unwrap_or(cfg.auto_analyze_enabled);
        }
        if let Ok(v) = env::var("PGTIKV_AUTO_ANALYZE_THRESHOLD") {
            cfg.auto_analyze_threshold = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.auto_analyze_threshold);
        }
        if let Ok(v) = env::var("PGTIKV_WORKER_SYSTEM_KEYSPACE") {
            cfg.system_keyspace = v;
        }

        cfg
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
            "PGTIKV_WORKER_ENABLED",
            "PGTIKV_WORKER_POLL_MS",
            "PGTIKV_WORKER_MAX_CONCURRENT_JOBS",
            "PGTIKV_WORKER_ID",
            "PGTIKV_WORKER_STATEMENT_TIMEOUT_MS",
            "PGTIKV_CRON_JOB_TIMEOUT_MS",
            "PGTIKV_WORKER_ORPHAN_TIMEOUT_SEC",
            "PGTIKV_WORKER_GC_BATCH_SIZE",
            "PGTIKV_AUTO_ANALYZE_ENABLED",
            "PGTIKV_AUTO_ANALYZE_THRESHOLD",
            "PGTIKV_WORKER_SYSTEM_KEYSPACE",
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
        assert_eq!(cfg.system_keyspace, DEFAULT_SYSTEM_KEYSPACE);

        for (key, value) in saved {
            match value {
                Some(v) => unsafe { env::set_var(key, v) },
                None => unsafe { env::remove_var(key) },
            }
        }
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
        assert_eq!(cfg.system_keyspace, "_sys_worker");
    }
}
