use super::*;
use crate::cron::types::{CronJob, CronJobLegacy, CronRun};
use crate::storage::backpressure::tikv_op;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronRunClaimStatus {
    Claimed,
    AlreadyClaimedForMinute,
    BlockedByRunningGuard,
}

fn deserialize_cron_job(data: &[u8]) -> anyhow::Result<CronJob> {
    match bincode::deserialize::<CronJob>(data) {
        Ok(job) => Ok(job),
        Err(_) => {
            let legacy: CronJobLegacy = bincode::deserialize(data)
                .context("Failed to deserialize cron job (legacy fallback)")?;
            Ok(legacy.into())
        }
    }
}

impl TikvStore {
    pub async fn put_cron_job(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job: &CronJob,
    ) -> Result<()> {
        let key = self.key(&encode_cron_job_key_v2(db_id, job.job_id));
        let data = bincode::serialize(job).context("Failed to serialize cron job")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn get_cron_job(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
    ) -> Result<Option<CronJob>> {
        let key = self.key(&encode_cron_job_key_v2(db_id, job_id));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(deserialize_cron_job(&data)?)),
            None => Ok(None),
        }
    }

    /// Byte-safe full scan of the `[prefix, prefix++0xFF)` range: page keys via
    /// `scan_keys` (1024 at a time), then point-get each value individually.
    ///
    /// Mirrors the worker-queue legacy drain (`scan_legacy_filtered`): no single
    /// RPC frame ever carries more than one value (each ≤ raft-entry-max-size), so
    /// a prefix holding many large values — e.g. cron jobs whose `command` is large
    /// user SQL — never builds a >64 MiB gRPC scan frame the way an unbounded
    /// `scan(.., SCAN_LIMIT)` would. Returns `(key, raw_value)` pairs in key order.
    async fn scan_prefix_values_bytesafe(
        &self,
        txn: &mut Transaction,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut end = prefix.to_vec();
        end.push(0xFF);
        let mut out = Vec::new();
        let mut cursor = prefix.to_vec();
        loop {
            let range: BoundRange = (cursor.clone()..end.clone()).into();
            let keys: Vec<Vec<u8>> = tikv_op!(txn.scan_keys(range, 1024).await)?
                .map(Vec::from)
                .collect();
            let n = keys.len();
            if n == 0 {
                break;
            }
            let mut last = Vec::new();
            for key in keys {
                last = key.clone();
                if !key.starts_with(prefix) {
                    continue;
                }
                if let Some(val) = tikv_op!(txn.get(key.clone()).await)? {
                    out.push((key, val.to_vec()));
                }
            }
            if n < 1024 || last.is_empty() {
                break;
            }
            cursor = last;
            cursor.push(0);
        }
        Ok(out)
    }

    /// Byte-safe paged scan returning only the keys under `[prefix, prefix++0xFF)`
    /// (values are never read). For delete-by-prefix paths that need the keys but
    /// never the (potentially large) values.
    async fn scan_prefix_keys_bytesafe(
        &self,
        txn: &mut Transaction,
        prefix: &[u8],
    ) -> Result<Vec<Vec<u8>>> {
        let mut end = prefix.to_vec();
        end.push(0xFF);
        let mut out = Vec::new();
        let mut cursor = prefix.to_vec();
        loop {
            let range: BoundRange = (cursor.clone()..end.clone()).into();
            let keys: Vec<Vec<u8>> = tikv_op!(txn.scan_keys(range, 1024).await)?
                .map(Vec::from)
                .collect();
            let n = keys.len();
            if n == 0 {
                break;
            }
            let mut last = Vec::new();
            for key in keys {
                last = key.clone();
                if key.starts_with(prefix) {
                    out.push(key);
                }
            }
            if n < 1024 || last.is_empty() {
                break;
            }
            cursor = last;
            cursor.push(0);
        }
        Ok(out)
    }

    /// List all cron jobs for a database.
    ///
    /// Uses a byte-safe scan (see [`Self::scan_prefix_values_bytesafe`]): the
    /// cron job VALUE carries the unbounded user `command`, so an unbounded
    /// `scan(.., SCAN_LIMIT)` here would build a single >64 MiB gRPC frame once a
    /// db accumulates enough large-command jobs and wedge every caller
    /// (schedule/unschedule/reconcile/`cron.job` view). Same bug class as the
    /// worker-queue scans fixed for #2576, in the cron-catalog access path.
    pub async fn list_cron_jobs(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<CronJob>> {
        let prefix = encode_cron_job_prefix_v2(db_id);
        let pairs = self.scan_prefix_values_bytesafe(txn, &prefix).await?;
        let mut jobs = Vec::with_capacity(pairs.len());
        for (_key, val) in pairs {
            jobs.push(deserialize_cron_job(&val)?);
        }
        Ok(jobs)
    }

    pub async fn delete_cron_job(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
    ) -> Result<()> {
        let key = self.key(&encode_cron_job_key_v2(db_id, job_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    pub async fn find_cron_job_by_name(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        jobname: &str,
        username: &str,
    ) -> Result<Option<CronJob>> {
        let jobs = self.list_cron_jobs(txn, db_id).await?;
        Ok(jobs
            .into_iter()
            .find(|j| j.username == username && j.jobname.as_deref() == Some(jobname)))
    }

    pub async fn put_cron_run(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        run: &CronRun,
    ) -> Result<()> {
        let key = self.key(&encode_cron_run_key_v2(db_id, run.run_id));
        let data = bincode::serialize(run).context("Failed to serialize cron run")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn get_cron_run(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        run_id: i64,
    ) -> Result<Option<CronRun>> {
        let key = self.key(&encode_cron_run_key_v2(db_id, run_id));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize cron run")?,
            )),
            None => Ok(None),
        }
    }

    #[allow(dead_code)]
    pub async fn delete_cron_run(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        run_id: i64,
    ) -> Result<()> {
        let key = self.key(&encode_cron_run_key_v2(db_id, run_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    pub async fn list_all_cron_runs(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        limit: usize,
    ) -> Result<Vec<CronRun>> {
        let prefix = encode_cron_run_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let scan_limit = scan_limit_to_u32(Some(limit));
        let pairs = tikv_op!(txn.scan(range, scan_limit).await)?;

        let mut runs = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let run: CronRun =
                bincode::deserialize(pair.value()).context("Failed to deserialize cron run")?;
            runs.push(run);
        }
        Ok(runs)
    }

    /// Paginated scan of cron runs. Returns `(runs, raw_keys)` where
    /// `raw_keys[i]` is the TiKV key for `runs[i]`, used for point-deletes.
    /// Pass the last element of `raw_keys` as `start_after` for the next page.
    pub async fn list_cron_runs_batch(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<(Vec<CronRun>, Vec<Vec<u8>>)> {
        let prefix = encode_cron_run_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);

        let range_start = match start_after {
            Some(last_key) => {
                // Start just past the last key: append 0x00 byte.
                let mut next = last_key.to_vec();
                next.push(0x00);
                next
            }
            None => prefix.clone(),
        };

        let range: BoundRange = (range_start..end).into();
        let scan_limit = scan_limit_to_u32(Some(limit));
        let pairs = tikv_op!(txn.scan(range, scan_limit).await)?;

        let mut runs = Vec::new();
        let mut keys = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let run: CronRun =
                bincode::deserialize(pair.value()).context("Failed to deserialize cron run")?;
            keys.push(key.to_vec());
            runs.push(run);
        }
        Ok((runs, keys))
    }

    /// Delete a single cron run by its raw TiKV key.
    pub async fn delete_cron_run_by_key(
        &self,
        txn: &mut Transaction,
        raw_key: Vec<u8>,
    ) -> Result<()> {
        txn_delete(txn, raw_key).await?;
        Ok(())
    }

    pub async fn try_claim_cron_run(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        scheduled_min: i64,
    ) -> Result<CronRunClaimStatus> {
        // Step 1: Check if there's a running guard for this job
        let guard_key = self.key(&encode_cron_running_guard_key_v2(db_id, job_id));
        if let Some(guard_data) = tikv_op!(txn.get(guard_key.clone()).await)? {
            // Guard exists, check if the referenced run is still active
            if guard_data.len() == 8 {
                let run_id = i64::from_be_bytes(
                    guard_data
                        .as_slice()
                        .try_into()
                        .map_err(|_| anyhow!("Invalid running guard format"))?,
                );

                // Check if the run exists and is in an active state
                if let Some(run) = self.get_cron_run(txn, db_id, run_id).await? {
                    use crate::cron::types::CronRunStatus;
                    match run.status {
                        CronRunStatus::Starting | CronRunStatus::Running => {
                            // Job is still running, reject this claim
                            return Ok(CronRunClaimStatus::BlockedByRunningGuard);
                        }
                        CronRunStatus::Succeeded
                        | CronRunStatus::Failed
                        | CronRunStatus::Cancelled => {
                            // Stale guard, delete it and continue
                            txn_delete(txn, guard_key).await?;
                        }
                    }
                } else {
                    // Run doesn't exist, stale guard, delete it
                    txn_delete(txn, guard_key).await?;
                }
            } else {
                // Invalid guard data, delete it
                txn_delete(txn, guard_key).await?;
            }
        }

        // Step 2: Proceed with existing same-minute claim logic
        let key = self.key(&encode_cron_claim_key_v2(db_id, job_id, scheduled_min));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            return Ok(CronRunClaimStatus::AlreadyClaimedForMinute);
        }
        txn_put(txn, key, vec![1]).await?;
        Ok(CronRunClaimStatus::Claimed)
    }

    pub async fn delete_cron_runs_for_job(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
    ) -> Result<()> {
        // Byte-safe: cron run values can be large (captured `return_message`), so
        // collect (key, value) via paged point-gets rather than one unbounded
        // `scan(.., SCAN_LIMIT)` frame.
        let prefix = encode_cron_run_prefix_v2(db_id);
        let pairs = self.scan_prefix_values_bytesafe(txn, &prefix).await?;
        for (key, val) in pairs {
            let run: CronRun =
                bincode::deserialize(&val).context("Failed to deserialize cron run")?;
            if run.job_id == job_id {
                txn_delete(txn, key).await?;
            }
        }
        Ok(())
    }

    pub async fn set_cron_enabled(&self, txn: &mut Transaction, db_id: u64) -> Result<()> {
        let key = self.key(&encode_cron_enabled_key_v2(db_id));
        txn_put(txn, key, vec![1u8]).await?;
        Ok(())
    }

    pub async fn remove_cron_enabled(&self, txn: &mut Transaction, db_id: u64) -> Result<()> {
        let key = self.key(&encode_cron_enabled_key_v2(db_id));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            txn_delete(txn, key).await?;
        }
        Ok(())
    }

    pub async fn is_cron_enabled(&self, txn: &mut Transaction, db_id: u64) -> Result<bool> {
        let key = self.key(&encode_cron_enabled_key_v2(db_id));
        Ok(tikv_op!(txn.get(key).await)?.is_some())
    }

    pub async fn set_cron_running_guard(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        run_id: i64,
    ) -> Result<()> {
        let key = self.key(&encode_cron_running_guard_key_v2(db_id, job_id));
        txn_put(txn, key, run_id.to_be_bytes().to_vec()).await?;
        Ok(())
    }

    pub async fn clear_cron_running_guard(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        expected_run_id: i64,
    ) -> Result<()> {
        let key = self.key(&encode_cron_running_guard_key_v2(db_id, job_id));
        if let Some(data) = tikv_op!(txn.get(key.clone()).await)? {
            if data.len() != 8 {
                // Self-heal malformed guard payloads.
                txn_delete(txn, key).await?;
                return Ok(());
            }

            let current_run_id = i64::from_be_bytes(
                data.as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid running guard format"))?,
            );
            if current_run_id == expected_run_id {
                txn_delete(txn, key).await?;
            }
        }
        Ok(())
    }

    pub async fn delete_all_cron_data(&self, txn: &mut Transaction, db_id: u64) -> Result<()> {
        let prefixes = [
            encode_cron_job_prefix_v2(db_id),
            encode_cron_run_prefix_v2(db_id),
            encode_cron_claim_prefix_v2(db_id),
            encode_cron_running_guard_prefix_v2(db_id),
        ];

        // Byte-safe: the cron-job prefix holds values carrying unbounded user
        // `command`. Delete by KEY only (no value reads), so DROP DATABASE cleanup
        // can never build a >64 MiB scan frame — the failure that previously left
        // large-command cron jobs un-droppable (no SQL recovery path).
        for prefix in &prefixes {
            for key in self.scan_prefix_keys_bytesafe(txn, prefix).await? {
                txn_delete(txn, key).await?;
            }
        }

        let seq_keys = [
            self.key(&encode_next_cron_job_id_key_v2(db_id)),
            self.key(&encode_next_cron_run_id_key_v2(db_id)),
        ];
        for key in seq_keys {
            if tikv_op!(txn.get(key.clone()).await)?.is_some() {
                txn_delete(txn, key).await?;
            }
        }

        Ok(())
    }

    pub async fn next_cron_job_id(&self, db_id: u64) -> Result<i64> {
        let key = self.key(&encode_next_cron_job_id_key_v2(db_id));
        self.autocommit_update_key(key, |current| {
            let next_val = match current {
                Some(data) => {
                    let id = i64::from_be_bytes(
                        data.try_into()
                            .map_err(|_| anyhow!("Invalid cron job ID format"))?,
                    );
                    id.checked_add(1)
                        .ok_or_else(|| anyhow!("Cron job ID overflow"))?
                }
                None => 1,
            };
            Ok((Some(next_val.to_be_bytes().to_vec()), next_val))
        })
        .await
    }

    pub async fn next_cron_run_id(&self, db_id: u64) -> Result<i64> {
        let key = self.key(&encode_next_cron_run_id_key_v2(db_id));
        self.autocommit_update_key(key, |current| {
            let next_val = match current {
                Some(data) => {
                    let id = i64::from_be_bytes(
                        data.try_into()
                            .map_err(|_| anyhow!("Invalid cron run ID format"))?,
                    );
                    id.checked_add(1)
                        .ok_or_else(|| anyhow!("Cron run ID overflow"))?
                }
                None => 1,
            };
            Ok((Some(next_val.to_be_bytes().to_vec()), next_val))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_cron_job_reads_current_shape() {
        let job = CronJob {
            job_id: 11,
            schedule: "*/10 * * * *".to_string(),
            command: "SELECT 1".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "postgres".to_string(),
            username: "admin".to_string(),
            active: true,
            jobname: Some("j".to_string()),
            max_runtime_ms: Some(30_000),
        };

        let data = bincode::serialize(&job).unwrap();
        let decoded = deserialize_cron_job(&data).unwrap();
        assert_eq!(decoded, job);
    }

    #[test]
    fn deserialize_cron_job_falls_back_to_legacy_shape() {
        let legacy = CronJobLegacy {
            job_id: 7,
            schedule: "0 * * * *".to_string(),
            command: "VACUUM".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "postgres".to_string(),
            username: "admin".to_string(),
            active: true,
            jobname: Some("legacy".to_string()),
        };

        let data = bincode::serialize(&legacy).unwrap();
        let decoded = deserialize_cron_job(&data).unwrap();
        assert_eq!(decoded.job_id, legacy.job_id);
        assert_eq!(decoded.schedule, legacy.schedule);
        assert_eq!(decoded.command, legacy.command);
        assert_eq!(decoded.max_runtime_ms, None);
    }

    #[test]
    fn deserialize_cron_job_rejects_invalid_payload() {
        let err = deserialize_cron_job(&[1, 2, 3, 4]).unwrap_err().to_string();
        assert!(err.contains("Failed to deserialize cron job"));
    }

    /// Regression: many large `command`s in one database must not build a single
    /// >64 MiB gRPC scan frame in `list_cron_jobs` / `find_cron_job_by_name` /
    /// `delete_all_cron_data`. Before the byte-safe scan fix these raised
    /// `OutOfRange: message length too large`, wedging cron globally and leaving
    /// the jobs un-droppable. Reverting the fix makes this test fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cron_catalog_scans_are_byte_safe_over_large_commands() {
        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_cron_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = crate::worker::config::WorkerConfig {
            enabled: true,
            system_keyspace,
            ..Default::default()
        };
        let store = crate::worker::init_system_store(pd_endpoints, &cfg)
            .await
            .expect("init system store")
            .expect("system store present when enabled");

        let db_id = 987_654_u64;
        // 14 jobs x ~5 MiB command = ~70 MiB total, comfortably over the 64 MiB
        // gRPC frame cap an unbounded `scan(.., SCAN_LIMIT)` would hit.
        let big = "x".repeat(5 * 1024 * 1024);
        let n: i64 = 14;
        {
            let mut txn = store.begin().await.expect("begin");
            for job_id in 1..=n {
                let job = CronJob {
                    job_id,
                    schedule: "* * * * *".to_string(),
                    command: format!("SELECT '{big}'"),
                    nodename: "localhost".to_string(),
                    nodeport: 5433,
                    database: "regress".to_string(),
                    username: "admin".to_string(),
                    active: true,
                    jobname: Some(format!("big_{job_id}")),
                    max_runtime_ms: None,
                };
                store
                    .put_cron_job(&mut txn, db_id, &job)
                    .await
                    .expect("put_cron_job");
            }
            txn.commit().await.expect("commit puts");
        }

        // list / find must be byte-safe (no OutOfRange frame).
        {
            let mut txn = store.begin().await.expect("begin");
            let jobs = store
                .list_cron_jobs(&mut txn, db_id)
                .await
                .expect("list_cron_jobs must be byte-safe over large commands");
            assert_eq!(jobs.len(), n as usize);
            let found = store
                .find_cron_job_by_name(&mut txn, db_id, "big_7", "admin")
                .await
                .expect("find_cron_job_by_name must be byte-safe");
            assert!(found.is_some());
            txn.commit().await.ok();
        }

        // delete_all_cron_data must clean up without reading the large values.
        {
            let mut txn = store.begin().await.expect("begin");
            store
                .delete_all_cron_data(&mut txn, db_id)
                .await
                .expect("delete_all_cron_data must be byte-safe");
            txn.commit().await.expect("commit delete");
        }
        {
            let mut txn = store.begin().await.expect("begin");
            let jobs = store
                .list_cron_jobs(&mut txn, db_id)
                .await
                .expect("list after delete");
            assert!(jobs.is_empty(), "all cron jobs must be deleted");
            txn.commit().await.ok();
        }
    }
}
