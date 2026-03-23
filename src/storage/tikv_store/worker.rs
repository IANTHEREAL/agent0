use super::*;
use crate::storage::backpressure::tikv_op;
use crate::worker::types::{TaskQueueEntry, TaskRegistryEntry, TaskType, WorkerClaim};

const GC_INSTANCE_STATE_VALUE_LEN: usize = 17;

/// Published GC instance state read back from `_sys_worker`.
pub struct GcInstanceState {
    pub instance_id: String,
    pub min_start_ts: Option<u64>,
    pub updated_at_version: u64,
}

fn encode_gc_instance_state_value(min_start_ts: Option<u64>, updated_at_version: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(GC_INSTANCE_STATE_VALUE_LEN);
    match min_start_ts {
        Some(ts) => {
            data.push(1);
            data.extend_from_slice(&ts.to_be_bytes());
        }
        None => {
            data.push(0);
            data.extend_from_slice(&0u64.to_be_bytes());
        }
    }
    data.extend_from_slice(&updated_at_version.to_be_bytes());
    data
}

fn decode_gc_instance_state_value(val: &[u8]) -> Option<(Option<u64>, u64)> {
    if val.len() < GC_INSTANCE_STATE_VALUE_LEN {
        return None;
    }
    let has_min = val[0] == 1;
    let min_ts = if has_min {
        Some(u64::from_be_bytes(val[1..9].try_into().unwrap_or([0; 8])))
    } else {
        None
    };
    let updated_at = u64::from_be_bytes(val[9..17].try_into().unwrap_or([0; 8]));
    Some((min_ts, updated_at))
}

fn gc_instance_state_scan_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    end.push(0xFF);
    end
}

impl TikvStore {
    // ========================================================================
    // Registry methods
    // ========================================================================

