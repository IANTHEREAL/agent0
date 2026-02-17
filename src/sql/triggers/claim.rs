//! Event claiming and quarantine logic.

use super::queue::{
    encode_trigger_dlq_key, encode_trigger_queue_prefix, generate_event_id, now_ms_i64,
    EventStatus, TriggerEvent, TriggerOp,
};
use super::worker::{KeyspaceQuota, TriggerWorker};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tikv_client::{BoundRange, Transaction};
use tracing::warn;

pub(super) struct ClaimEventsResult {
    pub(super) claimed: Vec<TriggerEvent>,
    pub(super) quarantined: usize,
}

#[async_trait]
pub(super) trait TriggerQueueTxn {
    async fn scan(&mut self, range: BoundRange, limit: u32) -> Result<Vec<tikv_client::KvPair>>;
    async fn get(&mut self, key: Vec<u8>) -> Result<Option<Vec<u8>>>;
    async fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()>;
    async fn delete(&mut self, key: Vec<u8>) -> Result<()>;
    async fn lock_keys(&mut self, keys: Vec<Vec<u8>>) -> Result<()>;
}

#[async_trait]
impl TriggerQueueTxn for Transaction {
    async fn scan(&mut self, range: BoundRange, limit: u32) -> Result<Vec<tikv_client::KvPair>> {
        Ok(Transaction::scan(self, range, limit).await?.collect())
    }

    async fn get(&mut self, key: Vec<u8>) -> Result<Option<Vec<u8>>> {
        Ok(Transaction::get(self, key).await?)
    }

    async fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        Ok(Transaction::put(self, key, value).await?)
    }

    async fn delete(&mut self, key: Vec<u8>) -> Result<()> {
        Ok(Transaction::delete(self, key).await?)
    }

    async fn lock_keys(&mut self, keys: Vec<Vec<u8>>) -> Result<()> {
        Ok(Transaction::lock_keys(self, keys).await?)
    }
}

impl TriggerWorker {
    pub(super) async fn quarantine_corrupt_trigger_queue_entry<T: TriggerQueueTxn + Send>(
        &self,
        txn: &mut T,
        keyspace: &str,
        quota: &Arc<KeyspaceQuota>,
        queue_key: Vec<u8>,
        value: &[u8],
        decode_err: &bincode::Error,
    ) -> Result<()> {
        use base64::Engine;
        use sha2::{Digest, Sha256};

        let id = queue_key
            .get(queue_key.len().saturating_sub(8)..)
            .and_then(|tail| tail.try_into().ok())
            .map(u64::from_be_bytes)
            .unwrap_or_else(generate_event_id);

        let created_at_ms = i64::try_from(id >> 22).unwrap_or_else(|_| now_ms_i64());

        let value_len = value.len();
        let preview_len = value_len.min(1024);
        let value_b64_prefix =
            base64::engine::general_purpose::STANDARD.encode(&value[..preview_len]);
        let value_sha256 = Sha256::digest(value);
        let value_sha256_hex = hex::encode(value_sha256);

        let msg = if preview_len == value_len {
            format!(
                "trigger queue decode failed (quarantined): err=\"{}\" queue_key_hex={} value_len={} value_sha256={} value_b64={}",
                decode_err,
                hex::encode(&queue_key),
                value_len,
                value_sha256_hex,
                value_b64_prefix
            )
        } else {
            format!(
                "trigger queue decode failed (quarantined): err=\"{}\" queue_key_hex={} value_len={} value_sha256={} value_b64_prefix_len={} value_b64_prefix={}",
                decode_err,
                hex::encode(&queue_key),
                value_len,
                value_sha256_hex,
                preview_len,
                value_b64_prefix
            )
        };

        let ev = TriggerEvent {
            id,
            trigger_name: "<corrupt>".to_string(),
            db_id: 0,
            table_name: "<unknown>".to_string(),
            operation: TriggerOp::Insert,
            old_row: None,
            new_row: None,
            created_at_ms,
            status: EventStatus::Failed,
            retry_count: 0,
            error_msg: Some(msg),
            worker_id: None,
            claimed_at_ms: None,
        };

        let dlq_key = encode_trigger_dlq_key(id);
        txn.put(dlq_key, bincode::serialize(&ev)?).await?;
        txn.delete(queue_key).await?;
        quota.dec_current_depth();

        warn!(
            "corrupt trigger queue entry quarantined to DLQ (keyspace={}, id={}, err={})",
            keyspace, id, decode_err
        );

        Ok(())
    }

