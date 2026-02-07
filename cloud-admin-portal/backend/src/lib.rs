pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod models;
pub mod services;
pub mod session;

use std::sync::Arc;

use sqlx::AnyPool;

use config::Config;
use session::SessionManager;

#[derive(Clone)]
pub struct AppState {
    pub db: AnyPool,
    pub config: Arc<Config>,
    pub sessions: Arc<SessionManager>,
    pub http_client: reqwest::Client,
}
