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

/// Ensure the system keyspace exists in PD before connecting via the TiKV client.
/// TiKV API v2 requires keyspaces to be registered in PD. If the keyspace does not
/// exist, this creates it via the PD HTTP API (idempotent).
async fn ensure_pd_keyspace(pd_endpoints: &[String], keyspace: &str) -> Result<()> {
    let pd_addr = pd_endpoints
        .first()
        .ok_or_else(|| anyhow::anyhow!("No PD endpoints configured"))?;

    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10));

    // Use mTLS if certs are available (matches TiKV client TLS config)
    let scheme = if let (Ok(ca_path), Ok(cert_path), Ok(key_path)) = (
        std::env::var("TIKV_CA_PATH"),
        std::env::var("TIKV_CERT_PATH"),
        std::env::var("TIKV_KEY_PATH"),
    ) {
        let ca_pem = std::fs::read(&ca_path)
            .map_err(|e| anyhow::anyhow!("Failed to read CA cert {}: {}", ca_path, e))?;
        let ca_cert = reqwest::Certificate::from_pem(&ca_pem)?;
        builder = builder.add_root_certificate(ca_cert);

        let cert_pem = std::fs::read(&cert_path)
            .map_err(|e| anyhow::anyhow!("Failed to read client cert {}: {}", cert_path, e))?;
        let key_pem = std::fs::read(&key_path)
            .map_err(|e| anyhow::anyhow!("Failed to read client key {}: {}", key_path, e))?;
        let mut identity_pem = cert_pem;
        identity_pem.extend_from_slice(&key_pem);
        let identity = reqwest::Identity::from_pem(&identity_pem)?;
        builder = builder.identity(identity).use_rustls_tls();

        "https"
    } else {
        "http"
    };

    let client = builder.build()?;

    let base_url = format!("{}://{}", scheme, pd_addr);

    // Try to create the keyspace (idempotent — PD returns 200 if already exists)
    let url = format!("{}/pd/api/v2/keyspaces", base_url);
    let body = serde_json::json!({
        "name": keyspace,
        "config": { "gc_management_type": "global_gc" }
    });

    match client.post(&url).json(&body).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                info!(
                    "PD keyspace '{}' ensured (created or already exists)",
                    keyspace
                );
                Ok(())
            } else {
                let body_text = resp.text().await.unwrap_or_default();
                // 409 Conflict means keyspace already exists — that's fine
                if status.as_u16() == 409 || body_text.contains("already exist") {
                    info!("PD keyspace '{}' already exists", keyspace);
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "PD create keyspace '{}' failed: {} {}",
                        keyspace,
                        status,
                        body_text
                    ))
                }
            }
        }
        Err(e) => {
            warn!("PD HTTP API unreachable for keyspace creation: {}", e);
            // Don't fail hard — the TiKV client connection will give a better error
            Ok(())
        }
    }
}

/// Initialize the system store for the unified worker engine.
/// Ensures the system keyspace exists in PD, then creates a TikvStore connection.
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

    // Ensure the keyspace exists in PD before connecting
    ensure_pd_keyspace(&pd_endpoints, &config.system_keyspace).await?;

    let store = TikvStore::new_system(pd_endpoints, &config.system_keyspace).await?;
    let store = Arc::new(store);
    info!("System store initialized successfully");
    Ok(Some(store))
}
