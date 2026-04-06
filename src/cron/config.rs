use std::env;

const DEFAULT_POLL_MS: u64 = 60_000;
const DEFAULT_MAX_RUNNING_JOBS: usize = 32;
const DEFAULT_MAX_JOBS_PER_DB: usize = 50;
const DEFAULT_GC_INTERVAL_SEC: u64 = 3_600;
const DEFAULT_RUN_RETENTION_DAYS: u64 = 7;
const DEFAULT_ORPHAN_TIMEOUT_SEC: u64 = 300;

#[derive(Debug, Clone)]
pub struct CronConfig {
    pub enabled: bool,
    pub poll_ms: u64,
    pub max_running_jobs: usize,
    pub max_jobs_per_db: usize,
    pub gc_interval_sec: u64,
    pub run_retention_days: u64,
    pub orphan_timeout_sec: u64,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_ms: DEFAULT_POLL_MS,
            max_running_jobs: DEFAULT_MAX_RUNNING_JOBS,
            max_jobs_per_db: DEFAULT_MAX_JOBS_PER_DB,
            gc_interval_sec: DEFAULT_GC_INTERVAL_SEC,
            run_retention_days: DEFAULT_RUN_RETENTION_DAYS,
            orphan_timeout_sec: DEFAULT_ORPHAN_TIMEOUT_SEC,
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

impl CronConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(v) = env::var("DB9_CRON_ENABLED") {
            cfg.enabled = parse_bool(&v).unwrap_or(cfg.enabled);
        }
        if let Ok(v) = env::var("DB9_CRON_POLL_MS") {
            cfg.poll_ms = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.poll_ms);
            cfg.poll_ms = cfg.poll_ms.max(DEFAULT_POLL_MS);
        }
        if let Ok(v) = env::var("DB9_CRON_MAX_RUNNING_JOBS") {
            cfg.max_running_jobs = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.max_running_jobs);
        }
        if let Ok(v) = env::var("DB9_CRON_MAX_JOBS_PER_DB") {
            cfg.max_jobs_per_db = v
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.max_jobs_per_db);
        }
        if let Ok(v) = env::var("DB9_CRON_GC_INTERVAL_SEC") {
            cfg.gc_interval_sec = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.gc_interval_sec);
        }
        if let Ok(v) = env::var("DB9_CRON_RUN_RETENTION_DAYS") {
            cfg.run_retention_days = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.run_retention_days);
        }
        if let Ok(v) = env::var("DB9_CRON_ORPHAN_TIMEOUT_SEC") {
            cfg.orphan_timeout_sec = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(cfg.orphan_timeout_sec);
        }

        cfg
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
            "DB9_CRON_ENABLED",
            "DB9_CRON_POLL_MS",
            "DB9_CRON_MAX_RUNNING_JOBS",
            "DB9_CRON_MAX_JOBS_PER_DB",
            "DB9_CRON_GC_INTERVAL_SEC",
            "DB9_CRON_RUN_RETENTION_DAYS",
            "DB9_CRON_ORPHAN_TIMEOUT_SEC",
        ];

        let saved: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|k| (k.to_string(), env::var(k).ok()))
            .collect();

        for key in &keys {
            env::remove_var(key);
        }

        let cfg = CronConfig::from_env();
        assert!(cfg.enabled);
        assert_eq!(cfg.poll_ms, DEFAULT_POLL_MS);
        assert_eq!(cfg.max_running_jobs, DEFAULT_MAX_RUNNING_JOBS);
        assert_eq!(cfg.max_jobs_per_db, DEFAULT_MAX_JOBS_PER_DB);
        assert_eq!(cfg.gc_interval_sec, DEFAULT_GC_INTERVAL_SEC);
        assert_eq!(cfg.run_retention_days, DEFAULT_RUN_RETENTION_DAYS);
        assert_eq!(cfg.orphan_timeout_sec, DEFAULT_ORPHAN_TIMEOUT_SEC);

        for (key, value) in saved {
            match value {
                Some(v) => env::set_var(key, v),
                None => env::remove_var(key),
            }
        }
    }
}
