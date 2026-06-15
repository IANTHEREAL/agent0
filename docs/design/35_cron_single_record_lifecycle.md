# 35 — Cron run-lifecycle: single-lifecycle, fence-token control plane

> **Status**: Draft — in progress (branch `fix/cron-single-record-fence-2631`). Tracks #2631 (follow-up to merged #2629).
Supersedes the ad-hoc cron control state in `src/storage/tikv_store/cron.rs` /
`src/worker/engine.rs` / `src/cron/worker.rs`.

## Problem

The cron control plane spreads ONE logical fire across THREE independent tenant keys with no
shared compare-and-set:

1. **per-minute claim** `sys_cron_claim_{job_id}_{min}` — a dumb `vec![1]` presence flag
   (per-FIRE dedup).
2. **running guard** `sys_cron_running_guard_{job_id}` — value = active `run_id`
   (job-level NO-OVERLAP across minutes).
3. **run-history record** `sys_cron_run_{run_id}` — `CronRun{status,...}` (the
   `cron.job_run_details` surface).

Because `finalize_cron_run` and the orphan reaper (`gc_database_batch_inner`) each touch only
*some* of these keys, via separate primitives, two correctness defects are structurally possible:

- **DEFECT 1 — double execution.** Finalize commits terminal cron state in a tenant txn fenced
  only by DB-liveness; the ownership pre-check (`is_worker_claim_owned_by`) is a rolled-back,
  `worker_id`-only read in a *system-store* txn — a cross-store TOCTOU. A worker that lost its
  lease can still commit terminal state; a takeover worker then re-runs the SQL.
- **DEFECT 2 — silent skip.** The orphan reaper flips a stuck run `Running→Failed` via
  `put_cron_run` but never clears the per-minute claim. The next claim hits
  `AlreadyClaimedForMinute` → the due row is dropped **without requeue** → one scheduled fire is
  silently lost (recovered only by the next registry sweep). *(Still live on master `a8b3861b`:
  the merged fix only added `clear_cron_claim` to the finalize happy-path, not the reaper.)*

Exactly-once for arbitrary cron SQL is **provably impossible** (the effect→marker crash gap; the
worker queue (system keyspace) and the effect (tenant keyspace) can never share one TiKV txn).
pg_cron itself does not promise it. This design therefore targets the strongest *achievable*
contract, validated against Temporal, AWS SWF, Restate, Oban, Quartz, Google "Reliable Cron",
Kubernetes CronJob, and Kleppmann's fencing-token result.

## Principle

Put the dedup authority **in the same store as the effect**, key it on the **logical work**, make
it the **single monotonic source of truth**, and let TiKV's pessimistic write-conflict be the
fence. The system-store `WorkerClaim`+lease governs **work distribution only** (all 6 task types),
never cron correctness.

## Data model — two tenant keys + one (frozen) history record

The running guard is `(db,job)`-scoped while the per-minute claim is `(db,job,minute)`-scoped — they
answer *different* questions (no-overlap vs per-fire dedup). A single collapsed key cannot retain a
past minute's terminal dedup marker AND a new minute's live state, so **two keys** are kept, both
written/cleared *only* inside the three CAS helpers (no public set/clear primitives), so neither can
be orphaned:

- **KEY A — CONTROL record**, per `(db_id, job_id, scheduled_min)`. Per-fire dedup + run state.
  `encode_cron_control_key_v2` (two fixed 8-byte fields, no `_` separator → clean prefix scans).
  Value `CronRunControl { state: CronRunState, fence_token: i64, run_id: i64, started_ms, deadline_ms, finalize_seq }`.
  Supersedes the dumb `vec![1]` claim flag.
- **KEY B — ACTIVE-RUN pointer**, per `(db_id, job_id)`. Job-level no-overlap. `encode_cron_active_key_v2`.
  Value `CronActiveRun { job_id, active_minute, run_id, fence_token, deadline_ms }`. Supersedes the
  running guard; O(1) point-get answering "is *any* minute of this job live?".
- **KEY C — HISTORY record** (`encode_cron_run_key_v2`, **unchanged key + frozen `CronRun` bincode
  shape** = the `cron.job_run_details` contract). Becomes a *projection* written inside the same CAS
  txn as a terminal transition — never the correctness authority.

`fence_token == run_id`, minted **inside the claim txn** via `bump_next_cron_run_id_in_txn`
(`get_for_update` on the existing per-db `next_cron_run_id` seq + `checked_add` + `txn_put`), so
"`fence_token` == this txn's minted run_id" is a store-guaranteed invariant. (Correction to the
original #2631 note: the seq is in the **tenant** keyspace — same store as the control record — so
the mint is cross-*txn*-same-*store*, **not** cross-store, and is foldable into the claim txn. The
"cross-store mint" residual was inaccurate.)

## State machine (every transition = one pessimistic tenant txn)

`begin()` → `get_for_update(control)` + `get_for_update(active)` → evaluate predicate → conditionally
`txn_put` both (+ project `CronRun` on terminal) → **`is_cron_enabled_for_update`** (only when the txn
CREATES/translates new authority — claim, takeover, straggler fold; see §Cron-disabled fence) →
`assert_database_alive_for_update` (preserve the DROP-DATABASE liveness fence) → `commit`.
`get_for_update` gives write-write conflict detection, so concurrent claim/takeover/reaper serialize;
the loser aborts.

