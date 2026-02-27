use crate::cron::config::CronConfig;
use crate::cron::worker::gc_database;
use crate::pool::TikvClientPool;
use crate::storage::TikvStore;
use crate::worker::config::WorkerConfig;
use crate::worker::now_epoch_ms;
use anyhow::Result;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

pub struct WorkerGc {
    system_store: Arc<TikvStore>,
    pool: Arc<TikvClientPool>,
    config: WorkerConfig,
}

struct ClaimGcBatch {
    scanned: usize,
    cleaned: u32,
    last_key: Option<Vec<u8>>,
}

impl WorkerGc {
    pub fn new(
        system_store: Arc<TikvStore>,
        pool: Arc<TikvClientPool>,
        config: WorkerConfig,
    ) -> Self {
        Self {
            system_store,
            pool,
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
        self.cleanup_cron_runs().await?;
        Ok(())
    }

    /// Scan worker registry for keyspaces with cron jobs and GC their run history.
    async fn cleanup_cron_runs(&self) -> Result<()> {
        let mut cron_config = CronConfig::from_env();
        cron_config.orphan_timeout_sec =
            effective_cron_orphan_timeout_sec(&cron_config, &self.config);

        let mut txn = self.system_store.begin().await?;
        let registry_entries = self.system_store.list_worker_registry(&mut txn).await?;
        txn.commit().await?;

        for entry in registry_entries {
            if !entry.has_cron() {
                continue;
            }

            let handle = match self.pool.acquire(Some(entry.keyspace.clone())).await {
                Ok(h) => h,
                Err(e) => {
                    warn!(
                        "cron GC: failed to acquire store for keyspace={}: {}",
                        entry.keyspace, e
                    );
                    continue;
                }
            };
            let store = handle.store().clone();

            if let Err(e) = gc_database(&store, entry.db_id, &cron_config).await {
                warn!(
                    "cron GC error for keyspace={} db_id={}: {}",
                    entry.keyspace, entry.db_id, e
                );
            }
        }

        Ok(())
    }

    /// Scan all claims and delete those older than orphan_timeout_sec.
    /// Orphaned claims are NOT re-enqueued — the next cron fire or scheduler
    /// handles retries. One-shot tasks stay failed.
    async fn cleanup_orphan_claims(&self) -> Result<()> {
        let batch_size = self.config.gc_batch_size.max(1);
        let now_ms = now_epoch_ms();
        let timeout_ms = (self.config.orphan_timeout_sec as i64).saturating_mul(1000);
        let cutoff = now_ms.saturating_sub(timeout_ms);

        let (cleaned, _) = run_claim_gc_batches(batch_size, |start_after, requested_batch_size| {
            self.cleanup_orphan_claims_batch(start_after, requested_batch_size, cutoff)
        })
        .await?;

        if cleaned > 0 {
            info!("GC: cleaned {} orphan claims", cleaned);
        }

        Ok(())
    }

    async fn cleanup_orphan_claims_batch(
        &self,
        start_after: Option<Vec<u8>>,
        batch_size: usize,
        cutoff: i64,
    ) -> Result<ClaimGcBatch> {
        let mut txn = self.system_store.begin().await?;

        let batch_result = async {
            let claims = self
                .system_store
                .list_worker_claims_batch(&mut txn, start_after.as_deref(), Some(batch_size))
                .await?;
            let scanned = claims.len();
            let last_key = claims.last().map(|(key, _)| key.clone());
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

            Ok::<ClaimGcBatch, anyhow::Error>(ClaimGcBatch {
                scanned,
                cleaned,
                last_key,
            })
        }
        .await;

        match batch_result {
            Ok(batch) => {
                if batch.cleaned > 0 {
                    txn.commit().await?;
                } else {
                    txn.rollback().await.ok();
                }
                Ok(batch)
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e)
            }
        }
    }
}

