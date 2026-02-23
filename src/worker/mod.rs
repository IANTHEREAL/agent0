pub mod config;
pub mod engine;
pub mod gc;
pub mod metrics;
pub mod types;

use crate::storage::TikvStore;
use anyhow::{Context, Result};
use config::WorkerConfig;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::time::sleep;
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

/// Ensure the system keyspace exists in PD before initializing worker store.
///
/// This keeps worker metadata isolated (`_sys_worker`) while removing the need
/// for manual keyspace pre-provisioning in local/dev and CI-like environments.
/// In TLS mode, skip HTTP provisioning and rely on existing pre-created keyspace.
async fn ensure_system_keyspace(pd_endpoints: &[String], keyspace: &str) -> Result<()> {
    if std::env::var("TIKV_CA_PATH").is_ok() {
        info!(
            "Skipping system keyspace ensure in TLS mode; expecting '{}' to be pre-created",
            keyspace
        );
        return Ok(());
    }

    let pd_primary = pd_endpoints
        .first()
        .ok_or_else(|| anyhow::anyhow!("no PD endpoint configured"))?;
    let base = format!("http://{}/pd/api/v2/keyspaces", pd_primary);
    let keyspace_url = format!("{}/{}", base, keyspace);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to build PD HTTP client")?;

    let mut last_err = String::new();
    for _ in 0..15 {
        // Idempotent create. Some PD versions return 500 with "keyspace already exists"
        // instead of 409, so we always verify with a follow-up GET before retrying.
        let post_resp = client
            .post(&base)
            .json(&serde_json::json!({ "name": keyspace }))
            .send()
            .await;

        match post_resp {
            Ok(resp) => {
                let status = resp.status();
                if !(status.is_success() || status.as_u16() == 409) {
                    let body = resp.text().await.unwrap_or_default();
                    if status.as_u16() == 500 || status.as_u16() == 503 {
                        // Fall through to GET existence check for idempotent success.
                    } else {
                        return Err(anyhow::anyhow!(
                            "failed to create system keyspace '{}': status={}, body={}",
                            keyspace,
                            status,
                            body
                        ));
                    }
                }
            }
            Err(e) => {
                last_err = format!("POST error: {}", e);
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        }

        match client.get(&keyspace_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(
                    "Ensured worker system keyspace '{}' on PD {}",
                    keyspace, pd_primary
                );
                return Ok(());
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last_err = format!("GET status={}, body={}", status, body);
            }
            Err(e) => {
                last_err = format!("GET error: {}", e);
            }
        }

        sleep(Duration::from_secs(1)).await;
    }

    Err(anyhow::anyhow!(
        "unable to ensure system keyspace '{}' on PD {} after retries: {}",
        keyspace,
        pd_primary,
        last_err
    ))
}

/// Initialize the system store for the unified worker engine.
///
/// Attempts to ensure the system keyspace exists in PD (best-effort) and then
/// initializes the isolated system store. No fallback keyspace is allowed, so
/// worker metadata remains isolated.
///
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

    if let Err(e) = ensure_system_keyspace(&pd_endpoints, &config.system_keyspace).await {
        warn!(
            "Failed to ensure worker system keyspace '{}': {}. Proceeding with direct init.",
            config.system_keyspace, e
        );
    }

    let store = TikvStore::new_system(pd_endpoints, &config.system_keyspace)
        .await
        .with_context(|| {
            format!(
                "failed to initialize isolated system keyspace '{}'; refusing fallback",
                config.system_keyspace
            )
        })?;
    let store = Arc::new(store);
    info!("System store initialized successfully");
    Ok(Some(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_system_store_signature_has_no_fallback_keyspace() {
        let cfg = WorkerConfig::default();
        let fut = init_system_store(Vec::new(), &cfg);
        drop(fut);
    }

    #[tokio::test]
    async fn test_init_system_store_disabled_short_circuits() {
        let mut cfg = WorkerConfig::default();
        cfg.enabled = false;
        let store = init_system_store(vec!["127.0.0.1:1".to_string()], &cfg)
            .await
            .expect("disabled worker should not attempt system-store init");
        assert!(store.is_none());
    }

    #[tokio::test]
    async fn test_init_system_store_enabled_failure_refuses_fallback() {
        let mut cfg = WorkerConfig::default();
        cfg.enabled = true;
        cfg.system_keyspace = "_sys_worker".to_string();

        let err = match init_system_store(vec!["127.0.0.1:1".to_string()], &cfg).await {
            Ok(_) => panic!(
                "enabled worker should fail without fallback when system keyspace is unavailable"
            ),
            Err(err) => err,
        };
        let msg = format!("{:#}", err);

        assert!(
            msg.contains("refusing fallback"),
            "error should contain no-fallback guard, got: {msg}"
        );
        assert!(
            msg.contains(&cfg.system_keyspace),
            "error should include target system keyspace, got: {msg}"
        );
    }
}
