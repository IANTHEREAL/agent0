use super::*;
use crate::cron::types::{
    CronActiveRun, CronJob, CronJobLegacy, CronRun, CronRunControl, CronRunState, CronRunStatus,
};
use crate::storage::backpressure::tikv_op;

/// Outcome of a single-lifecycle cron claim attempt (design 35). It carries the
/// minted fence + the `CronRun` to project so the caller never re-reads.
#[derive(Debug, Clone, PartialEq)]
pub enum CronClaimOutcome {
    /// Fresh claim — no prior state for this fire/job. `run.run_id` IS the minted
    /// fence (the caller presents it to `finalize_cron_run_cas`).
    Claimed { run: CronRun },
    /// Superseded a stale/expired control or active pointer (the old fence is now
    /// strictly lower, so the previous owner's terminal CAS will be rejected).
    TookOver { run: CronRun },
    /// This exact fire already reached a terminal state — genuine duplicate; the
    /// queue row should be dropped (NOT requeued — that is the next fire's job).
    AlreadyTerminalForMinute,
    /// A live run (this minute, or a different minute of the same job) holds the
    /// job — job-level no-overlap; keep the queue row and retry next tick.
    BlockedByLiveActive,
    /// Same disposition as `BlockedByLiveActive` (job is held; keep the queue row
    /// and retry), but THIS call performed a durable fold/translation first: a
    /// post-marker legacy straggler guard was translated into ACTIVE + matching
    /// `Running` CONTROL (and the carrier re-pointed) in this txn, and that LIVE
    /// active is precisely what now blocks the claim. The caller MUST COMMIT the
    /// txn even though the claim is blocked, so the folded straggler becomes a
    /// durable ACTIVE/CONTROL the orphan reaper can later supersede — otherwise the
    /// fold is rolled back every tick, the orphan is never made durable, and
    /// schedule progress is never guaranteed (design 35 §Post-marker straggler
    /// fold; the fold-commit P1 fix). A plain `BlockedByLiveActive` wrote nothing
    /// and is still rolled back.
    BlockedByLiveActiveFolded,
}

impl CronClaimOutcome {
    /// Whether this outcome reflects a durable write this call performed that MUST
    /// be committed even though the claim did not yield a run to execute. Only the
    /// straggler-fold block path translates legacy state and then blocks; a plain
    /// block (and the short-circuit returns) wrote nothing and must roll back.
    pub fn must_commit_blocked(&self) -> bool {
        matches!(self, Self::BlockedByLiveActiveFolded)
    }
}

/// Pure decision derived from the CONTROL + ACTIVE state that
/// [`TikvStore::claim_or_takeover_cron_run`] has already read under
/// `get_for_update`. Splitting the branch logic from the I/O keeps the
/// fence/no-overlap rules unit-testable without a cluster (the I/O method just
/// performs the writes the decision authorizes).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CronClaimDecision {
    /// Authorize the claim. `took_over == true` when prior CONTROL/ACTIVE state
    /// for this job is being superseded (a stale/expired owner), so the caller
    /// returns `TookOver` rather than `Claimed`.
    ///
    /// `supersede_minute` is set iff this takeover supersedes an EXPIRED ACTIVE
    /// pointer for a DIFFERENT minute than the one being claimed. In that case
    /// `claim_or_takeover_cron_run` overwrites `ACTIVE(job)` to the new minute,
    /// which would STRAND `CONTROL(job, active_minute)` non-terminal: the reaper
    /// drives off ACTIVE (now pointing elsewhere) and never reaches it, and a
    /// still-alive lost-deadline owner could later finalize it via its
    /// stale-but-matching fence. The caller MUST terminalize that superseded
    /// minute's CONTROL in the SAME claim txn so it is neither orphaned nor
    /// finalizable. (A SAME-minute takeover overwrites the same CONTROL key, so
    /// there is nothing to strand — `supersede_minute` is `None`.)
    Proceed {
        took_over: bool,
        supersede_minute: Option<i64>,
    },
    /// Short-circuit with this outcome — no write.
    Return(CronClaimOutcome),
}

/// Decide whether a claim may proceed, given the CONTROL record for this exact
/// fire (`control`, per db,job,minute) and the ACTIVE pointer for the job
/// (`active`, per db,job). The branch ORDER is load-bearing and must match the
/// design-35 state machine exactly:
///   1. CONTROL terminal for this minute    -> `AlreadyTerminalForMinute` (per-fire dedup).
///   2. CONTROL live (deadline in future)   -> `BlockedByLiveActive` (same fire still live; defensive).
///   3. ACTIVE live for a DIFFERENT minute  -> `BlockedByLiveActive` (job-level no-overlap).
///   4. otherwise                           -> `Proceed { took_over = any prior state existed,
///                                              supersede_minute = expired-ACTIVE's minute if it differs }`.
///
/// A terminal CONTROL whose `deadline_ms` is also in the future must still
/// return `AlreadyTerminalForMinute` (step 1 precedes step 2), and a stale
/// CONTROL/ACTIVE (deadline in the past) at step 4 is a takeover, NOT a block —
/// that is the DEFECT-2 no-silent-skip path.
///
/// On the step-4 takeover path the decision ALSO reports, via
/// `Proceed { supersede_minute }`, whether an EXPIRED ACTIVE pointer for a
/// DIFFERENT minute is being superseded: overwriting `ACTIVE(job)` to the new
/// minute would otherwise strand the superseded minute's CONTROL non-terminal
/// (reaper-unreachable + finalizable by its stale fence). The caller terminalizes
/// it in the same txn — keeping the per-fire dedup release atomic with the claim
/// (DEFECT-2 closed for the different-minute supersession too).
pub(crate) fn decide_cron_claim(
    control: Option<&CronRunControl>,
    active: Option<&CronActiveRun>,
    scheduled_min: i64,
    now_ms: i64,
) -> CronClaimDecision {
    if let Some(c) = control {
        if c.state.is_terminal() {
            return CronClaimDecision::Return(CronClaimOutcome::AlreadyTerminalForMinute);
        }
        if c.deadline_ms >= now_ms {
            return CronClaimDecision::Return(CronClaimOutcome::BlockedByLiveActive);
        }
    }
    if let Some(a) = active {
        if a.deadline_ms >= now_ms && a.active_minute != scheduled_min {
            return CronClaimDecision::Return(CronClaimOutcome::BlockedByLiveActive);
        }
    }
    // This is a takeover when prior CONTROL/ACTIVE state exists. If we are
    // superseding an EXPIRED ACTIVE pointer that names a DIFFERENT minute than
    // the one we are claiming, the caller is about to overwrite ACTIVE to the new
    // minute and would strand `CONTROL(job, active_minute)` non-terminal (the
    // reaper drives off ACTIVE and will never reach it; a lost-deadline owner
    // could still finalize it via its stale-but-matching fence). Surface that
    // superseded minute so the caller terminalizes it in the same txn. A
    // same-minute supersession overwrites the same CONTROL key in place — nothing
    // to strand.
    let supersede_minute = active.and_then(|a| {
        if a.active_minute != scheduled_min {
            Some(a.active_minute)
        } else {
            None
        }
    });
    CronClaimDecision::Proceed {
        took_over: control.is_some() || active.is_some(),
        supersede_minute,
    }
}

/// Pure accept-gate for a terminal CAS, given the CONTROL record
/// [`TikvStore::finalize_cron_run_cas`] read under `get_for_update`. Accepts iff
/// the stored CONTROL still carries `fence` AND is non-terminal. Returns false
/// (reject) when CONTROL is missing, a takeover minted a higher fence
/// (`fence_token != fence`, DEFECT-1 fix), or the fire already reached terminal.
///
/// Note the gate is on the FENCE, not the wall-clock lease: a sole owner whose
/// `deadline_ms` has lapsed but whose fence is unchanged still finalizes — the
/// lease is liveness, the fence is authority.
pub(crate) fn cron_finalize_accepts(control: Option<&CronRunControl>, fence: i64) -> bool {
    match control {
        Some(c) => c.fence_token == fence && !c.state.is_terminal(),
        None => false,
    }
}

/// Reconcile the terminal `(CronRunState, CronRun)` a *cleanup* path (the orphan
/// reaper, or a later-minute supersession) should commit for a fire, from the
/// fire's CURRENT shared `CronRun` (design 35 §Reaper reconcile). The cleanup
/// path drives the CONTROL/ACTIVE control-plane lifecycle, but it is NOT the
/// fire's owner — so it must never CLOBBER the owner's real result.
///
/// In the mixed-version window an OLD #2629 worker can FINISH after the new
/// control plane was minted: it finalizes through the legacy path, writing a real
/// terminal `CronRun` (e.g. `Succeeded`, with its return message) and clearing
/// only the legacy guard — it cannot terminalize the new CONTROL/ACTIVE it does
/// not know about. A later cleanup that sees that stale-but-fence-matching
/// control plane must therefore PRESERVE the existing terminal `CronRun` and only
/// terminalize CONTROL consistently with it — not project a forced `Failed` over
/// a `Succeeded` run (a non-owner terminal write admitted after the owner already
/// finalized).
///
/// - existing `CronRun` is ALREADY terminal ⇒ the owner finalized: keep its
///   status/return_message/end_time verbatim; terminal CONTROL state mirrors that
///   status (`Succeeded`/`Failed`/`Cancelled`).
/// - existing `CronRun` is NON-terminal (`Starting`/`Running`), or absent
///   (retention-GC'd in-progress record) ⇒ a genuine orphan the owner never
///   finalized: `Failed` is the correct cleanup outcome, stamped with
///   `failure_message` + `now`.
///
/// `fallback` supplies the synthetic identity (`run_id`/`job_id`) for the absent
/// case and the `failure_message` for the genuine-orphan case.
pub(crate) fn reconcile_cron_terminal(
    existing: Option<CronRun>,
    fallback_run_id: i64,
    fallback_job_id: i64,
    failure_message: &str,
    now_ms: i64,
) -> (CronRunState, CronRun) {
    match existing {
        Some(run) if run.status.is_terminal() => {
            // Owner finalized (possibly via the legacy path). Preserve verbatim;
            // terminalize CONTROL consistently with the recorded outcome.
            let terminal_state = CronRunState::from(run.status.clone());
            (terminal_state, run)
        }
        existing => {
            // Genuine orphan (in-progress or already retention-GC'd) — Failed.
            let mut run = existing.unwrap_or(CronRun {
                run_id: fallback_run_id,
                job_id: fallback_job_id,
                job_pid: None,
                database: String::new(),
                username: String::new(),
                command: String::new(),
                status: CronRunStatus::Failed,
                return_message: None,
                start_time: None,
                end_time: None,
            });
            run.status = CronRunStatus::Failed;
            run.return_message = Some(failure_message.to_string());
            run.end_time = Some(now_ms);
            (CronRunState::Failed, run)
        }
    }
}

/// Whether the new binary DUAL-WRITES the legacy running-guard as the
/// cross-generation no-overlap CARRIER (design 35 §Symmetric bridge). ON by
/// default for the whole rolling-deploy window: an OLD #2629 binary enforces
/// job-level no-overlap ONLY via the legacy running-guard (its
/// `try_claim_cron_run` reads guard → `CronRun.status`), which the new binary
/// otherwise never writes — so without the dual-write the two generations gate
/// no-overlap on DISJOINT keys and an old binary can run a fire concurrently
/// with a new-binary run. The guard is a PURE POINTER (`value = run_id ==
/// fence`); liveness is read from the shared `CronRun` the new binary already
/// projects, so re-introducing the writer imports NO old liveness/wall-clock
/// model. Set `DB9_CRON_LEGACY_GUARD_DUAL_WRITE=off` in the FLEET-version
/// removal release once every node reports the post-#2633 generation; the
/// flag, dual-write, fold, and translate machinery are then deleted together
/// (design 35 §Bridge removal gate).
pub(crate) fn legacy_guard_dual_write_enabled() -> bool {
    match std::env::var("DB9_CRON_LEGACY_GUARD_DUAL_WRITE") {
        Ok(v) => !matches!(
            v.trim().to_lowercase().as_str(),
            "0" | "false" | "f" | "no" | "n" | "off"
        ),
        Err(_) => true,
    }
}

/// Decode a legacy running-guard key+value back into `(job_id, run_id)` for
/// migration translation (design 35 §Migration). Legacy layout (master, removed
/// by this PR):
///   key   = `<guard_prefix> || job_id.to_be_bytes()` (8-byte BE i64 suffix)
///   value = `run_id.to_be_bytes()`                    (8-byte BE i64)
/// Returns `None` for a malformed key/value — a guard the old binary would
/// itself have self-healed (deleted), carrying no live run to preserve.
fn decode_legacy_guard(prefix: &[u8], key: &[u8], value: &[u8]) -> Option<(i64, i64)> {
    let suffix = key.strip_prefix(prefix)?;
    let job_bytes: [u8; 8] = suffix.try_into().ok()?;
    let value_bytes: [u8; 8] = value.try_into().ok()?;
    Some((
        i64::from_be_bytes(job_bytes),
        i64::from_be_bytes(value_bytes),
    ))
}

/// Decode a legacy per-minute claim key back into `(job_id, scheduled_min)` for
/// migration translation (design 35 §Migration). Legacy layout (master, removed
/// by this PR):
///   key = `<claim_prefix> || job_id.to_be_bytes() || b'_' || scheduled_min.to_be_bytes()`
/// (value is the dumb `vec![1]` presence flag — not needed). Returns `None` for
/// a malformed key.
fn decode_legacy_claim(prefix: &[u8], key: &[u8]) -> Option<(i64, i64)> {
    let suffix = key.strip_prefix(prefix)?;
    // 8 (job_id) + 1 (b'_') + 8 (scheduled_min)
    if suffix.len() != 17 || suffix[8] != b'_' {
        return None;
    }
    let job_bytes: [u8; 8] = suffix[..8].try_into().ok()?;
    let min_bytes: [u8; 8] = suffix[9..17].try_into().ok()?;
    Some((i64::from_be_bytes(job_bytes), i64::from_be_bytes(min_bytes)))
}

/// The single source of truth for the FROZEN orphan deadline every cron
/// CONTROL/ACTIVE record is stamped with (design 35 §Lifecycle, §Migration).
///
/// `deadline = now + max(global_orphan_timeout, this job's max_runtime)`.
///
/// SINGLE SOURCE OF TRUTH for "how long ONE legitimate cron run may take", in ms.
///
/// This is EXACTLY the value `claim_and_execute_core` times the run out at
/// (`max_runtime_ms.unwrap_or(cron_job_timeout_ms)`). The frozen orphan deadline
/// below maxes this in, and `worker::gc::effective_cron_orphan_floor_ms` derives
/// the default-job floor from it, so the orphan deadline is `>= now + executor
/// timeout` BY CONSTRUCTION — not by a coincidence of two separately-written
/// formulas. That coincidence WAS the drift class: the deadline floor was written
/// independently of the executor's window, so when one used the bare
/// `orphan_timeout_sec` while the executor ran to `cron_job_timeout_ms`, a still-
/// executing default run was classed expired and taken over mid-flight. The
/// executor (`engine.rs`), the orphan deadline, and the floor (`gc.rs`) now ALL
/// route through this one fn, so the window cannot be edited on one side and
/// silently diverge on another.
pub(crate) fn cron_execution_window_ms(
    job_max_runtime_ms: Option<u64>,
    cron_job_timeout_ms: u64,
) -> u64 {
    job_max_runtime_ms.unwrap_or(cron_job_timeout_ms)
}

