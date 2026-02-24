use std::env;
use std::sync::Arc;
use std::sync::RwLock;

const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS: u64 = 60_000;

pub(crate) fn env_bool(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            _ => false,
        })
        .unwrap_or(false)
}

pub(crate) fn env_string(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub statement_timeout_ms: u64,
    pub idle_in_transaction_session_timeout_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            statement_timeout_ms: DEFAULT_STATEMENT_TIMEOUT_MS,
            idle_in_transaction_session_timeout_ms: DEFAULT_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS,
        }
    }
}

impl ServerConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(v) = env::var("DB9_STATEMENT_TIMEOUT_MS") {
            cfg.statement_timeout_ms = v.parse::<u64>().ok().unwrap_or(cfg.statement_timeout_ms);
        }
        if let Ok(v) = env::var("DB9_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS") {
            cfg.idle_in_transaction_session_timeout_ms = v
                .parse::<u64>()
                .ok()
                .unwrap_or(cfg.idle_in_transaction_session_timeout_ms);
        }

        cfg
    }

    pub fn shared(self) -> SharedServerConfig {
        Arc::new(RwLock::new(self))
    }
}

pub type SharedServerConfig = Arc<RwLock<ServerConfig>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::OnceLock;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn test_default_values() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.statement_timeout_ms, 60_000);
        assert_eq!(cfg.idle_in_transaction_session_timeout_ms, 60_000);
    }

    #[test]
    fn test_from_env_uses_defaults_when_vars_not_set() {
        let _guard = test_lock().lock().unwrap();

        let keys = [
            "DB9_STATEMENT_TIMEOUT_MS",
            "DB9_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS",
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

        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.statement_timeout_ms, 60_000);
        assert_eq!(cfg.idle_in_transaction_session_timeout_ms, 60_000);

        for (key, value) in saved {
            match value {
                Some(v) => unsafe {
                    env::set_var(&key, v);
                },
                None => unsafe {
                    env::remove_var(&key);
                },
            }
        }
    }

    #[test]
    fn test_statement_timeout_env_override() {
        let _guard = test_lock().lock().unwrap();

        let key = "DB9_STATEMENT_TIMEOUT_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "30000");
        }
        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.statement_timeout_ms, 30_000);
        assert_eq!(cfg.idle_in_transaction_session_timeout_ms, 60_000);

        match saved {
            Some(v) => unsafe {
                env::set_var(key, v);
            },
            None => unsafe {
                env::remove_var(key);
            },
        }
    }

    #[test]
    fn test_idle_in_transaction_timeout_env_override() {
        let _guard = test_lock().lock().unwrap();

        let key = "DB9_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "45000");
        }
        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.statement_timeout_ms, 60_000);
        assert_eq!(cfg.idle_in_transaction_session_timeout_ms, 45_000);

        match saved {
            Some(v) => unsafe {
                env::set_var(key, v);
            },
            None => unsafe {
                env::remove_var(key);
            },
        }
    }

    #[test]
    fn test_zero_timeout_allowed() {
        let _guard = test_lock().lock().unwrap();

        let key = "DB9_STATEMENT_TIMEOUT_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "0");
        }
        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.statement_timeout_ms, 0);

        match saved {
            Some(v) => unsafe {
                env::set_var(key, v);
            },
            None => unsafe {
                env::remove_var(key);
            },
        }
    }

    #[test]
    fn test_invalid_env_var_falls_back_to_default() {
        let _guard = test_lock().lock().unwrap();

        let key = "DB9_STATEMENT_TIMEOUT_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "abc");
        }
        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.statement_timeout_ms, 60_000);

        match saved {
            Some(v) => unsafe {
                env::set_var(key, v);
            },
            None => unsafe {
                env::remove_var(key);
            },
        }
    }

    #[test]
    fn test_shared_config() {
        let cfg = ServerConfig {
            statement_timeout_ms: 30_000,
            idle_in_transaction_session_timeout_ms: 45_000,
        };
        let shared = cfg.shared();

        {
            let read_cfg = shared.read().unwrap();
            assert_eq!(read_cfg.statement_timeout_ms, 30_000);
            assert_eq!(read_cfg.idle_in_transaction_session_timeout_ms, 45_000);
        }

        {
            let mut write_cfg = shared.write().unwrap();
            write_cfg.statement_timeout_ms = 20_000;
        }

        {
            let read_cfg = shared.read().unwrap();
            assert_eq!(read_cfg.statement_timeout_ms, 20_000);
        }
    }
}
