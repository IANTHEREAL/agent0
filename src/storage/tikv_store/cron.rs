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

    /// Byte-safe scan of `[start, prefix_end)` reading exactly one key/value
    /// pair per RPC, optionally resumed after a key and/or capped to `limit`
    /// pairs. Single source of truth for every cron scan that reads values
    /// carrying unbounded user content (`command` / `return_message`).
    ///
    /// This deliberately avoids depending on transactional `scan_keys` being
    /// value-free on every deployed TiKV/client combination, and never asks
    /// TiKV for more than one value at a time. No single RPC frame carries more
    /// than one value (each <= raft-entry-max-size), so a prefix holding many
    /// large values -- e.g. cron jobs/runs whose `command` or `return_message`
    /// is large user content -- never builds a >64 MiB gRPC scan frame the way
    /// an unbounded `scan(.., SCAN_LIMIT)` would.
    ///
    /// `start_after`, when set, resumes strictly past that key (for pagination).
    /// `limit`, when set, caps the number of pairs returned. Returns
    /// `(key, raw_value)` pairs in key order.
    async fn scan_prefix_values_bytesafe_paged(
        &self,
        txn: &mut Transaction,
        prefix: &[u8],
        start_after: Option<&[u8]>,
        limit: Option<usize>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let end = encode_prefix_end(prefix);
        let mut out = Vec::new();
        let mut cursor = match start_after {
            // Resume strictly past the last key: append 0x00.
            Some(last) => {
                let mut next = last.to_vec();
                next.push(0);
                next
            }
            None => prefix.to_vec(),
        };
        loop {
            if let Some(limit) = limit {
                if out.len() >= limit {
                    break;
                }
            }
            let range: BoundRange = (cursor.clone()..end.clone()).into();
            let mut pairs = tikv_op!(txn.scan(range, 1).await)?;
            let Some(pair) = pairs.next() else {
                break;
            };
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(prefix) {
                break;
            }
            let key = key.to_vec();
            out.push((key.clone(), pair.value().to_vec()));
            cursor = key;
            cursor.push(0);
        }
        Ok(out)
    }

    /// Byte-safe full scan of `[prefix, prefix_end)` (unbounded). Thin wrapper
    /// over [`Self::scan_prefix_values_bytesafe_paged`].
    async fn scan_prefix_values_bytesafe(
        &self,
        txn: &mut Transaction,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_prefix_values_bytesafe_paged(txn, prefix, None, None)
            .await
    }

    /// Byte-safe paged scan returning keys under `[prefix, prefix_end)`.
    ///
    /// Values are read one at a time and discarded. This is slower than
    /// `scan_keys`, but robust for cleanup paths where correctness matters more
    /// than throughput.
    async fn scan_prefix_keys_bytesafe(
        &self,
        txn: &mut Transaction,
        prefix: &[u8],
    ) -> Result<Vec<Vec<u8>>> {
        let end = encode_prefix_end(prefix);
        let mut out = Vec::new();
        let mut cursor = prefix.to_vec();
        loop {
            let range: BoundRange = (cursor.clone()..end.clone()).into();
            let mut pairs = tikv_op!(txn.scan(range, 1).await)?;
            let Some(pair) = pairs.next() else {
                break;
            };
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(prefix) {
                break;
            }
            let key = key.to_vec();
            out.push(key.clone());
            cursor = key;
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

    /// List up to `limit` cron runs for a database.
    ///
    /// Byte-safe (see [`Self::scan_prefix_values_bytesafe_paged`]): the cron run
    /// VALUE carries a copy of the job's `command` plus the captured
    /// `return_message` (the failure path stores the full execution error), so an
    /// unbounded `scan(.., limit)` here would build a single >64 MiB gRPC frame
    /// once a db accumulates enough large-value runs and wedge the
    /// `cron.job_run_details` view -- the same bug class as the `cron.job` view.
    pub async fn list_all_cron_runs(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        limit: usize,
    ) -> Result<Vec<CronRun>> {
        let prefix = encode_cron_run_prefix_v2(db_id);
        let pairs = self
            .scan_prefix_values_bytesafe_paged(txn, &prefix, None, Some(limit))
            .await?;
        let mut runs = Vec::with_capacity(pairs.len());
        for (_key, val) in pairs {
            runs.push(bincode::deserialize(&val).context("Failed to deserialize cron run")?);
        }
        Ok(runs)
    }

    /// Paginated scan of cron runs. Returns `(runs, raw_keys)` where
    /// `raw_keys[i]` is the TiKV key for `runs[i]`, used for point-deletes.
    /// Pass the last element of `raw_keys` as `start_after` for the next page.
    ///
    /// Byte-safe (see [`Self::scan_prefix_values_bytesafe_paged`]): cron run
    /// values carry a copy of the job's `command` plus the captured
    /// `return_message`, so values are read one per RPC rather than via a single
    /// `scan(.., limit)` frame that could exceed the 64 MiB gRPC cap on the GC
    /// path.
    pub async fn list_cron_runs_batch(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<(Vec<CronRun>, Vec<Vec<u8>>)> {
        let prefix = encode_cron_run_prefix_v2(db_id);
        let pairs = self
            .scan_prefix_values_bytesafe_paged(txn, &prefix, start_after, Some(limit))
            .await?;
        let mut runs = Vec::with_capacity(pairs.len());
        let mut keys = Vec::with_capacity(pairs.len());
        for (key, val) in pairs {
            runs.push(bincode::deserialize(&val).context("Failed to deserialize cron run")?);
            keys.push(key);
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
        // collect one (key, value) per RPC rather than one unbounded
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
        // `command`. Read at most one value per RPC, so DROP DATABASE cleanup can
        // never build a >64 MiB scan frame -- the failure that previously left
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

    /// Regression: many large cron VALUES in one database must not build a single
    /// >64 MiB gRPC scan frame. This covers BOTH cron-job reads
    /// (`list_cron_jobs` / `find_cron_job_by_name`, value = large `command`) and
    /// cron-run reads (`list_all_cron_runs` backing the `cron.job_run_details`
    /// view, and the paginated `list_cron_runs_batch` GC path, value = large
    /// `return_message`), plus `delete_all_cron_data` cleanup. Before the
    /// byte-safe scan fix these raised `OutOfRange: message length too large`,
    /// wedging cron globally and leaving the data un-droppable. Reverting either
    /// the job-scan or the run-scan fix makes this test fail.
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

        // Cron RUNS carry a copy of the job `command` plus the captured
        // `return_message` (the failure path stores the full execution error).
        // ~14 x ~5 MiB return_message = ~70 MiB total, so the run-read paths must
        // also page one value per RPC. Separate txn to keep each commit bounded.
        let big_run = "y".repeat(5 * 1024 * 1024);
        {
            let mut txn = store.begin().await.expect("begin");
            for run_id in 1..=n {
                let run = CronRun {
                    run_id,
                    job_id: run_id,
                    job_pid: None,
                    database: "regress".to_string(),
                    username: "admin".to_string(),
                    command: "SELECT 1".to_string(),
                    status: crate::cron::types::CronRunStatus::Failed,
                    return_message: Some(big_run.clone()),
                    start_time: Some(0),
                    end_time: Some(1),
                };
                store
                    .put_cron_run(&mut txn, db_id, &run)
                    .await
                    .expect("put_cron_run");
            }
            txn.commit().await.expect("commit run puts");
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

        // Cron-run reads must be byte-safe (no OutOfRange frame) on BOTH the
        // `cron.job_run_details` view path and the paginated GC path.
        {
            let mut txn = store.begin().await.expect("begin");
            let runs = store
                .list_all_cron_runs(&mut txn, db_id, 1000)
                .await
                .expect("list_all_cron_runs must be byte-safe over large return_messages");
            assert_eq!(runs.len(), n as usize);

            // Paginated GC path must be byte-safe AND visit every run exactly once.
            let mut seen = std::collections::BTreeSet::new();
            let mut start_after: Option<Vec<u8>> = None;
            loop {
                let (page, keys) = store
                    .list_cron_runs_batch(&mut txn, db_id, start_after.as_deref(), 5)
                    .await
                    .expect("list_cron_runs_batch must be byte-safe");
                if page.is_empty() {
                    break;
                }
                for r in &page {
                    assert!(
                        seen.insert(r.run_id),
                        "run {} returned twice across pages",
                        r.run_id
                    );
                }
                start_after = keys.last().cloned();
                if page.len() < 5 {
                    break;
                }
            }
            assert_eq!(
                seen.len(),
                n as usize,
                "pagination must cover every run exactly once"
            );
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
            let runs = store
                .list_all_cron_runs(&mut txn, db_id, 1000)
                .await
                .expect("list runs after delete");
            assert!(runs.is_empty(), "all cron runs must be deleted");
            txn.commit().await.ok();
        }
    }
}