/// The orphan window must cover the LONGEST a legitimate run can take. Both the
/// no-overlap gate (`decide_cron_claim`) and the reaper (`reap_stale_active_runs`)
/// drive off `deadline >= now`, so a deadline shorter than the run's real
/// execution window would classify a STILL-LIVE run as expired — reopening the
/// takeover / double-exec window for exactly that job.
///
/// Deadline = `now + max(control floor, this job's execution window)`, where the
/// execution window is taken THROUGH `cron_execution_window_ms` — the same fn the
/// executor times out on — so the deadline is provably >= the executor timeout for
/// EVERY job (default OR explicit `max_runtime_ms`), not just by construction for
/// the default case via the floor.
///
/// CRITICAL: every path that stamps a cron deadline (the fresh per-claim path,
/// the per-claim straggler fold, and the bulk migration translate) MUST route
/// through this one function. Computing the `max()` in two places once let the
/// bulk migration stamp `now + global` while the fold stamped `now + max(...)`,
/// so a migrated long-running job's authority expired prematurely and a new
/// binary could take over and run concurrently with the still-live old run.
pub(crate) fn cron_effective_orphan_deadline_ms(
    now_ms: i64,
    global_orphan_timeout_ms: i64,
    job_max_runtime_ms: Option<u64>,
    cron_job_timeout_ms: u64,
) -> i64 {
    let exec_window_ms = i64::try_from(cron_execution_window_ms(
        job_max_runtime_ms,
        cron_job_timeout_ms,
    ))
    .unwrap_or(i64::MAX);
    let effective_window_ms = global_orphan_timeout_ms.max(exec_window_ms);
    now_ms.saturating_add(effective_window_ms.max(0))
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

    /// Cross-generation liveness probe (design 35 §Cross-generation liveness):
    /// is the run `run_id` still executing? The `CronRun.status` history record is
    /// the ONE liveness signal both an OLD #2629 binary and the new binary share —
    /// the OLD binary's `try_claim_cron_run` derefs the running-guard to this same
    /// record and blocks on a non-terminal status, and the new binary projects it
    /// at claim (`Running`) and finalize (terminal). A run is "live" iff a record
    /// exists AND its status is non-terminal (`Starting`/`Running`). A terminal
    /// record (run finished) OR an ABSENT one (never projected / aged out) is NOT
    /// live — a genuinely dead/abandoned run that MUST remain takeover-eligible so
    /// schedule progress is preserved and completed runs are not blocked forever.
    pub(crate) async fn cron_run_is_live(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        run_id: i64,
    ) -> Result<bool> {
        // "Live" is the exact complement of terminal — defer to the ONE authority
        // (`CronRunStatus::is_terminal`) instead of re-listing the live variants, so
        // a new status added there cannot be silently mis-classified here.
        Ok(matches!(
            self.get_cron_run(txn, db_id, run_id).await?,
            Some(run) if !run.status.is_terminal()
        ))
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

    /// Pessimistic-fence form of [`is_cron_enabled`]: read the cron-enabled marker
    /// under `get_for_update` so the read SERIALIZES with `remove_cron_enabled`'s
    /// delete of that same marker (the `DROP EXTENSION pg_cron` path).
    ///
    /// Mirrors [`database_alive_for_update`] exactly: the plain snapshot read of
    /// `is_cron_enabled` does NOT conflict with a concurrent `remove_cron_enabled`
    /// commit, so a claim/migration that plain-read `enabled == true`, then had a
    /// concurrent DROP commit `remove_cron_enabled` + `delete_all_cron_data`, would
    /// still commit its CONTROL/ACTIVE/CronRun/guard/marker writes — leaving
    /// orphaned control-plane state for a cron that no longer exists (and which the
    /// disabled-DB GC skip never reaps). Taking `get_for_update` here makes the two
    /// txns conflict: at most one commits. Use this — NOT the plain read — as the
    /// authoritative gate in EVERY txn that CREATES or TRANSLATES cron
    /// control-plane authority, in the SAME txn as those writes (single tenant
    /// keyspace → atomic). A `false` result means cron is disabled/dropped: the
    /// caller must NOT write any new control-plane state.
    pub async fn is_cron_enabled_for_update(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<bool> {
        let key = self.key(&encode_cron_enabled_key_v2(db_id));
        Ok(tikv_op!(txn.get_for_update(key).await)?.is_some())
    }

    pub async fn delete_all_cron_data(&self, txn: &mut Transaction, db_id: u64) -> Result<()> {
        let prefixes = [
            encode_cron_job_prefix_v2(db_id),
            encode_cron_run_prefix_v2(db_id),
            // Single-lifecycle control plane (design 35).
            encode_cron_control_prefix_v2(db_id),
            encode_cron_active_prefix_v2(db_id),
            // Legacy keys — swept for one release so a rolling-window leftover is
            // purged on DROP DATABASE / DROP EXTENSION pg_cron.
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
            self.key(&encode_cron_migrated_key_v3(db_id)),
        ];
        for key in seq_keys {
            if tikv_op!(txn.get(key.clone()).await)?.is_some() {
                txn_delete(txn, key).await?;
            }
        }

        Ok(())
    }

    /// Fail-closed migration gate for the single-lifecycle control plane
    /// (design 35 §Migration). Idempotent: if the per-db migration marker is
    /// absent, TRANSLATE the legacy state into the control/active model — never
    /// discard it — then delete the legacy keys and write the marker, all in one
    /// txn. The new claim path calls this BEFORE claiming, so during a rolling
    /// deploy the old (claim-flag + guard) and new (control + active) schemes
    /// never both gate the same fire.
    ///
    /// `now_ms` / `orphan_window_ms` are the SAME inputs a fresh claim uses to
    /// stamp its frozen deadline. Each migrated guard's deadline is computed by the
    /// ONE shared helper `cron_effective_orphan_deadline_ms` —
    /// `now + max(global_orphan_timeout, job.max_runtime)` — after resolving the
    /// guard's own job `max_runtime_ms` from the catalog IN THIS migration txn. A
    /// long-running job (max_runtime >> global) is therefore stamped with the same
    /// effective deadline the per-claim straggler fold uses; stamping `now +
    /// global` here (an earlier divergence) expired such a job's migrated authority
    /// before it could legitimately finish, letting a new binary take over and run
    /// concurrently with the still-live old run. The migrated guard must be stamped
    /// with a LIVE orphan deadline, not a zero deadline: the
    /// state machine reads no-overlap (`decide_cron_claim`) AND reaper eligibility
    /// (`reap_stale_active_runs`) off the SAME `deadline >= now` predicate, so a
    /// `deadline_ms = 0` migrated pointer would be classified as already-expired by
    /// BOTH — enforcing zero no-overlap and being instantly reapable, exactly the
    /// double-exec window translation exists to close. A live deadline gives the
    /// migrated guard the legacy guard's no-overlap authority for the orphan window
    /// and lets the reaper/takeover supersede it only AFTER it lapses (the legacy
    /// guard tracked liveness by `CronRun.status`; an orphan window is the closest
    /// wall-clock equivalent and matches what a fresh claim writes).
    ///
    /// Translation:
    ///
    /// * Legacy running-guard `(db,job) → run_id` is the live job-level NO-OVERLAP
    ///   authority. It becomes an ACTIVE pointer plus a matching `Running` CONTROL
    ///   record (same `fence_token == run_id`, same `active_minute == scheduled_min`,
    ///   both at `deadline_ms = cron_effective_orphan_deadline_ms(now, global,
    ///   job.max_runtime)`). The ACTIVE pointer keeps
    ///   no-overlap enforced for the orphan window — a still-running OLD binary's
    ///   fire blocks a new-binary claim for the same job until the window lapses —
    ///   then the next reaper tick reaps the pair cleanly via the fence CAS
    ///   (CONTROL→Failed "orphan recovery", ACTIVE deleted, history projected).
    ///   Without the matching CONTROL the reaper's fence CAS would find
    ///   `control == None`, reject, and orphan the ACTIVE pointer — so both halves
    ///   are written together with the same deadline.
    /// * Legacy per-minute claim `(db,job,min)` is the live PER-FIRE dedup flag.
    ///   It becomes a terminal (`Failed`) CONTROL tombstone for that minute so the
    ///   next claim short-circuits to `AlreadyTerminalForMinute` (drop the queue
    ///   row — this fire is already accounted for), preserving dedup across the
    ///   upgrade. A terminal record needs no reaper coverage (the accept-gate
    ///   rejects on terminal regardless of deadline), so it keeps `deadline_ms = 0`;
    ///   retention GC ages it out. The guard translation is applied FIRST and a
    ///   claim never overwrites an existing (guard-derived) CONTROL, so the live
    ///   run's `Running` state wins over a same-minute claim tombstone.
    ///
    /// Because live no-overlap + per-fire dedup are carried across the cutover
    /// rather than dropped, a run in flight on an OLD binary is still blocked by
    /// the translated ACTIVE/CONTROL state — no double-exec / no-overlap-violation
    /// window is opened (the discard-and-accept variant did open one).
    pub async fn ensure_cron_control_migrated(
        &self,
        db_id: u64,
        now_ms: i64,
        orphan_window_ms: i64,
        cron_job_timeout_ms: u64,
    ) -> Result<()> {
        let marker_key = self.key(&encode_cron_migrated_key_v3(db_id));
        let mut txn = self.begin().await?;
        let result: Result<bool> = async {
            if tikv_op!(txn.get_for_update(marker_key.clone()).await)?.is_some() {
                return Ok(false); // already migrated
            }

            // Cron-disabled fence (P1 migration fence). `DROP EXTENSION pg_cron`
            // removes the cron-enabled marker AND deletes all cron keys
            // (`delete_all_cron_data`). If that drop commits concurrently while this
            // migration runs, recreating CONTROL/ACTIVE/marker here would resurrect
            // orphaned control-plane state for a cron that no longer exists (and the
            // disabled-DB GC skip would never reap it). Refuse to migrate a disabled
            // cron — there is nothing legitimate to translate.
            //
            // Read the marker under `get_for_update`, NOT a plain snapshot: the
            // plain read does not conflict with `remove_cron_enabled`, so a DROP that
            // committed AFTER this txn's snapshot but BEFORE its commit would slip
            // past a plain check and let the marker/ACTIVE/CONTROL writes below land.
            // The fence makes the migration's enabled-read and the DROP's
            // marker-delete write-write conflict — at most one commits, and on a
            // retry the marker is gone so the migration no-ops. (The claim path takes
            // the same fence in its own write txn; this gate fences the migration's
            // OWN writes.)
            if !self.is_cron_enabled_for_update(&mut txn, db_id).await? {
                return Ok(false); // cron disabled/dropped — nothing to migrate
            }

            // 1) Translate live running-guards → ACTIVE + matching Running CONTROL.
            let guard_prefix = encode_cron_running_guard_prefix_v2(db_id);
            let guard_kv = self
                .scan_prefix_values_bytesafe(&mut txn, &guard_prefix)
                .await?;
            for (legacy_key, value) in guard_kv {
                // The guard run_id is the no-overlap authority; a malformed
                // payload carries no live run to preserve, so just delete it.
                if let Some((job_id, run_id)) =
                    decode_legacy_guard(&guard_prefix, &legacy_key, &value)
                {
                    // The migrated guard inherits the SAME frozen orphan deadline a
                    // fresh claim (and the per-claim straggler fold) would write —
                    // `now + max(global_orphan_timeout, job.max_runtime)` via the
                    // single shared helper — so it stays a LIVE no-overlap authority
                    // for the job's full legitimate runtime rather than expiring at
                    // `now + global`. A long-running job (max_runtime >> global) on
                    // an OLD binary is still in flight well past the global window;
                    // stamping `now + global` here (the per-claim fold stamps the
                    // longer effective deadline) would let a new-binary claim treat
                    // the migrated authority as expired and take over — running
                    // CONCURRENTLY with the still-live old run. We resolve the job's
                    // own `max_runtime_ms` in this same migration txn so both
                    // migration paths compute ONE deadline. A guard with no surviving
                    // catalog job (dropped/altered away) has no live runtime to honor,
                    // so it falls back to the global floor.
                    let job_max_runtime_ms = self
                        .get_cron_job(&mut txn, db_id, job_id)
                        .await?
                        .and_then(|j| j.max_runtime_ms);
                    let migrated_deadline_ms = cron_effective_orphan_deadline_ms(
                        now_ms,
                        orphan_window_ms,
                        job_max_runtime_ms,
                        cron_job_timeout_ms,
                    );
                    self.translate_legacy_guard(
                        &mut txn,
                        db_id,
                        job_id,
                        run_id,
                        migrated_deadline_ms,
                    )
                    .await?;
                    // RE-POINT, not delete, the carrier (design 35 §Symmetric
                    // bridge): `translate_legacy_guard` keeps `fence == run_id`, so
                    // the guard still names this run. Keeping it present means an
                    // OLD binary that wins a LATER fire of this job post-marker
                    // still serializes against the in-flight run via the carrier
                    // (the migration must not open a no-overlap gap by erasing it).
                    // With the dual-write flag off (fully-upgraded fleet) no old
                    // reader remains, so the carrier is dropped.
                    if legacy_guard_dual_write_enabled() {
                        self.put_cron_running_guard(&mut txn, db_id, job_id, run_id)
                            .await?;
                    } else {
                        txn_delete(&mut txn, legacy_key).await?;
                    }
                } else {
                    // Malformed guard: no live run to carry — delete it.
                    txn_delete(&mut txn, legacy_key).await?;
                }
            }

            // 2) Translate live per-minute claims → terminal CONTROL tombstones,
            //    skipping any minute a guard translation already owns.
            let claim_prefix = encode_cron_claim_prefix_v2(db_id);
            let claim_keys = self
                .scan_prefix_keys_bytesafe(&mut txn, &claim_prefix)
                .await?;
            for legacy_key in claim_keys {
                if let Some((job_id, scheduled_min)) =
                    decode_legacy_claim(&claim_prefix, &legacy_key)
                {
                    self.translate_legacy_claim(&mut txn, db_id, job_id, scheduled_min)
                        .await?;
                }
                txn_delete(&mut txn, legacy_key).await?;
            }

            txn_put(&mut txn, marker_key, vec![1u8]).await?;

            // DB-liveness fence (P2 migration fence), the SAME primitive the claim
            // and GC commit paths use. Take `get_for_update` on the tenant DB
            // metadata row in THIS migration txn so a concurrent DROP DATABASE (or a
            // DROP EXTENSION pg_cron that also tore down this db) conflicts and this
            // migration aborts rather than committing CONTROL/ACTIVE/marker into a
            // destroyed key range. With the cron-disabled check above this closes
            // both "cron dropped" and "db dropped" races against the migration's own
            // writes.
            //
            // A dropped DB is NOT an error — it is a clean not-actionable no-op,
            // exactly like the cron-disabled gate above: there is nothing legitimate
            // to migrate for a database whose metadata row is already gone. Use the
            // `Ok(false)`-on-dropped form (which still takes `get_for_update`, so the
            // concurrent-DROP race fence is preserved) and roll the whole txn back
            // (marker put included). Using the `assert_` Err form here would turn an
            // orphaned-cron skip — a worker draining a next-fire row for a DB dropped
            // long ago — into a propagated error that fails `claim_and_execute_core`
            // and leaks the worker claim instead of releasing it and reaping the row.
            if !self.database_alive_for_update(&mut txn, db_id).await? {
                return Ok(false); // db dropped — nothing to migrate
            }
            Ok(true)
        }
        .await;
        match result {
            Ok(true) => {
                txn.commit().await?;
                Ok(())
            }
            Ok(false) => {
                txn.rollback().await.ok();
                Ok(())
            }
            Err(e) => {
                txn.rollback().await.ok();
                Err(e)
            }
        }
    }

    /// Translate one legacy running-guard into the control/active model inside the
    /// migration txn: an ACTIVE pointer + a matching `Running` CONTROL record
    /// sharing `fence_token == run_id` and `active_minute == scheduled_min`, both
    /// stamped with the LIVE orphan deadline `deadline_ms` (design 35 §Migration).
    /// A live deadline (not `0`) is required for correctness: the no-overlap gate
    /// (`decide_cron_claim`) and the reaper (`reap_stale_active_runs`) read the
    /// SAME `deadline >= now` predicate, so a `deadline_ms = 0` pointer would be
    /// classed as expired by both — enforcing zero no-overlap and being instantly
    /// reapable, reopening the double-exec window. With a live deadline the
    /// migrated pair holds the legacy guard's no-overlap authority for the orphan
    /// window, then is reaped/superseded once it lapses. Keyed at the sentinel
    /// minute `0`: the guard carries no minute, only the no-overlap authority, and
    /// per-minute dedup is reconstructed separately from the claim keys; real fires
    /// use epoch-minute keys (~28.9M), so `0` never collides with a live claim.
    async fn translate_legacy_guard(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        run_id: i64,
        deadline_ms: i64,
    ) -> Result<()> {
        const MIGRATED_MIN: i64 = 0;

        let control = CronRunControl {
            state: CronRunState::Running,
            fence_token: run_id,
            run_id,
            scheduled_min: MIGRATED_MIN,
            started_ms: 0,
            deadline_ms,
            finalize_seq: 0,
        };
        let control_key = self.key(&encode_cron_control_key_v2(db_id, job_id, MIGRATED_MIN));
        txn_put(
            txn,
            control_key,
            bincode::serialize(&control).context("Failed to serialize migrated cron control")?,
        )
        .await?;

        let active = CronActiveRun {
            job_id,
            active_minute: MIGRATED_MIN,
            run_id,
            fence_token: run_id,
            deadline_ms,
        };
        let active_key = self.key(&encode_cron_active_key_v2(db_id, job_id));
        txn_put(
            txn,
            active_key,
            bincode::serialize(&active).context("Failed to serialize migrated cron active")?,
        )
        .await?;
        Ok(())
    }

    /// Translate one legacy per-minute claim flag into a terminal (`Failed`)
    /// CONTROL tombstone for that `(job,minute)` so dedup survives the upgrade
    /// (the next claim short-circuits to `AlreadyTerminalForMinute`). Never
    /// overwrites an existing CONTROL: a guard translation (the live `Running`
    /// run) takes precedence over a same-minute claim tombstone.
    async fn translate_legacy_claim(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        scheduled_min: i64,
    ) -> Result<()> {
        let control_key = self.key(&encode_cron_control_key_v2(db_id, job_id, scheduled_min));
        if tikv_op!(txn.get_for_update(control_key.clone()).await)?.is_some() {
            return Ok(()); // guard translation already owns this minute
        }
        let control = CronRunControl {
            state: CronRunState::Failed,
            // No run_id was associated with a bare claim flag; the fence is unused
            // for a terminal tombstone (the accept-gate rejects any finalize once
            // terminal) — 0 keeps it inert.
            fence_token: 0,
            run_id: 0,
            scheduled_min,
            started_ms: 0,
            deadline_ms: 0,
            finalize_seq: 0,
        };
        txn_put(
            txn,
            control_key,
            bincode::serialize(&control)
                .context("Failed to serialize migrated cron claim tombstone")?,
        )
        .await?;
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

    // ========================================================================
    // Single-lifecycle, fence-token cron control plane (design 35).
    //
    // Two tenant keys, written ONLY inside the CAS helpers below so neither can
    // be orphaned:
    //   CONTROL  (per db,job,minute) — per-fire dedup + state + fence token.
    //   ACTIVE   (per db,job)        — job-level no-overlap pointer.
    // `fence_token == run_id`, minted in-txn; every terminal/takeover transition
    // is gated on "my fence >= stored fence", so a worker that lost its lease
    // cannot commit terminal state (DEFECT 1) and no terminal writer can leave a
    // dangling dedup flag (DEFECT 2). The system-store WorkerClaim+lease remains
    // the distribution gate for all task types and is NOT the cron authority.
    // ========================================================================

    /// Mint the next per-db cron `run_id` (== fence token) INSIDE the caller's
    /// pessimistic txn, so the fence and the control-record write are one atomic
    /// unit. Ids may gap on rollback — only monotonicity matters.
    pub async fn bump_next_cron_run_id_in_txn(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<i64> {
        let key = self.key(&encode_next_cron_run_id_key_v2(db_id));
        let next_val = match tikv_op!(txn.get_for_update(key.clone()).await)? {
            Some(data) => {
                let id = i64::from_be_bytes(
                    data.as_slice()
                        .try_into()
                        .map_err(|_| anyhow!("Invalid cron run ID format"))?,
                );
                id.checked_add(1)
                    .ok_or_else(|| anyhow!("Cron run ID overflow"))?
            }
            None => 1,
        };
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }

    /// Dual-write the legacy running-guard as the cross-generation no-overlap
    /// CARRIER (design 35 §Symmetric bridge), byte-identical to what the removed
    /// #2629 writer produced (`value = run_id.to_be_bytes()`, `run_id == fence`).
    /// Called INSIDE the claim/fold txn that writes ACTIVE/CONTROL so the carrier
    /// commits atomically with the fence state and can never be orphaned. An OLD
    /// binary's `try_claim_cron_run` (a8b3861b) reads this guard → derefs `run_id`
    /// → reads the shared `CronRun.status` the new binary projects, so it blocks
    /// on a live new-binary run with ZERO old-binary change and imports no old
    /// liveness model (the guard carries none of its own). No-op when the
    /// dual-write flag is off (the FLEET-version removal release).
    async fn put_cron_running_guard(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        run_id: i64,
    ) -> Result<()> {
        if !legacy_guard_dual_write_enabled() {
            return Ok(());
        }
        let key = self.key(&encode_cron_running_guard_key_v2(db_id, job_id));
        txn_put(txn, key, run_id.to_be_bytes().to_vec()).await?;
        Ok(())
    }

    /// Clear the legacy running-guard CARRIER for `(db,job)` IFF it still points
    /// at `expected_run_id` (point-get + compare, mirroring the removed #2629
    /// `clear_cron_running_guard` at a8b3861b). Fence-matched so a guard a LATER
    /// generation re-pointed at a higher fence is never clobbered. Called inside
    /// every terminal/supersede txn for the run that owned the carrier, so a
    /// recovered/superseded run leaves no stale guard an old binary would honor
    /// forever. A malformed (non-8-byte) guard is self-healed (deleted), matching
    /// the old binary's own behavior. Runs regardless of the dual-write flag so a
    /// guard written before the flag was flipped off is still cleaned up.
    async fn clear_cron_running_guard_if(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        expected_run_id: i64,
    ) -> Result<()> {
        let key = self.key(&encode_cron_running_guard_key_v2(db_id, job_id));
        if let Some(data) = tikv_op!(txn.get_for_update(key.clone()).await)? {
            let Ok(bytes) = <[u8; 8]>::try_from(data.as_slice()) else {
                txn_delete(txn, key).await?;
                return Ok(());
            };
            if i64::from_be_bytes(bytes) == expected_run_id {
                txn_delete(txn, key).await?;
            }
        }
        Ok(())
    }

    /// List all ACTIVE-RUN pointers for a db (one per running job) — drives the
    /// orphan reaper without scanning run history. Byte-safe (values are small,
    /// fixed-shape).
    pub async fn list_cron_active(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<CronActiveRun>> {
        let prefix = encode_cron_active_prefix_v2(db_id);
        let pairs = self.scan_prefix_values_bytesafe(txn, &prefix).await?;
        let mut out = Vec::with_capacity(pairs.len());
        for (_k, val) in pairs {
            out.push(
                bincode::deserialize(val.as_slice())
                    .context("Failed to deserialize cron active-run pointer")?,
            );
        }
        Ok(out)
    }

    /// CLAIM or TAKEOVER a cron fire (single pessimistic txn). Takes `get_for_update`
    /// locks on the CONTROL and ACTIVE keys so concurrent claimers serialize and the
    /// loser aborts. `deadline_ms` is the orphan deadline (>> the system lease) past
    /// which a stale run may be superseded — pass `now_ms + effective_orphan_window`.
    pub async fn claim_or_takeover_cron_run(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        scheduled_min: i64,
        now_ms: i64,
        deadline_ms: i64,
        job: &CronJob,
        database: String,
    ) -> Result<CronClaimOutcome> {
        let control_key = self.key(&encode_cron_control_key_v2(db_id, job_id, scheduled_min));
        let active_key = self.key(&encode_cron_active_key_v2(db_id, job_id));

        let control: Option<CronRunControl> =
            match tikv_op!(txn.get_for_update(control_key.clone()).await)? {
                Some(d) => Some(
                    bincode::deserialize(d.as_slice())
                        .context("Failed to deserialize cron control record")?,
                ),
                None => None,
            };
        let mut active: Option<CronActiveRun> =
            match tikv_op!(txn.get_for_update(active_key.clone()).await)? {
                Some(d) => Some(
                    bincode::deserialize(d.as_slice())
                        .context("Failed to deserialize cron active-run pointer")?,
                ),
                None => None,
            };

        // Rolling-deploy straggler fold (design 35 §Migration, §Post-marker
        // straggler fold). The one-shot `ensure_cron_control_migrated` only
        // translates the legacy state present at the marker instant; it cannot cover
        // a guard an OLD #2629 binary writes AFTER the marker (the system-store
        // distribution claim is per-FIRE, so it does not serialize old vs new at job
        // granularity). A legacy running-guard is the live job-level no-overlap
        // authority an old binary holds, so it MUST be folded whenever there is NO
        // LIVE active authority for this job — i.e. when `ACTIVE(job)` is absent OR
        // present-but-EXPIRED (`deadline_ms < now_ms`). Gating only on
        // `active.is_none()` reopened the overlap: an EXPIRED ACTIVE left over from
        // an earlier migrated/finished fire would otherwise let `decide_cron_claim`
        // treat that stale pointer as takeover-eligible while a FRESH old-binary
        // guard sits un-folded — the new binary would overwrite ACTIVE and run
        // CONCURRENTLY with the still-in-flight old-binary run (job-level overlap /
        // double-run). An expired ACTIVE is precisely NOT live authority, so it must
        // not suppress the fold.
        //
        // When folded, translate the guard into ACTIVE + matching Running CONTROL
        // (sentinel minute 0) with this claim's frozen orphan deadline IN THIS TXN
        // (same `translate_legacy_guard` the bulk migration uses), delete the legacy
        // key, and re-read ACTIVE so the normal no-overlap decision below treats the
        // folded guard as the LIVE authority it is. Job-level no-overlap is thus
        // enforced against a post-marker straggler for the orphan window, after which
        // the reaper supersedes it cleanly. The legacy guard key + decoders are kept
        // for exactly this window; both are removed in the follow-up release once the
        // fleet is upgraded.
        // `folded` records whether this call performed a durable straggler fold
        // (translated a post-marker legacy guard into ACTIVE + matching Running
        // CONTROL + re-pointed carrier). When it did and `decide_cron_claim` then
        // blocks on the resulting LIVE active, the outcome must signal
        // `BlockedByLiveActiveFolded` so the production caller COMMITS the fold
        // (the orphan becomes durable ACTIVE/CONTROL the reaper can supersede)
        // instead of rolling it back every tick (the fold-commit P1 fix). A plain
        // block (no fold) wrote nothing and is still rolled back.
        let mut folded = false;
        let active_is_live = active.as_ref().is_some_and(|a| a.deadline_ms >= now_ms);
        if !active_is_live {
            let guard_key = self.key(&encode_cron_running_guard_key_v2(db_id, job_id));
            if let Some(value) = tikv_op!(txn.get_for_update(guard_key.clone()).await)? {
                let guard_prefix = encode_cron_running_guard_prefix_v2(db_id);
                let stripped_key = encode_cron_running_guard_key_v2(db_id, job_id);
                if let Some((_job, run_id)) =
                    decode_legacy_guard(&guard_prefix, &stripped_key, &value)
                {
                    // Idempotency (design 35 §Symmetric bridge). The carrier is now
                    // RE-POINTED (not deleted) when folded, so on a LATER claim the
                    // same guard is still present. If the current (expired) ACTIVE
                    // already names this guard's run — i.e. a prior claim already
                    // folded it — DO NOT re-translate it into a fresh LIVE ACTIVE:
                    // that would resurrect the folded run every tick and block the
                    // job forever. Skip the fold and let `decide_cron_claim` take
                    // over the expired ACTIVE normally; the supersede/finalize path
                    // then clears the carrier (fence-matched). Re-translation only
                    // happens for a guard whose run is NOT yet represented by ACTIVE
                    // (a genuinely un-folded post-marker straggler).
                    let already_folded = active.as_ref().is_some_and(|a| a.run_id == run_id);
                    if already_folded {
                        // Defense-in-depth (design 35 §Cross-generation liveness).
                        // The expired ACTIVE already names this guard's run; option
                        // (a) keeps its deadline at the FULL effective runtime
                        // (`now + max(global, max_runtime)`), so a still-live long
                        // run normally never reaches here expired. As a backstop —
                        // e.g. a clock skew, or a deadline that lapsed despite the
                        // run still executing — consult the ONE liveness signal both
                        // generations share: the run's `CronRun.status`. The OLD
                        // #2629 binary's `try_claim_cron_run` itself derefs the guard
                        // to this same `CronRun` and blocks on a non-terminal status,
                        // so when the underlying run is still `Starting`/`Running` we
                        // must NOT fall through and mint a fresh concurrent run — we
                        // BLOCK (the queue row is kept; the next tick retries) until
                        // the run is genuinely done. A terminal CronRun (the run
                        // finished) OR an ABSENT one (no live run to honor — never
                        // projected, or aged out) is a genuinely DEAD/abandoned run:
                        // we MUST still allow takeover so the reaper/schedule
                        // progress is preserved and a completed run is not blocked on
                        // forever. So this guards against double-exec WITHOUT
                        // resurrecting dead runs.
                        if self.cron_run_is_live(txn, db_id, run_id).await? {
                            return Ok(CronClaimOutcome::BlockedByLiveActive);
                        }
                        // fall through to decide_cron_claim with the expired ACTIVE
                    } else {
                        // The fold overwrites `ACTIVE(job)` to the sentinel minute 0. If
                        // an EXPIRED ACTIVE for a DIFFERENT minute is being displaced, its
                        // CONTROL would be stranded non-terminal (the reaper drives off
                        // ACTIVE, which will no longer name it; a lost-deadline owner could
                        // still finalize it via its stale-but-matching fence). Terminalize
                        // that superseded minute's CONTROL FIRST, in this same txn — the
                        // identical hazard `decide_cron_claim` handles via
                        // `supersede_minute`. (Absent ACTIVE, or an expired ACTIVE already
                        // at the sentinel minute, strands nothing.)
                        if let Some(stale) = active.as_ref() {
                            const MIGRATED_MIN: i64 = 0;
                            if stale.active_minute != MIGRATED_MIN {
                                self.terminalize_superseded_cron_control(
                                    txn,
                                    db_id,
                                    job_id,
                                    stale.active_minute,
                                    now_ms,
                                )
                                .await?;
                            }
                        }
                        // Translate the straggler guard into ACTIVE + matching Running
                        // CONTROL (sentinel minute 0) with this claim's live deadline.
                        // `translate_legacy_guard` keeps `fence == run_id`, so the
                        // carrier still names this run. This is a DURABLE write that
                        // the resulting LIVE active will block on below, so the caller
                        // must COMMIT it (see `folded`/`BlockedByLiveActiveFolded`).
                        folded = true;
                        self.translate_legacy_guard(txn, db_id, job_id, run_id, deadline_ms)
                            .await?;
                        // RE-POINT the carrier rather than delete-then-recreate
                        // (design 35 §Symmetric bridge). Deleting the guard here would
                        // leave an instant — between this delete and any later
                        // re-write — where an OLD binary's `try_claim_cron_run` sees NO
                        // guard and wrongly proceeds, double-running the very straggler
                        // we just folded. Re-pointing it at `run_id` (== this fold's
                        // fence, which `translate_legacy_guard` preserved) keeps the
                        // carrier CONTINUOUS so the cross-generation no-overlap never
                        // lapses. With the dual-write flag off (fleet fully upgraded, no
                        // old reader) the carrier is no longer needed and is deleted.
                        if legacy_guard_dual_write_enabled() {
                            self.put_cron_running_guard(txn, db_id, job_id, run_id)
                                .await?;
                        } else {
                            txn_delete(txn, guard_key).await?;
                        }
                        active = match tikv_op!(txn.get_for_update(active_key.clone()).await)? {
                            Some(d) => Some(
                                bincode::deserialize(d.as_slice())
                                    .context("Failed to deserialize cron active-run pointer")?,
                            ),
                            None => None,
                        };
                    }
                } else {
                    // Malformed guard: no live run to preserve (an old binary would
                    // itself self-heal it). Delete so it cannot resurface.
                    txn_delete(txn, guard_key).await?;
                }
            }
        }

        // Evaluate the design-35 state machine over the state just read. The
        // branch logic lives in `decide_cron_claim` (pure, unit-tested without a
        // cluster); this method only performs the writes it authorizes.
        let (took_over, supersede_minute) =
            match decide_cron_claim(control.as_ref(), active.as_ref(), scheduled_min, now_ms) {
                // A short-circuit block AFTER a durable fold (the straggler guard was
                // translated into the LIVE active that now blocks this claim) must be
                // reported as `BlockedByLiveActiveFolded` so the caller COMMITS the
                // fold rather than rolling it back; otherwise the orphan never becomes
                // durable ACTIVE/CONTROL and the reaper can never supersede it
                // (design 35 §Post-marker straggler fold). A short-circuit block that
                // did NOT fold wrote nothing and stays `BlockedByLiveActive` (rolled
                // back). `AlreadyTerminalForMinute` cannot co-occur with a fold (the
                // fold writes a non-terminal Running CONTROL at the sentinel minute,
                // not the claimed minute), so only the block outcome is upgraded.
                CronClaimDecision::Return(CronClaimOutcome::BlockedByLiveActive) if folded => {
                    return Ok(CronClaimOutcome::BlockedByLiveActiveFolded);
                }
                CronClaimDecision::Return(outcome) => return Ok(outcome),
                CronClaimDecision::Proceed {
                    took_over,
                    supersede_minute,
                } => (took_over, supersede_minute),
            };

        // Different-minute takeover: we are about to overwrite `ACTIVE(job)` from
        // the expired `active_minute` to `scheduled_min`. Terminalize the
        // superseded minute's CONTROL in THIS txn first, so it is neither orphaned
        // (the reaper drives off ACTIVE, which will no longer name it) nor later
        // finalizable by its lost-deadline owner's stale-but-matching fence. Reuse
        // the SAME fence-CAS gate every terminal transition uses
        // (`cron_finalize_accepts`): flip it to `Failed` presenting ITS OWN stored
        // fence (this authoritative takeover owns the job via a fresh higher fence
        // minted below; the superseded minute's own fence is the key the
        // accept-gate matches). A control that is already terminal, gone, or
        // re-fenced is left untouched — the gate rejects and we proceed. Single
        // keyspace ⇒ this terminalization commits atomically with the new
        // CONTROL/ACTIVE writes.
        if let Some(superseded_min) = supersede_minute {
            self.terminalize_superseded_cron_control(txn, db_id, job_id, superseded_min, now_ms)
                .await?;
        }

        // Fresh, or supersede a stale/expired control/active: mint a higher
        // fence and take ownership of BOTH keys atomically.
        let fence = self.bump_next_cron_run_id_in_txn(txn, db_id).await?;

        let new_control = CronRunControl {
            state: CronRunState::Running,
            fence_token: fence,
            run_id: fence,
            scheduled_min,
            started_ms: now_ms,
            deadline_ms,
            finalize_seq: 0,
        };
        txn_put(
            txn,
            control_key,
            bincode::serialize(&new_control).context("Failed to serialize cron control record")?,
        )
        .await?;

        let new_active = CronActiveRun {
            job_id,
            active_minute: scheduled_min,
            run_id: fence,
            fence_token: fence,
            deadline_ms,
        };
        txn_put(
            txn,
            active_key,
            bincode::serialize(&new_active)
                .context("Failed to serialize cron active-run pointer")?,
        )
        .await?;

        // Dual-write the legacy running-guard as the cross-generation no-overlap
        // CARRIER, in THIS same pessimistic txn (design 35 §Symmetric bridge).
        // The new binary's ACTIVE key answers no-overlap for new binaries; an OLD
        // #2629 binary cannot read it and gates no-overlap ONLY on this guard.
        // Writing it here (value = fence == run_id) makes the bridge SYMMETRIC:
        // the old binary's `try_claim_cron_run` derefs the guard to the shared
        // `CronRun` projected below and blocks on this live run. Atomic with
        // CONTROL/ACTIVE (single keyspace) ⇒ on rollback no guard, on commit
        // guard.run_id == ACTIVE.run_id == CONTROL.fence_token == fence.
        self.put_cron_running_guard(txn, db_id, job_id, fence)
            .await?;

        let run = CronRun {
            run_id: fence,
            job_id,
            job_pid: None,
            database,
            username: job.username.clone(),
            command: job.command.clone(),
            status: CronRunStatus::Running,
            return_message: None,
            start_time: Some(now_ms),
            end_time: None,
        };
        // Project the in-progress history record (parity with the old claim path)
        // so `cron.job_run_details` shows the running fire AND the orphan reaper
        // has a record to flip to Failed via `finalize_cron_run_cas`.
        self.put_cron_run(txn, db_id, &run).await?;
        if took_over {
            Ok(CronClaimOutcome::TookOver { run })
        } else {
            Ok(CronClaimOutcome::Claimed { run })
        }
    }

    /// Commit a TERMINAL transition for a fire, gated on the fence. Used by BOTH
    /// the owner's finalize and the orphan reaper (the reaper presents the stale
    /// run's fence read from the ACTIVE pointer and `terminal_state = Failed`).
    ///
    /// Accepts iff the stored CONTROL record still carries `fence` and is
    /// non-terminal. On reject (a takeover minted a higher fence, or already
    /// terminal) returns `false` — the caller logs and returns `Ok(())`; the
    /// takeover owner is authoritative (DEFECT 1 fix). On accept it writes the
    /// terminal CONTROL record, clears the ACTIVE pointer iff it still points at
    /// this run, and projects the `CronRun` history record — all in `txn`
    /// (DEFECT 2 fix: terminal state and dedup release are one write).
    pub async fn finalize_cron_run_cas(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        scheduled_min: i64,
        fence: i64,
        terminal_state: CronRunState,
        run: &CronRun,
    ) -> Result<bool> {
        debug_assert!(terminal_state.is_terminal());
        let control_key = self.key(&encode_cron_control_key_v2(db_id, job_id, scheduled_min));
        let active_key = self.key(&encode_cron_active_key_v2(db_id, job_id));

        let control: Option<CronRunControl> =
            match tikv_op!(txn.get_for_update(control_key.clone()).await)? {
                Some(d) => Some(
                    bincode::deserialize(d.as_slice())
                        .context("Failed to deserialize cron control record")?,
                ),
                None => None,
            };
        // Accept gate is the FENCE, not the wall-clock lease (see
        // `cron_finalize_accepts`): reject iff CONTROL is gone, a takeover
        // minted a higher fence (DEFECT-1), or the fire is already terminal.
        if !cron_finalize_accepts(control.as_ref(), fence) {
            return Ok(false);
        }
        let c = control.expect("cron_finalize_accepts guarantees Some when it returns true");

        let new_control = CronRunControl {
            state: terminal_state,
            fence_token: c.fence_token,
            run_id: c.run_id,
            scheduled_min,
            started_ms: c.started_ms,
            deadline_ms: c.deadline_ms,
            finalize_seq: c.finalize_seq.saturating_add(1),
        };
        txn_put(
            txn,
            control_key,
            bincode::serialize(&new_control).context("Failed to serialize cron control record")?,
        )
        .await?;

        // Clear the no-overlap pointer iff it still points at this run.
        if let Some(d) = tikv_op!(txn.get_for_update(active_key.clone()).await)? {
            let a: CronActiveRun = bincode::deserialize(d.as_slice())
                .context("Failed to deserialize cron active-run pointer")?;
            if a.run_id == fence {
                txn_delete(txn, active_key).await?;
            }
        }

        // Clear the cross-generation no-overlap CARRIER (legacy running-guard)
        // IFF it still points at this fence (design 35 §Symmetric bridge), atomic
        // with the terminal CONTROL write. Without this a terminal/reaped run
        // would leave a stale guard an OLD binary's `try_claim_cron_run` honors
        // FOREVER (it derefs the guard to a now-terminal `CronRun` and self-heals
        // only on a terminal status — but a guard re-pointed at a higher fence by
        // a later generation must NOT be clobbered, hence the fence match). The
        // reaper funnels through here, so orphan recovery clears the carrier too.
        self.clear_cron_running_guard_if(txn, db_id, job_id, fence)
            .await?;

        // Project the user-visible history record in the SAME txn.
        self.put_cron_run(txn, db_id, run).await?;
        Ok(true)
    }

    /// Terminalize a DIFFERENT-minute CONTROL record being superseded by a
    /// takeover, inside the takeover's own claim txn. When
    /// `claim_or_takeover_cron_run` takes over an expired `ACTIVE(job)` that names
    /// minute `superseded_min` while claiming a different minute, it overwrites
    /// `ACTIVE(job)` to the new minute. Without this step `CONTROL(job,
    /// superseded_min)` would be stranded non-terminal: the reaper drives off the
    /// ACTIVE pointer (which no longer names it) so it is never reaped, and a
    /// still-alive owner whose deadline lapsed could later finalize it via its
    /// stale-but-matching fence — a non-owner terminal write admitted.
    ///
    /// This flips that CONTROL to `Failed` through the SAME fence gate every
    /// terminal transition uses (`cron_finalize_accepts`): it presents the
    /// CONTROL's OWN stored fence, so the gate accepts iff the record is still
    /// present, non-terminal, and unchanged (not re-fenced by a concurrent
    /// claimer). It writes ONLY that minute's terminal CONTROL plus a Failed
    /// history projection — it deliberately does NOT touch `ACTIVE(job)` (the
    /// caller is overwriting it to the new minute in the same txn). Already
    /// terminal / gone / re-fenced ⇒ the gate rejects and this is a no-op; the
    /// claim proceeds either way.
    async fn terminalize_superseded_cron_control(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        superseded_min: i64,
        now_ms: i64,
    ) -> Result<()> {
        let control_key = self.key(&encode_cron_control_key_v2(db_id, job_id, superseded_min));
        let control: Option<CronRunControl> =
            match tikv_op!(txn.get_for_update(control_key.clone()).await)? {
                Some(d) => Some(
                    bincode::deserialize(d.as_slice())
                        .context("Failed to deserialize superseded cron control record")?,
                ),
                None => None,
            };
        let Some(c) = control else {
            return Ok(()); // nothing to terminalize
        };
        // Same fence gate as `finalize_cron_run_cas`, presenting the record's own
        // fence: reject (no-op) if already terminal or re-fenced by a concurrent
        // claimer that legitimately owns this minute.
        if !cron_finalize_accepts(Some(&c), c.fence_token) {
            return Ok(());
        }
        let new_control = CronRunControl {
            state: CronRunState::Failed,
            fence_token: c.fence_token,
            run_id: c.run_id,
            scheduled_min: superseded_min,
            started_ms: c.started_ms,
            deadline_ms: c.deadline_ms,
            finalize_seq: c.finalize_seq.saturating_add(1),
        };
        txn_put(
            txn,
            control_key,
            bincode::serialize(&new_control)
                .context("Failed to serialize superseded cron control record")?,
        )
        .await?;

        // Clear the cross-generation no-overlap CARRIER for the superseded run IFF
        // it still names that run (design 35 §Symmetric bridge). This path
        // deliberately does not touch `ACTIVE(job)` (the caller overwrites it to
        // the new minute), so the carrier must be cleared here or a superseded run
        // would leave a stale guard the old binary honors forever. Fence-matched:
        // when the carrier was already re-pointed at a different fence (e.g. the
        // straggler-fold path, where the guard names the folded straggler, not
        // this superseded `c.run_id`) the compare misses and the live carrier is
        // left intact. The caller's fresh CONTROL/ACTIVE write then re-establishes
        // the carrier for the new run, so it is never momentarily absent.
        self.clear_cron_running_guard_if(txn, db_id, job_id, c.run_id)
            .await?;

        // Project the history record for the superseded fire so
        // `cron.job_run_details` reflects it (parity with the reaper's projection).
        // RECONCILE from the shared CronRun, do NOT force Failed (design 35 §Reaper
        // reconcile): if the owner already finalized (e.g. an OLD #2629 worker
        // FINISHED via the legacy path after the new control plane was minted, but
        // could not terminalize this CONTROL it does not know about) the superseding
        // takeover must PRESERVE that authoritative terminal result, not clobber a
        // Succeeded run with Failed. Only a genuinely orphaned (non-terminal /
        // already retention-GC'd) superseded fire becomes Failed. Note the CONTROL
        // above is still set to `Failed` unconditionally because supersession means
        // this minute is being abandoned for a later minute — but the CONTROL state
        // is a dedup tombstone, not the user-visible outcome; the `CronRun` carries
        // the real result and must not be overwritten.
        let existing = self.get_cron_run(txn, db_id, c.run_id).await?;
        let (_terminal_state, run) = reconcile_cron_terminal(
            existing,
            c.run_id,
            job_id,
            "orphan recovery: superseded by later-minute takeover",
            now_ms,
        );
        self.put_cron_run(txn, db_id, &run).await?;
        Ok(())
    }

    /// Test-only plain read of the CONTROL record for a fire. Uses `get`, NOT
    /// `get_for_update`, so an inspector never contends with the code under test
    /// for the pessimistic lock. (`read_cron_control` was removed from the
    /// production surface — design 35 writes CONTROL only inside the CAS helpers
    /// and reads it under `get_for_update`; tests still need to assert the
    /// stored fence/state after a CAS.)
    #[cfg(test)]
    pub(crate) async fn read_cron_control(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
        scheduled_min: i64,
    ) -> Result<Option<CronRunControl>> {
        let key = self.key(&encode_cron_control_key_v2(db_id, job_id, scheduled_min));
        match tikv_op!(txn.get(key).await)? {
            Some(d) => Ok(Some(
                bincode::deserialize(d.as_slice())
                    .context("Failed to deserialize cron control record")?,
            )),
            None => Ok(None),
        }
    }

    /// Test-only plain read of the legacy running-guard CARRIER `run_id` for a
    /// `(db,job)` (design 35 §Symmetric bridge). Uses `get` (no lock contention),
    /// keeps the guard encoder private to the storage crate, and lets the cron +
    /// worker reaper tests assert carrier write/clear without re-encoding the key.
    /// Returns `None` when absent or malformed (not 8 bytes).
    #[cfg(test)]
    pub(crate) async fn read_cron_running_guard(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        job_id: i64,
    ) -> Result<Option<i64>> {
        let key = self.key(&encode_cron_running_guard_key_v2(db_id, job_id));
        match tikv_op!(txn.get(key).await)? {
            Some(d) => Ok(<[u8; 8]>::try_from(d.as_slice())
                .ok()
                .map(i64::from_be_bytes)),
            None => Ok(None),
        }
    }

    /// Test-only: plant a legacy running-guard `(db,job) → run_id` (8-byte value),
    /// the post-marker straggler an OLD #2629 binary writes. Lets cross-module
    /// tests (the worker reaper test) seed the straggler-fold scenario without the
    /// private `key()`/encoder surface.
    #[cfg(test)]
    pub(crate) async fn plant_legacy_running_guard(
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

    /// Test-only: write the per-db v3 migration marker so the new claim path treats
    /// the db as already migrated (the bulk pass would find nothing).
    #[cfg(test)]
    pub(crate) async fn write_cron_migrated_marker(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_cron_migrated_key_v3(db_id));
        txn_put(txn, key, vec![1u8]).await?;
        Ok(())
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

    // ========================================================================
    // PURE decision-logic tests (no cluster). These exercise the exact branch
    // logic of the claim/takeover state machine and the finalize accept-gate —
    // the locus of both DEFECT fixes — without a real txn. Each FAILS if the
    // corresponding fix is reverted (no tautologies).
    // ========================================================================

    fn ctrl(state: CronRunState, fence_token: i64, deadline_ms: i64) -> CronRunControl {
        CronRunControl {
            state,
            fence_token,
            run_id: fence_token,
            scheduled_min: 0,
            started_ms: 0,
            deadline_ms,
            finalize_seq: 0,
        }
    }

    fn active(active_minute: i64, fence_token: i64, deadline_ms: i64) -> CronActiveRun {
        CronActiveRun {
            job_id: 1,
            active_minute,
            run_id: fence_token,
            fence_token,
            deadline_ms,
        }
    }

    /// DEFECT 1 (decision half): a takeover that minted a HIGHER fence (F2) makes
    /// the original owner's terminal CAS (presenting F1) rejected, while the
    /// takeover owner (presenting F2) is accepted. Reverting the fence gate (e.g.
    /// dropping the `fence_token == fence` check) makes the F1 case wrongly
    /// accept and this test fails.
    #[test]
    fn finalize_rejects_stale_fence_accepts_current() {
        let now = 1_000_000i64;
        let f1 = 10i64;
        let f2 = 11i64; // takeover minted a strictly higher fence
        let control = ctrl(CronRunState::Running, f2, now + 60_000);
        assert!(
            !cron_finalize_accepts(Some(&control), f1),
            "the superseded owner's stale fence F1 must be REJECTED"
        );
        assert!(
            cron_finalize_accepts(Some(&control), f2),
            "the takeover owner's current fence F2 must be ACCEPTED"
        );
    }

    /// Finalize must reject when CONTROL is gone (nothing to finalize) and when
    /// the fire is already terminal (no double-finalize).
    #[test]
    fn finalize_rejects_missing_or_terminal_control() {
        assert!(
            !cron_finalize_accepts(None, 5),
            "missing CONTROL must be rejected"
        );
        for terminal in [
            CronRunState::Succeeded,
            CronRunState::Failed,
            CronRunState::Cancelled,
        ] {
            let control = ctrl(terminal, 5, 0);
            assert!(
                !cron_finalize_accepts(Some(&control), 5),
                "already-terminal CONTROL must be rejected even with matching fence ({terminal:?})"
            );
        }
    }

    /// INVARIANT 5 (expired-but-not-taken-over): a sole owner whose wall-clock
    /// lease has lapsed (deadline_ms < now) but whose fence is unchanged must
    /// still finalize — the lease is liveness, the fence is authority. If the
    /// gate were (wrongly) tightened to also require `deadline_ms >= now`, this
    /// fails.
    #[test]
    fn finalize_accepts_when_lease_lapsed_but_fence_intact() {
        let now = 2_000_000i64;
        let fence = 7i64;
        let control = ctrl(CronRunState::Running, fence, now - 1); // lease lapsed
        assert!(
            cron_finalize_accepts(Some(&control), fence),
            "a sole owner with a lapsed lease but intact fence MUST still finalize"
        );
    }

    /// The ONE shared effective-deadline helper both the per-claim path and the
    /// bulk migration route through: `now + max(global, execution_window)`, where
    /// `execution_window = cron_execution_window_ms(max_runtime, cron_job_timeout)`
    /// — the SAME fn the executor times out on. A long job (max_runtime > global)
    /// MUST get the longer window — stamping `now + global` (the pre-fix
    /// bulk-migration bug) would expire a still-live long run early and reopen the
    /// mixed-version takeover/overlap window.
    #[test]
    fn effective_orphan_deadline_uses_max_of_global_and_execution_window() {
        let now = 1_000_000i64;
        let global = 300_000i64;
        let cjt = 300_000u64; // cron_job_timeout == floor in the default config
                              // No max_runtime -> the default execution window (cron_job_timeout), which
                              // the floor already covers.
        assert_eq!(
            cron_effective_orphan_deadline_ms(now, global, None, cjt),
            now + global
        );
        // Short job (< global) -> global floor still wins.
        assert_eq!(
            cron_effective_orphan_deadline_ms(now, global, Some(60_000), cjt),
            now + global
        );
        // Long job (> global) -> the JOB runtime wins; NOT now + global.
        let long = 3_600_000i64;
        assert_eq!(
            cron_effective_orphan_deadline_ms(now, global, Some(u64::try_from(long).unwrap()), cjt),
            now + long,
            "a long job's deadline must cover its full runtime, not stop at now + global"
        );
    }

    /// THE by-construction invariant (design 35 §Effective floor): the frozen
    /// orphan deadline is ALWAYS `>= now + executor_timeout`, for every job config,
    /// because both derive from the one `cron_execution_window_ms`. This is what
    /// makes "a still-EXECUTING run is never classed expired and taken over" hold
    /// structurally rather than by a coincidence of two separately-written formulas.
    #[test]
    fn orphan_deadline_always_covers_executor_timeout() {
        let now = 1_000_000i64;
        for &orphan_timeout_sec in &[0u64, 300, 1_800] {
            for &cron_job_timeout_ms in &[0u64, 60_000, 1_800_000] {
                for &max_runtime_ms in &[
                    None,
                    Some(1u64),
                    Some(60_000),
                    Some(1_800_000),
                    Some(7_200_000),
                ] {
                    let floor = crate::worker::gc::effective_cron_orphan_floor_ms(
                        orphan_timeout_sec,
                        cron_job_timeout_ms,
                    );
                    let deadline = cron_effective_orphan_deadline_ms(
                        now,
                        floor,
                        max_runtime_ms,
                        cron_job_timeout_ms,
                    );
                    let executor_timeout =
                        cron_execution_window_ms(max_runtime_ms, cron_job_timeout_ms) as i64;
                    assert!(
                        deadline >= now + executor_timeout,
                        "deadline {deadline} < now+executor {} for O={orphan_timeout_sec}s \
                         T={cron_job_timeout_ms}ms mr={max_runtime_ms:?}",
                        now + executor_timeout
                    );
                }
            }
        }
    }

    /// INVARIANT 4 (per-fire dedup): a terminal CONTROL for this exact minute
    /// short-circuits to `AlreadyTerminalForMinute`, even when its deadline is
    /// also in the future (step-1 terminal check must precede step-2 live check).
    #[test]
    fn claim_decides_already_terminal_for_completed_minute() {
        let now = 3_000_000i64;
        for terminal in [
            CronRunState::Succeeded,
            CronRunState::Failed,
            CronRunState::Cancelled,
        ] {
            // deadline in the FUTURE to prove terminal precedes the live check.
            let control = ctrl(terminal, 1, now + 60_000);
            assert_eq!(
                decide_cron_claim(Some(&control), None, 100, now),
                CronClaimDecision::Return(CronClaimOutcome::AlreadyTerminalForMinute),
                "terminal CONTROL must dedup the fire ({terminal:?})"
            );
        }
    }

    /// Defensive same-minute-live branch: a non-terminal CONTROL for this exact
    /// minute whose deadline is in the future blocks (the queue row is kept).
    #[test]
    fn claim_decides_blocked_when_same_minute_still_live() {
        let now = 3_500_000i64;
        for state in [CronRunState::Claimed, CronRunState::Running] {
            let control = ctrl(state, 1, now + 60_000);
            assert_eq!(
                decide_cron_claim(Some(&control), None, 100, now),
                CronClaimDecision::Return(CronClaimOutcome::BlockedByLiveActive),
                "a live same-minute CONTROL must block ({state:?})"
            );
        }
    }

    /// INVARIANT 3 (job-level no-overlap): a live ACTIVE pointer for a DIFFERENT
    /// minute of the same job blocks a claim for minute M+k. Reverting the
    /// `active_minute != scheduled_min` no-overlap guard makes this proceed
    /// (overlapping runs) and this test fails.
    #[test]
    fn claim_decides_blocked_by_live_active_other_minute() {
        let now = 4_000_000i64;
        let m = 100i64;
        let later = m + 5;
        let a = active(m, 1, now + 60_000); // live, different minute
        assert_eq!(
            decide_cron_claim(None, None, later, now), // sanity: no active -> proceeds
            CronClaimDecision::Proceed {
                took_over: false,
                supersede_minute: None
            }
        );
        assert_eq!(
            decide_cron_claim(None, Some(&a), later, now),
            CronClaimDecision::Return(CronClaimOutcome::BlockedByLiveActive),
            "a live ACTIVE pointer for a different minute must block the job (no-overlap)"
        );
    }

    /// A live ACTIVE pointer for the SAME minute does NOT block via the
    /// no-overlap branch (the per-minute CONTROL governs same-minute dedup).
    #[test]
    fn claim_not_blocked_by_active_for_same_minute() {
        let now = 4_500_000i64;
        let m = 200i64;
        let a = active(m, 1, now + 60_000); // live, SAME minute
        assert_eq!(
            decide_cron_claim(None, Some(&a), m, now),
            CronClaimDecision::Proceed {
                took_over: true,
                // SAME minute: the claim overwrites the same CONTROL key in place,
                // so there is nothing to terminalize separately.
                supersede_minute: None
            },
            "an ACTIVE pointer for the same minute is takeover input, not a block"
        );
    }

    /// INVARIANT 2 (no silent skip via reaper, decision half): an ORPHANED fire
    /// — CONTROL Running + ACTIVE both with deadline in the PAST — claiming a
    /// LATER minute M+k must be a clean TAKEOVER (`Proceed { took_over: true }`),
    /// NOT `BlockedByLiveActive` and NOT `AlreadyTerminalForMinute`. This is the
    /// exact path the old code silently dropped. If the deadline-expiry takeover
    /// logic is reverted (e.g. blocking on any prior ACTIVE regardless of
    /// deadline), this proceeds becomes a block and the test fails.
    ///
    /// It is ALSO a different-minute takeover, so the decision must report
    /// `supersede_minute: Some(M)` — the caller terminalizes `CONTROL(job, M)` in
    /// the same claim txn so it is not stranded non-terminal (reaper-unreachable +
    /// finalizable by the lost-deadline owner's stale fence). Reverting that
    /// reporting drops the `Some(M)` and this test fails.
    #[test]
    fn claim_takes_over_orphaned_run_for_later_minute() {
        let now = 5_000_000i64;
        let m = 300i64;
        let later = m + 1;
        let control = ctrl(CronRunState::Running, 1, now - 1); // orphaned
        let a = active(m, 1, now - 1); // orphaned
        assert_eq!(
            decide_cron_claim(Some(&control), Some(&a), later, now),
            CronClaimDecision::Proceed {
                took_over: true,
                supersede_minute: Some(m)
            },
            "an orphaned run must be TAKEN OVER for a later minute (never silently \
             dropped) AND report the superseded minute so its CONTROL is terminalized"
        );
    }

    /// DEFECT 2 (different-minute supersession, decision half): the
    /// `supersede_minute` field is set ONLY when an EXPIRED ACTIVE pointer for a
    /// DIFFERENT minute is taken over (the case that strands the superseded
    /// minute's CONTROL), and is `None` for a same-minute takeover (overwrites the
    /// same CONTROL key in place) or when there is no prior ACTIVE. This is the
    /// pure half of the in-claim terminalization that closes the orphaned-CONTROL
    /// + stale-finalize vector.
    #[test]
    fn claim_reports_supersede_minute_only_for_different_minute_takeover() {
        let now = 5_500_000i64;
        let m = 700i64;
        let later = m + 3;
        let expired = active(m, 1, now - 1); // expired, different minute

        // Different-minute takeover -> report the superseded minute M.
        assert_eq!(
            decide_cron_claim(None, Some(&expired), later, now),
            CronClaimDecision::Proceed {
                took_over: true,
                supersede_minute: Some(m)
            },
            "a different-minute takeover must report the superseded minute for terminalization"
        );

        // Same-minute takeover -> nothing to terminalize separately.
        let expired_same = active(m, 1, now - 1);
        assert_eq!(
            decide_cron_claim(None, Some(&expired_same), m, now),
            CronClaimDecision::Proceed {
                took_over: true,
                supersede_minute: None
            },
            "a same-minute takeover overwrites the same CONTROL key: no separate terminalize"
        );

        // No prior ACTIVE -> nothing to supersede.
        assert_eq!(
            decide_cron_claim(None, None, later, now),
            CronClaimDecision::Proceed {
                took_over: false,
                supersede_minute: None
            }
        );
    }

    /// A fresh fire (no prior CONTROL/ACTIVE) is a plain claim, not a takeover.
    #[test]
    fn claim_decides_fresh_when_no_prior_state() {
        let now = 6_000_000i64;
        assert_eq!(
            decide_cron_claim(None, None, 400, now),
            CronClaimDecision::Proceed {
                took_over: false,
                supersede_minute: None
            }
        );
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
        let store = crate::worker::init_gc_registry_store(pd_endpoints, &cfg)
            .await
            .expect("init system store");

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

    // ========================================================================
    // CLUSTER I/O tests (#[ignore]) — exercise the full CONTROL/ACTIVE key
    // transitions and the fence CAS over a real TiKV txn. Run with:
    //   PD_ENDPOINTS=127.0.0.1:2379 cargo test -p db9-server <name> -- --ignored
    // ========================================================================

    /// Mirror the existing cluster-test store init: a fresh, uniquely-keyspaced
    /// system store so per-(db,job,minute) assertions are deterministic.
    async fn cron_cas_test_store(tag: &str) -> TikvStore {
        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let keyspace = format!(
            "_sys_cron_cas_{tag}_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        TikvStore::new_system(pd_endpoints, &keyspace)
            .await
            .expect("init raw system store")
    }

    /// Plant a live database metadata row at a FIXED `db_id` and enable cron for
    /// it, so the migration gate's DB-liveness + cron-enabled fences pass. The
    /// cluster cron tests drive synthetic numeric `db_id`s directly (they bypass
    /// the engine), so they must seed the same liveness signals production has.
    async fn plant_live_db_and_enable_cron(store: &TikvStore, db_id: u64, name: &str) {
        let mut txn = store.begin().await.expect("begin");
        let def = crate::model::DatabaseDef::new(db_id, name.to_string(), "admin".to_string());
        let id_key = store.key(&encode_database_id_key(db_id));
        txn_put(
            &mut txn,
            id_key,
            bincode::serialize(&def).expect("serialize database def"),
        )
        .await
        .expect("plant database id row");
        store
            .set_cron_enabled(&mut txn, db_id)
            .await
            .expect("enable cron");
        txn.commit().await.expect("commit db + cron-enabled");
    }

    fn test_cron_job(job_id: i64) -> CronJob {
        CronJob {
            job_id,
            schedule: "* * * * *".to_string(),
            command: "SELECT 1".to_string(),
            nodename: "localhost".to_string(),
            nodeport: 5433,
            database: "regress".to_string(),
            username: "admin".to_string(),
            active: true,
            jobname: Some(format!("job_{job_id}")),
            max_runtime_ms: None,
        }
    }

    /// INVARIANT 1 (storage half — no double-finalize). Worker A claims (fence
    /// F1); a takeover (claim with the stored deadline now in the past) mints
    /// F2 > F1. A finalize presenting F1 is REJECTED and CONTROL still holds F2;
    /// a finalize presenting F2 is ACCEPTED. Reverting the fence gate lets F1
    /// finalize and clobber the live takeover -> double execution.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_takeover_mints_higher_fence_and_rejects_stale_finalize() {
        let store = cron_cas_test_store("dbl_fin").await;
        let db_id = 700_001_u64;
        let job_id = 1i64;
        let minute = 28_900_000i64;
        let job = test_cron_job(job_id);

        // Worker A claims at T=1000 with a SHORT lease (deadline already in the
        // past relative to the takeover's `now`).
        let f1 = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    minute,
                    1000,
                    1500,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("first claim");
            txn.commit().await.expect("commit claim A");
            match outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("first claim must be Claimed, got {other:?}"),
            }
        };

        // Takeover at T=10_000 (> stored deadline 1500): orphaned -> TookOver,
        // mints F2 > F1.
        let f2 = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    minute,
                    10_000,
                    70_000,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("takeover claim");
            txn.commit().await.expect("commit takeover");
            match outcome {
                CronClaimOutcome::TookOver { run } => run.run_id,
                other => panic!("takeover must be TookOver, got {other:?}"),
            }
        };
        assert!(
            f2 > f1,
            "takeover must mint a strictly higher fence: {f2} > {f1}"
        );

        // Original owner A finalizes presenting STALE F1 -> rejected.
        {
            let mut txn = store.begin().await.expect("begin");
            let run = test_run(job_id, f1, CronRunStatus::Succeeded);
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    job_id,
                    minute,
                    f1,
                    CronRunState::Succeeded,
                    &run,
                )
                .await
                .expect("finalize F1");
            assert!(
                !accepted,
                "finalize with the superseded fence F1 MUST be rejected"
            );
            txn.commit().await.ok();
        }

        // CONTROL still holds F2 and is non-terminal (the takeover owner).
        {
            let mut txn = store.begin().await.expect("begin");
            let c = store
                .read_cron_control(&mut txn, db_id, job_id, minute)
                .await
                .expect("read control")
                .expect("control must exist");
            assert_eq!(
                c.fence_token, f2,
                "CONTROL must still hold F2 after rejected F1"
            );
            assert!(
                !c.state.is_terminal(),
                "rejected stale finalize must NOT mark CONTROL terminal"
            );
            txn.rollback().await.ok();
        }

        // Takeover owner B finalizes presenting F2 -> accepted.
        {
            let mut txn = store.begin().await.expect("begin");
            let run = test_run(job_id, f2, CronRunStatus::Succeeded);
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    job_id,
                    minute,
                    f2,
                    CronRunState::Succeeded,
                    &run,
                )
                .await
                .expect("finalize F2");
            assert!(
                accepted,
                "finalize with the current fence F2 MUST be accepted"
            );
            txn.commit().await.expect("commit finalize F2");
        }

        // CONTROL is now terminal; ACTIVE pointer cleared.
        {
            let mut txn = store.begin().await.expect("begin");
            let c = store
                .read_cron_control(&mut txn, db_id, job_id, minute)
                .await
                .expect("read control")
                .expect("control must exist");
            assert!(
                c.state.is_terminal(),
                "CONTROL must be terminal after accepted finalize"
            );
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert!(
                actives.is_empty(),
                "ACTIVE pointer must be cleared on finalize"
            );
            txn.rollback().await.ok();
        }
    }

    /// INVARIANT 2 (storage half — no silent skip). Claim minute M with a lease
    /// in the past (orphaned). Finalize it via the fence CAS to Failed (the path
    /// the reaper drives). Then claim a LATER minute M+k -> outcome is Claimed
    /// (NOT BlockedByLiveActive, NOT AlreadyTerminalForMinute). On the old code
    /// the per-minute claim flag was never cleared, so M+k was silently dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_reap_then_claim_later_minute_succeeds() {
        let store = cron_cas_test_store("no_skip").await;
        let db_id = 700_002_u64;
        let job_id = 1i64;
        let m = 28_900_000i64;
        let later = m + 1;
        let job = test_cron_job(job_id);

        // Claim M, orphaned (deadline 1500 < the finalize/claim `now`).
        let fence_m = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    1000,
                    1500,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("claim M");
            txn.commit().await.expect("commit claim M");
            match outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("claim M must be Claimed, got {other:?}"),
            }
        };

        // Reaper-equivalent: finalize M to Failed via the fence CAS. This both
        // tombstones CONTROL(M) terminal AND clears the ACTIVE pointer in one txn
        // — the DEFECT-2 fix.
        {
            let mut txn = store.begin().await.expect("begin");
            let run = test_run(job_id, fence_m, CronRunStatus::Failed);
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    fence_m,
                    CronRunState::Failed,
                    &run,
                )
                .await
                .expect("reap finalize M");
            assert!(accepted, "reaping an orphaned run must be accepted");
            txn.commit().await.expect("commit reap");
        }

        // ACTIVE must be empty after the reap (no dangling no-overlap pointer).
        {
            let mut txn = store.begin().await.expect("begin");
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert!(
                actives.is_empty(),
                "reaper must clear the ACTIVE pointer so a later minute is not blocked"
            );
            txn.rollback().await.ok();
        }

        // Now claim the LATER minute M+k. The old silent-skip path returned
        // AlreadyClaimed/Blocked here; the fix must yield a fresh Claimed.
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    later,
                    20_000,
                    80_000,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("claim later minute");
            assert!(
                matches!(outcome, CronClaimOutcome::Claimed { .. }),
                "a later minute after a reaped fire must be CLAIMED, got {outcome:?}"
            );
            txn.commit().await.expect("commit claim later");
        }
    }

    /// INVARIANT 2b (storage half — different-minute takeover terminalizes the
    /// superseded minute's CONTROL). The orphaned-CONTROL + stale-finalize vector:
    ///   1. F1 claims minute M (deadline in the past — orphaned, but NEVER reaped).
    ///   2. F2 claims a LATER minute M+1: a different-minute takeover. It overwrites
    ///      `ACTIVE(J)` from M to M+1, so the reaper (which drives off ACTIVE) can
    ///      no longer reach `CONTROL(J, M)`.
    /// Without the in-claim terminalization, `CONTROL(J, M)` lingers non-terminal
    /// `Running/F1`, and a still-alive F1 could later `finalize_cron_run_cas(M, F1)`
    /// — its stale fence still matches, so the stale finalize is ADMITTED (a
    /// non-owner terminal write). The fix terminalizes `CONTROL(J, M)` to `Failed`
    /// inside F2's claim txn. This test asserts:
    ///   (i)  after F2's takeover, `CONTROL(J, M)` is TERMINAL (Failed) — not
    ///        orphaned, no reaper needed;
    ///   (ii) a subsequent `finalize_cron_run_cas(M, F1)` is REJECTED (the
    ///        accept-gate rejects on terminal regardless of fence) — the stale
    ///        finalize is fenced out.
    /// Reverting the terminalization leaves `CONTROL(J, M)` Running and admits the
    /// stale finalize, failing both assertions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_later_minute_takeover_terminalizes_superseded_control() {
        let store = cron_cas_test_store("supersede").await;
        let db_id = 700_008_u64;
        let job_id = 1i64;
        let m = 28_900_000i64;
        let later = m + 1;
        let job = test_cron_job(job_id);

        // F1 claims M, orphaned (deadline 1500 < the later claim's `now`). This
        // run is NEVER reaped — F2 supersedes it via a later-minute takeover.
        let f1 = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    1000,
                    1500,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("claim M");
            txn.commit().await.expect("commit claim M");
            match outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("claim M must be Claimed, got {other:?}"),
            }
        };

        // F2 claims the LATER minute M+1 at T=10_000 (> M's deadline 1500): a
        // different-minute takeover. It overwrites ACTIVE(J) to M+1, so the reaper
        // can no longer reach CONTROL(J, M) — which the takeover must terminalize.
        let f2 = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    later,
                    10_000,
                    70_000,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("takeover later minute");
            txn.commit().await.expect("commit takeover later");
            match outcome {
                CronClaimOutcome::TookOver { run } => run.run_id,
                other => panic!("later-minute takeover must be TookOver, got {other:?}"),
            }
        };
        assert!(f2 > f1, "takeover must mint a strictly higher fence");

        // (i) CONTROL(J, M) — the superseded minute — is now TERMINAL (Failed) and
        //     not orphaned. ACTIVE now names M+1 (one pointer, the live takeover).
        {
            let mut txn = store.begin().await.expect("begin");
            let c_m = store
                .read_cron_control(&mut txn, db_id, job_id, m)
                .await
                .expect("read control M")
                .expect("superseded CONTROL(M) must still exist (terminal, not deleted)");
            assert_eq!(
                c_m.state,
                CronRunState::Failed,
                "the superseded minute's CONTROL must be terminalized to Failed in the takeover txn"
            );
            assert!(
                c_m.state.is_terminal(),
                "superseded CONTROL(M) must be terminal so a stale finalize is fenced out"
            );
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert_eq!(
                actives.len(),
                1,
                "exactly one ACTIVE pointer (the live takeover for M+1)"
            );
            assert_eq!(
                actives[0].active_minute, later,
                "ACTIVE must name the taken-over later minute"
            );
            txn.rollback().await.ok();
        }

        // (ii) A still-alive F1 finalizes its minute M presenting its stale fence
        //      F1. CONTROL(M) is terminal, so the accept-gate REJECTS it — the
        //      stale finalize is fenced out (no non-owner terminal write).
        {
            let mut txn = store.begin().await.expect("begin");
            let run = test_run(job_id, f1, CronRunStatus::Succeeded);
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    f1,
                    CronRunState::Succeeded,
                    &run,
                )
                .await
                .expect("finalize M with stale fence");
            assert!(
                !accepted,
                "a stale finalize for a superseded+terminalized minute MUST be rejected"
            );
            txn.rollback().await.ok();
        }

        // CONTROL(M) is unchanged (still Failed) after the rejected stale finalize.
        {
            let mut txn = store.begin().await.expect("begin");
            let c_m = store
                .read_cron_control(&mut txn, db_id, job_id, m)
                .await
                .expect("read control M")
                .expect("CONTROL(M) must exist");
            assert_eq!(
                c_m.state,
                CronRunState::Failed,
                "a rejected stale finalize must not alter the terminalized CONTROL(M)"
            );
            txn.rollback().await.ok();
        }
    }

    /// INVARIANT 3 (job-level no-overlap). Claim M (live, deadline in the
    /// future). A claim for a DIFFERENT minute M+k of the SAME job is blocked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_live_run_blocks_other_minute_same_job() {
        let store = cron_cas_test_store("no_overlap").await;
        let db_id = 700_003_u64;
        let job_id = 1i64;
        let m = 28_900_000i64;
        let later = m + 1;
        let job = test_cron_job(job_id);

        // Claim M, LIVE (deadline far in the future).
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    1000,
                    10_000_000,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("claim M");
            assert!(matches!(outcome, CronClaimOutcome::Claimed { .. }));
            txn.commit().await.expect("commit claim M");
        }

        // Claim M+k while M is still live -> BlockedByLiveActive (via the ACTIVE
        // pointer's different-minute no-overlap branch).
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    later,
                    2000,
                    10_000_000,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("claim later");
            assert_eq!(
                outcome,
                CronClaimOutcome::BlockedByLiveActive,
                "a different minute of a live job must be blocked (no-overlap)"
            );
            txn.rollback().await.ok();
        }
    }

    /// INVARIANT 4 (per-fire dedup). Claim M, finalize it terminal, then re-claim
    /// the SAME minute M -> AlreadyTerminalForMinute (genuine duplicate fire).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_reclaim_same_terminal_minute_is_dedup() {
        let store = cron_cas_test_store("dedup").await;
        let db_id = 700_004_u64;
        let job_id = 1i64;
        let m = 28_900_000i64;
        let job = test_cron_job(job_id);

        let fence = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    1000,
                    10_000_000,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("claim M");
            txn.commit().await.expect("commit claim M");
            match outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("claim must be Claimed, got {other:?}"),
            }
        };

        {
            let mut txn = store.begin().await.expect("begin");
            let run = test_run(job_id, fence, CronRunStatus::Succeeded);
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    fence,
                    CronRunState::Succeeded,
                    &run,
                )
                .await
                .expect("finalize M");
            assert!(accepted);
            txn.commit().await.expect("commit finalize M");
        }

        // Re-claim the SAME minute -> dedup.
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    20_000,
                    10_000_000,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("re-claim M");
            assert_eq!(
                outcome,
                CronClaimOutcome::AlreadyTerminalForMinute,
                "re-claiming a completed minute must be deduped"
            );
            txn.rollback().await.ok();
        }
    }

    /// INVARIANT 5 (storage half — expired-but-not-taken-over). Claim, do NOT
    /// take over, then finalize presenting the ORIGINAL fence even though the
    /// lease has lapsed -> accepted. The fence is authority; a lapsed lease alone
    /// does not block a sole owner's finalize.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_sole_owner_finalizes_after_lease_lapse() {
        let store = cron_cas_test_store("lease_lapse").await;
        let db_id = 700_005_u64;
        let job_id = 1i64;
        let m = 28_900_000i64;
        let job = test_cron_job(job_id);

        // Claim with a short lease (deadline 1500), no takeover happens.
        let fence = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    1000,
                    1500,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("claim M");
            txn.commit().await.expect("commit claim M");
            match outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("claim must be Claimed, got {other:?}"),
            }
        };

        // No takeover. Finalize with the original fence long after the lease
        // lapsed -> accepted.
        {
            let mut txn = store.begin().await.expect("begin");
            let run = test_run(job_id, fence, CronRunStatus::Succeeded);
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    job_id,
                    m,
                    fence,
                    CronRunState::Succeeded,
                    &run,
                )
                .await
                .expect("finalize after lapse");
            assert!(
                accepted,
                "a sole owner whose lease lapsed but whose fence is intact MUST finalize"
            );
            txn.commit().await.expect("commit finalize");
        }
    }

    /// INVARIANT 6 (migration TRANSLATES, never discards — design 35 §Migration).
    /// Plant a legacy running-guard (8-byte run_id) and a legacy per-minute claim
    /// (vec![1]); run the idempotent migration gate with a LIVE orphan window;
    /// then assert:
    ///   * both legacy keys are gone and the v3 marker exists;
    ///   * the guard became an ACTIVE pointer + a matching `Running` CONTROL
    ///     (same fence == run_id, `deadline_ms == now + orphan_window`) so the
    ///     migrated pointer is a LIVE no-overlap authority, not born-stale;
    ///   * a real-minute claim immediately after migration is `BlockedByLiveActive`
    ///     (the job-level no-overlap gate is actually EXERCISED — this is the
    ///     regression the old `deadline_ms = 0` translation silently passed);
    ///   * the claim became a terminal (`Failed`) CONTROL tombstone so per-fire
    ///     dedup survives the upgrade.
    /// This fails if the migration mints a born-stale (`deadline_ms = 0`) pointer
    /// (the post-claim block becomes Proceed/TookOver = no-overlap violation) or
    /// reverts to delete-and-accept.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_migration_translates_legacy_state_and_writes_marker() {
        let store = cron_cas_test_store("migrate").await;
        let db_id = 700_006_u64;
        let job_id = 7i64;
        let run_id = 123i64;
        let claim_min = 28_900_000i64;
        const MIGRATED_MIN: i64 = 0;
        // Migration `now` and orphan window: the migrated guard must inherit the
        // live deadline `now + orphan_window` so it keeps no-overlap authority.
        let migrate_now = 1_000_000i64;
        let orphan_window = 300_000i64; // 5 min, the default global orphan timeout
        let expected_deadline = migrate_now + orphan_window;

        // Plant legacy keys (no per-key encoders survive — extend the prefix).
        let mut legacy_guard = encode_cron_running_guard_prefix_v2(db_id);
        legacy_guard.extend_from_slice(&job_id.to_be_bytes());
        let mut legacy_claim = encode_cron_claim_prefix_v2(db_id);
        legacy_claim.extend_from_slice(&job_id.to_be_bytes());
        legacy_claim.push(b'_');
        legacy_claim.extend_from_slice(&claim_min.to_be_bytes());
        {
            let mut txn = store.begin().await.expect("begin");
            txn_put(
                &mut txn,
                store.key(&legacy_guard),
                run_id.to_be_bytes().to_vec(), // 8-byte run_id
            )
            .await
            .expect("plant legacy guard");
            txn_put(&mut txn, store.key(&legacy_claim), vec![1u8])
                .await
                .expect("plant legacy claim");
            txn.commit().await.expect("commit legacy plant");
        }

        // Marker must be absent before migration.
        {
            let mut txn = store.begin().await.expect("begin");
            let marker = tikv_op!(
                txn.get(store.key(&encode_cron_migrated_key_v3(db_id)))
                    .await
            )
            .expect("get marker");
            assert!(marker.is_none(), "marker must be absent before migration");
            txn.rollback().await.ok();
        }

        // The migration gate is fenced on DB-liveness + cron-enabled (the P2
        // migration fence): seed both so a legitimate migration commits.
        plant_live_db_and_enable_cron(&store, db_id, "regress_migrate").await;

        // Run the idempotent migration gate (opens/commits its own txn) with a
        // live orphan window so the migrated guard is stamped `now + window`.
        store
            .ensure_cron_control_migrated(db_id, migrate_now, orphan_window, orphan_window as u64)
            .await
            .expect("migrate");

        {
            let mut txn = store.begin().await.expect("begin");

            // Legacy prefixes swept.
            let guard_left = store
                .scan_prefix_keys_bytesafe(&mut txn, &encode_cron_running_guard_prefix_v2(db_id))
                .await
                .expect("scan guard");
            assert!(
                guard_left.is_empty(),
                "legacy running-guard keys must be swept"
            );
            let claim_left = store
                .scan_prefix_keys_bytesafe(&mut txn, &encode_cron_claim_prefix_v2(db_id))
                .await
                .expect("scan claim");
            assert!(
                claim_left.is_empty(),
                "legacy per-minute claim keys must be swept"
            );

            // Marker present.
            let marker = tikv_op!(
                txn.get(store.key(&encode_cron_migrated_key_v3(db_id)))
                    .await
            )
            .expect("get marker");
            assert!(marker.is_some(), "v3 migration marker must be written");

            // Guard → ACTIVE pointer (fence == run_id, LIVE deadline).
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert_eq!(
                actives.len(),
                1,
                "guard must translate to one ACTIVE pointer"
            );
            let a = &actives[0];
            assert_eq!(a.job_id, job_id);
            assert_eq!(a.run_id, run_id);
            assert_eq!(a.fence_token, run_id);
            assert_eq!(a.active_minute, MIGRATED_MIN);
            assert_eq!(
                a.deadline_ms, expected_deadline,
                "migrated guard must carry a LIVE orphan deadline (now + window), \
                 not a born-stale 0, or no-overlap is a silent no-op"
            );

            // Guard → matching Running CONTROL at the same (job, minute, fence).
            let guard_ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, MIGRATED_MIN)
                .await
                .expect("read guard control")
                .expect("guard control must exist");
            assert_eq!(guard_ctrl.state, CronRunState::Running);
            assert_eq!(guard_ctrl.fence_token, run_id);
            assert_eq!(guard_ctrl.run_id, run_id);
            assert_eq!(
                guard_ctrl.deadline_ms, expected_deadline,
                "migrated CONTROL must share the same live deadline as the ACTIVE pointer"
            );

            // Claim → terminal (Failed) CONTROL tombstone (dedup survives).
            let claim_ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, claim_min)
                .await
                .expect("read claim control")
                .expect("claim control must exist");
            assert_eq!(
                claim_ctrl.state,
                CronRunState::Failed,
                "claim must translate to a terminal dedup tombstone"
            );
            assert!(
                claim_ctrl.state.is_terminal(),
                "claim tombstone must be terminal so re-claim short-circuits"
            );

            txn.rollback().await.ok();
        }

        // The no-overlap GATE itself — not just the records' shape. A new-binary
        // claim for a REAL epoch-minute (its own CONTROL is None; the migrated
        // CONTROL sits at sentinel minute 0) while `now` is still inside the orphan
        // window MUST be BlockedByLiveActive: the migrated ACTIVE pointer is a live
        // job-level no-overlap authority, so the still-running old-binary fire is
        // not double-run. With the old `deadline_ms = 0` translation this fell
        // through to Proceed{took_over}→TookOver (double-exec) — this assertion is
        // the direct regression guard.
        let real_minute = claim_min + 1; // distinct from the sentinel(0) and the tombstone minute
        let claim_now = migrate_now + 1; // inside [migrate_now, expected_deadline)
        assert!(
            claim_now < expected_deadline,
            "test setup: the claim must occur before the migrated deadline lapses"
        );
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    real_minute,
                    claim_now,
                    claim_now + orphan_window,
                    &test_cron_job(job_id),
                    "regress".to_string(),
                )
                .await
                .expect("claim against migrated guard");
            assert_eq!(
                outcome,
                CronClaimOutcome::BlockedByLiveActive,
                "a claim while the migrated guard is still live MUST be blocked \
                 (no-overlap survives migration); a born-stale deadline_ms=0 guard \
                 would let this take over and double-run the job"
            );
            txn.rollback().await.ok();
        }

        // After the migrated deadline lapses, the same claim is a clean TAKEOVER —
        // the migrated guard is reaped/superseded only AFTER the orphan window, so
        // schedule progress is preserved (no permanent block).
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    real_minute,
                    expected_deadline + 1, // now past the migrated deadline
                    expected_deadline + 1 + orphan_window,
                    &test_cron_job(job_id),
                    "regress".to_string(),
                )
                .await
                .expect("takeover after migrated guard lapses");
            assert!(
                matches!(outcome, CronClaimOutcome::TookOver { .. }),
                "once the migrated orphan window lapses the claim must TAKE OVER \
                 (schedule progress), got {outcome:?}"
            );
            txn.commit().await.expect("commit takeover");
        }
    }

    /// INVARIANT 6b (post-marker cross-fire straggler — design 35 §Post-marker
    /// straggler fold). The one-shot migration only covers state present at the
    /// marker instant; an OLD #2629 binary can write a NEW legacy running-guard for
    /// a FRESH fire AFTER the marker exists. The new claim path must still serialize
    /// against it. Setup: write the v3 marker (db already "migrated", bulk scan is a
    /// no-op), then plant a fresh legacy running-guard (the straggler's write). Then:
    ///   * a real-minute claim while the straggler guard is live MUST be
    ///     `BlockedByLiveActive` (the fold translates the guard in-txn → live ACTIVE
    ///     → job-level no-overlap), the legacy guard key is consumed, AND a matching
    ///     Running CONTROL + ACTIVE now exist (subsequent ticks go through the
    ///     fence-CAS path, not the legacy shim);
    ///   * once the migrated orphan deadline lapses the same claim is a clean
    ///     TAKEOVER (schedule progress).
    /// This fails if the claim path reads only CONTROL/ACTIVE and ignores a
    /// post-marker legacy guard — the cross-fire double-exec the snapshot migration
    /// alone cannot close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_claim_folds_post_marker_straggler_guard_and_blocks() {
        let store = cron_cas_test_store("straggler").await;
        let db_id = 700_007_u64;
        let job_id = 9i64;
        let straggler_run_id = 555i64;
        const MIGRATED_MIN: i64 = 0;

        // The db is already migrated: write the v3 marker directly so the new claim
        // path treats this db as migrated (the bulk pass would find nothing).
        {
            let mut txn = store.begin().await.expect("begin");
            txn_put(
                &mut txn,
                store.key(&encode_cron_migrated_key_v3(db_id)),
                vec![1u8],
            )
            .await
            .expect("write marker");
            txn.commit().await.expect("commit marker");
        }

        // An OLD binary wins a fresh fire AFTER the marker and writes a NEW legacy
        // running-guard (8-byte run_id) for this job — the post-marker straggler.
        let guard_key = store.key(&encode_cron_running_guard_key_v2(db_id, job_id));
        {
            let mut txn = store.begin().await.expect("begin");
            txn_put(
                &mut txn,
                guard_key.clone(),
                straggler_run_id.to_be_bytes().to_vec(),
            )
            .await
            .expect("plant straggler guard");
            txn.commit().await.expect("commit straggler guard");
        }

        let real_minute = 28_900_500i64;
        let claim_now = 2_000_000i64;
        let orphan_window = 300_000i64;
        let deadline_ms = claim_now + orphan_window;

        // A claim while the straggler guard is live MUST be blocked (no-overlap).
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    real_minute,
                    claim_now,
                    deadline_ms,
                    &test_cron_job(job_id),
                    "regress".to_string(),
                )
                .await
                .expect("claim against straggler guard");
            assert_eq!(
                outcome,
                CronClaimOutcome::BlockedByLiveActiveFolded,
                "a claim while a POST-MARKER legacy guard is live MUST be blocked \
                 (cross-fire no-overlap); ignoring it double-runs the old-binary fire. \
                 The fold path returns the Folded variant so the production caller \
                 COMMITS the durable ACTIVE/CONTROL rather than rolling it back"
            );
            // The fold must commit: it translated the guard into ACTIVE+CONTROL and
            // re-pointed the legacy key, so later ticks never re-read the shim. (In
            // production the engine commits a `BlockedByLiveActiveFolded` outcome.)
            txn.commit().await.expect("commit fold");
        }

        // The carrier is RE-POINTED at the straggler's run_id (NOT deleted) so the
        // cross-generation no-overlap never lapses (design 35 §Symmetric bridge);
        // the translated ACTIVE/CONTROL exist with the straggler's run_id == fence
        // and a LIVE deadline at the sentinel minute.
        {
            let mut txn = store.begin().await.expect("begin");
            let guard_left = tikv_op!(txn.get(guard_key.clone()).await).expect("get guard");
            let guard_run_id = guard_left
                .as_deref()
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(i64::from_be_bytes);
            assert_eq!(
                guard_run_id,
                Some(straggler_run_id),
                "the folded carrier must be RE-POINTED at the straggler's run_id, \
                 not deleted (continuous cross-generation no-overlap)"
            );
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert_eq!(
                actives.len(),
                1,
                "fold must leave exactly one ACTIVE pointer"
            );
            let a = &actives[0];
            assert_eq!(a.job_id, job_id);
            assert_eq!(a.run_id, straggler_run_id);
            assert_eq!(a.fence_token, straggler_run_id);
            assert_eq!(a.active_minute, MIGRATED_MIN);
            assert_eq!(
                a.deadline_ms, deadline_ms,
                "folded ACTIVE must carry this claim's frozen live orphan deadline"
            );
            let ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, MIGRATED_MIN)
                .await
                .expect("read folded control")
                .expect("folded control must exist");
            assert_eq!(ctrl.state, CronRunState::Running);
            assert_eq!(ctrl.fence_token, straggler_run_id);
            txn.rollback().await.ok();
        }

        // Once the folded orphan deadline lapses, the same claim is a clean TAKEOVER
        // (schedule progress — no permanent block from the straggler).
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    real_minute,
                    deadline_ms + 1, // past the folded deadline
                    deadline_ms + 1 + orphan_window,
                    &test_cron_job(job_id),
                    "regress".to_string(),
                )
                .await
                .expect("takeover after folded guard lapses");
            assert!(
                matches!(outcome, CronClaimOutcome::TookOver { .. }),
                "once the folded orphan window lapses the claim must TAKE OVER, got {outcome:?}"
            );
            txn.commit().await.expect("commit takeover");
        }
    }

    /// INVARIANT 6c (post-marker straggler fold over an EXPIRED ACTIVE — the
    /// reopened cross-fire overlap the `active.is_none()` gate missed). The fold
    /// must run whenever there is NO LIVE active authority — `ACTIVE(job)` absent OR
    /// present-but-EXPIRED — not only when ACTIVE is absent. The exact rolling-deploy
    /// interleaving this guards:
    ///   1. an earlier fire left an EXPIRED `ACTIVE(job)` (deadline in the past) at a
    ///      real minute M, with its non-terminal `CONTROL(job, M)` still present
    ///      (migrated/finished, never reaped);
    ///   2. an OLD binary then wins a LATER fresh fire and writes a FRESH legacy
    ///      running-guard (its run is still in flight);
    ///   3. a NEW binary claims another fire while that guard is live.
    /// With the old `active.is_none()` gate, step 3 SKIPPED the fold (ACTIVE is
    /// present, just expired), `decide_cron_claim` treated the expired ACTIVE as
    /// takeover-eligible, and the new binary OVERWROTE ACTIVE and ran CONCURRENTLY
    /// with the in-flight old-binary run — a job-level overlap / double-run. The fix
    /// folds the fresh guard FIRST and blocks on the resulting LIVE active. This test
    /// asserts:
    ///   (i)   the claim is `BlockedByLiveActive` (the fold won, NOT a takeover);
    ///   (ii)  the fresh legacy guard key is consumed;
    ///   (iii) exactly one ACTIVE remains — the folded straggler at the sentinel
    ///         minute `0`, with the straggler's `run_id == fence` and a LIVE deadline;
    ///   (iv)  the displaced expired ACTIVE's `CONTROL(job, M)` is terminalized to
    ///         `Failed` (not stranded reaper-unreachable + stale-finalizable).
    /// Reverting the fix to `active.is_none()` makes the claim a `TookOver` (overlap)
    /// and leaves the fresh guard un-folded, failing (i)–(iii).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_claim_folds_straggler_guard_over_expired_active_no_overlap() {
        let store = cron_cas_test_store("expired_active_straggler").await;
        let db_id = 700_009_u64;
        let job_id = 11i64;
        let stale_run_id = 222i64;
        let straggler_run_id = 777i64;
        const MIGRATED_MIN: i64 = 0;

        // The db is already migrated.
        {
            let mut txn = store.begin().await.expect("begin");
            txn_put(
                &mut txn,
                store.key(&encode_cron_migrated_key_v3(db_id)),
                vec![1u8],
            )
            .await
            .expect("write marker");
            txn.commit().await.expect("commit marker");
        }

        // Step 1: an earlier fire of M is now EXPIRED but never reaped — plant the
        // expired ACTIVE(job)=M and a matching non-terminal CONTROL(job, M). Both
        // deadlines are in the past relative to the later claim's `now`.
        let stale_min = 28_900_700i64;
        let stale_deadline = 1_000i64;
        {
            let mut txn = store.begin().await.expect("begin");
            let stale_control = CronRunControl {
                state: CronRunState::Running,
                fence_token: stale_run_id,
                run_id: stale_run_id,
                scheduled_min: stale_min,
                started_ms: 0,
                deadline_ms: stale_deadline,
                finalize_seq: 0,
            };
            txn_put(
                &mut txn,
                store.key(&encode_cron_control_key_v2(db_id, job_id, stale_min)),
                bincode::serialize(&stale_control).expect("serialize stale control"),
            )
            .await
            .expect("plant stale control");
            let stale_active = CronActiveRun {
                job_id,
                active_minute: stale_min,
                run_id: stale_run_id,
                fence_token: stale_run_id,
                deadline_ms: stale_deadline,
            };
            txn_put(
                &mut txn,
                store.key(&encode_cron_active_key_v2(db_id, job_id)),
                bincode::serialize(&stale_active).expect("serialize stale active"),
            )
            .await
            .expect("plant expired active");
            txn.commit().await.expect("commit stale state");
        }

        // Step 2: an OLD binary wins a fresh fire AFTER the marker and writes a FRESH
        // legacy running-guard — its run is still in flight.
        let guard_key = store.key(&encode_cron_running_guard_key_v2(db_id, job_id));
        {
            let mut txn = store.begin().await.expect("begin");
            txn_put(
                &mut txn,
                guard_key.clone(),
                straggler_run_id.to_be_bytes().to_vec(),
            )
            .await
            .expect("plant straggler guard");
            txn.commit().await.expect("commit straggler guard");
        }

        let real_minute = 28_900_800i64; // distinct from M and the sentinel(0)
        let claim_now = 2_000_000i64; // >> the expired deadline 1_000
        let orphan_window = 300_000i64;
        let deadline_ms = claim_now + orphan_window;
        assert!(
            stale_deadline < claim_now,
            "test setup: the planted ACTIVE must be EXPIRED at claim time"
        );

        // Step 3: a NEW binary claims another fire while the fresh guard is live.
        // (i) Despite the EXPIRED ACTIVE, the fold must win → BlockedByLiveActive,
        //     NOT a takeover into a concurrent run.
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    real_minute,
                    claim_now,
                    deadline_ms,
                    &test_cron_job(job_id),
                    "regress".to_string(),
                )
                .await
                .expect("claim against straggler guard over expired active");
            assert_eq!(
                outcome,
                CronClaimOutcome::BlockedByLiveActiveFolded,
                "with an EXPIRED ACTIVE present, a live post-marker guard must STILL be \
                 folded and block (no-overlap); the `active.is_none()` gate wrongly \
                 took over the expired ACTIVE and double-ran the old-binary fire. The \
                 fold returns the Folded variant so the engine commits the translation"
            );
            txn.commit().await.expect("commit fold");
        }

        // (ii)–(iv) verify the post-state.
        {
            let mut txn = store.begin().await.expect("begin");
            // (ii) the carrier is RE-POINTED at the straggler's run_id (not deleted)
            // so cross-generation no-overlap stays continuous (design 35 §Symmetric
            // bridge).
            let guard_left = tikv_op!(txn.get(guard_key.clone()).await).expect("get guard");
            let guard_run_id = guard_left
                .as_deref()
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(i64::from_be_bytes);
            assert_eq!(
                guard_run_id,
                Some(straggler_run_id),
                "the folded carrier must be RE-POINTED at the straggler's run_id, \
                 not deleted (continuous cross-generation no-overlap)"
            );
            // (iii) exactly one ACTIVE — the folded straggler at the sentinel minute.
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert_eq!(
                actives.len(),
                1,
                "fold must leave exactly one ACTIVE pointer (the straggler), not the \
                 stale one and not a fresh takeover"
            );
            let a = &actives[0];
            assert_eq!(a.job_id, job_id);
            assert_eq!(
                a.run_id, straggler_run_id,
                "the surviving ACTIVE must be the folded straggler, not the stale run \
                 nor a freshly-minted takeover fence"
            );
            assert_eq!(a.fence_token, straggler_run_id);
            assert_eq!(a.active_minute, MIGRATED_MIN);
            assert_eq!(
                a.deadline_ms, deadline_ms,
                "folded ACTIVE must carry this claim's frozen live orphan deadline"
            );
            // The folded straggler's Running CONTROL exists at the sentinel minute.
            let folded_ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, MIGRATED_MIN)
                .await
                .expect("read folded control")
                .expect("folded control must exist");
            assert_eq!(folded_ctrl.state, CronRunState::Running);
            assert_eq!(folded_ctrl.fence_token, straggler_run_id);
            // (iv) the displaced expired ACTIVE's CONTROL(M) is terminalized, not
            //      stranded non-terminal (reaper-unreachable + stale-finalizable).
            let stale_ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, stale_min)
                .await
                .expect("read stale control M")
                .expect("displaced CONTROL(M) must still exist (terminal, not deleted)");
            assert!(
                stale_ctrl.state.is_terminal(),
                "the displaced expired ACTIVE's CONTROL(M) must be terminalized so it is \
                 not reaper-unreachable nor finalizable by its lost-deadline owner's fence"
            );
            txn.rollback().await.ok();
        }

        // (v) a still-alive owner of the displaced minute M presenting its stale
        //     fence is fenced out (CONTROL(M) is terminal) — no non-owner write.
        {
            let mut txn = store.begin().await.expect("begin");
            let run = test_run(job_id, stale_run_id, CronRunStatus::Succeeded);
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    job_id,
                    stale_min,
                    stale_run_id,
                    CronRunState::Succeeded,
                    &run,
                )
                .await
                .expect("finalize displaced M with stale fence");
            assert!(
                !accepted,
                "a stale finalize for the terminalized displaced minute MUST be rejected"
            );
            txn.rollback().await.ok();
        }

        // Once the folded orphan deadline lapses, the same claim is a clean TAKEOVER
        // (schedule progress preserved — no permanent block from the straggler).
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    real_minute,
                    deadline_ms + 1, // past the folded deadline
                    deadline_ms + 1 + orphan_window,
                    &test_cron_job(job_id),
                    "regress".to_string(),
                )
                .await
                .expect("takeover after folded guard lapses");
            assert!(
                matches!(outcome, CronClaimOutcome::TookOver { .. }),
                "once the folded orphan window lapses the claim must TAKE OVER, got {outcome:?}"
            );
            txn.commit().await.expect("commit takeover");
        }
    }

    /// Replay an OLD #2629 binary's no-overlap check (a8b3861b
    /// `try_claim_cron_run` step 1: guard point-get → deref `run_id` → read the
    /// shared `CronRun.status`, block on `Starting|Running`) against the SAME
    /// store, byte-for-byte, with no production dependency on the removed method.
    /// Returns `true` iff the old binary would reject the claim
    /// (`BlockedByRunningGuard`).
    async fn old_binary_blocked_by_running_guard(
        store: &TikvStore,
        db_id: u64,
        job_id: i64,
    ) -> bool {
        let mut txn = store.begin().await.expect("begin old-binary check");
        let guard_key = store.key(&encode_cron_running_guard_key_v2(db_id, job_id));
        let blocked = match tikv_op!(txn.get(guard_key).await).expect("get guard") {
            Some(data) if data.len() == 8 => {
                let run_id = i64::from_be_bytes(data.as_slice().try_into().unwrap());
                match store
                    .get_cron_run(&mut txn, db_id, run_id)
                    .await
                    .expect("get cron run")
                {
                    Some(run) => {
                        matches!(run.status, CronRunStatus::Starting | CronRunStatus::Running)
                    }
                    None => false, // stale guard: old binary deletes and proceeds
                }
            }
            _ => false, // no guard / malformed: old binary proceeds
        };
        txn.rollback().await.ok();
        blocked
    }

    /// THE CLASS TEST (design 35 §Symmetric bridge — the inverse interleaving the
    /// doc never covered). The NEW binary claims fire M (writes ACTIVE + CONTROL
    /// and DUAL-WRITES the legacy running-guard carrier). An OLD #2629 binary then
    /// runs its no-overlap check for a DIFFERENT later fire of the SAME job: it
    /// reads the guard → derefs `run_id` → reads the shared `CronRun.status` the
    /// new binary projected (`Running`) → must return BlockedByRunningGuard. This
    /// closes "block 7": before the dual-write the new binary wrote no guard, so
    /// the old binary saw no carrier and would run CONCURRENTLY (job-level
    /// overlap). FAILS on head c2cb620d (no guard written → not blocked); PASSES
    /// with the dual-write. The companion direction (old writes guard, new folds
    /// it → BlockedByLiveActive) is covered by
    /// `cluster_claim_folds_post_marker_straggler_guard_and_blocks`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_old_binary_sees_new_run_via_dual_written_guard_no_overlap() {
        let store = cron_cas_test_store("dual_guard").await;
        let db_id = 700_011_u64;
        let job_id = 13i64;
        let minute = 28_900_900i64;
        let job = test_cron_job(job_id);

        // NEW binary claims fire M (fresh) — writes CONTROL + ACTIVE + projects a
        // Running CronRun + DUAL-WRITES the carrier, all in one txn.
        let fence = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    minute,
                    1_000,
                    301_000, // live orphan deadline
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("new binary claim M");
            txn.commit().await.expect("commit claim M");
            match outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("claim M must be Claimed, got {other:?}"),
            }
        };

        // The dual-written carrier exists and names the new fence (== run_id).
        {
            let mut txn = store.begin().await.expect("begin");
            let guard_key = store.key(&encode_cron_running_guard_key_v2(db_id, job_id));
            let data = tikv_op!(txn.get(guard_key).await)
                .expect("get guard")
                .expect("carrier must be dual-written by the new claim");
            assert_eq!(
                i64::from_be_bytes(data.as_slice().try_into().unwrap()),
                fence,
                "the dual-written carrier must point at the new fence (== run_id)"
            );
            txn.rollback().await.ok();
        }

        // THE ASSERTION: an OLD #2629 binary's no-overlap check for a DIFFERENT
        // later fire of this job MUST be blocked, reading liveness from the shared
        // Running CronRun via the carrier. This is the interleaving block 7 left
        // open. (Pre-fix: no carrier → not blocked → the old binary double-runs.)
        assert!(
            old_binary_blocked_by_running_guard(&store, db_id, job_id).await,
            "an OLD #2629 binary must see the new-binary run via the dual-written \
             carrier and block (cross-generation no-overlap); without the dual-write \
             it sees no guard and runs concurrently — the block-7 hole"
        );

        // After the new binary FINALIZES the run, the carrier is cleared (the
        // shared CronRun is now terminal), so the old binary is no longer blocked
        // — schedule progress preserved, no stale guard left to honor forever.
        {
            let mut txn = store.begin().await.expect("begin");
            let run = test_run(job_id, fence, CronRunStatus::Succeeded);
            let accepted = store
                .finalize_cron_run_cas(
                    &mut txn,
                    db_id,
                    job_id,
                    minute,
                    fence,
                    CronRunState::Succeeded,
                    &run,
                )
                .await
                .expect("finalize M");
            assert!(accepted, "owner finalize must be accepted");
            txn.commit().await.expect("commit finalize");
        }
        assert!(
            !old_binary_blocked_by_running_guard(&store, db_id, job_id).await,
            "after finalize the carrier must be cleared (fence-matched), so the old \
             binary is no longer blocked — no stale guard honored forever"
        );
    }

    /// ATOMICITY (design 35 §Symmetric bridge). The carrier dual-write commits in
    /// ONE pessimistic txn with CONTROL + ACTIVE (single tenant keyspace), so it
    /// can never be orphaned: on ROLLBACK no guard is present; on COMMIT
    /// `guard.run_id == ACTIVE.run_id == CONTROL.fence_token == fence`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_dual_written_guard_is_atomic_with_control_and_active() {
        let store = cron_cas_test_store("guard_atomic").await;
        let db_id = 700_013_u64;
        let job_id = 17i64;
        let minute = 28_901_000i64;
        let job = test_cron_job(job_id);

        // ROLLBACK: claim then roll back — no carrier, no ACTIVE, no CONTROL.
        {
            let mut txn = store.begin().await.expect("begin");
            store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    minute,
                    1_000,
                    301_000,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("claim (to be rolled back)");
            txn.rollback().await.ok();
        }
        {
            let mut txn = store.begin().await.expect("begin");
            assert_eq!(
                store
                    .read_cron_running_guard(&mut txn, db_id, job_id)
                    .await
                    .expect("read carrier"),
                None,
                "on rollback the carrier must NOT be present (atomic with the claim)"
            );
            assert!(
                store
                    .list_cron_active(&mut txn, db_id)
                    .await
                    .expect("list active")
                    .is_empty(),
                "on rollback ACTIVE must NOT be present"
            );
            txn.rollback().await.ok();
        }

        // COMMIT: claim and commit — carrier, ACTIVE, CONTROL all name the fence.
        let fence = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    minute,
                    1_000,
                    301_000,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("claim (committed)");
            txn.commit().await.expect("commit claim");
            match outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("claim must be Claimed, got {other:?}"),
            }
        };
        {
            let mut txn = store.begin().await.expect("begin");
            let guard = store
                .read_cron_running_guard(&mut txn, db_id, job_id)
                .await
                .expect("read carrier")
                .expect("carrier present after commit");
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert_eq!(actives.len(), 1, "one ACTIVE after commit");
            let ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, minute)
                .await
                .expect("read control")
                .expect("control present");
            assert_eq!(
                (guard, actives[0].run_id, ctrl.fence_token),
                (fence, fence, fence),
                "guard.run_id == ACTIVE.run_id == CONTROL.fence_token == fence"
            );
            txn.rollback().await.ok();
        }
    }

    #[test]
    fn decode_legacy_guard_roundtrips_and_rejects_malformed() {
        let db_id = 12u64;
        let job_id = 9i64;
        let run_id = 4242i64;
        let mut key = encode_cron_running_guard_prefix_v2(db_id);
        key.extend_from_slice(&job_id.to_be_bytes());
        let prefix = encode_cron_running_guard_prefix_v2(db_id);
        let value = run_id.to_be_bytes().to_vec();
        assert_eq!(
            decode_legacy_guard(&prefix, &key, &value),
            Some((job_id, run_id))
        );
        // Malformed value (not 8 bytes) and malformed key (wrong suffix len).
        assert_eq!(decode_legacy_guard(&prefix, &key, &[1u8]), None);
        let mut short_key = prefix.clone();
        short_key.extend_from_slice(&[1u8, 2, 3]);
        assert_eq!(decode_legacy_guard(&prefix, &short_key, &value), None);
        // Wrong prefix.
        assert_eq!(decode_legacy_guard(b"zzz", &key, &value), None);
    }

    #[test]
    fn decode_legacy_claim_roundtrips_and_rejects_malformed() {
        let db_id = 12u64;
        let job_id = 9i64;
        let minute = 28_900_123i64;
        let prefix = encode_cron_claim_prefix_v2(db_id);
        let mut key = prefix.clone();
        key.extend_from_slice(&job_id.to_be_bytes());
        key.push(b'_');
        key.extend_from_slice(&minute.to_be_bytes());
        assert_eq!(decode_legacy_claim(&prefix, &key), Some((job_id, minute)));
        // Missing separator / wrong length.
        let mut bad = prefix.clone();
        bad.extend_from_slice(&job_id.to_be_bytes());
        bad.extend_from_slice(&minute.to_be_bytes());
        assert_eq!(decode_legacy_claim(&prefix, &bad), None);
        // Wrong prefix.
        assert_eq!(decode_legacy_claim(b"zzz", &key), None);
    }

    fn test_run(job_id: i64, run_id: i64, status: CronRunStatus) -> CronRun {
        CronRun {
            run_id,
            job_id,
            job_pid: None,
            database: "regress".to_string(),
            username: "admin".to_string(),
            command: "SELECT 1".to_string(),
            status,
            return_message: None,
            start_time: Some(0),
            end_time: Some(1),
        }
    }

    /// REGRESSION (design 35 §One effective deadline + cross-generation liveness).
    /// A BULK-migrated guard for a LONG-RUNNING job must inherit the SAME effective
    /// orphan deadline a fresh claim / the per-claim straggler fold use —
    /// `now + max(global_orphan_timeout, job.max_runtime)` — NOT `now + global`.
    /// When the bulk path stamped only `now + global`, a job whose `max_runtime`
    /// (1h) far exceeds the global window (5m) had its migrated authority expire at
    /// `+5m` while the OLD-binary run was still legitimately executing; a new-binary
    /// claim for a later fire then saw the EXPIRED ACTIVE, took over, and ran
    /// CONCURRENTLY with the still-live old run (mixed-version job-level overlap).
    ///
    /// Setup: a job with `max_runtime_ms = 1h`; a planted legacy running-guard from
    /// an "old binary" run plus that run's `CronRun(Running)` (the cross-generation
    /// liveness truth both binaries share). Then:
    ///   * migrate the db: the migrated ACTIVE/CONTROL deadline MUST be
    ///     `migrate_now + max(global, max_runtime)` = `migrate_now + 1h`, not
    ///     `migrate_now + global`;
    ///   * advance the clock PAST `global` but BEFORE `max_runtime`: a new-binary
    ///     claim for a later fire MUST be `BlockedByLiveActive` (no takeover / no
    ///     concurrent run) — the job's full legitimate runtime is still protected;
    ///   * after `max_runtime` (now genuinely dead — also flip the `CronRun` to
    ///     terminal so the cross-generation liveness backstop agrees it is done): the
    ///     same claim is a clean `TookOver` (schedule progress is preserved; a dead
    ///     run is not blocked on forever).
    ///
    /// Pre-fix (`now + global` in the bulk path) this fails at the mid-window claim:
    /// the deadline is `migrate_now + global`, so at `migrate_now + global + 1` the
    /// ACTIVE is expired and `decide_cron_claim` takes over → `TookOver` while the
    /// old run is still live.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_bulk_migrated_long_job_guard_uses_job_deadline_no_premature_takeover() {
        let store = cron_cas_test_store("migrate_longjob").await;
        let db_id = 700_010_u64;
        let job_id = 11i64;
        let run_id = 909i64;
        const MIGRATED_MIN: i64 = 0;

        let migrate_now = 1_000_000i64;
        let global_orphan = 300_000i64; // 5 min — the global orphan timeout floor
        let max_runtime = 3_600_000i64; // 1 h — far exceeds the global window
                                        // The effective deadline both migration paths MUST agree on.
        let expected_deadline = migrate_now + global_orphan.max(max_runtime); // = migrate_now + 1h
                                                                              // The (wrong) deadline the bulk path used pre-fix.
        let buggy_global_deadline = migrate_now + global_orphan;

        // Persist the catalog job (with the long max_runtime) so the migration can
        // resolve max_runtime_ms inside its own txn — this is what the fix adds.
        let mut job = test_cron_job(job_id);
        job.max_runtime_ms = Some(u64::try_from(max_runtime).unwrap());
        {
            let mut txn = store.begin().await.expect("begin");
            store
                .put_cron_job(&mut txn, db_id, &job)
                .await
                .expect("put job");
            // Plant the legacy running-guard (8-byte run_id value) — no per-key
            // encoder survives, so extend the prefix as the bulk path expects.
            let mut legacy_guard = encode_cron_running_guard_prefix_v2(db_id);
            legacy_guard.extend_from_slice(&job_id.to_be_bytes());
            txn_put(
                &mut txn,
                store.key(&legacy_guard),
                run_id.to_be_bytes().to_vec(),
            )
            .await
            .expect("plant legacy guard");
            // Plant the OLD-binary run's shared CronRun, STILL RUNNING — the
            // cross-generation liveness truth. Same v2 run key the old #2629 binary
            // wrote (a8b3861b), keyed by the guard's run_id.
            store
                .put_cron_run(
                    &mut txn,
                    db_id,
                    &test_run(job_id, run_id, CronRunStatus::Running),
                )
                .await
                .expect("plant running CronRun");
            txn.commit().await.expect("commit plant");
        }

        // Seed DB-liveness + cron-enabled so the fenced migration commits (the P2
        // migration fence).
        plant_live_db_and_enable_cron(&store, db_id, "regress_migrate_longjob").await;

        // Bulk migration: resolves the job's max_runtime inside its txn and stamps
        // the migrated guard with the SHARED effective deadline.
        store
            .ensure_cron_control_migrated(db_id, migrate_now, global_orphan, global_orphan as u64)
            .await
            .expect("migrate");

        // The migrated ACTIVE + CONTROL must carry the JOB-EFFECTIVE deadline, not
        // the bare global one — the direct regression assertion.
        {
            let mut txn = store.begin().await.expect("begin");
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert_eq!(
                actives.len(),
                1,
                "guard must translate to one ACTIVE pointer"
            );
            let a = &actives[0];
            assert_eq!(a.run_id, run_id);
            assert_eq!(a.active_minute, MIGRATED_MIN);
            assert_eq!(
                a.deadline_ms, expected_deadline,
                "migrated ACTIVE must use now + max(global, max_runtime), not now + global"
            );
            assert_ne!(
                a.deadline_ms, buggy_global_deadline,
                "migrated ACTIVE must NOT expire at now + global for a long job"
            );
            let ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, MIGRATED_MIN)
                .await
                .expect("read control")
                .expect("migrated control must exist");
            assert_eq!(
                ctrl.deadline_ms, expected_deadline,
                "CONTROL must share the deadline"
            );
            txn.rollback().await.ok();
        }

        // Mid-window: PAST global, BEFORE max_runtime. The old run is still validly
        // running, so a new-binary claim for a LATER fire MUST be blocked — no
        // takeover, no concurrent run. (Pre-fix the deadline was buggy_global_deadline
        // and this took over.)
        let mid_now = buggy_global_deadline + 1; // > now+global, < now+max_runtime
        assert!(
            mid_now < expected_deadline,
            "test setup: mid_now before effective deadline"
        );
        let later_minute = 28_900_001i64; // a real epoch-minute, distinct from sentinel 0
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    later_minute,
                    mid_now,
                    cron_effective_orphan_deadline_ms(
                        mid_now,
                        global_orphan,
                        job.max_runtime_ms,
                        global_orphan as u64,
                    ),
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("mid-window claim");
            assert_eq!(
                outcome,
                CronClaimOutcome::BlockedByLiveActive,
                "a still-live long job's migrated authority must BLOCK a later-fire claim \
                 before max_runtime — not take over and double-run"
            );
            txn.rollback().await.ok();
        }

        // After max_runtime the run is genuinely dead. Flip the shared CronRun to
        // terminal so the cross-generation liveness backstop also agrees the run is
        // done — then the same claim must TAKE OVER (schedule progress preserved, a
        // dead run is not blocked on forever).
        {
            let mut txn = store.begin().await.expect("begin");
            store
                .put_cron_run(
                    &mut txn,
                    db_id,
                    &test_run(job_id, run_id, CronRunStatus::Failed),
                )
                .await
                .expect("terminalize old CronRun");
            txn.commit().await.expect("commit terminalize");
        }
        let dead_now = expected_deadline + 1; // past the effective deadline
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    later_minute,
                    dead_now,
                    cron_effective_orphan_deadline_ms(
                        dead_now,
                        global_orphan,
                        job.max_runtime_ms,
                        global_orphan as u64,
                    ),
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("post-deadline takeover");
            assert!(
                matches!(outcome, CronClaimOutcome::TookOver { .. }),
                "once the job-effective deadline lapses AND the run is terminal the claim \
                 must TAKE OVER (schedule progress), got {outcome:?}"
            );
            txn.commit().await.expect("commit takeover");
        }
    }

    /// REGRESSION (design 35 §Effective floor). A DEFAULT cron job (no per-job
    /// `max_runtime_ms`) has a legitimate EXECUTION window of `cron_job_timeout_ms`
    /// (the claim_and_execute path runs it until `max_runtime_ms.unwrap_or(
    /// cron_job_timeout_ms)`), which is far longer than the bare control
    /// `orphan_timeout_sec`. The frozen CONTROL/ACTIVE orphan floor must therefore
    /// be `max(orphan_timeout, cron_job_timeout)` (the shared
    /// `effective_cron_orphan_floor_ms`), NOT raw `orphan_timeout`.
    ///
    /// Setup: a default job; claim fire M with the EFFECTIVE floor. Then:
    ///   * PAST `orphan_timeout` (5 min) but BEFORE `cron_job_timeout` (30 min): a
    ///     later-fire claim MUST be `BlockedByLiveActive` — the ACTIVE is still live
    ///     because its frozen deadline covers the full execution window; no new
    ///     fence is minted, no takeover, no overlap with the still-running job;
    ///   * PAST `cron_job_timeout` (genuinely overran): the same claim MUST
    ///     `TookOver` — a dead run is not blocked on forever (schedule progress).
    ///
    /// Pre-fix the claim path stamped the deadline from raw `orphan_timeout` only,
    /// so at `now + orphan_timeout + 1` the still-running default job's ACTIVE was
    /// classed expired and the later fire took over and double-ran. The mid-window
    /// `BlockedByLiveActive` assertion below fails pre-fix.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_default_job_active_deadline_covers_cron_job_timeout_no_premature_takeover() {
        let store = cron_cas_test_store("default_job_floor").await;
        let db_id = 700_011_u64;
        let job_id = 12i64;
        let minute = 28_900_000i64;
        // A DEFAULT job: no per-job max_runtime, so its legitimate execution window
        // is `cron_job_timeout_ms`.
        let job = test_cron_job(job_id);
        assert_eq!(job.max_runtime_ms, None, "test setup: default job");

        plant_live_db_and_enable_cron(&store, db_id, "regress_default_job_floor").await;

        // Defaults: orphan_timeout 5 min, cron_job_timeout 30 min.
        let orphan_timeout_sec = 300u64; // 5 min — the bare control timeout
        let cron_job_timeout_ms = 1_800_000u64; // 30 min — the execution window
        let claim_now = 1_000_000i64;

        // The shared effective floor: a default job's frozen deadline MUST cover the
        // execution window, so the floor is cron_job_timeout (> orphan_timeout).
        let floor_ms = crate::worker::gc::effective_cron_orphan_floor_ms(
            orphan_timeout_sec,
            cron_job_timeout_ms,
        );
        assert_eq!(
            floor_ms, cron_job_timeout_ms as i64,
            "default-job floor must be max(orphan_timeout, cron_job_timeout) = cron_job_timeout"
        );
        // The (wrong) deadline window the claim path used pre-fix: raw orphan_timeout.
        let buggy_orphan_window_ms = (orphan_timeout_sec as i64) * 1000;
        let expected_deadline = cron_effective_orphan_deadline_ms(
            claim_now,
            floor_ms,
            job.max_runtime_ms,
            cron_job_timeout_ms,
        );
        assert_eq!(
            expected_deadline,
            claim_now + cron_job_timeout_ms as i64,
            "default-job frozen deadline = now + cron_job_timeout"
        );

        // Claim fire M with the effective floor. The frozen ACTIVE/CONTROL deadline
        // must cover the full execution window.
        let f1 = {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    minute,
                    claim_now,
                    expected_deadline,
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("first claim");
            txn.commit().await.expect("commit claim");
            match outcome {
                CronClaimOutcome::Claimed { run } => run.run_id,
                other => panic!("first claim must be Claimed, got {other:?}"),
            }
        };

        // The frozen ACTIVE deadline must span the execution window, not the bare
        // orphan timeout — the direct regression assertion.
        let f1_fence = {
            let mut txn = store.begin().await.expect("begin");
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert_eq!(actives.len(), 1, "claim must write one ACTIVE pointer");
            let a = &actives[0];
            assert_eq!(
                a.deadline_ms, expected_deadline,
                "ACTIVE deadline must be now + cron_job_timeout for a default job"
            );
            assert_ne!(
                a.deadline_ms,
                claim_now + buggy_orphan_window_ms,
                "ACTIVE deadline must NOT expire at now + orphan_timeout for a default job"
            );
            txn.rollback().await.ok();
            a.fence_token
        };

        // Mid-window: PAST orphan_timeout, BEFORE cron_job_timeout. The default job
        // is still validly running, so a later-fire claim MUST be blocked — no
        // takeover, no new fence. (Pre-fix the deadline was claim_now +
        // orphan_timeout and this took over.)
        let mid_now = claim_now + buggy_orphan_window_ms + 1; // > now+orphan, < now+cron_job_timeout
        assert!(
            mid_now < expected_deadline,
            "test setup: mid_now past orphan_timeout but before the effective deadline"
        );
        let later_minute = minute + 60_000; // a distinct later epoch-minute
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    later_minute,
                    mid_now,
                    cron_effective_orphan_deadline_ms(
                        mid_now,
                        floor_ms,
                        job.max_runtime_ms,
                        cron_job_timeout_ms,
                    ),
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("mid-window claim");
            assert_eq!(
                outcome,
                CronClaimOutcome::BlockedByLiveActive,
                "a still-running default job must BLOCK a later-fire claim before \
                 cron_job_timeout — not take over and double-run"
            );
            txn.rollback().await.ok();
        }

        // No new fence was minted by the blocked claim: ACTIVE still holds F1's fence.
        {
            let mut txn = store.begin().await.expect("begin");
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert_eq!(actives.len(), 1, "blocked claim must not add an ACTIVE");
            assert_eq!(
                actives[0].fence_token, f1_fence,
                "blocked claim must NOT mint a new fence"
            );
            assert_eq!(
                actives[0].run_id, f1,
                "ACTIVE must still point at the live run"
            );
            txn.rollback().await.ok();
        }

        // Past cron_job_timeout the run has genuinely overrun. Terminalize the shared
        // CronRun so the cross-generation liveness backstop agrees it is done, then
        // the same claim must TAKE OVER (schedule progress preserved).
        {
            let mut txn = store.begin().await.expect("begin");
            store
                .put_cron_run(
                    &mut txn,
                    db_id,
                    &test_run(job_id, f1, CronRunStatus::Failed),
                )
                .await
                .expect("terminalize CronRun");
            txn.commit().await.expect("commit terminalize");
        }
        let dead_now = expected_deadline + 1; // past now + cron_job_timeout
        {
            let mut txn = store.begin().await.expect("begin");
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut txn,
                    db_id,
                    job_id,
                    later_minute,
                    dead_now,
                    cron_effective_orphan_deadline_ms(
                        dead_now,
                        floor_ms,
                        job.max_runtime_ms,
                        cron_job_timeout_ms,
                    ),
                    &job,
                    "regress".to_string(),
                )
                .await
                .expect("post-deadline takeover");
            assert!(
                matches!(outcome, CronClaimOutcome::TookOver { .. }),
                "once the default job's full execution window lapses AND the run is \
                 terminal the claim must TAKE OVER (schedule progress), got {outcome:?}"
            );
            txn.commit().await.expect("commit takeover");
        }
    }

    /// REGRESSION (design 35 §Migration fence — the P2 migration fence). The
    /// migration gate must NOT recreate control-plane state for a cron that has been
    /// dropped. `DROP EXTENSION pg_cron` removes the cron-enabled marker AND deletes
    /// all cron keys (`delete_all_cron_data`). If a migration runs after that drop
    /// committed, recreating CONTROL/ACTIVE/marker from leftover legacy keys would
    /// resurrect orphaned cron state for an extension that no longer exists. The
    /// cron-disabled fence inside `ensure_cron_control_migrated` makes the migration
    /// a no-op when cron is disabled.
    ///
    /// Setup mirrors the post-DROP race: a live DB row exists, but cron is DISABLED
    /// (the enabled marker absent), and a leftover legacy guard key is present (as a
    /// rolling-window straggler an old binary could write). Then run the migration
    /// and assert it wrote NO marker, NO ACTIVE, NO CONTROL — nothing orphaned.
    ///
    /// Pre-fix (no cron-enabled gate) the migration translated the leftover guard
    /// into ACTIVE + CONTROL and wrote the marker, leaving orphaned control-plane
    /// state for the dropped extension; this test's "no migrated keys" assertions
    /// then fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_migration_skips_when_cron_disabled_no_orphaned_keys() {
        let store = cron_cas_test_store("migrate_dropped").await;
        let db_id = 700_011_u64;
        let job_id = 13i64;
        let run_id = 777i64;

        // Live DB row, but cron is DISABLED (simulating DROP EXTENSION pg_cron has
        // already committed: it removed the cron-enabled marker). Plant a leftover
        // legacy running-guard a post-drop straggler could have left behind.
        {
            let mut txn = store.begin().await.expect("begin");
            let def = crate::model::DatabaseDef::new(
                db_id,
                "regress_migrate_dropped".to_string(),
                "admin".to_string(),
            );
            txn_put(
                &mut txn,
                store.key(&encode_database_id_key(db_id)),
                bincode::serialize(&def).expect("serialize def"),
            )
            .await
            .expect("plant db row");
            // NOTE: deliberately NOT calling set_cron_enabled — cron is dropped.
            let mut legacy_guard = encode_cron_running_guard_prefix_v2(db_id);
            legacy_guard.extend_from_slice(&job_id.to_be_bytes());
            txn_put(
                &mut txn,
                store.key(&legacy_guard),
                run_id.to_be_bytes().to_vec(),
            )
            .await
            .expect("plant leftover legacy guard");
            txn.commit().await.expect("commit plant");
        }

        // Run the migration gate against the dropped cron.
        store
            .ensure_cron_control_migrated(db_id, 1_000_000, 300_000, 300_000)
            .await
            .expect("migrate (no-op for disabled cron)");

        // No control-plane state may have been recreated: no marker, no ACTIVE, no
        // CONTROL at the sentinel minute.
        {
            let mut txn = store.begin().await.expect("begin");
            let marker = tikv_op!(
                txn.get(store.key(&encode_cron_migrated_key_v3(db_id)))
                    .await
            )
            .expect("get marker");
            assert!(
                marker.is_none(),
                "the migration must NOT write a marker for a dropped/disabled cron"
            );
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert!(
                actives.is_empty(),
                "the migration must NOT recreate an ACTIVE pointer for a dropped cron, got {actives:?}"
            );
            const MIGRATED_MIN: i64 = 0;
            let ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, MIGRATED_MIN)
                .await
                .expect("read control");
            assert!(
                ctrl.is_none(),
                "the migration must NOT recreate a CONTROL record for a dropped cron"
            );
            txn.rollback().await.ok();
        }
    }

    /// P1 REGRESSION (concurrent DROP EXTENSION pg_cron vs a claim/migration that
    /// already read cron ENABLED). The static `cluster_migration_skips_when_cron_disabled`
    /// test only covers a cron that is ALREADY disabled before the write txn opens —
    /// the plain snapshot read catches that. This test covers the live RACE: a
    /// control-plane write txn reads `is_cron_enabled` ENABLED on its snapshot, THEN
    /// a concurrent `DROP EXTENSION pg_cron` commits `remove_cron_enabled` +
    /// `delete_all_cron_data`, THEN the write txn proceeds to its commit.
    ///
    /// With the OLD plain-snapshot gate the write txn never re-checks the marker, so
    /// it commits CONTROL/ACTIVE for a cron that no longer exists — orphaned state
    /// the disabled-DB GC skip (`gc_database` returns early when cron is disabled)
    /// never reaps. The fix makes the AUTHORITATIVE gate a `get_for_update`
    /// (`is_cron_enabled_for_update`) taken in the SAME txn as the control-plane
    /// writes: the fenced read either sees the marker already gone (committed DROP →
    /// abort the claim) or write-write-conflicts the DROP's delete (one side aborts).
    /// Either way no orphaned CONTROL/ACTIVE survives. Pre-fix the final assertions
    /// (no ACTIVE / no CONTROL) fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TiKV / PD cluster"]
    async fn cluster_claim_fences_cron_enabled_against_drop_extension() {
        let store = cron_cas_test_store("claim_vs_drop").await;
        let db_id = 700_012_u64;
        let job_id = 21i64;
        let m = 28_900_000i64;
        let job = test_cron_job(job_id);

        // Live DB row + cron ENABLED — the pre-race state a worker observes.
        plant_live_db_and_enable_cron(&store, db_id, "regress_claim_vs_drop").await;

        // T1 = the claim write txn. Open it and read cron-enabled on its snapshot
        // (the cheap fast path) — it sees ENABLED, as a real claim does before the
        // concurrent DROP commits.
        let mut claim_txn = store.begin().await.expect("begin claim txn");
        assert!(
            store
                .is_cron_enabled(&mut claim_txn, db_id)
                .await
                .expect("snapshot enabled read"),
            "precondition: cron must read enabled on the claim txn's snapshot"
        );

        // T2 = a concurrent DROP EXTENSION pg_cron that commits BETWEEN T1's snapshot
        // read and T1's control-plane commit: remove the enabled marker and wipe all
        // cron data, exactly as `execute_drop_extension_cmd` does.
        {
            let mut drop_txn = store.begin().await.expect("begin drop txn");
            store
                .remove_cron_enabled(&mut drop_txn, db_id)
                .await
                .expect("remove cron enabled");
            store
                .delete_all_cron_data(&mut drop_txn, db_id)
                .await
                .expect("delete all cron data");
            drop_txn.commit().await.expect("commit concurrent DROP");
        }

        // T1 now reaches its control-plane writes + authoritative fence, mirroring
        // the production claim arm: write CONTROL/ACTIVE, then the
        // `is_cron_enabled_for_update` gate, then commit only if still enabled.
        let claim_outcome: Result<bool> = async {
            let outcome = store
                .claim_or_takeover_cron_run(
                    &mut claim_txn,
                    db_id,
                    job_id,
                    m,
                    1000,
                    10_000_000,
                    &job,
                    "regress".to_string(),
                )
                .await?;
            assert!(
                matches!(outcome, CronClaimOutcome::Claimed { .. }),
                "claim wrote CONTROL/ACTIVE on its snapshot, got {outcome:?}"
            );
            // AUTHORITATIVE cron-disabled fence — the fix. Under `get_for_update`
            // this observes the committed DROP's marker delete and reports disabled.
            if !store
                .is_cron_enabled_for_update(&mut claim_txn, db_id)
                .await?
            {
                return Ok(false); // disabled -> abort the claim (no commit)
            }
            Ok(true)
        }
        .await;

        match claim_outcome {
            // Fence saw cron disabled (or a write-write conflict surfaced as Err):
            // roll back so NO control-plane state is committed.
            Ok(false) | Err(_) => {
                claim_txn.rollback().await.ok();
            }
            Ok(true) => {
                // If the fence said "still enabled" the only way the marker can have
                // been re-checked under get_for_update yet still be present is if the
                // DROP's write-write conflict forced THIS commit to fail instead.
                // Either path must NOT leave orphaned state, so attempt the commit
                // and let it abort if it conflicts.
                claim_txn.commit().await.ok();
            }
        }

        // Whatever interleaving the fence forced, cron is disabled and NO orphaned
        // CONTROL/ACTIVE may remain for the dropped extension.
        {
            let mut txn = store.begin().await.expect("begin verify");
            assert!(
                !store
                    .is_cron_enabled(&mut txn, db_id)
                    .await
                    .expect("read enabled"),
                "DROP EXTENSION pg_cron committed: cron must be disabled"
            );
            let actives = store
                .list_cron_active(&mut txn, db_id)
                .await
                .expect("list active");
            assert!(
                actives.is_empty(),
                "claim must leave NO orphaned ACTIVE pointer after a concurrent DROP, got {actives:?}"
            );
            let ctrl = store
                .read_cron_control(&mut txn, db_id, job_id, m)
                .await
                .expect("read control");
            assert!(
                ctrl.is_none(),
                "claim must leave NO orphaned CONTROL record after a concurrent DROP"
            );
            txn.rollback().await.ok();
        }
    }
}
