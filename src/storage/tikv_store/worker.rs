use super::*;
use crate::worker::types::{TaskQueueEntry, TaskRegistryEntry, WorkerClaim};

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
        match txn.get(key).await? {
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
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

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
            &entry.keyspace,
            entry.db_id,
            entry.task_id,
        ));
        let data = bincode::serialize(entry).context("Failed to serialize worker queue entry")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Scan all due queue entries across all priorities (0-255) up to the given time.
    ///
    /// Returns entries ordered by priority (lower value = higher priority sorts first),
    /// then by fire_time (earlier times sort first). This single range scan covers all
    /// priority levels in one pass, ensuring higher-priority tasks are processed before
    /// lower-priority ones regardless of fire_time.
    pub async fn scan_due_queue_entries(
        &self,
        txn: &mut Transaction,
        now_ms: i64,
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, TaskQueueEntry)>> {
        let start = encode_worker_queue_prefix();
        let end = encode_worker_queue_scan_end(255, now_ms);
        let range: BoundRange = (start.clone()..end).into();
        let scan_limit = scan_limit_to_u32(Some(limit as usize));
        let pairs = txn.scan(range, scan_limit).await?;

        let mut results = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&start) {
                continue;
            }
            let entry: TaskQueueEntry = bincode::deserialize(pair.value())
                .context("Failed to deserialize worker queue entry")?;
            results.push((key.to_vec(), entry));
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
    ) -> Result<Vec<Vec<u8>>> {
        let prefix = encode_worker_queue_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut matching_keys = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let entry: TaskQueueEntry = bincode::deserialize(pair.value())
                .context("Failed to deserialize worker queue entry")?;
            if entry.keyspace == keyspace && entry.db_id == db_id && entry.task_id == task_id {
                matching_keys.push(key.to_vec());
            }
        }
        Ok(matching_keys)
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
            keyspace,
            db_id,
            task_id,
            fire_time_min,
        ));
        if txn.get(key.clone()).await?.is_some() {
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
    ) -> Result<()> {
        let key = self.key(&encode_worker_claim_key(
            keyspace,
            db_id,
            task_id,
            fire_time_min,
        ));
        txn_delete(txn, key).await?;
        Ok(())
    }

    pub async fn list_worker_claims(
        &self,
        txn: &mut Transaction,
    ) -> Result<Vec<(Vec<u8>, WorkerClaim)>> {
        let prefix = encode_worker_claim_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

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
        match txn.get(key).await? {
            Some(data) => Ok(Some(
                String::from_utf8(data).context("Failed to decode bg result as UTF-8")?,
            )),
            None => Ok(None),
        }
    }
}