    pub(super) async fn claim_events<T: TriggerQueueTxn + Send>(
        &self,
        txn: &mut T,
        keyspace: &str,
        quota: &Arc<KeyspaceQuota>,
        limit: usize,
    ) -> Result<ClaimEventsResult> {
        use std::ops::Bound;

        let prefix = encode_trigger_queue_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);

        let scan_limit: u32 = u32::try_from(limit.saturating_mul(4).max(16)).unwrap_or(u32::MAX);
        let mut candidate_keys: Vec<Vec<u8>> = Vec::new();
        let mut quarantined = 0usize;

        let mut start: Option<Vec<u8>> = None;
        loop {
            if candidate_keys.len() >= limit {
                break;
            }

            let range: BoundRange = match start.as_ref() {
                None => (prefix.clone()..end.clone()).into(),
                Some(last) => BoundRange::new(
                    Bound::Excluded(last.clone().into()),
                    Bound::Excluded(end.clone().into()),
                ),
            };

            let mut batch_last = None;
            let mut scanned = 0usize;
            for pair in txn.scan(range, scan_limit).await? {
                scanned += 1;

                if candidate_keys.len() >= limit {
                    break;
                }

                let key_slice: &[u8] = pair.key().as_ref().into();
                let key_bytes = key_slice.to_vec();
                batch_last = Some(key_bytes.clone());

                let ev: TriggerEvent = match bincode::deserialize(pair.value()) {
                    Ok(v) => v,
                    Err(e) => {
                        quarantined += 1;
                        self.quarantine_corrupt_trigger_queue_entry(
                            txn,
                            keyspace,
                            quota,
                            key_bytes,
                            pair.value(),
                            &e,
                        )
                        .await?;
                        continue;
                    }
                };
                if ev.status == EventStatus::Pending {
                    candidate_keys.push(key_bytes);
                }
            }

            if scanned < scan_limit as usize {
                break;
            }

            start = batch_last;
            if start.is_none() {
                break;
            }
        }

        if candidate_keys.is_empty() {
            return Ok(ClaimEventsResult {
                claimed: Vec::new(),
                quarantined,
            });
        }

        txn.lock_keys(candidate_keys.clone()).await?;

        let mut claimed = Vec::with_capacity(limit);
        let now_ms = now_ms_i64();
        for key in candidate_keys {
            let Some(val) = txn.get(key.clone()).await? else {
                continue;
            };
            let mut ev: TriggerEvent = match bincode::deserialize(&val) {
                Ok(v) => v,
                Err(e) => {
                    quarantined += 1;
                    self.quarantine_corrupt_trigger_queue_entry(
                        txn, keyspace, quota, key, &val, &e,
                    )
                    .await?;
                    continue;
                }
            };
            if ev.status != EventStatus::Pending {
                continue;
            }
            ev.status = EventStatus::Processing;
            ev.worker_id = Some(self.worker_id.clone());
            ev.claimed_at_ms = Some(now_ms);

            txn.put(key, bincode::serialize(&ev)?).await?;
            claimed.push(ev);
            if claimed.len() >= limit {
                break;
            }
        }

        Ok(ClaimEventsResult {
            claimed,
            quarantined,
        })
    }
}

#[cfg(test)]
pub(super) struct MemTxn {
    pub(super) kv: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
}

