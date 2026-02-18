pub mod config;
pub mod engine;
pub mod gc;
pub mod metrics;
pub mod types;

use crate::storage::TikvStore;
use anyhow::Result;
use config::WorkerConfig;
use std::sync::{Arc, OnceLock};
use tracing::info;

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
) -> Result<Option<Arc<TikvStore>>> {
    if !config.enabled {
        info!("Worker engine disabled, skipping system store initialization");
        return Ok(None);
    }

    info!(
        "Initializing system store for keyspace: {}",
        config.system_keyspace
    );

    let store = TikvStore::new_system(pd_endpoints, &config.system_keyspace).await?;
    let store = Arc::new(store);
    info!("System store initialized successfully");
    Ok(Some(store))
}