### Cron-disabled fence (DROP EXTENSION pg_cron)

`DROP EXTENSION pg_cron` commits `remove_cron_enabled` (delete the cron-enabled marker) +
`delete_all_cron_data`, and it does **not** touch the DB metadata row — so the DROP-DATABASE liveness
fence does **not** catch it. The cron-enabled marker is therefore the serialization point a control-plane
write txn must fence on. The cheap `is_cron_enabled` snapshot read used as an admission fast-path does
**not** conflict with `remove_cron_enabled`, so a claim/migration that plain-read `enabled` then had a
concurrent DROP commit would still write `CONTROL`/`ACTIVE`/`CronRun`/marker — orphaned control-plane
state for a cron that no longer exists, which the disabled-DB GC skip (`gc_database` returns early when
cron is disabled) never reaps.

The authoritative gate is **`is_cron_enabled_for_update`** (mirrors `database_alive_for_update`:
`get_for_update` on `encode_cron_enabled_key_v2`), taken in the SAME txn as the control-plane writes.
The fenced read either sees the marker already gone (committed DROP → abort, no writes) or
write-write-conflicts the DROP's marker delete (one side aborts). Apply it only where a txn **creates or
translates** new authority; finalize/reap of EXISTING state must still be allowed to clean up when cron
is disabled. Sites:

| Write txn | Creates/translates authority? | Cron-enabled-for-update fence |
|---|---|---|
| Claim/takeover (`claim_and_record_cron_run` commit arm) | yes — writes `CONTROL`/`ACTIVE`/`CronRun` | **yes** |
| Straggler fold (same path, blocked-but-folded arm) | yes — translates legacy guard → `ACTIVE`/`CONTROL` | **yes** |
| Bulk migration (`ensure_cron_control_migrated`) | yes — translates legacy guard/claim → `CONTROL`/`ACTIVE`/marker | **yes** |
| Finalize (`finalize_cron_run`/`_cas`) | no — terminalizes the run it owns | no (DB-liveness only) |
| Reaper (`reap_stale_active_runs`) | no — finalizes EXISTING orphaned `ACTIVE`/`CONTROL` | no (DB-liveness only) |
| Re-enqueue next fire (`load_next_cron_queue_entry`) | no — produces a system-store queue entry, not control-plane authority; the next claim is fenced | no (DB-liveness + snapshot read) |

The engine cleanup re-enqueues the next fire whenever the claimed minute is **DONE** — i.e. for the ran-it outcomes (`Claimed`/`TookOver`) **and** for `AlreadyTerminalForMinute` (see §Schedule progress on a reaper-recovered crash). It does **not** re-enqueue for a still-live block (`BlockedByLiveActive`/`Folded`, the live run will requeue when it finalizes) or for any not-actionable claim (cron disabled, job gone/inactive, stale payload, DB dropped). The decision lives in one place (`cron_outcome_requeues_next_fire`, engine.rs) keyed on the `CronClaimOutcome`, decoupled from `cron_run.is_some()`.

**CLAIM / TAKEOVER** (`claim_or_takeover_cron_run`):
1. control terminal for this minute → `AlreadyTerminalForMinute` (this fire already completed → drop queue row **and re-enqueue the next fire** — the minute is done, so guaranteed schedule progress requires M+1 even though THIS worker did not run it; see §Schedule progress on a reaper-recovered crash).
2. control `{Claimed,Running}` & `deadline >= now` → `BlockedByLiveActive` (same fire live; keep row — defensive).
3. active live (`deadline >= now`) & `active_minute != min` → `BlockedByLiveActive` (**job-level no-overlap**; keep row, retry).
4. else (fresh, or supersede a stale/expired active/control): mint `fence=run_id`; write control `Running` + active, both at `deadline = now + effective_orphan_timeout` (the frozen orphan deadline — see §Lifecycle below; `effective_orphan_timeout = max(max(orphan_timeout, cron_job_timeout), job.max_runtime)`, NOT the bare `orphan_timeout`) → `Claimed` / `TookOver`. A superseded run's OLD fence is now strictly `<` stored → its later finalize is rejected.
   - **Different-minute supersession (in-claim terminalization).** When step 4 supersedes an *expired* ACTIVE pointer whose `active_minute != min` (the takeover is for a LATER minute), the claim overwrites `ACTIVE(job)` to the new minute. The superseded minute's CONTROL key (`CONTROL(job, active_minute)`) is a *different* key from the one this claim writes, so it would be left non-terminal `Running/F_old`: the reaper drives off the ACTIVE pointer (now naming the new minute) and never reaches it, and a still-alive lost-deadline owner could later `finalize_cron_run_cas(active_minute, F_old)` — its stale fence still matches the stranded record, so the stale terminal write is admitted. To close this, `decide_cron_claim` reports `supersede_minute = active_minute` on this path and the claim terminalizes that CONTROL **to `Failed` in the SAME txn**, through the SAME fence gate (`cron_finalize_accepts`, presenting the record's own fence) every terminal transition uses — no new key family, no divergent gate. After it, `CONTROL(active_minute)` is terminal, so (i) it is not orphaned and the reaper need not reach it, and (ii) the later stale finalize is rejected (accept-gate rejects on terminal regardless of fence). A same-minute takeover overwrites the same CONTROL key in place, so `supersede_minute = None` and there is nothing to strand.