#[cfg(test)]
impl Default for MemTxn {
    fn default() -> Self {
        Self {
            kv: std::collections::BTreeMap::new(),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl TriggerQueueTxn for MemTxn {
    async fn scan(&mut self, range: BoundRange, limit: u32) -> Result<Vec<tikv_client::KvPair>> {
        let (start, end) = range.into_keys();
        let start: Vec<u8> = start.into();
        let take = usize::try_from(limit).unwrap_or(usize::MAX);

        let mut out = Vec::new();
        if let Some(end) = end {
            let end: Vec<u8> = end.into();
            for (key, value) in self.kv.range(start.clone()..end).take(take) {
                out.push(tikv_client::KvPair::new(key.clone(), value.clone()));
            }
        } else {
            for (key, value) in self.kv.range(start..).take(take) {
                out.push(tikv_client::KvPair::new(key.clone(), value.clone()));
            }
        }
        Ok(out)
    }

    async fn get(&mut self, key: Vec<u8>) -> Result<Option<Vec<u8>>> {
        Ok(self.kv.get(&key).cloned())
    }

    async fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        self.kv.insert(key, value);
        Ok(())
    }

    async fn delete(&mut self, key: Vec<u8>) -> Result<()> {
        self.kv.remove(&key);
        Ok(())
    }

    async fn lock_keys(&mut self, _keys: Vec<Vec<u8>>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MemTxn;
    use crate::sql::triggers::queue::{
        encode_trigger_dlq_key, encode_trigger_queue_key, EventStatus, TriggerEvent, TriggerOp,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::super::worker::TriggerWorker;

    #[tokio::test]
    async fn claim_events_quarantines_poison_and_claims_later_pending() {
        let worker = TriggerWorker::new_for_test();
        let quota = Arc::new(super::super::worker::KeyspaceQuota {
            max_queue_depth: 1000,
            max_events_per_batch: 10,
            max_retries: 3,
            current_depth: AtomicUsize::new(100),
        });

        let mut txn = MemTxn::default();
        let keyspace = "ks";

        let corrupt_count = 16u64;
        for id in 1..=corrupt_count {
            txn.kv
                .insert(encode_trigger_queue_key(id), vec![0xde, 0xad, 0xbe, 0xef]);
        }

        let pending_id = 999u64;
        let pending = TriggerEvent {
            id: pending_id,
            trigger_name: "t".to_string(),
            db_id: 1,
            table_name: "public.tbl".to_string(),
            operation: TriggerOp::Insert,
            old_row: None,
            new_row: None,
            created_at_ms: 0,
            status: EventStatus::Pending,
            retry_count: 0,
            error_msg: None,
            worker_id: None,
            claimed_at_ms: None,
        };
        txn.kv.insert(
            encode_trigger_queue_key(pending_id),
            bincode::serialize(&pending).unwrap(),
        );

        let res = worker
            .claim_events(&mut txn, keyspace, &quota, 1)
            .await
            .unwrap();

        assert_eq!(res.quarantined, corrupt_count as usize);
        assert_eq!(res.claimed.len(), 1);
        assert_eq!(res.claimed[0].id, pending_id);
        assert_eq!(res.claimed[0].status, EventStatus::Processing);

        // Corrupt keys removed and quarantined to DLQ.
        for id in 1..=corrupt_count {
            assert!(!txn.kv.contains_key(&encode_trigger_queue_key(id)));
            let dlq_key = encode_trigger_dlq_key(id);
            let val = txn.kv.get(&dlq_key).expect("dlq entry missing");
            let ev: TriggerEvent = bincode::deserialize(val).unwrap();
            assert_eq!(ev.id, id);
            assert_eq!(ev.status, EventStatus::Failed);
            assert_eq!(ev.trigger_name, "<corrupt>");
            assert!(ev
                .error_msg
                .as_deref()
                .unwrap_or_default()
                .contains("decode failed"));
        }

        // Pending key remains, but is marked Processing.
        let stored = txn
            .kv
            .get(&encode_trigger_queue_key(pending_id))
            .expect("pending key missing");
        let stored_ev: TriggerEvent = bincode::deserialize(stored).unwrap();
        assert_eq!(stored_ev.status, EventStatus::Processing);

        assert_eq!(
            quota.current_depth.load(Ordering::Relaxed),
            100usize.saturating_sub(corrupt_count as usize)
        );
    }

    #[tokio::test]
    async fn claim_events_quarantines_poison_even_without_pending() {
        let worker = TriggerWorker::new_for_test();
        let quota = Arc::new(super::super::worker::KeyspaceQuota {
            max_queue_depth: 1000,
            max_events_per_batch: 10,
            max_retries: 3,
            current_depth: AtomicUsize::new(3),
        });

        let mut txn = MemTxn::default();
        let keyspace = "ks";

        for id in 1..=3u64 {
            txn.kv.insert(encode_trigger_queue_key(id), vec![0, 1, 2]);
        }

        let res = worker
            .claim_events(&mut txn, keyspace, &quota, 1)
            .await
            .unwrap();

        assert!(res.claimed.is_empty());
        assert_eq!(res.quarantined, 3);

        for id in 1..=3u64 {
            assert!(!txn.kv.contains_key(&encode_trigger_queue_key(id)));
            assert!(txn.kv.contains_key(&encode_trigger_dlq_key(id)));
        }
        assert_eq!(quota.current_depth.load(Ordering::Relaxed), 0);
    }
}
