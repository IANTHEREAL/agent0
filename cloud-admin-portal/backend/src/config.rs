use std::env;

use crate::DEFAULT_PG_PORT;

#[derive(Clone, Debug)]
pub struct Config {
    pub pd_endpoints: String,
    pub pg_host: String,
    pub pg_port: u16,
    pub pg_public_endpoints: String,
    pub api_port: u16,
    pub api_host: String,
    pub database_url: String,
    pub cors_origins: Vec<String>,
    pub api_keys: Vec<String>,
    pub reconciler_enabled: bool,
    pub reconciler_interval_secs: u64,
    pub session_ttl_hours: u64,
    pub credential_key: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let api_keys_raw = env::var("PGTIKV_API_KEYS").unwrap_or_default();
        let api_keys: Vec<String> = api_keys_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let cors_raw = env::var("PGTIKV_CORS_ORIGINS")
            .unwrap_or_else(|_| "http://localhost:5173,http://localhost:3000".into());
        let cors_origins: Vec<String> = cors_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Self {
            pd_endpoints: env::var("PGTIKV_PD_ENDPOINTS")
                .or_else(|_| env::var("PD_ENDPOINTS"))
                .unwrap_or_else(|_| "127.0.0.1:2379".into()),
            pg_host: env::var("PGTIKV_PG_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            pg_port: env::var("PGTIKV_PG_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_PG_PORT),
            pg_public_endpoints: env::var("PGTIKV_PG_PUBLIC_ENDPOINTS")
                .unwrap_or_else(|_| format!("127.0.0.1:{DEFAULT_PG_PORT}")),
            api_port: env::var("PGTIKV_API_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8090),
            api_host: env::var("PGTIKV_API_HOST").unwrap_or_else(|_| "0.0.0.0".into()),
            database_url: env::var("PGTIKV_DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://data/portal.db?mode=rwc".into()),
            cors_origins,
            api_keys,
            reconciler_enabled: env::var("PGTIKV_RECONCILER_ENABLED")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true),
            reconciler_interval_secs: env::var("PGTIKV_RECONCILER_INTERVAL_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            session_ttl_hours: env::var("PGTIKV_SESSION_TTL_HOURS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
            credential_key: env::var("PGTIKV_CREDENTIAL_KEY")
                .ok()
                .filter(|k| !k.is_empty()),
        }
    }

    pub fn parse_public_endpoints(&self) -> Vec<(String, u16)> {
        self.pg_public_endpoints
            .split(',')
            .filter_map(|ep| {
                let ep = ep.trim();
                if ep.is_empty() {
                    return None;
                }
                if let Some((host, port_str)) = ep.rsplit_once(':') {
                    port_str.parse().ok().map(|port| (host.to_string(), port))
                } else {
                    Some((ep.to_string(), DEFAULT_PG_PORT))
                }
            })
            .collect()
    }

    pub fn auth_enabled(&self) -> bool {
        !self.api_keys.is_empty()
    }
}
