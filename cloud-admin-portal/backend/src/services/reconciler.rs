use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sqlx::AnyPool;

use crate::db;
use crate::services::pd_client::PdClient;
use crate::{tenant_state, TENANT_ID_LEN};

pub struct Reconciler {
    pd: PdClient,
    db: AnyPool,
    interval: Duration,
    running: Arc<AtomicBool>,
}

impl Reconciler {
    pub fn new(pd: PdClient, db: AnyPool, interval_secs: u64) -> Self {
        Self {
            pd,
            db,
            interval: Duration::from_secs(interval_secs),
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    pub async fn sync_new_keyspaces(&self) -> usize {
        let keyspaces = self.pd.list_keyspaces().await;
        let existing = match db::get_all_keyspaces_from_db(&self.db).await {
            Ok(v) => v,
            Err(e) => { tracing::warn!("Failed to list DB keyspaces: {e}"); return 0; }
        };

        let existing_ks: std::collections::HashSet<String> = existing.iter().map(|(_, ks)| ks.clone()).collect();
        let mut count = 0;

        for ks_val in &keyspaces {
            let name = match ks_val.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => continue,
            };
            if name == "DEFAULT" || name == "default" || existing_ks.contains(name) {
                continue;
            }
            let id = match name.strip_prefix(crate::KEYSPACE_PREFIX) {
                Some(stripped) if stripped.len() == TENANT_ID_LEN && stripped.chars().all(|c| c.is_ascii_alphanumeric()) => stripped,
                _ => continue,
            };
            let now = chrono::Utc::now().to_rfc3339();
            if db::insert_tenant(&self.db, id, name, tenant_state::ACTIVE, &now).await.is_ok() {
                tracing::info!("Synced keyspace from PD: {name} -> tenant {id}");
                count += 1;
            }
        }
        count
    }

    pub async fn start(self) {
        while self.running.load(Ordering::Relaxed) {
            tokio::time::sleep(self.interval).await;
            if !self.running.load(Ordering::Relaxed) {
                break;
            }
            self.run_cycle().await;
        }
    }

    async fn run_cycle(&self) {
        let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();

        if let Ok(stuck) = db::get_stuck_tenants(&self.db, tenant_state::CREATING, &cutoff).await {
            for t in &stuck {
                let exists = self.pd.get_keyspace(&t.keyspace).await.is_some();
                let (new_state, reason) = if exists {
                    (tenant_state::ACTIVE, "Recovered by reconciler: keyspace exists")
                } else {
                    (tenant_state::CREATE_FAILED, "Recovered by reconciler: keyspace not found")
                };
                if let Err(e) = db::update_tenant_state(&self.db, &t.id, new_state, Some(reason)).await {
                    tracing::warn!("Failed to recover tenant {}: {e}", t.id);
                }
            }
        }

        if let Ok(stuck) = db::get_stuck_tenants(&self.db, tenant_state::DISABLING, &cutoff).await {
            for t in &stuck {
                if let Err(e) = db::update_tenant_state(&self.db, &t.id, tenant_state::DISABLED, Some("Recovered by reconciler")).await {
                    tracing::warn!("Failed to recover tenant {}: {e}", t.id);
                }
            }
        }
    }
}
