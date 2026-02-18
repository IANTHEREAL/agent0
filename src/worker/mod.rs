pub mod config;
pub mod engine;
pub mod gc;
pub mod metrics;
pub mod types;

use crate::storage::TikvStore;
use anyhow::Result;
use config::WorkerConfig;
use std::sync::{Arc, OnceLock};
use tracing::{info, warn};

static SYSTEM_STORE: OnceLock<Arc<TikvStore>> = OnceLock::new();
static WORKER_NOTIFY: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();

/// Set the global system store. Called once during startup.
pub fn set_system_store(store: Arc<TikvStore>) {
    SYSTEM_STORE.set(store).ok(); // Ignore if already set
}

/// Get the global system store. Returns None if worker is disabled.
pub fn get_system_store() -> Option<&'static Arc<TikvStore>> {
    SYSTEM_STORE.get()
}

pub fn set_worker_notify(notify: Arc<tokio::sync::Notify>) {
    WORKER_NOTIFY.set(notify).ok();
}

pub fn wake_worker() {
    if let Some(notify) = WORKER_NOTIFY.get() {
        notify.notify_one();
    }
}

/// Initialize the system store for the unified worker engine.
/// The system keyspace must be pre-created in PD by the backend before pg-tikv starts.
/// Returns None if worker is disabled via config.
pub async fn init_system_store(
    pd_endpoints: Vec<String>,
    config: &WorkerConfig,
    fallback_keyspace: Option<&str>,
) -> Result<Option<Arc<TikvStore>>> {
    if !config.enabled {
        info!("Worker engine disabled, skipping system store initialization");
        return Ok(None);
    }

    info!(
        "Initializing system store for keyspace: {}",
        config.system_keyspace
    );

    match TikvStore::new_system(pd_endpoints.clone(), &config.system_keyspace).await {
        Ok(store) => {
            let store = Arc::new(store);
            info!("System store initialized successfully");
            Ok(Some(store))
        }
        Err(primary_err) => {
            let Some(fallback) = fallback_keyspace else {
                return Err(primary_err);
            };
            if fallback.eq_ignore_ascii_case(&config.system_keyspace) {
                return Err(primary_err);
            }

            warn!(
                "System store init failed on keyspace '{}': {}. Falling back to startup keyspace '{}'",
                config.system_keyspace, primary_err, fallback
            );

            let store = TikvStore::new_system(pd_endpoints, fallback).await?;
            let store = Arc::new(store);
            warn!(
                "System store fallback active on keyspace '{}'; worker metadata is not isolated",
                fallback
            );
            Ok(Some(store))
        }
    }
}
