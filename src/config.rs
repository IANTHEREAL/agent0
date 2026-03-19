use std::env;
use std::sync::RwLock;
use std::sync::{Arc, OnceLock};

const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_TCP_KEEPALIVE_IDLE_MS: u64 = 60_000;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Db9AuthMode {
    Password,
    Both,
    Token,
}

impl Db9AuthMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "password" => Some(Self::Password),
            "both" => Some(Self::Both),
            "token" => Some(Self::Token),
            _ => None,
        }
    }

    pub fn canonical_name(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Both => "both",
            Self::Token => "token",
        }
    }
}

static DB9_AUTH_MODE: OnceLock<Db9AuthMode> = OnceLock::new();

pub(crate) fn db9_auth_mode() -> Db9AuthMode {
    *DB9_AUTH_MODE.get_or_init(|| match env_string("DB9_AUTH_MODE") {
        Some(raw) => Db9AuthMode::parse(&raw).unwrap_or_else(|| {
            tracing::warn!("Invalid DB9_AUTH_MODE value '{raw}', falling back to 'password'");
            Db9AuthMode::Password
        }),
        None => Db9AuthMode::Password,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmbeddingProvider {
    OpenAICompatible,
    Bedrock,
}

impl EmbeddingProvider {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai_compatible" | "openai-compatible" => Some(Self::OpenAICompatible),
            "bedrock" | "aws_bedrock" | "aws-bedrock" => Some(Self::Bedrock),
            _ => None,
        }
    }

    pub fn from_env_str(s: &str) -> Self {
        Self::parse(s).unwrap_or(Self::OpenAICompatible)
    }

    pub fn canonical_name(self) -> &'static str {
        match self {
            Self::OpenAICompatible => "openai",
            Self::Bedrock => "bedrock",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    pub provider_name: String,
    pub api_key: Option<String>,
    pub endpoint: String,
    pub model: String,
    pub dimensions: u32,
}

impl EmbeddingConfig {
    pub fn from_env() -> Self {
        let provider_name =
            env_string("EMBEDDING_PROVIDER").unwrap_or_else(|| "openai".to_string());
        let provider = EmbeddingProvider::from_env_str(&provider_name);

        let model = env_string("EMBEDDING_MODEL")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_EMBEDDING_MODEL.to_string());

        Self {
            provider_name: provider.canonical_name().to_string(),
            api_key: env_string("EMBEDDING_API_KEY"),
            endpoint: embedding_endpoint_from_env(&provider),
            model,
            dimensions: std::env::var("EMBEDDING_DIMENSIONS")
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|d| *d > 0)
                .unwrap_or(DEFAULT_EMBEDDING_DIMENSIONS),
        }
    }
}

fn embedding_endpoint_from_env(provider: &EmbeddingProvider) -> String {
    // Support both EMBEDDING_ENDPOINT and EMBEDDING_BASE_URL naming styles.
    let raw = env_string("EMBEDDING_ENDPOINT")
        .or_else(|| env_string("EMBEDDING_BASE_URL"))
        .unwrap_or_else(|| DEFAULT_EMBEDDING_ENDPOINT.to_string());
    normalize_embedding_endpoint(&raw, provider)
}