async fn run_claim_gc_batches<F, Fut>(batch_size: usize, mut run_batch: F) -> Result<(u32, usize)>
where
    F: FnMut(Option<Vec<u8>>, usize) -> Fut,
    Fut: Future<Output = Result<ClaimGcBatch>>,
{
    let batch_size = batch_size.max(1).min(u32::MAX as usize);
    let mut total_cleaned = 0u32;
    let mut batch_count = 0usize;
    let mut start_after: Option<Vec<u8>> = None;

    loop {
        let batch = run_batch(start_after.clone(), batch_size).await?;
        if batch.scanned == 0 {
            break;
        }

        batch_count += 1;
        total_cleaned = total_cleaned.saturating_add(batch.cleaned);
        start_after = batch.last_key;

        if batch.scanned < batch_size {
            break;
        }
    }

    Ok((total_cleaned, batch_count))
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

fn effective_cron_orphan_timeout_sec(
    cron_config: &CronConfig,
    worker_config: &WorkerConfig,
) -> u64 {
    let worker_timeout_sec = worker_config.cron_job_timeout_ms.saturating_add(999) / 1000;
    cron_config.orphan_timeout_sec.max(worker_timeout_sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future;

    #[test]
    fn test_orphan_timeout_calc() {
        let now_ms: i64 = 1_700_000_000_000;
        let orphan_timeout_sec: u64 = 300;
        let timeout_ms = (orphan_timeout_sec as i64).saturating_mul(1000);
        let cutoff = now_ms.saturating_sub(timeout_ms);

        let old_claim = now_ms - 301_000;
        assert!(
            old_claim < cutoff,
            "claim older than timeout should be detected as orphan"
        );

        let recent_claim = now_ms - 299_000;
        assert!(
            recent_claim >= cutoff,
            "claim within timeout should NOT be orphaned"
        );

        let edge_claim = cutoff;
        assert!(
            edge_claim >= cutoff,
            "claim exactly at cutoff boundary is not orphaned"
        );
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

    #[test]
    fn test_effective_cron_orphan_timeout_respects_worker_timeout() {
        let cron_cfg = CronConfig {
            orphan_timeout_sec: 300,
            ..Default::default()
        };

        let worker_cfg = WorkerConfig {
            cron_job_timeout_ms: 1_800_000,
            ..Default::default()
        };

        assert_eq!(
            effective_cron_orphan_timeout_sec(&cron_cfg, &worker_cfg),
            1_800
        );
    }

    #[test]
    fn test_effective_cron_orphan_timeout_keeps_larger_cron_value() {
        let cron_cfg = CronConfig {
            orphan_timeout_sec: 7_200,
            ..Default::default()
        };

        let worker_cfg = WorkerConfig {
            cron_job_timeout_ms: 1_800_000,
            ..Default::default()
        };

        assert_eq!(
            effective_cron_orphan_timeout_sec(&cron_cfg, &worker_cfg),
            7_200
        );
    }

    #[tokio::test]
    async fn cleanup_orphan_claims_respects_batch_size() {
        let orphan_count = 7usize;
        let batch_size = 3usize;
        let expected_batches = orphan_count.div_ceil(batch_size);
        let all_claim_keys: Vec<Vec<u8>> = (0..orphan_count).map(|idx| vec![idx as u8]).collect();
        let mut processed_keys: Vec<Vec<u8>> = Vec::new();

        let (cleaned, batches) = run_claim_gc_batches(batch_size, |start_after, requested_size| {
            let start_index = start_after
                .as_ref()
                .and_then(|key| {
                    all_claim_keys
                        .iter()
                        .position(|candidate_key| candidate_key == key)
                })
                .map(|idx| idx + 1)
                .unwrap_or(0);
            let end_index = (start_index + requested_size).min(all_claim_keys.len());
            let page_keys = all_claim_keys[start_index..end_index].to_vec();
            processed_keys.extend(page_keys.iter().cloned());

            let scanned = page_keys.len();
            let last_key = page_keys.last().cloned();
            future::ready(Ok(ClaimGcBatch {
                scanned,
                cleaned: scanned as u32,
                last_key,
            }))
        })
        .await
        .expect("pagination loop should succeed");

        assert_eq!(batches, expected_batches);
        assert_eq!(cleaned, orphan_count as u32);
        assert_eq!(processed_keys, all_claim_keys);
    }

    #[tokio::test]
    async fn run_claim_gc_batches_clamps_batch_size_above_u32_max() {
        let oversized_batch_size = (u32::MAX as usize) + 1;
        let mut call_count = 0usize;

        let (cleaned, batches) =
            run_claim_gc_batches(oversized_batch_size, |start_after, requested_size| {
                assert_eq!(
                    requested_size,
                    u32::MAX as usize,
                    "batch size must be clamped at consumption point",
                );

                let result = match call_count {
                    0 => {
                        assert_eq!(start_after, None);
                        ClaimGcBatch {
                            scanned: u32::MAX as usize,
                            cleaned: 0,
                            last_key: Some(vec![1]),
                        }
                    }
                    1 => {
                        assert_eq!(start_after, Some(vec![1]));
                        ClaimGcBatch {
                            scanned: 1,
                            cleaned: 1,
                            last_key: Some(vec![2]),
                        }
                    }
                    _ => panic!("loop should terminate after second batch"),
                };
                call_count += 1;
                future::ready(Ok(result))
            })
            .await
            .expect("pagination loop should succeed");

        assert_eq!(batches, 2, "must continue after first full capped batch");
        assert_eq!(cleaned, 1);
        assert_eq!(call_count, 2);
    }
}
