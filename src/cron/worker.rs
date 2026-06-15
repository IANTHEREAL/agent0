use crate::cron::config::CronConfig;
use crate::sql::executor::core::retry::is_retryable_tikv_error;
use crate::storage::TikvStore;
use crate::worker::engine::{
    is_retryable_region_error, region_error_backoff, REGION_ERROR_MAX_RETRIES,
};
use anyhow::Result;
use std::sync::Arc;
use tikv_client::TimestampExt;
use tracing::{info, warn};

/// GC batch size — number of cron runs to process per transaction.
/// Keeps pessimistic lock hold time bounded regardless of total run count.
const GC_BATCH_SIZE: usize = 500;

/// Maximum retries per batch for transient TiKV errors (region routing +
/// write conflict / deadlock).
const GC_BATCH_MAX_RETRIES: u32 = REGION_ERROR_MAX_RETRIES;

pub(crate) async fn gc_database(
    store: &Arc<TikvStore>,
    db_id: u64,
    config: &CronConfig,
) -> Result<()> {
    let now = now_ms();
    let retention_cutoff = now.saturating_sub(
        i64::try_from(
            config
                .run_retention_days
                .saturating_mul(24)
                .saturating_mul(3600)
                .saturating_mul(1000),
        )
        .unwrap_or(i64::MAX),
    );

    // Pre-check: if cron is disabled for this database, skip immediately.
    {
        let mut check_txn = store.begin().await?;
        let enabled = store.is_cron_enabled(&mut check_txn, db_id).await?;
        check_txn.rollback().await.ok();
        if !enabled {
            return Ok(());
        }
    }

    let mut total_recovered = 0usize;
    let mut total_deleted = 0usize;

    // Orphan recovery — the SINGLE path (design 35). Drives off the bounded
    // per-job ACTIVE pointers (one per running job), not a run-history scan. For
    // each pointer past its FROZEN deadline, terminate the run through the SAME
    // fence CAS the owner's finalize uses: it flips CONTROL -> Failed, releases
    // the ACTIVE pointer, and projects the Failed history record in one txn. The
    // fence gate means a live takeover (higher fence) is never clobbered, and a
    // job that stops firing does not leave CONTROL/ACTIVE state behind. History
    // GC below is retention-DELETE-only — it never recomputes an orphan cutoff
    // and never writes run status, so a lowered `max_runtime_ms` can no longer
    // retroact a still-live fenced run to Failed.
    total_recovered =
        total_recovered.saturating_add(reap_stale_active_runs(store, db_id, now).await?);

    let mut cursor: Option<Vec<u8>> = None;

    loop {
        let batch_result =
            gc_database_batch(store, db_id, retention_cutoff, cursor.as_deref()).await;

        match batch_result {
            Ok(batch) => {
                total_deleted = total_deleted.saturating_add(batch.deleted);
                if batch.is_final {
                    break;
                }
                cursor = batch.last_key;
            }
            Err(e) => {
                // Non-retryable error — propagate.
                return Err(e);
            }
        }
    }

    if total_recovered > 0 || total_deleted > 0 {
        info!(
            db_id,
            recovered_orphans = total_recovered,
            deleted_runs = total_deleted,
            "cron GC complete"
        );
    }
    Ok(())
}

