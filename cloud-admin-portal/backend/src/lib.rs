pub const KEYSPACE_PREFIX: &str = "tipg_tenant_";
pub const TENANT_ID_LEN: usize = 12;
pub const DEFAULT_ADMIN_USER: &str = "admin";
pub const DEFAULT_ADMIN_PASSWORD: &str = "admin";
pub const DEFAULT_PG_PORT: u16 = 5433;
pub const OBSERVABILITY_USER: &str = "_pgtikv_sys_observer";

pub mod tenant_state {
    pub const CREATING: &str = "CREATING";
    pub const ACTIVE: &str = "ACTIVE";
    pub const DISABLING: &str = "DISABLING";
    pub const DISABLED: &str = "DISABLED";
    pub const CREATE_FAILED: &str = "CREATE_FAILED";
    pub const SUSPENDED: &str = "SUSPENDED";
}

pub mod api;
pub mod auth;
pub mod cli_common;
pub mod config;
pub mod crypto;
pub mod db;
pub mod device_code;
pub mod error;
pub mod models;
pub mod services;
pub mod session;

use std::sync::Arc;

use sqlx::AnyPool;

use config::Config;
use device_code::DeviceCodeStore;
use services::fs9_client::Fs9Client;
use session::SessionManager;

#[derive(Clone)]
pub struct AppState {
    pub db: AnyPool,
    pub config: Arc<Config>,
    pub sessions: Arc<SessionManager>,
    pub device_codes: Arc<DeviceCodeStore>,
    pub http_client: reqwest::Client,
    pub fs9_client: Option<Arc<Fs9Client>>,
}
