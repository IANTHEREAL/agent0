use crate::storage::TikvStore;
use crate::worker::config::WorkerConfig;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

pub struct WorkerGc {
    system_store: Arc<TikvStore>,
    config: WorkerConfig,
}

impl WorkerGc {
    pub fn new(system_store: Arc<TikvStore>, config: WorkerConfig) -> Self {
        Self {
            system_store,
            config,
        }
    }

    /// Main GC loop — runs on a separate interval from the engine tick.
    /// GC interval is 10 minutes (hardcoded, not configurable).
    pub async fn run(&self) {
        // Add random jitter (0-60s) to avoid thundering herd across workers
        let jitter = rand_jitter_secs(60);
        tokio::time::sleep(Duration::from_secs(jitter)).await;

        let mut interval = tokio::time::interval(Duration::from_secs(600));
        loop {
            interval.tick().await;
            if let Err(e) = self.gc_tick().await {
                warn!("Worker GC tick error: {}", e);
            }
        }
    }

    async fn gc_tick(&self) -> Result<()> {
        self.cleanup_orphan_claims().await?;
        Ok(())
    }

    /// Scan all claims and delete those older than orphan_timeout_sec.
    /// Orphaned claims are NOT re-enqueued — the next cron fire or scheduler
    /// handles retries. One-shot tasks stay failed.
    async fn cleanup_orphan_claims(&self) -> Result<()> {
        let mut txn = self.system_store.begin().await?;

        let gc_result = async {
            let claims = self.system_store.list_worker_claims(&mut txn).await?;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let timeout_ms = (self.config.orphan_timeout_sec as i64).saturating_mul(1000);
            let cutoff = now_ms.saturating_sub(timeout_ms);
            let mut cleaned = 0u32;

            for (key, claim) in claims {
                if claim.claimed_at < cutoff {
                    self.system_store
                        .delete_worker_queue_entry(&mut txn, &key)
                        .await?;
                    cleaned += 1;
                    warn!(
                        "GC: cleaned orphan claim worker={} type={:?} claimed_at={}",
                        claim.worker_id, claim.task_type, claim.claimed_at
                    );
                }
            }

            Ok::<u32, anyhow::Error>(cleaned)
        }
        .await;

        match gc_result {
            Ok(cleaned) if cleaned > 0 => {
                txn.commit().await?;
                info!("GC: cleaned {} orphan claims", cleaned);
            }
            Ok(_) => {
                txn.rollback().await.ok();
            }
            Err(e) => {
                txn.rollback().await.ok();
                return Err(e);
            }
        }

        Ok(())
    }
}

/// Generate random jitter in seconds (0..max_secs) using time-based seed.
fn rand_jitter_secs(max_secs: u64) -> u64 {
    use std::time::SystemTime;
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (seed % max_secs as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orphan_timeout_calc() {
        let now_ms: i64 = 1_700_000_000_000;
        let orphan_timeout_sec: u64 = 300;
        let timeout_ms = (orphan_timeout_sec as i64).saturating_mul(1000);
        let cutoff = now_ms.saturating_sub(timeout_ms);

        let old_claim = now_ms - 301_000;
        assert!(old_claim < cutoff, "claim older than timeout should be detected as orphan");

        let recent_claim = now_ms - 299_000;
        assert!(recent_claim >= cutoff, "claim within timeout should NOT be orphaned");

        let edge_claim = cutoff;
        assert!(!(edge_claim < cutoff), "claim exactly at cutoff boundary is not orphaned");
    }

    #[test]
    fn test_rand_jitter_secs_within_bounds() {
        for max in [1u64, 10, 60, 120, 3600] {
            let jitter = rand_jitter_secs(max);
            assert!(jitter < max, "jitter {} should be < max {}", jitter, max);
        }
    }

    #[test]
    fn test_rand_jitter_secs_max_one() {
        let jitter = rand_jitter_secs(1);
        assert_eq!(jitter, 0, "jitter with max=1 must be 0");
    }
}