/// Reap cron runs whose ACTIVE pointer has passed its orphan deadline, via the
/// fence CAS (design 35). Each reap runs in its own short txn; a pointer that a
/// live takeover has refreshed (higher fence) is left untouched by the CAS.
/// Best-effort: a per-pointer error is logged and skipped so one bad job does
/// not block the rest of GC. Returns the number of runs reaped.
async fn reap_stale_active_runs(store: &Arc<TikvStore>, db_id: u64, now: i64) -> Result<usize> {
    let stale: Vec<_> = {
        let mut txn = store.begin().await?;
        let actives = store.list_cron_active(&mut txn, db_id).await?;
        txn.rollback().await.ok();
        actives
            .into_iter()
            .filter(|a| a.deadline_ms < now)
            .collect()
    };

    let mut reaped = 0usize;
    for a in stale {
        let mut txn = store.begin().await?;
        // Register with the GC safepoint so collection cannot advance past this
        // reap txn's snapshot while it reads the run record + commits the fence
        // CAS (mirrors gc_database_batch_inner).
        let mut txn_guard = crate::worker::active_txn_registry::global_registry()
            .map(|registry| registry.track_worker_txn(txn.start_timestamp().version()));
        let result: Result<bool> = async {
            // RECONCILE from the shared CronRun, do NOT force Failed (design 35
            // §Reaper reconcile). If the owner already finalized this fire (e.g. an
            // OLD #2629 worker FINISHED via the legacy path after the new control
            // plane was minted — it wrote a real terminal `CronRun` but could not
            // clear the new CONTROL/ACTIVE it does not know about), preserve that
            // authoritative terminal status/message/end_time and terminalize
            // CONTROL consistently with it. Only a genuinely orphaned (non-terminal
            // or already retention-GC'd) run becomes Failed. Forcing Failed here
            // would clobber a Succeeded run — a non-owner terminal write admitted
            // after the owner already finalized.
            let existing = store.get_cron_run(&mut txn, db_id, a.run_id).await?;
            let (terminal_state, run) = crate::storage::cron::reconcile_cron_terminal(
                existing,
                a.run_id,
                a.job_id,
                "orphan recovery: execution timed out",
                now,
            );
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    a.job_id,
                    a.active_minute,
                    a.fence_token,
                    terminal_state,
                    &run,
                )
                .await?;
            if accepted {
                store
                    .assert_database_alive_for_update(&mut txn, db_id)
                    .await?;
            }
            Ok(accepted)
        }
        .await;

        match result {
            Ok(true) => {
                txn.commit().await?;
                reaped = reaped.saturating_add(1);
            }
            Ok(false) => {
                // A live takeover holds a higher fence — leave it to the owner.
                txn.rollback().await.ok();
            }
            Err(e) => {
                if txn.rollback().await.is_err() {
                    if let Some(g) = txn_guard.as_mut() {
                        g.quarantine();
                    }
                }
                warn!(
                    db_id,
                    job_id = a.job_id,
                    "cron GC: failed to reap stale active run, skipping: {e}"
                );
            }
        }
        drop(txn_guard);
    }
    Ok(reaped)
}

struct GcBatchResult {
    deleted: usize,
    last_key: Option<Vec<u8>>,
    is_final: bool,
}

