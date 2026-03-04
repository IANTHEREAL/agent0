use std::env;
use std::sync::RwLock;
use std::sync::{Arc, OnceLock};

const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_CONNECTIONS: u32 = 1000;
const DEFAULT_EMBEDDING_ENDPOINT: &str =
    "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/embeddings";
pub(crate) const DEFAULT_EMBEDDING_MODEL: &str = "text-embedding-v4";
const DEFAULT_EMBEDDING_DIMENSIONS: u32 = 1024;

pub(crate) fn canonical_embedding_model(model: &str) -> Option<&'static str> {
    if model.trim().eq_ignore_ascii_case(DEFAULT_EMBEDDING_MODEL) {
        Some(DEFAULT_EMBEDDING_MODEL)
    } else {
        None
    }
}

pub(crate) fn env_bool(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
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
pub struct EmbeddingConfig {
    pub api_key: Option<String>,
    pub endpoint: String,
    pub model: String,
    pub dimensions: u32,
}

impl EmbeddingConfig {
    pub fn from_env() -> Self {
        let model = env_string("EMBEDDING_MODEL")
            .map(|value| {
                if let Some(canonical) = canonical_embedding_model(&value) {
                    canonical.to_string()
                } else {
                    tracing::warn!(
                        requested = %value,
                        actual = DEFAULT_EMBEDDING_MODEL,
                        "unsupported EMBEDDING_MODEL; forcing text-embedding-v4"
                    );
                    DEFAULT_EMBEDDING_MODEL.to_string()
                }
            })
            .unwrap_or_else(|| DEFAULT_EMBEDDING_MODEL.to_string());

        Self {
            api_key: env_string("EMBEDDING_API_KEY"),
            endpoint: embedding_endpoint_from_env(),
            model,
            dimensions: std::env::var("EMBEDDING_DIMENSIONS")
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|d| *d > 0)
                .unwrap_or(DEFAULT_EMBEDDING_DIMENSIONS),
        }
    }

    pub fn is_available(&self) -> bool {
        self.api_key.is_some()
    }
}

fn embedding_endpoint_from_env() -> String {
    // Support both EMBEDDING_ENDPOINT and EMBEDDING_BASE_URL naming styles.
    let raw = env_string("EMBEDDING_ENDPOINT")
        .or_else(|| env_string("EMBEDDING_BASE_URL"))
        .unwrap_or_else(|| DEFAULT_EMBEDDING_ENDPOINT.to_string());
    normalize_embedding_endpoint(&raw)
}

fn normalize_embedding_endpoint(raw: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(raw) {
        let mut path = url.path().trim_end_matches('/').to_string();
        if path.is_empty() {
            path = "/".to_string();
        }
        if !path.ends_with("/embeddings") {
            if path == "/" {
                path = "/embeddings".to_string();
            } else {
                path.push_str("/embeddings");
            }
        }
        url.set_path(&path);
        return url.to_string();
    }

    let trimmed = raw.trim_end_matches('/');
    if trimmed.ends_with("/embeddings") {
        trimmed.to_string()
    } else {
        format!("{}/embeddings", trimmed)
    }
}

static EMBEDDING_CONFIG: OnceLock<EmbeddingConfig> = OnceLock::new();

pub fn get_embedding_config() -> &'static EmbeddingConfig {
    EMBEDDING_CONFIG.get_or_init(EmbeddingConfig::from_env)
}

pub fn init_embedding_config() {
    let _ = get_embedding_config();
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub statement_timeout_ms: u64,
    pub idle_in_transaction_session_timeout_ms: u64,
    pub max_connections: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            statement_timeout_ms: DEFAULT_STATEMENT_TIMEOUT_MS,
            idle_in_transaction_session_timeout_ms: DEFAULT_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS,
            max_connections: DEFAULT_MAX_CONNECTIONS,
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
        if let Ok(v) = env::var("DB9_MAX_CONNECTIONS") {
            cfg.max_connections = v
                .parse::<u32>()
                .ok()
                .filter(|&n| n > 0)
                .unwrap_or(cfg.max_connections);
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
        assert_eq!(cfg.max_connections, 1000);
    }

    #[test]
    fn test_from_env_uses_defaults_when_vars_not_set() {
        let _guard = test_lock().lock().unwrap();

        let keys = [
            "DB9_STATEMENT_TIMEOUT_MS",
            "DB9_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS",
            "DB9_MAX_CONNECTIONS",
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
        assert_eq!(cfg.max_connections, 1000);

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
    fn test_max_connections_env_override() {
        let _guard = test_lock().lock().unwrap();

        let key = "DB9_MAX_CONNECTIONS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "50");
        }
        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.max_connections, 50);

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
    fn test_max_connections_zero_falls_back_to_default() {
        let _guard = test_lock().lock().unwrap();

        let key = "DB9_MAX_CONNECTIONS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "0");
        }
        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.max_connections, 1000);

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
    fn test_max_connections_invalid_falls_back_to_default() {
        let _guard = test_lock().lock().unwrap();

        let key = "DB9_MAX_CONNECTIONS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "not_a_number");
        }
        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.max_connections, 1000);

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
            max_connections: 500,
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

    #[test]
    fn test_normalize_embedding_endpoint_base_url() {
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com/v1"),
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com"),
            "https://api.openai.com/embeddings"
        );
    }

    #[test]
    fn test_normalize_embedding_endpoint_keeps_existing_embeddings_path() {
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com/v1/embeddings"),
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com/v1/embeddings/"),
            "https://api.openai.com/v1/embeddings"
        );
    }

    #[test]
    fn test_normalize_embedding_endpoint_preserves_query() {
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com/v1/embeddings?x=1"),
            "https://api.openai.com/v1/embeddings?x=1"
        );
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com/v1?x=1"),
            "https://api.openai.com/v1/embeddings?x=1"
        );
    }

    #[test]
    fn test_normalize_embedding_endpoint_fallback_for_non_url_inputs() {
        assert_eq!(
            normalize_embedding_endpoint("api.openai.com/v1"),
            "api.openai.com/v1/embeddings"
        );
        assert_eq!(
            normalize_embedding_endpoint("api.openai.com/v1/embeddings"),
            "api.openai.com/v1/embeddings"
        );
    }
}