pub(crate) fn normalize_embedding_endpoint(raw: &str, provider: &EmbeddingProvider) -> String {
    // Bedrock endpoints already contain /invoke in the ARN URL — never append /embeddings.
    if *provider == EmbeddingProvider::Bedrock {
        return raw.trim_end_matches('/').to_string();
    }

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
    pub tcp_keepalive_idle_ms: u64,
    pub max_connections: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            statement_timeout_ms: DEFAULT_STATEMENT_TIMEOUT_MS,
            idle_in_transaction_session_timeout_ms: DEFAULT_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS,
            tcp_keepalive_idle_ms: DEFAULT_TCP_KEEPALIVE_IDLE_MS,
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
        if let Ok(v) = env::var("DB9_TCP_KEEPALIVE_IDLE_MS") {
            cfg.tcp_keepalive_idle_ms = v.parse::<u64>().ok().unwrap_or(cfg.tcp_keepalive_idle_ms);
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
        assert_eq!(cfg.tcp_keepalive_idle_ms, 60_000);
        assert_eq!(cfg.max_connections, 1000);
    }

    #[test]
    fn test_from_env_uses_defaults_when_vars_not_set() {
        let _guard = test_lock().lock().unwrap();

        let keys = [
            "DB9_STATEMENT_TIMEOUT_MS",
            "DB9_IDLE_IN_TRANSACTION_SESSION_TIMEOUT_MS",
            "DB9_TCP_KEEPALIVE_IDLE_MS",
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
        assert_eq!(cfg.tcp_keepalive_idle_ms, 60_000);
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
        assert_eq!(cfg.tcp_keepalive_idle_ms, 60_000);

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
    fn test_tcp_keepalive_idle_env_override() {
        let _guard = test_lock().lock().unwrap();

        let key = "DB9_TCP_KEEPALIVE_IDLE_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "45000");
        }
        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.statement_timeout_ms, 60_000);
        assert_eq!(cfg.idle_in_transaction_session_timeout_ms, 60_000);
        assert_eq!(cfg.tcp_keepalive_idle_ms, 45_000);

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
    fn test_tcp_keepalive_idle_zero_disables_keepalive() {
        let _guard = test_lock().lock().unwrap();

        let key = "DB9_TCP_KEEPALIVE_IDLE_MS";
        let saved = env::var(key).ok();

        unsafe {
            env::set_var(key, "0");
        }
        let cfg = ServerConfig::from_env();
        assert_eq!(cfg.tcp_keepalive_idle_ms, 0);

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
            tcp_keepalive_idle_ms: 20_000,
            max_connections: 500,
        };
        let shared = cfg.shared();

        {
            let read_cfg = shared.read().unwrap();
            assert_eq!(read_cfg.statement_timeout_ms, 30_000);
            assert_eq!(read_cfg.idle_in_transaction_session_timeout_ms, 45_000);
            assert_eq!(read_cfg.tcp_keepalive_idle_ms, 20_000);
        }

        {
            let mut write_cfg = shared.write().unwrap();
            write_cfg.statement_timeout_ms = 20_000;
        }

        {
            let read_cfg = shared.read().unwrap();
            assert_eq!(read_cfg.statement_timeout_ms, 20_000);
            assert_eq!(read_cfg.tcp_keepalive_idle_ms, 20_000);
        }
    }

    #[test]
    fn test_normalize_embedding_endpoint_base_url() {
        let openai = EmbeddingProvider::OpenAICompatible;
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com/v1", &openai),
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com", &openai),
            "https://api.openai.com/embeddings"
        );
    }

    #[test]
    fn test_normalize_embedding_endpoint_keeps_existing_embeddings_path() {
        let openai = EmbeddingProvider::OpenAICompatible;
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com/v1/embeddings", &openai),
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com/v1/embeddings/", &openai),
            "https://api.openai.com/v1/embeddings"
        );
    }

    #[test]
    fn test_normalize_embedding_endpoint_preserves_query() {
        let openai = EmbeddingProvider::OpenAICompatible;
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com/v1/embeddings?x=1", &openai),
            "https://api.openai.com/v1/embeddings?x=1"
        );
        assert_eq!(
            normalize_embedding_endpoint("https://api.openai.com/v1?x=1", &openai),
            "https://api.openai.com/v1/embeddings?x=1"
        );
    }

    #[test]
    fn test_normalize_embedding_endpoint_fallback_for_non_url_inputs() {
        let openai = EmbeddingProvider::OpenAICompatible;
        assert_eq!(
            normalize_embedding_endpoint("api.openai.com/v1", &openai),
            "api.openai.com/v1/embeddings"
        );
        assert_eq!(
            normalize_embedding_endpoint("api.openai.com/v1/embeddings", &openai),
            "api.openai.com/v1/embeddings"
        );
    }

    #[test]
    fn test_normalize_bedrock_endpoint_no_embeddings_suffix() {
        let bedrock = EmbeddingProvider::Bedrock;
        let url = "https://bedrock-runtime.us-west-2.amazonaws.com/model/arn:aws:bedrock:us-west-2:123456789012:application-inference-profile/example-profile/invoke";
        assert_eq!(normalize_embedding_endpoint(url, &bedrock), url);
    }

    #[test]
    fn test_normalize_bedrock_endpoint_trims_trailing_slash() {
        let bedrock = EmbeddingProvider::Bedrock;
        assert_eq!(
            normalize_embedding_endpoint("https://bedrock.example.com/invoke/", &bedrock),
            "https://bedrock.example.com/invoke"
        );
    }

    #[test]
    fn test_embedding_provider_from_env_str() {
        assert_eq!(
            EmbeddingProvider::from_env_str("bedrock"),
            EmbeddingProvider::Bedrock
        );
        assert_eq!(
            EmbeddingProvider::from_env_str("BEDROCK"),
            EmbeddingProvider::Bedrock
        );
        assert_eq!(
            EmbeddingProvider::from_env_str("aws_bedrock"),
            EmbeddingProvider::Bedrock
        );
        assert_eq!(
            EmbeddingProvider::from_env_str("aws-bedrock"),
            EmbeddingProvider::Bedrock
        );
        assert_eq!(
            EmbeddingProvider::from_env_str("openai"),
            EmbeddingProvider::OpenAICompatible
        );
        assert_eq!(
            EmbeddingProvider::from_env_str(""),
            EmbeddingProvider::OpenAICompatible
        );
        assert_eq!(
            EmbeddingProvider::from_env_str("whatever"),
            EmbeddingProvider::OpenAICompatible
        );
    }
}