/// Process one batch of cron runs with retry on transient TiKV errors.
async fn gc_database_batch(
    store: &Arc<TikvStore>,
    db_id: u64,
    retention_cutoff: i64,
    start_after: Option<&[u8]>,
) -> Result<GcBatchResult> {
    for attempt in 0..=GC_BATCH_MAX_RETRIES {
        let result = gc_database_batch_inner(store, db_id, retention_cutoff, start_after).await;

        match result {
            Ok(batch) => return Ok(batch),
            Err(e)
                if attempt < GC_BATCH_MAX_RETRIES
                    && (is_retryable_region_error(&e) || is_retryable_tikv_error(&e)) =>
            {
                warn!(
                    db_id,
                    attempt = attempt + 1,
                    max_retries = GC_BATCH_MAX_RETRIES,
                    "cron GC batch: transient error, retrying with fresh txn"
                );
                tracing::debug!(db_id, "cron GC batch error detail: {e}");
                region_error_backoff(attempt).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Inner batch logic: scan one page and point-delete runs past retention. Uses a
/// single short-lived pessimistic transaction. Retention-DELETE-only — orphan
/// recovery is owned exclusively by `reap_stale_active_runs` (design 35).
async fn gc_database_batch_inner(
    store: &Arc<TikvStore>,
    db_id: u64,
    retention_cutoff: i64,
    start_after: Option<&[u8]>,
) -> Result<GcBatchResult> {
    let mut txn = store.begin().await?;
    let mut txn_guard = crate::worker::active_txn_registry::global_registry()
        .map(|registry| registry.track_worker_txn(txn.start_timestamp().version()));

    let batch_result = async {
        let (runs, raw_keys) = store
            .list_cron_runs_batch(&mut txn, db_id, start_after, GC_BATCH_SIZE)
            .await?;

        if runs.is_empty() {
            return Ok(GcBatchResult {
                deleted: 0,
                last_key: None,
                is_final: true,
            });
        }

        let is_final = runs.len() < GC_BATCH_SIZE;
        let last_key = raw_keys.last().cloned();

        let mut deleted = 0usize;

        // History GC is retention-DELETE-only (design 35). Orphan recovery
        // (Running → Failed) is owned EXCLUSIVELY by `reap_stale_active_runs`,
        // which terminates through the fence CAS keyed on the ACTIVE pointer's
        // frozen deadline. The legacy history-scan branch recomputed the cutoff
        // from the CURRENT per-job max_runtime_ms each cycle, so a `cron.alter_job`
        // that LOWERED max_runtime_ms after a fire claimed could publish `Failed`
        // for a still-live fenced run (its ACTIVE deadline still in the future) —
        // a cross-deadline divergence violating design 35's invariant that the
        // history record is a projection, never written outside the terminal CAS.
        // Collapsing to the single fence-CAS path removes that divergence.
        for (run, raw_key) in runs.into_iter().zip(raw_keys) {
            // Retention is TERMINAL-ONLY (design 35 §Retention vs orphan recovery).
            // A NON-TERMINAL run (`Starting`/`Running`) is the live history record
            // the cross-generation no-overlap bridge dereferences for liveness: an
            // OLD #2629 binary reads the running-guard → `run_id` → THIS `CronRun`
            // and blocks while its status is non-terminal. Deleting it while ACTIVE
            // + the dual-written guard are still live would make that deref see an
            // ABSENT run, mis-classify it as dead, and run CONCURRENTLY — no-overlap
            // broken. A long-running run whose `max_runtime_ms` exceeds retention
            // would otherwise be deleted by `start_time` past the cutoff while still
            // executing. So a non-terminal run is NEVER retention-eligible; it is
            // the orphan reaper's responsibility (`reap_stale_active_runs`, off the
            // frozen deadline), and only its terminal transition makes it eligible.
            //
            // A terminal run is keyed strictly by `end_time` (always set on a
            // terminal projection — the finalize/reaper/supersede paths all stamp
            // it). `start_time` is NOT a retention fallback: it would re-introduce
            // the "delete a live long run by its start" bug. A terminal run missing
            // `end_time` (should not happen) is treated as not-yet-eligible.
            let retention_eligible =
                run.status.is_terminal() && run.end_time.is_some_and(|end| end < retention_cutoff);
            if retention_eligible {
                store.delete_cron_run_by_key(&mut txn, raw_key).await?;
                deleted = deleted.saturating_add(1);
            }
            // Runs not retention-eligible are untouched — no lock acquired on them.
        }

        Ok(GcBatchResult {
            deleted,
            last_key,
            is_final,
        })
    }
    .await;

    match batch_result {
        Ok(batch) => {
            if batch.deleted > 0 {
                store
                    .assert_database_alive_for_update(&mut txn, db_id)
                    .await?;
                txn.commit().await?;
            } else {
                txn.rollback().await.ok();
            }
            Ok(batch)
        }
        Err(e) => {
            if txn.rollback().await.is_err() {
                if let Some(g) = txn_guard.as_mut() {
                    g.quarantine();
                }
            }
            Err(e)
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::types::{CronRun, CronRunState, CronRunStatus};

    #[test]
    fn gc_database_batch_tracks_its_transaction() {
        let source = include_str!("worker.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("cron/worker.rs must contain #[cfg(test)]");
        let batch_fn = prod_source
            .split("async fn gc_database_batch_inner(")
            .nth(1)
            .expect("gc_database_batch_inner must exist");

        assert!(
            batch_fn.contains("track_worker_txn(txn.start_timestamp().version())"),
            "gc_database_batch_inner must register with the GC safepoint to \
             prevent GC overrun while the batch transaction is active"
        );
    }

    #[test]
    fn gc_uses_point_deletes_not_delete_all_reinsert() {
        // Guard: gc_database_batch_inner must use point-delete (delete_cron_run_by_key)
        // and NOT the old delete-all-reinsert pattern (delete_cron_runs_for_job).
        let source = include_str!("worker.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("cron/worker.rs must contain #[cfg(test)]");
        let batch_fn = prod_source
            .split("async fn gc_database_batch_inner(")
            .nth(1)
            .expect("gc_database_batch_inner must exist");

        assert!(
            batch_fn.contains("delete_cron_run_by_key"),
            "gc_database_batch_inner must use point-deletes (delete_cron_run_by_key) \
             to minimize pessimistic lock acquisitions"
        );
        assert!(
            !batch_fn.contains("delete_cron_runs_for_job"),
            "gc_database_batch_inner must NOT use delete_cron_runs_for_job — \
             that pattern locks all keys including unchanged survivors"
        );
    }

    // ========================================================================
    // CLUSTER I/O test (#[ignore]) for the REAL orphan reaper. Run with:
    //   PD_ENDPOINTS=127.0.0.1:2379 cargo test -p db9-server \
    //     reaper_clears_active_pointer_so_later_minute_is_not_skipped -- --ignored
    // ========================================================================

    /// INVARIANT 2 (no silent skip via the REAL reaper). Claim minute M with the
    /// deadline already in the past (orphaned), then run `reap_stale_active_runs`.
    /// The reaper must (a) finalize CONTROL(M) -> Failed via the fence CAS AND
    /// (b) clear the ACTIVE pointer in the SAME txn, so a subsequent claim of a
    /// LATER minute M+k returns `Claimed` — NOT `BlockedByLiveActive`, NOT
    /// `AlreadyTerminalForMinute`. On the old code the reaper never cleared the
    /// per-minute claim, so M+k was silently dropped; reverting the DEFECT-2 fix
    /// makes this test fail.
    ///
    /// The reaper's accept branch calls `assert_database_alive_for_update`, so the
    /// DB must have a live metadata row — we mint one via `create_database` and
    /// drive all cron keys off the returned `def.id`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn reaper_clears_active_pointer_so_later_minute_is_not_skipped() {
        use crate::cron::types::CronJob;
        use crate::storage::CronClaimOutcome;

        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_cron_reaper_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = crate::worker::config::WorkerConfig {
            enabled: true,
            system_keyspace,
            ..Default::default()
        };
        let store: Arc<TikvStore> = crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
            .await
            .expect("init system store");

        // Mint a live DB metadata row so the reaper's liveness fence passes.
        let db_id = {
            let mut txn = store.begin().await.expect("begin");
            let def = store
                .create_database(&mut txn, "regress_reaper", "admin", false)
                .await
                .expect("create_database")
                .expect("def");
            txn.commit().await.expect("commit create_database");
            def.id
        };

        let job_id = 1i64;
        let m = 28_900_000i64;
        let later = m + 1;
        let job = CronJob {
            job_id,
            schedule: "* * * * *".to_string(),
            command: "SELECT 1".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "regress_reaper".to_string(),
            username: "admin".to_string(),
            active: true,
            jobname: Some("j".to_string()),
            max_runtime_ms: None,
        };

        // `now` drives both the orphan filter and the later claim. Claim M with a
        // deadline strictly in the past relative to `now`.
        let now = now_ms();
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    now - 60_000,
                    now - 1, // deadline already past
                    &job,
                    "regress_reaper".to_string(),
                )
                .await
                .expect("claim M");
            assert!(matches!(outcome, CronClaimOutcome::Claimed { .. }));
            txn.commit().await.expect("commit claim M");
        }

        // Drive the REAL reaper.
        let reaped = reap_stale_active_runs(&store, db_id, now)
            .await
            .expect("reap");
        assert_eq!(reaped, 1, "the orphaned run must be reaped exactly once");

        // ACTIVE pointer must be cleared by the reaper.
        {
            let mut txn = store.begin().await.expect("begin");
            let actives = store.list_cron_active(&mut txn, db_id).await.expect("list");
            assert!(
                actives.is_empty(),
                "the reaper must clear the ACTIVE pointer (DEFECT-2 fix)"
            );
            // CONTROL(M) must be a terminal (Failed) tombstone.
            let c = store
                .read_cron_control(&mut txn, db_id, job_id, m)
                .await
                .expect("read control")
                .expect("control exists");
            assert_eq!(c.state, CronRunState::Failed, "reaped fire must be Failed");
            // The cross-generation no-overlap CARRIER (legacy running-guard the
            // new claim dual-wrote) must ALSO be cleared by the reaper — it funnels
            // through `finalize_cron_run_cas`, which clears the guard fence-matched
            // (design 35 §Symmetric bridge). A reaped run must leave no stale guard
            // an old binary would honor forever.
            let guard = store
                .read_cron_running_guard(&mut txn, db_id, job_id)
                .await
                .expect("read carrier");
            assert!(
                guard.is_none(),
                "the reaper must clear the dual-written carrier (no stale guard left)"
            );
            txn.rollback().await.ok();
        }

        // A LATER minute must now be claimable — the fire is NOT silently skipped.
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    later,
                    now + 1000,
                    now + 600_000,
                    &job,
                    "regress_reaper".to_string(),
                )
                .await
                .expect("claim later");
            assert!(
                matches!(outcome, CronClaimOutcome::Claimed { .. }),
                "a later minute after a reaped fire must be CLAIMED, got {outcome:?}"
            );
            txn.commit().await.expect("commit claim later");
        }
    }

    /// REGRESSION (design 35 §Retention vs orphan recovery — the P1 retention fix).
    /// Retention GC must NEVER delete a NON-TERMINAL (`Starting`/`Running`) CronRun,
    /// even when its `start_time` is far past the retention cutoff. That run is the
    /// cross-generation no-overlap bridge's liveness record: an OLD #2629 binary
    /// derefs the running-guard → `run_id` → THIS `CronRun` and blocks while its
    /// status is non-terminal. Deleting a still-live long run (whose `max_runtime`
    /// exceeds retention) would make that deref see an ABSENT run, mis-classify it
    /// as dead, and run CONCURRENTLY — no-overlap broken. A TERMINAL run past
    /// retention (keyed by `end_time`) IS deleted.
    ///
    /// Pre-fix the predicate was `run.end_time.or(run.start_time) < cutoff`, so a
    /// `Running` run with no `end_time` was deleted by its old `start_time` — this
    /// test plants exactly that run and asserts it survives; reverting the fix
    /// deletes it and fails the survival assertion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_retention_gc_never_deletes_non_terminal_run() {
        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_cron_retention_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = crate::worker::config::WorkerConfig {
            enabled: true,
            system_keyspace,
            ..Default::default()
        };
        let store: Arc<TikvStore> = crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
            .await
            .expect("init system store");

        // Mint a live DB metadata row so the GC commit's liveness fence passes.
        let db_id = {
            let mut txn = store.begin().await.expect("begin");
            let def = store
                .create_database(&mut txn, "regress_retention", "admin", false)
                .await
                .expect("create_database")
                .expect("def");
            txn.commit().await.expect("commit create_database");
            def.id
        };

        let now = now_ms();
        // Retention cutoff is well in the past; BOTH planted runs' timestamps are
        // older than it, so the predicate's TERMINAL gate (not the timestamp) is
        // what decides each run's fate.
        let old_ts = now - 30 * 24 * 3600 * 1000; // ~30 days ago
        let retention_cutoff = now - 7 * 24 * 3600 * 1000; // 7-day retention

        // (a) A long-running NON-TERMINAL run: started 30 days ago, still Running,
        //     no end_time (max_runtime exceeds retention). MUST survive.
        let live_run = CronRun {
            run_id: 1001,
            job_id: 1,
            job_pid: None,
            database: "regress_retention".to_string(),
            username: "admin".to_string(),
            command: "SELECT pg_sleep(1e9)".to_string(),
            status: CronRunStatus::Running,
            return_message: None,
            start_time: Some(old_ts),
            end_time: None,
        };
        // (b) A TERMINAL run finished 30 days ago. MUST be deleted (past retention).
        let dead_run = CronRun {
            run_id: 1002,
            job_id: 1,
            job_pid: None,
            database: "regress_retention".to_string(),
            username: "admin".to_string(),
            command: "SELECT 1".to_string(),
            status: CronRunStatus::Succeeded,
            return_message: None,
            start_time: Some(old_ts),
            end_time: Some(old_ts + 1000),
        };
        {
            let mut txn = store.begin().await.expect("begin");
            store
                .put_cron_run(&mut txn, db_id, &live_run)
                .await
                .expect("plant live run");
            store
                .put_cron_run(&mut txn, db_id, &dead_run)
                .await
                .expect("plant dead run");
            txn.commit().await.expect("commit plants");
        }

        // Run ONE retention batch (the whole history fits in a single page).
        let batch = gc_database_batch(&store, db_id, retention_cutoff, None)
            .await
            .expect("gc batch");
        assert_eq!(
            batch.deleted, 1,
            "exactly the one TERMINAL past-retention run must be deleted"
        );

        // The NON-TERMINAL run survives; the TERMINAL run is gone.
        {
            let mut txn = store.begin().await.expect("begin");
            let (runs, _keys) = store
                .list_cron_runs_batch(&mut txn, db_id, None, 100)
                .await
                .expect("list runs");
            txn.rollback().await.ok();
            let ids: Vec<i64> = runs.iter().map(|r| r.run_id).collect();
            assert!(
                ids.contains(&1001),
                "the non-terminal long-running run MUST survive retention GC \
                 (it is the cross-generation no-overlap liveness record), got {ids:?}"
            );
            assert!(
                !ids.contains(&1002),
                "the terminal past-retention run MUST be deleted, got {ids:?}"
            );
        }
    }

    /// REGRESSION (design 35 §Post-marker straggler fold — the P1 fold-commit fix).
    /// A crashed OLD #2629 binary leaves a live legacy running-guard but never
    /// finishes its run. When the NEW binary claims a later fire it folds that guard
    /// into durable ACTIVE + matching `Running` CONTROL and returns
    /// `BlockedByLiveActiveFolded`, which the engine COMMITS (it is not a plain
    /// block). This test reproduces that committed-fold state, then drives the REAL
    /// orphan reaper once the folded deadline lapses and asserts the orphan IS
    /// reaped: CONTROL → Failed, ACTIVE cleared, carrier cleared. Pre-fix the engine
    /// rolled the fold back, so ACTIVE/CONTROL never became durable and there was
    /// nothing for the reaper to reap — schedule progress was never guaranteed.
    ///
    /// (We commit the fold txn directly here to model the engine's
    /// `BlockedByLiveActiveFolded` → commit decision; the engine-level wiring is
    /// unit-tested in `worker::engine::tests::only_folded_block_requests_commit`.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_committed_fold_is_reapable() {
        use crate::storage::CronClaimOutcome;

        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_cron_foldreap_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = crate::worker::config::WorkerConfig {
            enabled: true,
            system_keyspace,
            ..Default::default()
        };
        let store: Arc<TikvStore> = crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
            .await
            .expect("init system store");

        // Live DB row so the reaper's liveness fence (and the fold commit) pass.
        let db_id = {
            let mut txn = store.begin().await.expect("begin");
            let def = store
                .create_database(&mut txn, "regress_foldreap", "admin", false)
                .await
                .expect("create_database")
                .expect("def");
            txn.commit().await.expect("commit create_database");
            def.id
        };

        let job_id = 7i64;
        let straggler_run_id = 4242i64;
        const MIGRATED_MIN: i64 = 0;

        let job = crate::cron::types::CronJob {
            job_id,
            schedule: "* * * * *".to_string(),
            command: "SELECT 1".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "regress_foldreap".to_string(),
            username: "admin".to_string(),
            active: true,
            jobname: Some("j".to_string()),
            max_runtime_ms: None,
        };

        // The db is already migrated; an OLD binary then crashed mid-run, leaving a
        // live legacy running-guard (its `CronRun` was never finalized).
        {
            let mut txn = store.begin().await.expect("begin");
            store
                .write_cron_migrated_marker(&mut txn, db_id)
                .await
                .expect("write marker");
            store
                .plant_legacy_running_guard(&mut txn, db_id, job_id, straggler_run_id)
                .await
                .expect("plant straggler guard");
            txn.commit().await.expect("commit marker + guard");
        }

        let real_minute = 28_900_600i64;
        let now = now_ms();
        let orphan_window = 300_000i64;
        let deadline_ms = now + orphan_window;

        // The NEW binary claims a later fire: it folds the straggler guard and is
        // BLOCKED-but-FOLDED. The engine COMMITS this (it carries durable writes).
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    real_minute,
                    now,
                    deadline_ms,
                    &job,
                    "regress_foldreap".to_string(),
                )
                .await
                .expect("claim folds straggler");
            assert_eq!(
                outcome,
                CronClaimOutcome::BlockedByLiveActiveFolded,
                "a crashed-old-run straggler fold must report BlockedByLiveActiveFolded \
                 so the engine commits the durable ACTIVE/CONTROL"
            );
            // Model the engine committing a folded block.
            txn.commit().await.expect("COMMIT the fold");
        }

        // The fold is durable: ACTIVE + Running CONTROL exist at the sentinel minute.
        {
            let mut txn = store.begin().await.expect("begin");
            let actives = store.list_cron_active(&mut txn, db_id).await.expect("list");
            assert_eq!(actives.len(), 1, "the committed fold must leave one ACTIVE");
            assert_eq!(actives[0].run_id, straggler_run_id);
            let ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, MIGRATED_MIN)
                .await
                .expect("read control")
                .expect("folded CONTROL must be durable");
            assert_eq!(ctrl.state, CronRunState::Running);
            txn.rollback().await.ok();
        }

        // Once the folded deadline lapses, the REAL reaper must reap the orphan.
        let reap_now = deadline_ms + 1;
        let reaped = reap_stale_active_runs(&store, db_id, reap_now)
            .await
            .expect("reap");
        assert_eq!(
            reaped, 1,
            "the committed-fold orphan must be reapable once its deadline lapses"
        );

        // Post-reap: CONTROL → Failed, ACTIVE cleared, carrier cleared.
        {
            let mut txn = store.begin().await.expect("begin");
            let actives = store.list_cron_active(&mut txn, db_id).await.expect("list");
            assert!(actives.is_empty(), "reaper must clear the folded ACTIVE");
            let ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, MIGRATED_MIN)
                .await
                .expect("read control")
                .expect("control exists");
            assert_eq!(
                ctrl.state,
                CronRunState::Failed,
                "the reaped folded orphan's CONTROL must be Failed"
            );
            let guard = store
                .read_cron_running_guard(&mut txn, db_id, job_id)
                .await
                .expect("read carrier");
            assert!(
                guard.is_none(),
                "the reaper must clear the carrier for the reaped fold"
            );
            txn.rollback().await.ok();
        }
    }

    /// REGRESSION (design 35 §Reaper reconcile — the P1 reaper-clobber fix).
    /// In the mixed-version window an OLD #2629 worker can FINISH after the new
    /// control plane was minted: it finalizes through the legacy path, writing a
    /// REAL terminal `CronRun` (here `Succeeded`, with its return message) and
    /// clearing only the legacy guard — it CANNOT terminalize the new
    /// CONTROL/ACTIVE it does not know about, so those linger `Running`. Later the
    /// NEW reaper sees that ACTIVE deadline expire. Pre-fix it FORCED `Failed` and
    /// `finalize_cron_run_cas` projected that Failed over the owner's real
    /// `Succeeded` — a non-owner terminal write clobbering an already-finalized
    /// result. Post-fix the reaper RECONCILES from the shared `CronRun`: it
    /// PRESERVES the existing terminal status/message/end_time and only cleans up
    /// the stale control plane (CONTROL terminalized consistently, ACTIVE + guard
    /// cleared). Reverting the reconcile (forcing Failed) flips the preserved
    /// `Succeeded` assertion and fails this test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_reaper_preserves_existing_terminal_result_and_cleans_control_plane() {
        use crate::cron::types::CronJob;
        use crate::storage::CronClaimOutcome;

        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_cron_reapreconcile_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = crate::worker::config::WorkerConfig {
            enabled: true,
            system_keyspace,
            ..Default::default()
        };
        let store: Arc<TikvStore> = crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
            .await
            .expect("init system store");

        // Live DB row so the reaper's liveness fence passes.
        let db_id = {
            let mut txn = store.begin().await.expect("begin");
            let def = store
                .create_database(&mut txn, "regress_reapreconcile", "admin", false)
                .await
                .expect("create_database")
                .expect("def");
            txn.commit().await.expect("commit create_database");
            def.id
        };

        let job_id = 11i64;
        let m = 28_900_900i64;
        let later = m + 1;
        let job = CronJob {
            job_id,
            schedule: "* * * * *".to_string(),
            command: "SELECT 1".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "regress_reapreconcile".to_string(),
            username: "admin".to_string(),
            active: true,
            jobname: Some("j".to_string()),
            max_runtime_ms: None,
        };

        // The new control plane is minted (CONTROL+ACTIVE Running, guard dual-written,
        // CronRun Running projected). Deadline already in the past relative to `now`.
        let now = now_ms();
        let run_id = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    now - 60_000,
                    now - 1, // deadline already past
                    &job,
                    "regress_reapreconcile".to_string(),
                )
                .await
                .expect("claim M");
            let run_id = match &outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("expected Claimed, got {other:?}"),
            };
            txn.commit().await.expect("commit claim M");
            run_id
        };

        // Model the OLD #2629 worker FINISHING via the legacy path: it overwrites the
        // shared `CronRun` to a REAL terminal Succeeded result, but cannot touch the
        // new CONTROL/ACTIVE (which stay Running). The user's real outcome now lives
        // in `CronRun` while the control plane is stale-but-fence-matching.
        let owner_message = "1 row updated".to_string();
        {
            let mut txn = store.begin().await.expect("begin");
            let succeeded = CronRun {
                run_id,
                job_id,
                job_pid: None,
                database: "regress_reapreconcile".to_string(),
                username: "admin".to_string(),
                command: "SELECT 1".to_string(),
                status: CronRunStatus::Succeeded,
                return_message: Some(owner_message.clone()),
                start_time: Some(now - 60_000),
                end_time: Some(now - 100),
            };
            store
                .put_cron_run(&mut txn, db_id, &succeeded)
                .await
                .expect("owner legacy-finalize CronRun -> Succeeded");
            txn.commit().await.expect("commit owner finalize");
        }

        // Drive the REAL reaper over the expired ACTIVE.
        let reaped = reap_stale_active_runs(&store, db_id, now)
            .await
            .expect("reap");
        assert_eq!(
            reaped, 1,
            "the stale ACTIVE must be reconciled exactly once"
        );

        // (i) The CronRun's REAL terminal result is PRESERVED — NOT clobbered to Failed.
        {
            let mut txn = store.begin().await.expect("begin");
            let run = store
                .get_cron_run(&mut txn, db_id, run_id)
                .await
                .expect("get run")
                .expect("CronRun exists");
            txn.rollback().await.ok();
            assert_eq!(
                run.status,
                CronRunStatus::Succeeded,
                "the owner's terminal Succeeded result MUST be preserved by the reaper, \
                 not clobbered to Failed (design 35 §Reaper reconcile)"
            );
            assert_eq!(
                run.return_message,
                Some(owner_message),
                "the owner's original return_message MUST be preserved"
            );
            assert_eq!(
                run.end_time,
                Some(now - 100),
                "the owner's original end_time MUST be preserved (not stamped to reap-now)"
            );
        }

        // (ii) The stale control plane IS cleaned up: CONTROL terminalized (consistent
        //      with the preserved Succeeded), ACTIVE + guard cleared (no orphan).
        {
            let mut txn = store.begin().await.expect("begin");
            let actives = store.list_cron_active(&mut txn, db_id).await.expect("list");
            assert!(
                actives.is_empty(),
                "the reaper must clear the stale ACTIVE pointer"
            );
            let c = store
                .read_cron_control(&mut txn, db_id, job_id, m)
                .await
                .expect("read control")
                .expect("control exists");
            assert_eq!(
                c.state,
                CronRunState::Succeeded,
                "CONTROL must be terminalized CONSISTENTLY with the preserved CronRun \
                 (Succeeded), not forced to Failed"
            );
            let guard = store
                .read_cron_running_guard(&mut txn, db_id, job_id)
                .await
                .expect("read carrier");
            assert!(
                guard.is_none(),
                "the reaper must clear the dual-written carrier (no stale guard left)"
            );
            txn.rollback().await.ok();
        }

        // A LATER minute remains claimable — schedule progress is preserved.
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    later,
                    now + 1000,
                    now + 600_000,
                    &job,
                    "regress_reapreconcile".to_string(),
                )
                .await
                .expect("claim later");
            assert!(
                matches!(outcome, CronClaimOutcome::Claimed { .. }),
                "a later minute after a reconciled fire must be CLAIMED, got {outcome:?}"
            );
            txn.commit().await.expect("commit claim later");
        }
    }

    /// REGRESSION companion: a GENUINELY orphaned run (the owner never finalized —
    /// `CronRun` is still non-terminal `Running`) MUST still be reaped to `Failed`,
    /// so schedule progress is preserved for a truly-dead worker. This guards the
    /// other branch of the reconcile predicate: reconcile must not over-correct
    /// into never failing a real orphan.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_reaper_still_fails_genuine_non_terminal_orphan() {
        use crate::cron::types::CronJob;
        use crate::storage::CronClaimOutcome;

        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_cron_orphanfail_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = crate::worker::config::WorkerConfig {
            enabled: true,
            system_keyspace,
            ..Default::default()
        };
        let store: Arc<TikvStore> = crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
            .await
            .expect("init system store");

        let db_id = {
            let mut txn = store.begin().await.expect("begin");
            let def = store
                .create_database(&mut txn, "regress_orphanfail", "admin", false)
                .await
                .expect("create_database")
                .expect("def");
            txn.commit().await.expect("commit create_database");
            def.id
        };

        let job_id = 12i64;
        let m = 28_901_000i64;
        let job = CronJob {
            job_id,
            schedule: "* * * * *".to_string(),
            command: "SELECT 1".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "regress_orphanfail".to_string(),
            username: "admin".to_string(),
            active: true,
            jobname: Some("j".to_string()),
            max_runtime_ms: None,
        };

        // Claim with an already-past deadline; the owner NEVER finalizes — the
        // projected `CronRun` stays `Running` (non-terminal).
        let now = now_ms();
        let run_id = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    now - 60_000,
                    now - 1,
                    &job,
                    "regress_orphanfail".to_string(),
                )
                .await
                .expect("claim M");
            let run_id = match &outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("expected Claimed, got {other:?}"),
            };
            txn.commit().await.expect("commit claim M");
            run_id
        };

        let reaped = reap_stale_active_runs(&store, db_id, now)
            .await
            .expect("reap");
        assert_eq!(reaped, 1, "a genuine orphan must be reaped exactly once");

        // The non-terminal orphan IS finalized to Failed (schedule progress).
        {
            let mut txn = store.begin().await.expect("begin");
            let run = store
                .get_cron_run(&mut txn, db_id, run_id)
                .await
                .expect("get run")
                .expect("CronRun exists");
            let c = store
                .read_cron_control(&mut txn, db_id, job_id, m)
                .await
                .expect("read control")
                .expect("control exists");
            txn.rollback().await.ok();
            assert_eq!(
                run.status,
                CronRunStatus::Failed,
                "a genuinely orphaned non-terminal run MUST be reaped to Failed"
            );
            assert_eq!(
                c.state,
                CronRunState::Failed,
                "CONTROL for a genuine orphan must be Failed"
            );
        }
    }
}
