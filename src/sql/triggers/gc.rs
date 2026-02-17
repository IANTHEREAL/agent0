//! Garbage collection for trigger queue.

use super::queue::{
    encode_trigger_dlq_key, encode_trigger_dlq_prefix, encode_trigger_queue_prefix, now_ms_i64,
    EventStatus, TriggerEvent,
};
use super::worker::{KeyspaceQuota, TriggerWorker};
use crate::pool::TikvClientPool;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tikv_client::BoundRange;
use tracing::{info, warn};

impl TriggerWorker {
    pub(super) async fn gc_loop(&self, pool: Arc<TikvClientPool>) {
        let mut interval = tokio::time::interval(Duration::from_secs(self.config.gc_interval_sec));
        while !self.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            interval.tick().await;

            let keyspaces = self.list_known_keyspaces();
            for keyspace in keyspaces {
                if let Err(e) = self.gc_keyspace(&pool, &keyspace).await {
                    warn!("trigger GC error for {}: {}", keyspace, e);
                }
            }
        }
    }

    fn list_known_keyspaces(&self) -> Vec<String> {
        self.active_keyspaces
            .iter()
            .map(|r| r.key().clone())
            .collect()
    }

    async fn gc_keyspace(&self, pool: &Arc<TikvClientPool>, keyspace: &str) -> Result<()> {
        let store = pool.get_client(Some(keyspace.to_string())).await?;
        let quota = self.get_quota(keyspace);
        let cutoff_orphan_ms = now_ms_i64()
            .saturating_sub((self.config.orphan_timeout_sec.saturating_mul(1000)) as i64);
        let cutoff_dlq_ms = now_ms_i64().saturating_sub(
            (self
                .config
                .dlq_retention_days
                .saturating_mul(24 * 3600 * 1000)) as i64,
        );

        let mut txn = store.begin().await?;

        let recovered = self
            .recover_orphans(&mut txn, keyspace, cutoff_orphan_ms, &quota)
            .await?;
        let dlq_deleted = self.delete_old_dlq(&mut txn, cutoff_dlq_ms).await?;

        txn.commit().await?;

        if recovered > 0 || dlq_deleted > 0 {
            info!(
                "trigger GC for {}: recovered {}, deleted dlq {}",
                keyspace, recovered, dlq_deleted
            );
        }

        Ok(())
    }

    async fn recover_orphans(
        &self,
        txn: &mut tikv_client::Transaction,
        keyspace: &str,
        cutoff_claimed_ms: i64,
        quota: &Arc<KeyspaceQuota>,
    ) -> Result<usize> {
        use std::ops::Bound;

        let prefix = encode_trigger_queue_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);

        let mut recovered = 0usize;
        let mut start: Option<Vec<u8>> = None;
        loop {
            let range: BoundRange = match start.as_ref() {
                None => (prefix.clone()..end.clone()).into(),
                Some(last) => BoundRange::new(
                    Bound::Excluded(last.clone().into()),
                    Bound::Excluded(end.clone().into()),
                ),
            };

            let mut batch_last = None;
            let mut scanned = 0usize;
            for pair in txn.scan(range, 256).await? {
                scanned += 1;
                let key_slice: &[u8] = pair.key().as_ref().into();
                let key = key_slice.to_vec();
                batch_last = Some(key.clone());
                let mut ev: TriggerEvent = match bincode::deserialize(pair.value()) {
                    Ok(v) => v,
                    Err(e) => {
                        self.quarantine_corrupt_trigger_queue_entry(
                            txn,
                            keyspace,
                            quota,
                            key,
                            pair.value(),
                            &e,
                        )
                        .await?;
                        continue;
                    }
                };

                if ev.status != EventStatus::Processing {
                    continue;
                }
                let Some(claimed_at) = ev.claimed_at_ms else {
                    continue;
                };
                if claimed_at >= cutoff_claimed_ms {
                    continue;
                }

                ev.retry_count = ev.retry_count.saturating_add(1);
                if ev.retry_count >= quota.max_retries {
                    ev.status = EventStatus::Failed;
                    ev.worker_id = None;
                    ev.claimed_at_ms = None;
                    let dlq_key = encode_trigger_dlq_key(ev.id);
                    txn.put(dlq_key, bincode::serialize(&ev)?).await?;
                    txn.delete(key).await?;
                    quota.dec_current_depth();
                } else {
                    ev.status = EventStatus::Pending;
                    ev.worker_id = None;
                    ev.claimed_at_ms = None;
                    txn.put(key, bincode::serialize(&ev)?).await?;
                    self.mark_active(keyspace);
                }
                recovered += 1;
            }

            if scanned < 256 {
                break;
            }
            start = batch_last;
            if start.is_none() {
                break;
            }
        }

        Ok(recovered)
    }

    async fn delete_old_dlq(
        &self,
        txn: &mut tikv_client::Transaction,
        cutoff_ms: i64,
    ) -> Result<usize> {
        use std::ops::Bound;

        let cutoff_ms_u64 = u64::try_from(cutoff_ms.max(0)).unwrap_or(0);
        let cutoff_id = cutoff_ms_u64 << 22;

        let prefix = encode_trigger_dlq_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);

        let mut deleted = 0usize;
        let mut start: Option<Vec<u8>> = None;
        loop {
            let range: BoundRange = match start.as_ref() {
                None => (prefix.clone()..end.clone()).into(),
                Some(last) => BoundRange::new(
                    Bound::Excluded(last.clone().into()),
                    Bound::Excluded(end.clone().into()),
                ),
            };

            let mut batch_last = None;
            let mut scanned = 0usize;
            for pair in txn.scan(range, 256).await? {
                scanned += 1;
                let key_slice: &[u8] = pair.key().as_ref().into();
                let key_bytes = key_slice.to_vec();
                batch_last = Some(key_bytes.clone());

                if key_bytes.len() < 8 {
                    continue;
                }
                let id_bytes = &key_bytes[key_bytes.len() - 8..];
                let id = u64::from_be_bytes(id_bytes.try_into().unwrap());
                if id < cutoff_id {
                    txn.delete(key_bytes).await?;
                    deleted += 1;
                }
            }

            if scanned < 256 {
                break;
            }
            start = batch_last;
            if start.is_none() {
                break;
            }
        }

        Ok(deleted)
    }
}