    pub async fn put_worker_registry(
        &self,
        txn: &mut Transaction,
        entry: &TaskRegistryEntry,
    ) -> Result<()> {
        let key = self.key(&encode_worker_registry_key(&entry.keyspace, entry.db_id));
        let data =
            bincode::serialize(entry).context("Failed to serialize worker registry entry")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn get_worker_registry(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<Option<TaskRegistryEntry>> {
        let key = self.key(&encode_worker_registry_key(keyspace, db_id));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data)
                    .context("Failed to deserialize worker registry entry")?,
            )),
            None => Ok(None),
        }
    }

    pub async fn list_worker_registry(
        &self,
        txn: &mut Transaction,
    ) -> Result<Vec<TaskRegistryEntry>> {
        let prefix = encode_worker_registry_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut entries = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let entry: TaskRegistryEntry = bincode::deserialize(pair.value())
                .context("Failed to deserialize worker registry entry")?;
            entries.push(entry);
        }
        Ok(entries)
    }

    pub async fn delete_worker_registry(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_worker_registry_key(keyspace, db_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    pub async fn update_registry_task_types(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        set_bits: u8,
        clear_bits: u8,
    ) -> Result<()> {
        let mut entry = self
            .get_worker_registry(txn, keyspace, db_id)
            .await?
            .unwrap_or_else(|| TaskRegistryEntry::new(keyspace.to_string(), db_id));

        entry.task_types |= set_bits;
        entry.task_types &= !clear_bits;

        if entry.task_types == 0 && entry.job_count == 0 {
            self.delete_worker_registry(txn, keyspace, db_id).await?;
        } else {
            self.put_worker_registry(txn, &entry).await?;
        }
        Ok(())
    }

    // ========================================================================
    // Queue methods
    // ========================================================================

    pub async fn put_worker_queue_entry(
        &self,
        txn: &mut Transaction,
        entry: &TaskQueueEntry,
        fire_time_ms: i64,
    ) -> Result<()> {
        let key = self.key(&encode_worker_queue_key(
            entry.priority,
            fire_time_ms,
            entry.task_type.to_bitmask(),
            &entry.keyspace,
            entry.db_id,
            entry.task_id,
        )?);
        let data = bincode::serialize(entry).context("Failed to serialize worker queue entry")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Scan all due queue entries across all priorities (0-255) up to the given time.
    ///
    /// Returns entries ordered by priority (lower value = higher priority sorts first),
    /// then by fire_time (earlier times sort first).
    ///
    /// The queue key is ordered as `priority -> fire_time -> ...`, so a single upper bound
    /// on `(priority=255, fire_time=now)` is not sufficient to constrain fire_time for
    /// lower priorities. We therefore scan each priority range independently.
    pub async fn scan_due_queue_entries(
        &self,
        txn: &mut Transaction,
        now_ms: i64,
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, TaskQueueEntry)>> {
        let mut results = Vec::new();
        if limit == 0 {
            return Ok(results);
        }

        // Use `now+1ms` as exclusive upper bound so entries scheduled exactly at `now_ms`
        // are included in the half-open range scan.
        let due_exclusive = now_ms.saturating_add(1);

        for priority in 0u8..=255 {
            if results.len() >= limit as usize {
                break;
            }

            let mut start = encode_worker_queue_prefix();
            start.push(priority);
            let end = encode_worker_queue_scan_end(priority, due_exclusive)?;
            let range: BoundRange = (start.clone()..end).into();
            let remaining = limit as usize - results.len();
            let scan_limit = scan_limit_to_u32(Some(remaining));
            let pairs = tikv_op!(txn.scan(range, scan_limit).await)?;

            for pair in pairs {
                let key: &[u8] = pair.key().as_ref().into();
                if !key.starts_with(&start) {
                    continue;
                }
                let entry = TaskQueueEntry::deserialize_compat(pair.value())
                    .context("Failed to deserialize worker queue entry")?;
                results.push((key.to_vec(), entry));
            }
        }
        Ok(results)
    }

    pub async fn delete_worker_queue_entry(&self, txn: &mut Transaction, key: &[u8]) -> Result<()> {
        txn_delete(txn, key.to_vec()).await?;
        Ok(())
    }

    pub async fn scan_queue_entries_for_task(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        task_type: TaskType,
    ) -> Result<Vec<Vec<u8>>> {
        let prefix = encode_worker_queue_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut matching_keys = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let entry = TaskQueueEntry::deserialize_compat(pair.value())
                .context("Failed to deserialize worker queue entry")?;
            if entry.keyspace == keyspace
                && entry.db_id == db_id
                && entry.task_id == task_id
                && entry.task_type == task_type
            {
                matching_keys.push(key.to_vec());
            }
        }
        Ok(matching_keys)
    }

    /// Scan all cron queue entries for a specific (keyspace, db_id).
    /// Returns (raw_key, task_id) pairs for all matching cron entries.
    pub async fn scan_cron_queue_entries_for_db(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
    ) -> Result<Vec<(Vec<u8>, i64)>> {
        let prefix = encode_worker_queue_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut results = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let entry = TaskQueueEntry::deserialize_compat(pair.value())
                .context("Failed to deserialize worker queue entry")?;
            if entry.keyspace == keyspace
                && entry.db_id == db_id
                && entry.task_type == TaskType::Cron
            {
                results.push((key.to_vec(), entry.task_id));
            }
        }
        Ok(results)
    }

    // ========================================================================
    // Claim methods
    // ========================================================================

    pub async fn try_claim_worker_task(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_min: i64,
        claim: &WorkerClaim,
    ) -> Result<bool> {
        let key = self.key(&encode_worker_claim_key(
            claim.task_type.to_bitmask(),
            keyspace,
            db_id,
            task_id,
            fire_time_min,
        ));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            return Ok(false);
        }
        txn_put(
            txn,
            key,
            bincode::serialize(claim).context("Failed to serialize worker claim")?,
        )
        .await?;
        Ok(true)
    }

    pub async fn delete_worker_claim(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        fire_time_min: i64,
        task_type: TaskType,
    ) -> Result<()> {
        let key = self.key(&encode_worker_claim_key(
            task_type.to_bitmask(),
            keyspace,
            db_id,
            task_id,
            fire_time_min,
        ));
        txn_delete(txn, key).await?;
        Ok(())
    }

    /// Delete a worker claim by its raw TiKV key.
    ///
    /// Used by GC when iterating claims via `list_worker_claims_batch`,
    /// which returns raw keys directly. Callers must only pass keys
    /// obtained from claim-namespace scans.
    pub async fn delete_worker_claim_by_raw_key(
        &self,
        txn: &mut Transaction,
        key: &[u8],
    ) -> Result<()> {
        debug_assert!(
            key.starts_with(&encode_worker_claim_prefix()),
            "delete_worker_claim_by_raw_key called with non-claim key"
        );
        txn_delete(txn, key.to_vec()).await?;
        Ok(())
    }

    pub async fn list_worker_claims(
        &self,
        txn: &mut Transaction,
    ) -> Result<Vec<(Vec<u8>, WorkerClaim)>> {
        self.list_worker_claims_batch(txn, None, None).await
    }

    pub async fn list_worker_claims_batch(
        &self,
        txn: &mut Transaction,
        start_after: Option<&[u8]>,
        limit: Option<usize>,
    ) -> Result<Vec<(Vec<u8>, WorkerClaim)>> {
        if matches!(limit, Some(0)) {
            return Ok(Vec::new());
        }

        let prefix = encode_worker_claim_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let start = match start_after {
            Some(last_key) => {
                let mut next_start = last_key.to_vec();
                next_start.push(0x00);
                next_start
            }
            None => prefix.clone(),
        };
        let range: BoundRange = (start..end).into();
        let pairs = tikv_op!(txn.scan(range, scan_limit_to_u32(limit)).await)?;

        let mut results = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let claim: WorkerClaim =
                bincode::deserialize(pair.value()).context("Failed to deserialize worker claim")?;
            results.push((key.to_vec(), claim));
        }
        Ok(results)
    }

    // ========================================================================
    // Background SQL task ID allocation
    // ========================================================================

    /// Allocate the next collision-free background task ID for a (keyspace, db_id) scope.
    ///
    /// Uses a dedicated TiKV CAS sequence key so that concurrent allocations across
    /// multiple db9-server instances always produce distinct IDs.
    pub async fn next_bg_task_id(&self, keyspace: &str, db_id: u64) -> Result<i64> {
        let key = self.key(&encode_worker_bg_task_seq_key(keyspace, db_id));
        self.autocommit_update_key(key, |current| {
            let next_val = match current {
                Some(data) => {
                    let id = i64::from_be_bytes(
                        data.try_into()
                            .map_err(|_| anyhow!("Invalid bg task sequence format"))?,
                    );
                    id.checked_add(1)
                        .ok_or_else(|| anyhow!("Background task ID overflow"))?
                }
                None => 1,
            };
            Ok((Some(next_val.to_be_bytes().to_vec()), next_val))
        })
        .await
    }

    // ========================================================================
    // Background SQL result methods
    // ========================================================================

    pub async fn put_bg_result(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
        result_text: &str,
    ) -> Result<()> {
        let key = self.key(&encode_worker_bg_result_key(keyspace, db_id, task_id));
        txn_put(txn, key, result_text.as_bytes().to_vec()).await?;
        Ok(())
    }

    pub async fn get_bg_result(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        task_id: i64,
    ) -> Result<Option<String>> {
        let key = self.key(&encode_worker_bg_result_key(keyspace, db_id, task_id));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(
                String::from_utf8(data).context("Failed to decode bg result as UTF-8")?,
            )),
            None => Ok(None),
        }
    }

    // ========================================================================
    // GC instance state methods (shared cross-instance registry)
    // ========================================================================

    /// GC instance state stored in `_sys_worker` for cross-instance coordination.
    pub async fn put_gc_instance_state(
        &self,
        txn: &mut Transaction,
        instance_id: &str,
        min_start_ts: Option<u64>,
        updated_at_version: u64,
    ) -> Result<()> {
        let key = self.key(&encode_gc_instance_state_key(instance_id));
        let data = encode_gc_instance_state_value(min_start_ts, updated_at_version);
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Delete a GC instance state record from the shared registry.
    pub async fn delete_gc_instance_state(
        &self,
        txn: &mut Transaction,
        instance_id: &str,
    ) -> Result<()> {
        let key = self.key(&encode_gc_instance_state_key(instance_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    /// Scan all GC instance states from the shared registry.
    pub async fn scan_gc_instance_states(
        &self,
        txn: &mut Transaction,
    ) -> Result<Vec<GcInstanceState>> {
        let prefix = self.key(&encode_gc_instance_state_prefix());
        let end = gc_instance_state_scan_end(&prefix);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut results = Vec::new();
        for pair in pairs {
            let key_bytes: &[u8] = pair.key().as_ref().into();
            if !key_bytes.starts_with(&prefix) {
                break;
            }
            let id_bytes = &key_bytes[prefix.len()..];
            let instance_id = String::from_utf8_lossy(id_bytes).to_string();

            // Accept both the current 17-byte format and the older 25-byte
            // format that appended max_untracked_timeout_sec.
            if let Some((min_ts, updated_at)) = decode_gc_instance_state_value(pair.value()) {
                results.push(GcInstanceState {
                    instance_id,
                    min_start_ts: min_ts,
                    updated_at_version: updated_at,
                });
            }
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn gc_instance_state_value_round_trips_current_format() {
        let encoded = encode_gc_instance_state_value(Some(123), 456);
        assert_eq!(encoded.len(), GC_INSTANCE_STATE_VALUE_LEN);
        assert_eq!(
            decode_gc_instance_state_value(&encoded),
            Some((Some(123), 456))
        );
    }

    #[test]
    fn gc_instance_state_value_decodes_legacy_format_with_timeout_tail() {
        let mut encoded = encode_gc_instance_state_value(Some(123), 456);
        encoded.extend_from_slice(&789u64.to_be_bytes());
        assert_eq!(
            decode_gc_instance_state_value(&encoded),
            Some((Some(123), 456))
        );
    }

    #[test]
    fn gc_instance_state_scan_end_stays_within_prefix_family() {
        let prefix = b"_sys_worker_gc_instance_abc".to_vec();
        let end = gc_instance_state_scan_end(&prefix);
        assert_eq!(end, [prefix, vec![0xFF]].concat());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bg_task_id_atomic_counter_concurrent_allocations_are_unique() {
        let next_id = Arc::new(AtomicI64::new(0));
        let task_count = 100;
        let mut handles = Vec::with_capacity(task_count);

        for _ in 0..task_count {
            let next_id = Arc::clone(&next_id);
            handles.push(tokio::spawn(async move {
                // Mirror the monotonic increment contract behind TiKV CAS allocation.
                next_id.fetch_add(1, Ordering::SeqCst) + 1
            }));
        }

        let mut ids = Vec::with_capacity(task_count);
        for handle in handles {
            ids.push(handle.await.expect("spawned allocator task should join"));
        }

        let unique: HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            task_count,
            "concurrent atomic allocations must be unique"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn next_bg_task_id_concurrent_allocations_are_unique() {
        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_worker_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = crate::worker::config::WorkerConfig {
            enabled: true,
            system_keyspace: system_keyspace.clone(),
            ..Default::default()
        };
        let store = crate::worker::init_system_store(pd_endpoints, &cfg)
            .await
            .expect("failed to initialize system store for bg task ID concurrency test")
            .expect("worker system store should be present when enabled");

        let scope_keyspace = format!(
            "test_bg_task_seq_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let db_id = 4242_u64;

        let task_count = 100;
        let mut handles = Vec::with_capacity(task_count);
        for _ in 0..task_count {
            let store = Arc::clone(&store);
            let keyspace = scope_keyspace.clone();
            handles.push(tokio::spawn(async move {
                let mut backoff_ms = 1_u64;
                for attempt in 0..20 {
                    match store.next_bg_task_id(&keyspace, db_id).await {
                        Ok(id) => return id,
                        Err(err)
                            if err.to_string().contains("autocommit update failed after")
                                && attempt < 19 =>
                        {
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            backoff_ms = (backoff_ms * 2).min(50);
                        }
                        Err(err) => {
                            panic!("next_bg_task_id allocation should succeed: {err}");
                        }
                    }
                }
                panic!("next_bg_task_id allocation exhausted retry attempts")
            }));
        }

        let mut ids = Vec::with_capacity(task_count);
        for handle in handles {
            ids.push(handle.await.expect("spawned allocator task should join"));
        }

        let unique: HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            task_count,
            "concurrent next_bg_task_id allocations must be unique"
        );
    }
}