**FINALIZE** (`finalize_cron_run_cas`, presents `F`): accept iff `control.fence_token == F` &&
`control.state ∈ {Claimed,Running}`; else **Rejected** (DEFECT 1 fix — a lost-lease worker holds an
old fence). On accept: write terminal control, `delete(active)` iff `active.run_id == F`, **clear the
running-guard carrier iff it still names `F`** (fence-matched — see §Symmetric bridge), project
`CronRun`. Rejected → caller logs and returns `Ok(())` (takeover owner is authoritative).

**REAPER** (`reap_stale_active_runs` → `finalize_cron_run_cas`): driven off the active pointer (one
per job, not a run-history scan). For each pointer with `active.deadline_ms < now`, present
`active.fence_token` to the SAME fence CAS the owner's finalize uses.
Accept iff `control.state ∈ {Claimed,Running}` && `control.fence_token == fence` (never clobber a
higher live fence). On accept: `delete(active)`, clear the running-guard carrier (fence-matched),
project the reconciled `CronRun`, and terminalize CONTROL. The control record IS the dedup key, so
after reap the next minute's claim takes the fresh path → `Claimed`.

**The reaper RECONCILES from `CronRun.status`; it never clobbers an existing terminal result.**
The reaper is a cleanup path, NOT the fire's owner, so the terminal status/message it commits is
`existing.status if existing.is_terminal() else Failed` — derived from the fire's CURRENT shared
`CronRun` (`reconcile_cron_terminal`), not a forced `Failed`. This closes a mixed-version clobber:
an OLD #2629 worker can FINISH after the new control plane was minted — it finalizes through the
LEGACY path (a real terminal `CronRun`, e.g. `Succeeded` with its return message) and clears only
the legacy guard; it cannot terminalize the new CONTROL/ACTIVE it does not know about, so those
linger `Running` at the SAME fence. When the reaper later sees that ACTIVE deadline expire, the
fence CAS still ACCEPTS (CONTROL is `Running` with the matching fence). Forcing `Failed` here would
project that `Failed` over the owner's real `Succeeded` — a non-owner terminal write admitted AFTER
the owner already finalized. Instead the reaper PRESERVES the existing terminal
status/return_message/end_time verbatim and terminalizes CONTROL *consistently with* it
(`Succeeded`/`Failed`/`Cancelled`), while still cleaning up the stale control plane (ACTIVE + guard
cleared). Only a genuinely orphaned fire (`CronRun` still `Starting`/`Running`, or already
retention-GC'd) becomes `Failed` — preserving schedule progress for a truly-dead worker. The
later-minute supersession path (`terminalize_superseded_cron_control`) shares the same reconcile:
it abandons CONTROL(M) as a `Failed` dedup tombstone (M is being skipped) but its `CronRun`
PROJECTION reconciles from the existing record, so a Succeeded-then-superseded fire keeps its real
user-visible result.

**DEFECT 2 — orphaned dedup flag — is closed, but the closure has two parts, not one.** For the
*single-key* case it is structural: the per-minute claim flag and the running guard are collapsed into
the CONTROL/ACTIVE records, written only inside the CAS helpers, so a terminal transition that
overwrites a key cannot leave a stale dedup flag behind — there is no separate flag to orphan. The
earlier blanket "structurally impossible — there is no separate flag to orphan" claim was, however,
**incomplete for the different-key supersession**: the reaper drives *only* off the ACTIVE pointer
(one per job), so when a different-minute takeover overwrites `ACTIVE(job)` from the expired minute M
to the new minute M+1, `CONTROL(job, M)` is a *different* key the reaper never revisits — it would
linger non-terminal `Running/F_M`, and a still-alive lost-deadline owner could `finalize_cron_run_cas(M,
F_M)` and have its stale-but-matching fence admitted (a non-owner terminal write). This is closed not
by the reaper but **in the claim CAS**: the different-minute takeover terminalizes `CONTROL(M)` to
`Failed` in its own txn (see State machine §CLAIM step 4, "Different-minute supersession"), through the
same fence gate, so M is terminal before the reaper would ever need it and the stale finalize is
rejected. The reaper remains the single path for *same-key* orphan recovery (a job that simply stops
firing); the claim handles the *cross-key* supersession its own overwrite creates.

### Schedule progress on a reaper-recovered crash (`AlreadyTerminalForMinute` → re-enqueue)

DEFECT 2's *silent-skip* half (a completed fire whose next fire is never scheduled) has a residual the
single-key dedup closure alone does not cover, on the boundary between the reaper and the engine
cleanup:

1. Worker A claims minute M — `CONTROL(M)=Running` + `ACTIVE(job)`; the system due row for M still
   exists (A has not yet deleted it).
2. A **crashes** before its cleanup/requeue runs.
3. The reaper recovers the orphan via `finalize_cron_run_cas`: `CONTROL(M)` → terminal, `ACTIVE`
   cleared. The due row for M is in the **system** store, which the reaper does not touch — it still
   exists.
4. The system claim expires and worker B re-claims the still-present due row for M. The claim reads
   terminal `CONTROL(M)` and returns **`AlreadyTerminalForMinute`** (step 1 of the state machine) — B
   owns no run, so it runs no finalize.

The minute is **done** (A's fire completed, recovered to terminal by the reaper). If B's cleanup
dropped the due row *without* re-enqueuing M+1 — because the requeue was gated on "did THIS worker run
the fire" (`cron_run.is_some()`) — the schedule would silently stall until a much-later registry
sweep, which is strictly weaker than the guaranteed-schedule-progress contract. The earlier "recovered
only by the next registry sweep" wording was a backstop, not the contract.

**Fix.** The re-enqueue is gated on "the claimed minute is **DONE**", not "this worker ran it". A
minute is done for `Claimed`/`TookOver` (we ran it) **and** for `AlreadyTerminalForMinute` (some path —
including a reaper-recovered crash — completed it). `cron_outcome_requeues_next_fire` (engine.rs) is the
single source of truth, keyed on the `CronClaimOutcome`; the engine threads its result as an explicit
`requeue_next_fire` signal out of `claim_and_record_cron_run` rather than re-deriving it from
`cron_run.is_some()`. A still-live block (`BlockedByLiveActive`/`Folded`) is **not** done — the live run
keeps the due row and itself re-enqueues M+1 when it finalizes — and a not-actionable claim (cron
disabled, job gone/inactive, stale payload, DB dropped) yields no fire. The re-enqueue uses the **same**
fenced, idempotent next-fire path the ran-it case uses (`load_next_cron_queue_entry`'s DB-liveness fence
+ `enqueue_task_v2_unless_db_dropped`'s dropped-DB tombstone fence in the same system txn). The cron due
key is deterministic on `(priority, fire_time, type, keyspace, db_id, task_id)`, so multiple workers
reclaiming the same terminal minute write M+1 to the **same** key — it is enqueued at most once. No
second enqueue mechanism is introduced.

This is the **single** orphan-recovery path. History GC (`gc_database_batch_inner`) is
**retention-DELETE-only**: it never recomputes an orphan cutoff and never writes run status, so it
cannot publish a terminal projection for a still-live fenced run. (The earlier history-scan branch
flipped `Running → Failed` via `put_cron_run` outside any fence CAS, recomputing the cutoff from the
*current* `max_runtime_ms` each cycle; a `cron.alter_job` lowering `max_runtime_ms` after a fire
claimed could then mark a still-live run Failed while its frozen ACTIVE deadline was still in the
future — a cross-deadline divergence that violated KEY C's "projection, never written outside the
terminal CAS" invariant. Collapsing to one fence-keyed terminal path removes it.)

`deadline_ms = started_ms + effective_orphan_timeout` (computed at claim time and FROZEN onto the
CONTROL/ACTIVE records, `≫` the 60s system lease), so a normal lease-renewing run is never superseded
mid-flight; only a genuinely orphaned run (past the orphan timeout) is taken over. Because the
deadline is frozen — not recomputed from the live `max_runtime_ms` — a later `cron.alter_job` cannot
retroact a running fire's orphan classification. *(Future hardening: have the lease renewer also
advance `deadline_ms` so the takeover window shrinks from orphan-timeout toward the lease — see Open
risks.)*

**`effective_orphan_timeout` = `max(effective_floor, execution_window)`**, where
**`effective_floor = max(orphan_timeout, cron_job_timeout)`** and
**`execution_window = cron_execution_window_ms(job.max_runtime, cron_job_timeout) = job.max_runtime.unwrap_or(cron_job_timeout)`**.
The deadline must cover the LONGEST a run can *legitimately* execute, which is precisely the value the
execution path times the fire out at. That "legitimate execution window" is now a SINGLE source of truth
— **`storage::cron::cron_execution_window_ms`** — from which THREE sites derive, so they cannot drift:

1. the **executor** (`claim_and_execute_core`) times the run out at `cron_execution_window_ms(...)`;
2. the **per-claim orphan deadline** `cron_effective_orphan_deadline_ms` = `now + max(effective_floor, cron_execution_window_ms(...))`;
3. the **effective floor** `gc::effective_cron_orphan_floor_ms` = `max(orphan_timeout, cron_execution_window_ms(None, cron_job_timeout))` — itself the single floor referenced by the per-claim path, the bulk migration, the per-claim straggler fold, AND the GC reaper view (`effective_cron_orphan_timeout_sec`).

Because the deadline maxes IN the exact same `cron_execution_window_ms` the executor uses, the invariant
**`orphan_deadline >= now + executor_timeout`** holds **BY CONSTRUCTION** for every job config (default
OR explicit `max_runtime`), not by a coincidence of two separately-written formulas — so a still-EXECUTING
run can never be classed expired and taken over (unit test `orphan_deadline_always_covers_executor_timeout`).
The prior cut wrote the floor (`max(orphan_timeout, cron_job_timeout)`) independently of the executor's
window; that independence WAS the drift class — a default job (`max_runtime = None`) executing up to
`cron_job_timeout` (30 min) had its ACTIVE classed expired after the bare `orphan_timeout` (5 min) and a
later fire took over mid-run, violating per-job no-overlap. The worker-queue **system-claim** lease orphan
(`try_claim_worker_task` / `cleanup_orphan_claims`) is a separate concern and keeps the raw
`orphan_timeout` — it gates the queue claim, which the live run actively renews, not the cron control deadline.

## Contract

- **Exactly-once** for single-statement transactional DML — *deferred* (see Open risks): fold the
  Done transition into the effect txn so concurrent executions write-conflict on the control key.
- **At-most-once while the lease/deadline is live + guaranteed schedule progress** for everything
  else (this is what closes both defects; it is what ships first).
- **At-least-once on takeover** for DDL / `CREATE INDEX CONCURRENTLY` / external-effect SQL (S3/HTTP)
  — irreducible; recommend user-side idempotency. Optionally bounded by carrying `fence_token` as an
  idempotency key into the DDL journal / index naming.

## Migration (rolling deploy)

New key prefixes (control/active), **not** a same-key value upgrade — an old binary reading a
multi-byte value on the running-guard key (which it length-checks `== 8` and *deletes* otherwise)
would erase live no-overlap state. `ensure_cron_control_migrated(db_id)` runs eagerly at reconcile,
before any claim. It **translates** the legacy state **present at the marker instant** — it never
discards it (discard-and-accept would reopen the double-exec + no-overlap window during the rolling
deploy, because the legacy guard/claim scheme and the new control/active scheme gate on *disjoint*
keys, so the fail-closed marker alone does not exclude a concurrently-running old binary). Legacy
state an old binary writes *after* the marker is handled by the per-claim straggler fold below — the
bulk pass only covers the pre-marker snapshot. All in one fail-closed txn:

- **live running-guard `(db,job) → run_id`** (the no-overlap authority) → an ACTIVE pointer **plus a
  matching `Running` CONTROL record**, sharing `fence_token == run_id` and `active_minute ==
  scheduled_min`, both stamped with a **LIVE orphan deadline** computed by the **one shared helper**
  `cron_effective_orphan_deadline_ms(now, global_orphan_timeout, job.max_runtime)` =
  `now + max(global_orphan_timeout, job.max_runtime)` — the **SAME** effective deadline a fresh claim
  and the per-claim straggler fold use. The migration resolves the guard's own job `max_runtime_ms`
  from the catalog **inside the migration txn** (per guard) so a long-running job (`max_runtime >>
  global`) does **not** have its migrated authority expire at `now + global` while it is still
  legitimately executing — that divergence (bulk path `now + global`, fold path
  `now + max(...)`) let a new binary treat the migrated authority as expired, take over, and run
  **concurrently** with the still-live old run (mixed-version job-level overlap). Computing the
  deadline in **one** place is the fix; a guard whose job has no surviving catalog row falls back to
  the global floor. The deadline **must be live, not `0`**: the state machine reads no-overlap
  (`decide_cron_claim`) AND
  reaper eligibility (`reap_stale_active_runs`) off the *same* `deadline >= now` predicate, so a
  `deadline_ms = 0` pointer would be classified as already-expired by **both** — enforcing zero
  no-overlap (the next real-minute claim falls through to `TookOver` and **double-runs** the
  still-in-flight old-binary fire) and being instantly reapable. The legacy guard tracked liveness by
  `CronRun.status`, not wall-clock; an orphan window is its closest wall-clock equivalent and matches
  exactly what a fresh claim writes, so the migrated guard retains the legacy guard's no-overlap
  authority **for the orphan window**, then is reaped/superseded by the fence CAS once it lapses
  (CONTROL→Failed, ACTIVE deleted, history projected) — preserving schedule progress. The matching
  CONTROL is required — without it the reaper's fence CAS reads `control == None`, rejects, and
  orphans the ACTIVE pointer. The guard carries no minute, so the pair is keyed at the sentinel
  minute `0` (real fires use epoch-minute keys ~28.9M, so `0` never collides with a live claim).
- **live per-minute claim `(db,job,min)`** (the per-fire dedup flag) → a terminal (`Failed`) CONTROL
  tombstone for that minute, so the next claim short-circuits to `AlreadyTerminalForMinute` (dedup
  survives the upgrade). A terminal record needs no reaper coverage (the accept-gate rejects on
  terminal regardless of deadline), so it keeps `deadline_ms = 0`; retention GC ages it out. Guard
  translation runs first and a claim never overwrites an existing CONTROL, so the live `Running` run
  wins over a same-minute claim tombstone.

It then deletes legacy keys and writes a per-db migration marker. The new claim path is
**fail-closed**: it refuses to claim a db until the marker exists, so a new binary never races an old
one on the *same* fire. The *cross-fire* straggler (a different, later fire while an old-binary run is
still in flight) is closed by the per-claim fold below, not by the marker. `delete_all_cron_data`
sweeps both legacy and new prefixes for one release; legacy encoders/shim are removed in a follow-up
once the fleet is fully upgraded.

**The migration's OWN writes are fenced (migration fence).** `ensure_cron_control_migrated` writes
`CONTROL`/`ACTIVE`/marker, and it runs *before* the claim path's `is_cron_enabled` / database-existence
checks. `DROP EXTENSION pg_cron` removes the cron-enabled marker **and** deletes all cron keys
(`delete_all_cron_data`); `DROP DATABASE` destroys the whole key range. If either committed
concurrently, a migration translating leftover legacy keys would **recreate orphaned control-plane
state** for a cron/db that no longer exists. So the migration takes the **same fences the claim and GC
commit paths use, inside its own txn**: (i) it verifies **`is_cron_enabled_for_update(db_id)`** right
after the marker check (a disabled/dropped cron has nothing legitimate to translate → no-op, no writes).
A **plain** `is_cron_enabled` snapshot read does **not** suffice: it does not conflict with
`remove_cron_enabled`, so a DROP that committed after this txn's snapshot but before its commit would
slip past it and let the marker/`ACTIVE`/`CONTROL` writes land. The `get_for_update` form serializes the
enabled-read against the DROP's marker delete (write-write conflict → one side aborts). And (ii) it takes
`assert_database_alive_for_update(db_id)` (`get_for_update` on the DB metadata row) immediately before
committing, so a concurrent DROP DATABASE conflicts and the migration aborts rather than committing into
a destroyed range. Together these close both the "cron dropped" and "db dropped" races against the
migration's writes; reusing the existing fence primitives keeps it a single clean path.

### Symmetric cross-generation no-overlap bridge (the running-guard carrier)

Job-level no-overlap is a **cross-fire per-`(db,job)` invariant** ("is *any* minute of
this job live?"). The only cross-generation serialization both an OLD #2629 binary and a
NEW binary honor is TiKV's pessimistic write-conflict on a shared key — and the
system-store `WorkerClaim`+lease is keyed **per-FIRE** (`fire_time_ms`), so it governs
**work distribution only** and *cannot* express a cross-fire per-job invariant. The new
binary enforces no-overlap via the tenant `ACTIVE(db,job)` key, which an old binary never
reads/writes; the old binary enforces it **only** via the legacy running-guard
`(db,job) → run_id`, which the new binary otherwise never writes. So the two generations
gate the same invariant on **disjoint keys**, with only a one-directional bridge (new
folds old's guard — see below). The *inverse* (old reads new's run) is unclosable by the
fold alone: there is no key the old binary reads that the new binary writes.

The bridge is made **SYMMETRIC** by having the new binary **DUAL-WRITE the legacy
running-guard** as the cross-generation no-overlap **carrier**, inside the SAME pessimistic
claim/finalize txn that writes `ACTIVE`/`CONTROL`. The carrier is a **pure pointer**
(`value = run_id == fence`); it carries **no liveness of its own** — an old binary's
`try_claim_cron_run` reads the guard, derefs `run_id`, and reads the **shared `CronRun`
status** the new binary already projects (`Running` at claim, terminal at finalize). So an
old binary blocks on a live new-binary run with **ZERO old-binary change**, and **no old
wall-clock/liveness model is re-imported**. The carrier is written/cleared at exactly the
sites that own the fenced state, so it is atomic with it and never orphaned:

- **claim/takeover** (fresh path) → `put_cron_running_guard(fence)` alongside the `ACTIVE`
  put (one txn ⇒ on rollback no guard, on commit `guard.run_id == ACTIVE.run_id ==
  CONTROL.fence_token == fence`);
- **straggler fold** (and the bulk migration) → the folded guard is **RE-POINTED at the
  fold's fence (== run_id), not deleted-then-recreated**, so there is **no instant** where
  an old `try_claim` sees no guard between a delete and a put (carrier continuity). The
  fold is **idempotent**: a guard whose run is already represented by the current ACTIVE is
  *not* re-translated into a fresh live ACTIVE (that would resurrect the folded run every
  tick and block the job forever) — it falls through to the normal takeover, which clears
  the carrier;
- **finalize / reaper** (`finalize_cron_run_cas`) → `clear_cron_running_guard_if(fence)`
  alongside the `ACTIVE` delete, **fence-matched** (point-get + compare, never clobber a
  guard a later generation re-pointed at a higher fence);
- **different-minute supersede** (`terminalize_superseded_cron_control`) →
  `clear_cron_running_guard_if(superseded_run_id)` (this path deliberately does not touch
  `ACTIVE`, so the carrier must be cleared here or a superseded run leaves a stale guard the
  old binary honors forever).

Because every terminal/supersede path clears the carrier fence-matched and every
claim/fold re-establishes it, the carrier is **continuous while a run is live and absent
once it is terminal** — the no-overlap invariant holds across BOTH interleavings (new-runs
→ old-blocks via the carrier; old-runs → new-blocks via the fold).

**Bridge removal gate (fleet-version, not per-db marker).** The dual-write, fold, and
translate machinery is a **rolling-deploy shim**, removed in a follow-up release once
**every node reports the post-#2633 binary generation** (a fleet-version gate — distinct
from the per-db migration marker, which only proves one db's snapshot was translated). The
dual-write is behind the `DB9_CRON_LEGACY_GUARD_DUAL_WRITE` config flag (default **ON**);
the removal release flips it **off** (claim/fold then delete rather than re-point the
carrier — clears any leftover) and then deletes the flag, the guard encoder,
`decode_legacy_guard`, the fold, `translate_legacy_guard`, and the carrier
write/clear helpers together. (Fleet version-reporting is out of scope here; this design
records the config flag + the documented gate.)

### Post-marker straggler fold (per-claim, not snapshot)

The one-shot `ensure_cron_control_migrated` translates only the legacy state present **at the marker
instant**. It cannot, by construction, cover a guard an OLD binary writes **after** the marker: the
system-store distribution claim (`encode_worker_claim_key`) is keyed per-FIRE (it includes
`fire_time_ms`), so it does **not** serialize an old vs a new binary at job granularity. Concretely,
an old-#2629 straggler can win a *fresh* fire of job B (its per-fire distribution claim succeeds),
write a NEW legacy running-guard, and run long; the new binary then wins a *later* fire of B, sees
the marker, skips the (now-empty) bulk scan, finds no CONTROL/ACTIVE for B, and would mint a fresh
ACTIVE and execute concurrently with the still-live old run — a job-level no-overlap violation +
double-exec across two different fires. A single snapshot translation can never close this, because
the legacy guard is a **live writer** for the whole rolling window, not a frozen pre-cutover blob.

The fix makes the legacy guard a **first-class input to the new no-overlap decision** for the
(bounded) rolling window, rather than relying on a one-shot snapshot. In `claim_or_takeover_cron_run`,
whenever there is **no LIVE active authority** for the job, the claim point-gets the legacy
running-guard for `(db,job)` **in the same pessimistic txn**. If a live guard is present it is
translated **right there** via the SAME `translate_legacy_guard` the bulk migration uses (ACTIVE +
matching `Running` CONTROL at the sentinel minute `0`, sharing `fence_token == run_id`, both stamped
with **this claim's frozen orphan deadline**), the legacy key is deleted, and ACTIVE is re-read — so
the normal `decide_cron_claim` below sees the live ACTIVE and returns `BlockedByLiveActive` (the queue
row is kept and retried). A malformed guard carries no live run (an old binary would itself self-heal
it) and is simply deleted.

**"No LIVE active authority" — not merely "ACTIVE absent" (load-bearing).** The fold must run when
`ACTIVE(job)` is absent **OR present-but-EXPIRED** (`deadline_ms < now_ms`), because an expired ACTIVE
is precisely *not* live authority. Gating the fold only on "ACTIVE absent" reopens the overlap during
a rolling deploy: (1) the new binary migrates an old in-flight run into `ACTIVE(job)` with
`deadline = T`; (2) that run finishes and clears its legacy guard, but ACTIVE lingers until `T`; (3) an
OLD binary then wins a *later* fire and writes a FRESH legacy running-guard; (4) after `T` a new binary
claims another fire — `ACTIVE` is present (expired), so an "absent-only" gate **skips** the fold and
leaves the fresh guard un-folded; (5) `decide_cron_claim` sees the expired ACTIVE as takeover-eligible,
overwrites it, and executes **concurrently** with the still-in-flight old-binary run — job-level
overlap / double-run. So the claim path translates/folds the fresh guard FIRST and blocks on the
resulting LIVE active, rather than taking over the expired ACTIVE while ignoring the guard. When the
fold displaces an expired ACTIVE that names a **different** minute than the sentinel `0`, that minute's
CONTROL is terminalized in the same txn (via `terminalize_superseded_cron_control`) — the identical
stranding hazard `decide_cron_claim` closes through `supersede_minute`, so the displaced CONTROL is
neither reaper-unreachable nor finalizable by a lost-deadline owner's stale fence.

**The fold's writes MUST COMMIT even though the claim is blocked (fold-commit).** The fold translates
the straggler guard into durable `ACTIVE`/`CONTROL` + re-pointed carrier, and *that* live ACTIVE is
precisely what makes `decide_cron_claim` return blocked. But "blocked" is the disposition the engine
otherwise maps to "keep the queue row and **roll back** the txn" — which would discard the fold every
tick, so the orphaned straggler never becomes a durable `ACTIVE`/`CONTROL` the reaper can supersede,
and **schedule progress is never guaranteed** (the crashed old run's guard is folded-then-discarded
forever). So `claim_or_takeover_cron_run` distinguishes a **blocked-but-folded** claim (it performed a
durable fold) from a **plain block** (it wrote nothing): only the former returns
`CronClaimOutcome::BlockedByLiveActiveFolded`, whose `must_commit_blocked()` tells the engine to
**COMMIT** the txn (under the same DB-liveness fence as a claimed run) rather than roll it back. A
plain `BlockedByLiveActive` — including the idempotent `already_folded` liveness-backstop block, which
only *reads* — still rolls back (no needless commit). The queue disposition is identical for both
(keep + retry); only the commit/rollback decision differs. The committed fold is then reapable: once
its frozen orphan deadline lapses the reaper finalizes `CONTROL → Failed`, clears `ACTIVE`, and clears
the carrier — exactly as for any other orphaned run.

This is the single clean path — no special-case block branch: the straggler guard becomes ordinary
ACTIVE state, so job-level no-overlap is enforced **in BOTH cross-generation interleavings** for the
orphan window. (i) **old-runs → new-blocks:** an old binary's in-flight guard is folded into a live
ACTIVE and the new claim returns `BlockedByLiveActive` (this section). (ii) **new-runs → old-blocks:**
the new binary's claim/fold **dual-writes the same guard as the carrier** (see §Symmetric
cross-generation no-overlap bridge), so an old binary's `try_claim_cron_run` derefs it to the shared
`Running` `CronRun` and returns `BlockedByRunningGuard`. The carrier is **re-pointed (not deleted)** on
fold so it stays continuous. Once the orphan window lapses the reaper supersedes the pair via the fence
CAS exactly as for any other orphaned run, **clearing the carrier fence-matched** (schedule progress
preserved, no stale guard left). The bulk migration handles efficiency (one txn for pre-marker state);
the per-claim guard check handles correctness (post-marker stragglers). Both funnel through
`translate_legacy_guard`, so there is no second translation semantics to keep in sync. The guard
point-get happens whenever there is no live active authority (ACTIVE miss or expired ACTIVE), so a
steady-state (fully-migrated) claim pays one extra point-get against an absent key until the shim is
removed.

This narrows the earlier "translation closes the no-overlap window" claim, which was scoped to a single
snapshot and therefore covered only the *same-fire* race; the cross-fire straggler is now closed too,
by the per-claim fold. The legacy guard key encoder + `decode_legacy_guard` are retained for exactly
this window and removed in the follow-up release once the fleet is fully upgraded.

### One effective deadline + cross-generation liveness backstop (no premature takeover)

Every cron CONTROL/ACTIVE deadline — the fresh per-claim path, the per-claim straggler fold, AND the
bulk migration translate — is stamped through the **one** helper
`cron_effective_orphan_deadline_ms(now, global_orphan_timeout, job.max_runtime)` =
`now + max(global_orphan_timeout, job.max_runtime)`. Computing the `max()` in two places was a defect:
the bulk migration stamped `now + global` (ignoring `max_runtime`) while the fold stamped
`now + max(...)`, so a **migrated long-running job** (`max_runtime >> global`, e.g. `1h` vs `5m`) had
its migrated authority expire at `+5m` while the old binary's run was still validly executing — the new
binary then saw the EXPIRED ACTIVE and took over, running **concurrently** with the still-live old run.
The fix routes all three paths through the single helper (the migration resolves the guard's own job
`max_runtime_ms` from the catalog inside its txn), so a long job's migrated authority covers its full
legitimate runtime. **This deadline unification is the load-bearing fix.**

As **defense-in-depth**, the idempotent `already_folded` branch (where the expired ACTIVE already names
the guard's run and the claim would fall through to `decide_cron_claim` and take over) consults the ONE
liveness signal both generations share — the run's **`CronRun.status`** — before falling through. The
OLD #2629 binary's `try_claim_cron_run` itself derefs the running-guard to this **same** `CronRun` and
blocks on a non-terminal status, and the new binary projects it `Running` at claim / terminal at
finalize. So when `cron_run_is_live(run_id)` reports the underlying run is still `Starting`/`Running`,
the claim returns `BlockedByLiveActive` (keep the queue row; retry next tick) rather than minting a
fresh concurrent run — even if a clock skew or an unexpected deadline lapse let the wall-clock gate fall
through. Crucially this does **not** resurrect dead runs: a **terminal** `CronRun` (the run finished) OR
an **absent** one (never projected — a genuinely dead/abandoned run) is NOT live, so takeover is still
allowed and schedule progress / orphan reaping is preserved. The backstop only ever *blocks* a run the
shared history says is still executing; it never *blocks forever* on a run that has completed or
vanished. For "absent == dead" to be **sound** the history record must not vanish *while the run is
still live* — guaranteed by the retention rule below.

### Retention is terminal-only (the liveness record must outlive a live run)

The `CronRun` history record is the cross-generation no-overlap bridge's liveness signal: an OLD #2629
binary derefs the running-guard → `run_id` → **this `CronRun`** and blocks while its status is
non-terminal, and the deadline backstop above treats an **absent** `CronRun` as dead. Retention GC must
therefore **never delete a NON-TERMINAL (`Starting`/`Running`) run**. A long-running run whose
`max_runtime_ms` exceeds the retention window would otherwise be deleted by its `start_time` past the
cutoff **while still executing** — an old binary would then deref the guard to an ABSENT record,
mis-classify it as dead, and run **concurrently** (no-overlap broken); and the backstop's "absent ==
dead" would (wrongly) admit a takeover of a still-live run. A live run's reaping is the **orphan
reaper's** job (`reap_stale_active_runs`, off the frozen ACTIVE deadline), **not** retention's; only the
reaper's terminal transition (which stamps `end_time`) makes the run retention-eligible. So the GC
predicate is **terminal-only, keyed strictly by `end_time`**: `run.status.is_terminal() &&
end_time < cutoff`. `start_time` is **not** a retention fallback — using it (`end_time.or(start_time)`)
was the defect that deleted a live long run by its start. This makes the history record persist for the
full lifetime of a live run, so the liveness signal the bridge and backstop depend on is always present
while it is needed.

## Open risks / deferred

1. **Exactly-once fold (deferred).** Requires `execute_task` to expose "this command compiled to one
   tenant DML txn." Largest plumbing gap; ship without it (all cron = at-most-once-while-lease +
   at-least-once-on-takeover) and add it behind a conservative classifier later.
2. **Lease/deadline divergence.** First cut sets `deadline = started + orphan_timeout` (matches
   today). Hardening: renewer advances the control deadline so the takeover window → lease.
3. **Control-record retention GC.** Terminal control records are retained for dedup of late
   re-deliveries; needs a retention sweep aligned with the existing run `retention_cutoff`.
4. **Migration marker is fail-closed** → a db whose migration repeatedly fails stalls its cron; make
   migration idempotent, retried every reconcile, alert if unmigrated past N cycles.
5. **Sub-minute fires.** Control key uses `scheduled_min`; if sub-minute schedules are ever added the
   key must switch to full `fire_time_ms`.
6. **pg_cron no-overlap parity** must be validated against real PostgreSQL 17.7 before any cron
   integration `.expected` changes (per the SQL Test Contract).
