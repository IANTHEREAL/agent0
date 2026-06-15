# Worker Kernel and Background Work Protocols

> **Status**: Draft v2.1 — implementable specification
>
> Part I is the architecture contract. Part II is the normative
> implementation specification: an engineer should be able to implement the
> kernel from this document alone. Where Part I sketches an API shape and
> Part II gives a signature, Part II wins. Current authoritative contracts
> remain under `docs/sot/**` and the implementation under `src/**`.

**Date:** 2026-06-12 (v2.1; v1/v2 same date)
**Scope:** worker metadata, worker inventory, executable queue with leased
claims, bounded maintenance, crash-recovery discovery, and lifecycle fencing.

**Implementation status:** this document is the target kernel contract, not a
claim that every mechanism has landed in PR #2625. The current branch closes
the database liveness fence and HNSW S3 external-object lifecycle pieces.
Leased/renewed worker claims in K4 are now implemented: `WorkerClaim` carries a
`lease_until_ms`, the executing worker renews at ~lease/3 (a lost renewal
cancels the run before its next tenant commit), claim acquisition is
`get_for_update`-based, and GC reaps only expired leases. Legacy
`claimed_at`-only rows synthesize their lease as `claimed_at + orphan_timeout`
during the rolling-deploy window. The V1->V2 `_worker_queue_` migration runs on
the single canonical startup path (`init_gc_registry_store`, used by both
production and tests) before the worker engine begins ticking.

v2 incorporates the kernel-gap review of v1 against the live code on
`codex/regression-gate-worker-enabled-fail-closed`; v2.1 adds the
implementation specification (Part II). The deltas are listed in
[Changes from v1](#changes-from-v1).

# Part I — Architecture and Contracts

## Summary

The worker subsystem is a small hard kernel plus per-feature protocols.

The kernel owns four durable primitives and one fence:

- one always-on `_sys_worker` metadata store;
- one canonical per-database inventory row — **the only durable object whose
  absence may lose work**;
- one executable queue with **leased, CAS-acquired claims** and singleton task
  identity;
- ordered cleanup (queue before inventory) with key-cursor paged traversal and
  bounded budgets for every step;
- a **database liveness fence** that every tenant-writing background
  transaction must cross at commit.

Two central rules:

```text
1. Registry inventory tells the worker which databases may need a visit.
   It is not, by itself, active work truth.

2. Exactly one durable object is load-bearing for discovery: the canonical
   inventory row. If it exists, every feature can rebuild its work from
   feature truth. Everything else — registry bits, in-memory sweep state,
   singleton queue rows for maintenance — is advisory or reconstructible.
```

Rule 2 is the major simplification over v1. v1 classified registry bits into
"false negatives allowed" and "false negatives forbidden" and defended the
forbidden class with cleanup ordering and API rules. v2 removes the forbidden
class entirely: the sweep probes feature truth for every inventory row, so a
lost, raced, or destroyed bit can never lose work. One invariant replaces a
per-feature matrix.

## Problem Statement

Historically `_worker_registry_` carried unstated, conflicting roles: feature
gate, task-type bitset, crash-recovery discovery, maintenance inventory, stale
cleanup list, and retry anchor. Each role has different tolerance for false
positives and false negatives, and several incidents (lost `task_types` bits,
unordered cleanup, unbounded walkers) came from conflating them.

The v1 draft fixed the role taxonomy but left the four hardest mechanisms
unspecified, and the code shows each gap is live:

- claims have no lease (`WorkerClaim` carries only `claimed_at`; GC reaps any
  claim older than a flat timeout, so a long legitimate task can be reaped
  alive and double-executed);
- registry-bit updates are read-modify-write without a locking read, so
  concurrent producers can lose bits;
- `DROP DATABASE` destroys key ranges with no generic fence against in-flight
  background commits (only HNSW has a hand-built fence);
- all sweep state is per-replica and in-memory, and the duplicate-sweep cost
  model was never written down.

This revision specifies those mechanisms with the minimum machinery that is
actually correct, and names what is deliberately rejected.

## Design Goals

- Keep the core model small enough that new worker features reuse the same
  primitives instead of inventing their own scanners, queues, and cleanup
  paths.
- Make metadata durability independent from worker execution enablement.
- Bound all fanout by construction: startup, periodic sweep, S3 listing,
  marker cleanup, queue reap, tenant acquisition, and per-database probes.
- Make every hint safe to lose. Only the inventory row may be load-bearing.
- Make duplicate work safe and *accounted for* where multi-replica sweeps are
  allowed.
- Prefer compile-time API boundaries and behavioral tests over review-only
  conventions and source-string tests.
- Make recovery behavior explainable from durable state, not from
  process-local timing or best-effort callbacks.

## Non-goals

- No worker microservice, no leader election, no separate physical stores for
  recovery registry / inventory / queue.
- No exactly-once guarantee for **external** side effects (S3, HTTP calls made
  by user SQL). Features with external effects must be idempotent at the
  feature layer.
- No cluster-level capability registry in v1 (see
  [Execution capability](#k1-always-on-system-store-and-execution-capability)).
- No sweep page leases in v1 (see [Multi-replica model](#multi-replica-model)).
- HNSW recovery marker writes are not made atomic with tenant DML; recovery
  uses inventory probing instead.

## Mental Model

| Class | Meaning | Authority | May be lost? |
| --- | --- | --- | --- |
| Inventory | This database may need a visit | `_sys_worker` row | **No** — the one load-bearing object |
| Diagnostic hint | Breadcrumb of which features touched this db | `task_types` bits | Yes — no consumer may branch on it |
| Executable work | A task that can be claimed and run | worker queue V2 | Singleton maintenance: yes (re-derived by probe). Event tasks (cron fire, trigger event, bgsql): no — protected by enqueue protocol |
| Leased claim | One worker owns one task attempt until lease expiry | worker claims | Yes — expiry re-opens the task |
| Feature truth | The durable condition being reconciled | tenant store / queue / S3, per feature | Defined per feature |

The load-bearing chain is therefore:

```text
inventory row exists (fail-closed at producers, repaired by anti-entropy)
  -> bounded per-database visit
    -> unconditional truth probes (journal, outbox, cron catalog, schema walk,
       stats freshness)
      -> singleton enqueue / direct repair
```

No step in this chain consults a registry bit.

## Hard Kernel

### K1. Always-on system store and execution capability

The `_sys_worker` store is initialized for every SQL-serving process before it
can create foreground state that depends on worker recovery metadata.
`DB9_WORKER_ENABLED` controls background execution only; it must not control
whether foreground producers can write required recovery metadata.

Required API shape:

```rust
worker::system_store() -> Result<&Arc<TikvStore>>   // fail-closed accessor
worker::execution_enabled() -> bool                 // process-local flag
```

The absence of the metadata store is an infrastructure failure, not a feature
mode.

**Producer-write vs admission-check (the load-bearing distinction).** Two
separate decisions both touch `execution_enabled()`, and they must not be
conflated:

- *Producer writes* — durably recording work that an ordinary committed
  statement requires (HNSW merge singleton, auto-ANALYZE, async-trigger
  activation, cron next-fire, bg DDL/SQL, storage scan). These go through the
  always-on `system_store()` and are **NEVER gated on `execution_enabled()`**:
  the foreground statement commits regardless of local execution, so gating the
  producer write silently drops recovery work on a producer-only node (or on an
  enabled→disabled flip between accumulate and flush). An execution-enabled node
  in the homogeneous fleet performs the work.
- *Admission checks* — user-facing functions that exist solely to schedule
  background work (`cron.schedule`/`alter_job`, `pg_background_launch`,
  `db9_refresh_storage_stats`, `REFRESH MATERIALIZED VIEW CONCURRENTLY`,
  `CREATE INDEX` for HNSW/CONCURRENTLY) MAY read `execution_enabled()` and return
  an explicit error when no worker exists. That is loud refusal, not silent loss.

One legitimate execution-strategy gate is the after-trigger async/sync choice
(`trigger_body_needs_async && execution_enabled()`): when execution is disabled
the async-needing body runs SYNCHRONOUSLY inline instead of being deferred, so
the trigger always executes — no drop — and the durable enqueue itself
(`flush_trigger_activations`) is still ungated.

**Capability scope (decided for v1):** `execution_enabled()` is process-local,
and feature admission checks (`cron.schedule`, `pg_background_launch`,
`CREATE INDEX CONCURRENTLY`, async-trigger-requiring DML) read it. This is
only correct if the fleet is homogeneous. Therefore v1 declares an explicit
deployment constraint:

```text
All SQL-serving processes of one cluster must run with the same
DB9_WORKER_ENABLED value.
```

A heterogeneous deployment (SQL nodes without workers + dedicated worker
nodes) requires moving capability to a durable cluster setting. That is
deferred until such a deployment is actually wanted; admission code must go
through one helper (`worker::execution_capability()`) so the future swap is a
one-site change.

### K2. Canonical inventory row

```text
_worker_registry_{canonical_keyspace}_{db_id} -> TaskRegistryEntry
```

Contract:

- Every live database must eventually have exactly one canonical inventory
  row. `DEFAULT`/`default` and any other aliases must collapse to one row.
  The sweep migrates a stored non-canonical alias row when it visits one:
  ensure the canonical row, then remove the alias via ordered cleanup (K5) —
  upgrades therefore converge without a one-shot migration job.
- **Type-enforced canonicalization:** every kernel API takes a
  `CanonicalKeyspace` newtype, constructible only via
  `CanonicalKeyspace::from(raw)`. No kernel entry point accepts `&str`. (The
  live bug class: `store().keyspace()` yields `"DEFAULT"` while queue rows are
  keyed `"default"`; today each call site canonicalizes — or forgets to — by
  convention.)
- A producer that creates durable tenant state requiring discovery (DDL
  journal entry, trigger outbox row, cron job, background DDL operation) must
  ensure the row exists **before that state's commit**, and must fail the
  operation if it cannot (fail-closed).
- Producers have no bit-setting obligation. `task_types` and `job_count` are
  diagnostic breadcrumbs retained for serialization compatibility; no consumer
  may branch on them. (Consequence: the row-creation race between two
  concurrent `ensure_row` calls is harmless — both write an identical fresh
  row — so no locking read is needed on this path.)
- Inventory rows may outlive active work. A row without work costs one
  bounded visit per cycle; a false positive is acceptable.

Required API shape:

```rust
worker::inventory::ensure_row(sys, ck: CanonicalKeyspace, db_id) -> Result<()>
worker::inventory::scan_page(sys, cursor, limit) -> Result<(Vec<Entry>, Cursor)>
worker::inventory::reap_queue_then_delete(sys, ck, db_id) -> Result<usize>
```

Direct registry deletion and full-registry listing are not public production
operations.

**Anti-entropy (named repairers).** "Missing inventory for a live database is
a repairable defect" requires naming who repairs it. Three layers, all of
which already exist in some form and are hereby made contractual:

1. `CREATE DATABASE` writes the row fail-closed before tenant commit
   (guaranteed for every database created after adoption).
2. Tenant-store creation (pool cache miss, including the sweep's own
   re-acquisitions after idle eviction) scans the tenant database catalog in
   pages and ensures a row per database. Failure must increment a visible
   repair-failure metric, not just log.
3. Producer `ensure_row` calls repair on the next feature write.

Residual exposure: a keyspace with zero inventory rows and zero connections is
invisible until its next login. That loses only freshness work (stats,
autoanalyze), never recovery work — recovery-requiring state cannot exist
without a producer having run layer 3. Accepted; no fourth mechanism.

### K3. Paged global traversal

No production reconciler may materialize the full registry, queue, or claim
set.

- Cursor is key-based, not offset-based.
- Limit is hard and externally visible in metrics.
- Mutation during traversal may repeat or skip within one cycle; the next
  cycle must converge.
- Page processing must not keep tenant clients resident after the page ends.
- The sweep cycle cadence is its own config (`sweep_cycle_interval`), not an
  alias of the HNSW interval.

### K4. Executable queue and leased claims

The queue answers which tasks can be claimed and run. Claims decide who runs
them. v1 listed claim semantics as a property; v2 specifies the mechanism,
because it is the kernel's hardest part and the current code has none of it.

**Claim acquisition is CAS, not get-then-put.** A claim is acquired with
constraint-checked insert (TiKV `insert`, assertion NotExist, or
`get_for_update` + absent check). Under the default pessimistic transactions a
plain `get` takes no lock, so get-then-put's single-winner property would rest
on client conflict-handling details rather than construction.

**Claims carry a lease and are renewed.**

```text
WorkerClaim { worker_id, task_type, lease_until_ms }
```

- Lease default ~60s; the executing worker renews at ~lease/3 by
  `get_for_update` on its own claim, ownership check, extend, commit.
- A failed or lost renewal aborts the execution promptly (wired into the
  existing cancel-token plumbing). The executor must not start a new tenant
  commit after its lease has lapsed without renewal.
- GC reaps **only expired leases** (legacy `claimed_at`-only rows use the old
  flat timeout during the rolling-deploy window, then that path is deleted).
- Lease durations must be large relative to plausible clock skew; sub-second
  leases are forbidden.

**What the lease guarantees — and what it doesn't.** The queue provides
at-most-once execution *while a lease is live*. It does not promise
exactly-once across lease expiry. This is sufficient for all same-cluster
effects, by this argument:

```text
Tenant data and worker metadata live in the same TiKV cluster. A worker that
cannot renew its lease is partitioned from (or rejected by) that cluster, so
its tenant commits fail for the same reason. Lease expiry therefore implies
the old holder can no longer produce same-cluster effects.
```

External side effects (S3 uploads, HTTP calls inside cron SQL) are outside
this argument and must be idempotent or best-effort at the feature layer.
Features additionally keep their truth-side guards (cron per-minute run
claim, HNSW meta `get_for_update`, CIC index-state machine); the kernel lease
makes those guards rarely needed rather than redundant.

**Commit-adjacent ownership fence on long-task paths.** A long task whose
lease can lapse mid-run (cron, CIC backfill, HNSW merge, storage scan) must
fence every *tenant* commit on still owning its *system-store* claim — not
merely on DB-liveness. Two failure modes are distinct: a dropped DB (caught by
`fence::assert_db_alive`) and a stolen lease (DB still alive, only ownership
changed; caught only by an ownership re-check). The CIC/HNSW paths express this
as `lease_cancel.bail_if_cancelled()` before each batch/phase commit. The cron
**finalize** path — which commits *terminal* tenant state (terminal `CronRun`,
cleared running guard, released per-minute claim) after the renewer is already
stopped — expresses it as an `is_worker_claim_owned_by` re-check against
`_sys_worker`, hoisted **ahead** of the finalize commit. If the claim is gone
or taken over, the worker skips finalize *and* the whole cleanup/requeue block
and leaves the row for the new owner (the same verdict the post-finalize
ownership-checked cleanup already produced, just one step earlier so a non-owner
never commits terminal cron state). A non-owner committing terminal state is a
correctness defect, not a benign duplicate.

**Per-minute cron claim is scoped to a live run.** `try_claim_cron_run` writes
a `(db_id, job_id, scheduled_min)` claim to dedup same-minute fires. It must be
released when the run reaches a terminal state (in the same tenant txn that
clears the running guard), *not* kept as run history. Otherwise a takeover
worker re-claiming the same `scheduled_min` after an expired lease hits
`AlreadyClaimedForMinute`, and the cron tick then deletes the due row **without
requeue** — one scheduled fire silently lost. (When the original owner instead
*lost* its lease and never finalized, the running guard stays `Running`, so the
takeover sees `BlockedByRunningGuard` → keep the row → next tick retries; no
fire is lost.)

**Singleton task identity is owned by the kernel API.**

```rust
worker::queue::enqueue_singleton(identity, payload, policy) -> Result<EnqueueResult>
worker::queue::claim(identity, lease) -> Result<Option<ClaimHandle>>
ClaimHandle::renew() / ClaimHandle::complete()   // identity+nonce-checked
```

- `enqueue_singleton` assigns a fresh non-zero nonce internally (no call-site
  `rand` rituals) and **checks pending/claimed state inside the same
  transaction as the write**: if the singleton is already pending or claimed,
  it does not overwrite. (Live gap: the storage-scan sweep enqueues on stats
  staleness alone, so every cycle rewrites the claimed task's nonce, the
  finishing worker's identity check then leaves the row, and each big-database
  scan is followed by one guaranteed redundant scan.)
- `complete()` is read-compare-delete on identity+nonce so an older attempt
  can never delete a newer logical task.
- Multi-entry tasks (cron schedules, trigger events, bgsql requests) keep
  per-event identity and are never collapsed by singleton logic.

### K5. Ordered cleanup

```text
reap queue entries for (keyspace, db_id)
then delete inventory row
```

If queue cleanup fails, keep the inventory row; the next bounded sweep retries
from durable inventory. Deleting the row first destroys the retry anchor.
Direct deletion is private; the ordered helper is the only production path.

### K6. Bounded substeps and the shared observation walk

Outer registry paging is not enough; every step under one entry has its own
budget. In addition, v2 merges the per-feature tenant scans:

```text
One bounded, paged schema walk per database visit produces observations
consumed by all detectors that need schema-derived truth:
  - HNSW indexes with pending deltas
  - CIC/BgDdl index states (Building / WriteOnly)
  - (future detectors, e.g. autoanalyze candidates)
```

Rationale: the HNSW probe already pays `list_tables` + per-table `get_schema`
per database per cycle. Running CIC detection as a second independent walk
doubles the dominant cost for nothing; gating either walk behind a registry
bit recreates the false-negative class. One walk, many consumers.

The walk and every other substep are budgeted: S3 LIST pages, retired-marker
delete batches, queue reap batches, journal repair per-entry transactions,
cron history GC batches. A single large database may spread its walk across
visits (per-visit page budget; resume or restart next visit — convergence
over cycles is sufficient for maintenance).

The walk's read snapshot — like every long-lived background transaction —
must register with the GC safepoint for its full duration (quarantining on
failed rollback) so storage GC cannot advance past a live sweep snapshot.

### K7. Failure domains and state residency

Failures are classified by what they invalidate; one kind's failure must not
back off another kind for the same database.

| Failure | Scope |
| --- | --- |
| Cannot acquire tenant store / resolve liveness | Entry-level |
| DDL journal reconciliation failed | DDL kind only |
| HNSW S3 listing failed | HNSW S3 kind only |
| Storage scan enqueue failed | Storage scan kind only |

**State residency rule:** all sweep bookkeeping (cursor, backoffs, grace
strikes) is per-replica, in-memory, and lost on restart. Therefore every use
of such state must be fail-safe in the loss direction: losing it may only
*delay* destructive actions (grace restarts) or *repeat* idempotent ones,
never enable a destructive action earlier. Any future state whose loss would
violate this must be made durable instead.

### K8. Database liveness fence

Generalizes the hand-built HNSW meta fence to all background writes.

```text
Every transaction that writes tenant data on behalf of background work —
task execution commits, DDL journal repair, CIC repair — must
get_for_update the database liveness key (the database metadata row) in
that same transaction before commit, and abort if it is absent.

DROP DATABASE deletes the liveness key in its metadata transaction, before
any data-range destruction.
```

The pessimistic lock serializes every fenced commit either entirely before
the drop (its writes are then destroyed together with the database) or after
it (the fence read sees absence and aborts). Without this, a task that
resolved the database before the drop can commit after `unsafe_destroy_range`
and resurrect orphan keys in the destroyed range.

The fence is one locked read per background commit; multi-batch operations
(journal repair, backfill) fence each batch transaction. Foreground client
transactions share the same hazard in principle; adopting the fence there is
out of scope here but uses the same primitive.

**Fence failure is a DEFINITE non-commit.** When a write also stages an
external object before its tenant commit (HNSW S3 graph upload + durable
intent), the disposition of that object on failure depends on *where* the
failure is. The fence read (`get_for_update` of the liveness key) and any
other pre-commit step run strictly before `txn.commit()`; an error from them —
database dropped (`Ok(false)` → abort) or a TiKV read error — means the commit
never executed, so the staged object will never be referenced by committed meta
and MUST be cleaned up, exactly like an oversize bail or a delete-delta-keys
failure. Only `txn.commit()` itself is ambiguous (TiKV may have committed
despite a client-visible error); only that error retains the object and its
durable intent for safepoint GC reconciliation. Concretely, the fence and the
commit must therefore be two separate fallible steps, never fused in one
fallible block — fusing them routes a definite fenced abort (e.g. a
DROP DATABASE race) into the ambiguous-commit retain path and leaves a no-meta
S3 orphan until later GC.

## Core Sweep

One maintenance loop over inventory; a small scheduler, not a feature
framework. Startup is a fast catch-up mode of the same loop. The sweep runs
in its own loop, isolated from queue-task execution: a long-running claimed
task (timeout-exempt BgDdl backfill, a 30-minute cron job) must not be able
to starve recovery.

```text
loop:
  page = inventory::scan_page(cursor, limit)
  for entry in page:
    if entry-level backoff says skip: continue
    handle = acquire tenant (canonical keyspace); remember for eviction
    if database does not resolve: missing-database protocol; continue

    # unconditional truth probes — no registry-bit gating anywhere
    probe ddl journal prefix        -> repair entries (fenced txns)
    probe trigger outbox prefix     -> re-enqueue leftovers (event identity)
    probe cron enabled + catalog    -> reconcile queue entries, history GC
    probe stats freshness           -> enqueue_singleton(storage scan)
    walk = bounded_observation_walk(handle)
    walk.hnsw_pending               -> enqueue_singleton(hnsw merge)
    walk.cic_incomplete             -> repair iff no pending/claimed BgDdl task

    drop handle
  evict_if_idle(touched keyspaces)
  advance cursor; on cycle completion apply sweep_cycle_interval
```

Each probe failure records its own kind backoff (K7). The loop may run on
multiple replicas (next section).

## Multi-replica Model

Decided: duplicate sweeps across replicas are accepted; no page leases, no
leader election in v1. This is only honest with the cost model written down:

- Per cycle, the fleet pays **R ×** (tenant acquisitions + probes + the
  observation walk). The walk dominates; it is bounded per visit and budgeted.
- Duplicate *enqueue decisions* are made harmless by construction:
  singleton enqueues check pending/claimed in-transaction (K4); event
  enqueues are deduplicated by event identity; repairs are idempotent and
  fenced.
- Grace counting (missing database) is per-replica: each replica must
  independently observe the absence in ≥2 of its own completed cycles.
  Restart resets the count — fail-safe direction (delays deletion).
- **Revisit trigger:** if measured duplicate-sweep cost (R × walk reads)
  becomes material in production, add per-page leases in the queue store.
  Until measured, leases are rejected as premature.

## Outer Protocols

Template (v2 — `false_negative_allowed` is gone; hints can no longer be
load-bearing):

```text
feature:
truth:
probe (per visit):
producer obligation:
enqueue identity:
execution guard (truth-side):
boundedness:
failure scope:
delivery:
```

### DDL journal recovery

```text
feature: DDL journal recovery
truth: tenant-local DDL journal entries
probe: bounded journal prefix scan, every visit
producer obligation: ensure_row fail-closed before journal-dependent commit
enqueue identity: none (direct fenced repair during the visit)
execution guard: per-entry repair transactions are liveness-fenced
boundedness: per-entry txns; batched range deletes with txn rotation
failure scope: DDL journal kind
delivery: effectively exactly-once (repair is idempotent and fenced)
```

A durable journal entry must never exist without a discoverable recovery
source; since discovery is now "inventory row + unconditional probe", the
fail-closed obligation collapses to `ensure_row` before the journal can
commit. No bit is involved.

### Async triggers (tenant outbox — new in v2)

```text
feature: async triggers
truth: tenant-local trigger outbox rows + executable queue entries
probe: bounded outbox prefix scan, every visit
producer obligation: outbox row written IN THE SAME tenant transaction as the
  triggering DML (this is the only cross-domain-safe construction); ensure_row
  fail-closed at trigger DDL time
enqueue identity: outbox row id (deterministic; re-enqueue overwrites)
execution guard: worker deletes queue entry on completion; enqueuer deletes
  outbox row after queue commit
boundedness: outbox scan paged; enqueue batched
failure scope: task execution
delivery: at-least-once (crash between queue commit and outbox delete, or
  between execution and queue delete, replays). Trigger functions must
  tolerate replay.
```

This replaces the current post-commit `tokio::spawn` + warn enqueue, which
silently drops fired triggers on crash and literally logs
"Dropping … activation(s)" when execution is disabled. With the outbox,
worker-disabled means durable deferral: events accumulate and run when
execution returns — consistent with K1's "durability independent from
execution".

### HNSW delta recovery

```text
feature: HNSW delta recovery
truth: tenant-local HNSW delta/meta state
probe: shared observation walk (existence probe per index), every visit
producer obligation: none beyond inventory row (DML hot path writes nothing
  to the system store — unchanged)
enqueue identity: singleton per (table, index) merge target
execution guard: merge transaction get_for_update on HNSW meta (existing)
boundedness: walk budget; delta probe is scan(limit=1)
failure scope: HNSW delta kind
delivery: at-least-once enqueue; merge idempotent under meta lock
```

### HNSW S3 cleanup

```text
feature: HNSW S3 cleanup
truth: S3 objects + tenant HNSW meta + retired-version markers
probe: S3 LIST pages + marker pages, every visit where S3 is configured
producer obligation: lifecycle transitions write markers (existing)
enqueue identity: none (direct bounded cleanup) unless made async later
execution guard: marker deleted only after object lifecycle resolved
boundedness: LIST pages, delete batches; one large index must not monopolize
  a sweep cycle
failure scope: HNSW S3 kind (must never back off correctness kinds)
delivery: best-effort external cleanup; deletes are idempotent
```

### Storage size scan

```text
feature: storage size scan
truth: persisted stats freshness + queue pending/claimed state
probe: stats freshness read, every visit
producer obligation: none (sweep-driven; SQL function may also enqueue)
enqueue identity: singleton per (keyspace, db_id) via enqueue_singleton
  (pending/claimed checked in-transaction — fixes the redundant-rescan churn)
execution guard: none needed (scan output overwrite is idempotent)
boundedness: one enqueue decision per visit; execution paginates with its own
  budget and rate limit
failure scope: storage scan kind / task execution
delivery: at-least-once; idempotent
```

### Cron

```text
feature: cron scheduling
truth: tenant cron catalog + executable queue entries
probe: cron-enabled read + catalog/queue reconcile, every visit
producer obligation: schedule/alter/unschedule maintain catalog + queue in
  their own paths; ensure_row fail-closed at schedule time
enqueue identity: per (job, fire time)
execution guard: tenant per-minute run claim + running guard (existing)
boundedness: per-database reconcile; history GC batched within the visit
failure scope: cron kind
delivery: per-minute at-most-once via run claim
```

### CIC / BgDdl recovery

```text
feature: concurrent index / background DDL recovery
truth: tenant index states + BgDdl queue pending/claimed state
probe: shared observation walk (index states), every visit
producer obligation: ensure_row fail-closed before enqueueing background DDL
enqueue identity: operation-specific BgDdl task
execution guard: an index in Building/WriteOnly may be marked failed only if
  no pending or claimed BgDdl task exists; repair txns are liveness-fenced
boundedness: walk budget
failure scope: CIC kind
delivery: repair idempotent; backfill protected by lease + state machine
```

### BgSql

```text
feature: background SQL (pg_background_launch)
truth: executable queue entries + result rows
probe: none (queue is authoritative; no sweep role beyond queue hygiene)
producer obligation: ensure_row; admission requires execution capability (K1)
enqueue identity: allocated request id
execution guard: leased claim only
boundedness: queue scan; result retention GC
failure scope: task execution
delivery: at-least-once across lease expiry (same-cluster effects argument
  applies; statements with external effects are the user's responsibility,
  matching pg_background semantics)
```

### AutoAnalyze

```text
feature: auto analyze
truth: table modification stats + queue pending/claimed state
probe: none required in v1 (producer-driven); optional future detector on the
  observation walk
producer obligation: best-effort enqueue with singleton identity per table
enqueue identity: singleton per (db, table)
execution guard: none needed (ANALYZE idempotent)
boundedness: enqueue dedup via singleton API
failure scope: auto analyze kind / task execution
delivery: best-effort; loss tolerable (next DML re-triggers)
```

### Queue hygiene

```text
feature: queue and claim hygiene
truth: queue rows, claim rows, result rows
probe: claim scan reaps expired leases only; legacy V1 queue drains via the
  gated byte-safe path; bg results expire per retention policy
boundedness: batched scans and deletes
failure scope: hygiene kind
```

## Database Lifecycle Protocols

### CREATE DATABASE (decided)

```text
write canonical inventory row (fail-closed, bounded retries)
then commit tenant metadata
```

If registration fails, CREATE DATABASE fails before tenant commit. If the
process crashes between row write and tenant commit, the orphan row is removed
by the missing-database grace protocol. The alternative (register after
commit with a durable retry marker) is rejected: the retry marker would itself
need the durability the row already provides.

Success therefore implies the row exists — and if grace deletion ever races a
slow create, anti-entropy layer 2 (K2) repairs on the next acquisition.

### DROP DATABASE

```text
delete tenant metadata INCLUDING the liveness key   (arms the K8 fence)
clean feature-external state (HNSW text keys, S3)
destroy data ranges
write dropped-DB TOMBSTONE in the SYSTEM store      (arms the cross-store fence)
reap worker queue entries for (keyspace, db_id)
delete canonical inventory row                      (ordered cleanup, K5)
```

If queue reap fails, retain the inventory row; the sweep retries cleanup via
the missing-database protocol. Tasks claimed after the metadata delete abort
at liveness resolution or at the fence; tasks already past their last commit
were serialized before the drop and their writes die with the range.

**Cross-store next-fire fence (issue #2628 item 2).** The K8 liveness key lives
in the TENANT store, but every cron next-fire enqueue commits in the SYSTEM
store, so a single-txn fence across the two stores is impossible. Closing that
window requires a fence object IN the system store: DROP's reap writes a durable
dropped-DB **tombstone** (`_wq_dropped_db_{keyspace}_{db_id}`, committed before
the queue scan), and every cross-store enqueue routes through ONE helper
(`enqueue_task_v2_unless_db_dropped`) that takes `get_for_update` on that
tombstone in the SAME system transaction as `put_task_v2`. The three producers
that must use it: cron reconcile enqueue, post-exec cron next-fire, and the SQL
cron-enqueue path. A concurrent DROP-reap (tombstone put) and an enqueue
(tombstone `get_for_update`) then conflict under pessimistic txns — at most one
commits, and on enqueue retry the committed tombstone forces suppression. The
former self-healing residual (a DROP committing between the tenant fence and the
system enqueue) is therefore PREVENTED, not merely recoverable. `db_id` is
monotonic / non-recycled, so the tombstone is safe to keep forever and can never
falsely fence a future database.

A tombstone write failure (e.g. system store unavailable) must NOT fail DROP: it
returns an error from the reap, which the DROP path logs and ignores, degrading
to the prior self-healing behavior (the consumer-side `get_database_by_id → None`
skip in `execute_task`, plus the queue reap, remain as defense-in-depth).

### Missing database during sweep

A missing database read is not proof the row should be deleted; it can be a
cross-domain race (e.g. CREATE between row write and tenant commit).

- Delete only after the same replica observes the absence in ≥2 of its own
  completed cycles (in-memory strikes; restart resets — fail-safe), or after
  a confirmed tombstone / disabled keyspace.
- Deletion always uses ordered cleanup (K5).

### Disabled keyspace

- Do not repeatedly acquire tenant clients; back off at entry level.
- Retain the inventory row and queue state until the keyspace's lifecycle
  outcome is known: re-enabled (sweep resumes normally — nothing was lost,
  because nothing load-bearing was deleted) or decommissioned (operator-driven
  ordered cleanup).
- Never delete recovery-relevant state on the DISABLED signal alone.

## API Boundary Rules

Not publicly callable from feature code:

- direct `delete_worker_registry`;
- production full-registry or full-claim listing;
- raw `put_task_v2` for singleton types (nonce/pending handling is the
  singleton API's job);
- claim writes outside `worker::queue::claim` (get-then-put claims are
  forbidden by construction);
- any kernel entry point taking a raw `&str` keyspace.

Feature code calls:

```rust
worker::inventory::ensure_row(...)
worker::inventory::scan_page(...)
worker::inventory::reap_queue_then_delete(...)
worker::queue::enqueue_singleton(...)
worker::queue::enqueue_event(...)
worker::queue::claim(...) -> ClaimHandle    // renew()/complete()
worker::fence::assert_db_alive(txn, db_id)  // the K8 locked read
```

## Metrics

Kernel:

- inventory pages processed, cycle completions, cycle duration;
- tenant acquisitions, idle evictions;
- entry-level and per-kind failures/backoffs;
- **contract-violation counters** (these are the alarms that a protocol is
  being violated in production): expired claims reaped, claim renewal
  failures, fence aborts, identity-check completion mismatches,
  inventory rows repaired by anti-entropy, grace deletions, outbox replays.

Feature:

- DDL journal entries recovered;
- trigger outbox rows replayed vs fast-path enqueued;
- HNSW pending indexes observed per full cycle; S3 objects listed/deleted;
- storage scans enqueued vs skipped-pending;
- cron jobs reconciled, history rows GC'd;
- CIC states repaired vs skipped-pending.

Batch metrics must not be named as global backlog metrics unless they cover a
full cycle.

## Required Behavioral Tests

Source-string tests cannot see these contracts; each item below is a runtime
behavior test.

Kernel — claims and fence:

- Two concurrent claim attempts on one task yield exactly one winner.
- A claim under active renewal is never reaped; after renewal stops, the
  lease expires, GC reaps it, and the task becomes claimable again.
- Renewal failure cancels the running execution before its next tenant
  commit.
- A background transaction committing after DROP's metadata delete aborts at
  the fence; after drop completes, no keys exist in the destroyed range
  (resurrection test).
- Cross-store next-fire fence (#2628 item 2): a DROP-reap that writes the
  dropped-DB tombstone and a concurrent cron next-fire enqueue that reads it via
  `get_for_update` in the same system txn as `put_task_v2` cannot both commit —
  exactly one wins; when DROP wins, no stale `_sys_worker` next-fire row remains
  for the dropped db_id, and a tombstone for one db_id never fences another.
  (`dropped_db_tombstone_makes_cross_store_nextfire_orphan_impossible`).

Kernel — inventory:

- `DEFAULT` and `default` cannot produce two live inventory rows; kernel APIs
  reject raw strings at compile time.
- Direct inventory delete is unavailable outside ordered cleanup; queue reap
  failure retains the row.
- Missing database requires ≥2 same-replica cycle observations before
  deletion.
- A large sweep never retains more than the page limit of idle tenant clients
  after eviction — including when entry processing fails after acquisition.
- The observation walk's snapshot is registered with the GC safepoint for its
  full duration.
- A stored `DEFAULT` alias row is migrated to the canonical row on visit and
  the database is not swept twice.

Queue:

- Repeated singleton reconciliation against a pending or claimed task leaves
  exactly one logical task and does not change its nonce (no redundant
  storage-scan after completion).
- Completing an older deterministic attempt cannot delete a newer singleton
  task.
- Cron/event tasks are never collapsed by singleton identity.

Feature:

- With `task_types = 0` on the inventory row, DDL journal entries, pending
  HNSW deltas, incomplete CIC states, and cron jobs are all still discovered
  and recovered (bit-independence, generalized from the HNSW guarantee).
- Worker-disabled journaled DDL either ensures the inventory row before the
  journal can commit or fails before creating journaled state.
- A crash between tenant DML commit and trigger enqueue leaves an outbox row
  that the next sweep replays; replay after successful execution does not
  fire twice in the common path (and documented at-least-once otherwise).
- CIC repair does not invalidate an index with pending or claimed BgDdl work.
- HNSW S3 cleanup stays within configured page budgets.
- Cron reconcile + history GC do bounded per-database work per visit.

## Review Checklist

Every worker change answers:

- What is the feature truth, and does the per-visit probe see it without any
  registry bit?
- Is the producer's `ensure_row` fail-closed and ordered before the dependent
  commit?
- What is the enqueue identity? Does it go through the singleton/event API
  (nonce + pending check inside)?
- Does every tenant-writing transaction cross the liveness fence?
- What happens at lease expiry mid-execution — which truth-side guard or
  idempotency covers the replay?
- What is the maximum work per visit and per sweep page, multiplied by
  replica count?
- Which failure-backoff scope applies, and is its state loss fail-safe?

## Decided and Deferred Questions

Decided in v2:

- **CREATE DATABASE ordering:** inventory row before tenant commit,
  fail-closed, with missing-row grace (rationale above).
- **Registry bits:** diagnostic only, never consulted; the false-negative
  hint class is abolished rather than defended.
- **HNSW-capable database inventory:** closed — unconditional probing over
  the shared walk is the design; no separate feature inventory.
- **Claim model:** leased + CAS acquisition + renewal; at-most-once while
  leased; same-cluster effects covered by the shared-cluster argument.

Deferred, with named triggers:

- **Sweep page leases:** add only if measured duplicate-sweep cost (R × walk)
  is material in production.
- **Cluster-level execution capability:** add only when a heterogeneous
  worker deployment is actually wanted; until then the homogeneity constraint
  (K1) holds and admission stays behind one helper.
- **Tenant-local feature summary (walk gate):** if the measured per-visit
  walk cost on feature-less databases becomes material, add a per-database
  feature-summary row **in the tenant store**, maintained in the same
  transaction as index DDL. Being same-domain, it cannot have cross-store
  false negatives — unlike registry bits, which remain banned as gates.
- **Foreground liveness fencing:** same primitive as K8, separate effort.

# Part II — Implementation Specification

Normative. Conventions used throughout:

- **Txn modes.** `store.begin()` is pessimistic; `store.begin_optimistic()`
  is optimistic. Rule: any read whose result gates a write to the same or a
  dependent key must be a **locked read** (`get_for_update`) inside a
  pessimistic transaction, unless this spec explicitly states the race is
  benign. Long read-only scans use optimistic/snapshot transactions and must
  register with the GC safepoint via `active_txn_registry`.
- **Serialization.** bincode 1.x is not self-describing: a trailing field
  with `#[serde(default)]` does NOT decode from old bytes. Struct evolution
  uses the `deserialize_compat` fallback pattern (try new struct, fall back
  to the old layout); new value *families* use a leading format byte
  (`[VERSION][bincode]`, as `WQ_FORMAT_V1` does).
- **Time.** Epoch milliseconds via `worker::now_epoch_ms()`. Leases compare
  wall clocks across processes; minimum lease is 10 s and sub-second leases
  are rejected at config parse.
- **Randomness.** Nonces and outbox ids use `rand::thread_rng()`, range
  `1..=MAX` (zero is reserved as "absent").

## II.1 Module layout

```
src/worker/
  kernel/
    mod.rs        // CanonicalKeyspace, TaskIdentity, re-exports
    inventory.rs  // ensure_row, scan_page, reap_queue_then_delete, migrate_alias_row
    claim.rs      // ClaimHandle: acquire / renew / complete / release; lease GC helper
    queue.rs      // enqueue_singleton, enqueue_event (thin wrappers over put_task_v2)
    fence.rs      // assert_db_alive
  engine.rs       // execution loop (tick): consumes kernel only
  engine/sweep.rs // maintenance loop: visits, probes, observation walk
  config.rs       // + new knobs (II.6)
src/sql/triggers/outbox.rs   // tenant trigger outbox write/replay helpers
src/storage/encoding/metadata_keys.rs  // + outbox key encoders
src/storage/tikv_store/worker.rs       // raw KV ops only (no policy)
```

After adoption, the following are **deleted or made non-`pub`**:
`update_registry_task_types` (all call sites become `ensure_row`),
registry-bit gating in the sweep, the post-commit `tokio::spawn` trigger
enqueue in `executor/core/mod.rs`, per-call-site nonce generation, and the
`include_str!` source-string tests replaced by II.9.

## II.2 On-disk formats

### System keyspace (`WorkerConfig.system_keyspace`, default `_sys_worker`)

| Family | Key format | Value |
| --- | --- | --- |
| Inventory | `_worker_registry_{ks_len:u16be}{ks}_{db_id:u64be}` | `bincode(TaskRegistryEntry)` — `task_types`/`job_count` retained but diagnostic |
| Due (V2) | `_wq_due_v2_{priority:u8}{fire_time_ms:memcmp i64}{task_type:u8}{ks_len:u16be}{ks}_{db_id:u64be}_{task_id:i64be}` | `[WQ_FORMAT_V1][bincode(TaskDescriptorV2)]` (small; split types carry no command) |
| Payload (V2) | `_wq_payload_v2_{task_type:u8}{ks_len:u16be}{ks}_{db_id:u64be}_{task_id:i64be}{fire_time_ms:memcmp}` | `[WQ_FORMAT_V1][bincode(TaskPayloadV2)]` |
| Index (V2) | `_wq_idx_v2_{ks_len:u16be}{ks}_{db_id:u64be}_{task_type:u8}{task_id:i64be}{fire_time_ms:memcmp}` | `[priority:u8]` (1 byte; reconstructs the due key) |
| Queue schema version | `_wq_schema_version` | `2` after V1 rows are migrated; production queue paths are V2-only |
| Queue migration lock | `_wq_migration_lock` | `i64be` lock acquisition epoch millis for V1→V2 migration |
| Claim | `_worker_claim_{task_type:u8}{ks_len:u16be}{ks}_{db_id:u64be}_{task_id:i64be}_{fire_time_ms:i64be}` | `bincode(WorkerClaim)` — see II.3 for the V2 lease layout and compat decode |
| Bg result | `_worker_bg_result_{ks_len:u16be}{ks}_{db_id:u64be}_{task_id:i64be}` | UTF-8 result text |
| Bg id seq | per (ks, db) CAS counter key (existing) | `i64be` |
| GC instance | `_gc_instance_{instance_id}` | 17-byte state (existing) |
| Legacy due | `_worker_queue_…` (same layout as due V2) | `bincode(TaskQueueEntry)` — startup migration input only, never written |

`put_task_v2` writes due + index (+ payload for split types) in ONE
transaction; `delete_task_v2` removes the same set in ONE transaction. No
production path may write or delete a subset.

### Tenant keyspace (per database)

| Family | Key format | Value |
| --- | --- | --- |
| DDL journal | `{db_data_prefix}sys_ddl_journal_{journal_id:u64be}` | `bincode(DdlJournalEntry)` (existing) |
| Trigger outbox (NEW) | `{db_data_prefix}sys_trigger_outbox_{outbox_id:i64be}` | `[OUTBOX_FORMAT_V1=1][bincode(TriggerOutboxRow)]` |
| Liveness key | `_sys_dbid_{db_id}` (`encode_database_id_key`) | database definition (existing) — the K8 fence target; deleted by DROP's metadata transaction |

### Deterministic (singleton) identity constants

| Task type | priority | fire_time_ms | task_id | payload split |
| --- | --- | --- | --- | --- |
| StorageSizeScan | 200 | 0 | `db_id as i64` | no |
| HnswMerge | 192 | 0 | `hnsw_merge_task_id(table_id, index_id)` | no |
| AutoAnalyze | 128 | 0 | `table_id as i64` | no (**change**: today fire=now; becomes deterministic singleton) |
| AsyncTrigger (event) | 200 | `outbox.created_at_ms` | `outbox_id` | yes |

`uses_deterministic_queue_key()` returns true for StorageSizeScan, HnswMerge,
AutoAnalyze. Cron, BgSql, BgDdl keep their existing event identities and
priorities unchanged.

## II.3 Core types

```rust
/// The only keyspace type kernel APIs accept. Canonicalization today maps
/// the legacy alias "DEFAULT" -> "default" and is otherwise identity.
pub struct CanonicalKeyspace(String);
impl CanonicalKeyspace {
    pub fn from(raw: &str) -> Self;   // applies alias mapping
    pub fn as_str(&self) -> &str;
}

pub struct TaskIdentity {
    pub ck: CanonicalKeyspace,
    pub db_id: u64,
    pub task_type: TaskType,
    pub task_id: i64,
    pub fire_time_ms: i64,
}
impl TaskIdentity {
    pub fn singleton(ck, db_id, task_type) -> Self;  // II.2 constants
    fn due_key(&self, priority: u8) -> Vec<u8>;
    fn claim_key(&self) -> Vec<u8>;
}

/// V2 claim value. Decode: try this struct; on error fall back to the legacy
/// {worker_id, claimed_at, task_type} layout and synthesize
/// lease_until_ms = claimed_at + orphan_timeout_sec*1000.
pub struct WorkerClaim {
    pub worker_id: String,
    pub claimed_at: i64,      // refreshed on EVERY renewal (see M1, II.8)
    pub task_type: TaskType,
    pub lease_until_ms: i64,  // NEW
}

pub struct TriggerOutboxRow {
    pub outbox_id: i64,        // random 1..=i64::MAX
    pub trigger_id: i64,
    pub command: String,
    pub username: String,
    pub created_at_ms: i64,    // becomes the queue fire_time (deterministic)
}

pub enum EnqueueResult { Enqueued, AlreadyPending }
```

## II.4 Kernel API contracts

Each entry states: transaction ownership, steps, and concurrency argument.

### inventory::ensure_row(sys, ck, db_id) -> Result<bool>

Owns its transaction. `get(registry_key)`; if absent, `put` a fresh
`TaskRegistryEntry`; commit. Returns whether it created the row. The
create/create race is benign (identical content, bits are diagnostic) — no
locked read. Callers that need fail-closed semantics use
`ensure_row_with_retry(sys, ck, db_id, attempts=3)` (exponential backoff from
25 ms) and propagate the error.

### inventory::scan_page(sys, txn, start_after, limit) -> (Vec<Entry>, Option<cursor>)

Caller's transaction. Range `[start_after+0x00, prefix_end)`, scan `limit`.
Next cursor = last key iff `len == limit`. Never materializes the registry.

### inventory::reap_queue_then_delete(sys, ck, db_id) -> Result<usize>

Owns its transactions. Phase 1: read txn collects the work list from the V2
index (`_wq_idx_v2_` prefix for (ck, db)). Phase 2: delete in batches of 256
per transaction via `delete_task_v2` (idempotent; safe to retry). Phase 3:
separate txn deletes the registry row. Any error in phase 1–2 returns before
phase 3 — the row is the durable retry anchor. Legacy `_worker_queue_` rows are
not consulted here; startup schema migration must convert them before normal
production paths run.

### inventory::migrate_alias_row(sys, alias_entry) -> Result<()>

Called by the sweep when `scan_page` yields a row whose stored keyspace ≠
`CanonicalKeyspace::from(stored)`. Steps: `ensure_row` for the canonical
keyspace; then `reap_queue_then_delete(sys, alias_as_stored, db_id)` — the
full ordered cleanup, because legacy queue rows may be keyed under the alias
string. Idempotent; safe under concurrent replicas.

### claim::acquire(sys, identity, lease_sec, worker_id) -> Result<Option<ClaimHandle>>

One pessimistic transaction:

1. `get_for_update(claim_key)` — the locked read IS the CAS.
2. `Some(v)` where `decode_compat(v).lease_until_ms >= now` → commit nothing,
   return `None` (held by a live owner).
3. `Some(v)` expired → **takeover**: proceed as if absent (this makes
   recovery from a dead worker independent of GC timing).
4. Absent or expired → `put(claim_key, WorkerClaim{worker_id, claimed_at: now,
   task_type, lease_until_ms: now + lease_sec*1000})`; commit; return a
   `ClaimHandle` carrying the identity, a local copy of the lease, and a
   fresh `CancellationToken`.

### ClaimHandle::renew(sys) / spawn_renewal

`renew`: one pessimistic txn — `get_for_update(claim_key)`; absent or
`worker_id != self` → `Err(RenewLost)`; else `put` with
`claimed_at = now` and `lease_until_ms = now + lease_sec*1000`; commit;
update the local lease copy. (`claimed_at` is refreshed so a mixed-version
fleet's OLD GC, which reaps on `claimed_at` age, never reaps a renewed
claim.)

`spawn_renewal(handle)`: background task ticking every
`claim_renew_interval_sec`; on any `Err` it fires `handle.cancel` and stops.
The executor passes `handle.cancel` into `run_with_guards` so a lost lease
cancels execution, and additionally checks `handle.locally_valid(now)` before
each tenant commit (belt-and-suspenders; the fence read covers the database
side, this covers the lease side).

### ClaimHandle::complete(sys, processed_nonce, outcome) / release(sys)

`complete`, one transaction:

1. `delete(claim_key)`.
2. If the task type keeps its row on failure (`HnswMerge`) and outcome is
   failure → skip row deletion.
3. Else if deterministic type: `get(due_key)`; decode the current nonce
   (descriptor for V2, `deserialize_compat` for legacy); delete the due
   entry **only if** `current_nonce == processed_nonce`.
4. Else (event types): delete the scanned due key unconditionally.
5. Commit. (Cron requeue of the next fire and the BgSql result write join
   this same transaction, as today.)

`release` deletes only the claim (used when declining to execute after a
takeover, e.g. the post-claim existence re-check fails).

Before any terminal *tenant* commit that precedes `complete` on a path where
the renewer has already stopped (cron finalize), re-check claim ownership
(`is_worker_claim_owned_by` on `_sys_worker`) and, if lost, skip both the
terminal commit and `complete`/cleanup — the takeover worker is the authority.
See §K4 "Commit-adjacent ownership fence on long-task paths".

### queue::enqueue_singleton(sys, task_type, ck, db_id) -> Result<EnqueueResult>

Owns its transaction. Build `TaskIdentity::singleton`; `get(due_key)`:
`Some` → `AlreadyPending` (never overwrite — a pending row will run; a
claimed row's nonce must not churn). Absent → fresh random nonce,
`put_task_v2`, commit, `wake_worker()`, return `Enqueued`. The unlocked get
is benign: a lost race produces one row (last write wins on identical keys),
and "claimed" implies the row committed long before any current snapshot.
`db9_refresh_storage_stats()` surfaces `AlreadyPending` as a notice instead
of pretending to enqueue.

### queue::enqueue_event(sys, txn, entry, fire_time_ms)

Thin wrapper over `put_task_v2` for event-identity tasks (cron requeue,
bgsql, outbox replay). Validates nonce rules per type; rejects raw `&str`
keyspaces at the type level.

### fence::assert_db_alive(tenant_txn, db_id) -> Result<()>

`get_for_update(encode_database_id_key(db_id))`; `None` →
`Err(DatabaseDropped)` (caller aborts/rolls back). **Mandatory call sites**
(immediately before commit of any tenant-writing background transaction):

- `execute_task`'s per-attempt SQL transaction;
- storage-scan stats persist transaction;
- each rotated DDL-journal repair transaction;
- the CIC repair transaction;
- cron run claim/finalize transactions;
- the outbox-delete transaction of trigger replay.

Exempt with reason: HNSW merge (its `get_for_update` on the HNSW meta key —
deleted by DROP step 1 — is already an equivalent fence).

### capability::execution_capability() -> bool

Wraps `execution_enabled()`. The only admission predicate features may call;
swapping in a cluster-level capability later is a one-site change.

## II.5 Algorithms

### A1. Execution tick

1. One read txn: `scan_due_v2(now, limit=1000)`. Values are V2 descriptors
   only; legacy `_worker_queue_` rows are migrated before worker polling starts.
2. Per due entry, under the concurrency semaphore:
   a. `claim::acquire` (lease from config). `None` → skip.
   b. Post-claim existence re-check of the due key; absent → `release`, skip.
   c. Hydrate payload for split types (by exact identity).
   d. `spawn_renewal(handle)`.
   e. Execute. Every tenant-writing transaction ends with
      `fence::assert_db_alive` + `handle.locally_valid` before commit.
   f. `handle.complete(processed_nonce, outcome)` — joins cron requeue /
      bgsql result write.
3. Task-level errors are recorded per type; the tick never aborts on one
   poison entry.

### A2. Sweep visit (maintenance loop, own tokio task — never shares a
select loop with A1)

Pacing: page every `sweep_page_interval_sec` (catch-up: 2 s); when the
cursor wraps, wait `sweep_cycle_interval_sec` before the next cycle.

Per page: `inventory::scan_page(cursor, registry_reconcile_batch_size)`;
per entry:

1. If stored keyspace is non-canonical → `migrate_alias_row`, continue.
2. Entry-level backoff check (in-memory; II.6 backoff shape). Skipped
   DISABLED keyspaces are retained, never deleted on the signal alone.
3. Acquire tenant handle (canonical keyspace); record for end-of-page
   `evict_if_idle` on **all** acquired keyspaces, success or failure.
4. Resolve `get_database_by_id`. Missing → grace: first observation this
   replica records a strike; second observation in a later completed cycle →
   `reap_queue_then_delete`; clear all in-memory state for the key.
5. Probes, each wrapped in its per-kind backoff (failure of one never skips
   another):
   a. **DDL journal**: scan journal prefix; repair each entry in its own
      fenced transaction(s) with batched range deletes (existing logic).
   b. **Trigger outbox**: scan outbox prefix (batch
      `outbox_replay_batch`); for each row `enqueue_event` (deterministic
      identity → idempotent), then delete the outbox row in a fenced tenant
      txn after the enqueue commits.
   c. **Cron**: `is_cron_enabled`; reconcile catalog vs `_wq_idx_v2_` rows
      (enqueue missing, delete orphans), then bounded history GC.
   d. **Storage scan**: read stats freshness; stale →
      `enqueue_singleton(StorageSizeScan)`.
   e. **Observation walk** (A3) → for each pending HNSW index:
      `enqueue_singleton`-equivalent merge enqueue; for each transitional
      CIC index with no pending/claimed BgDdl task: mark Invalid in a fenced
      transaction.
6. No step reads `task_types`.

### A2.6 Convergent legacy-queue drain (maintenance loop, global — NOT per-db)

Runs on the maintenance loop alongside the per-database sweep (A2), but is a
single GLOBAL step over the `_worker_queue_` prefix (which is not per-database),
not part of the per-entry visit. Self-paced via an in-memory
`(legacy_drain_last_at, legacy_drain_empty_streak)`:

1. If `now - legacy_drain_last_at < legacy_drain_interval(empty_streak)` → skip.
   Cadence is `legacy_drain_active_interval` until the queue has been empty for
   `legacy_drain_grace_empty_sweeps` consecutive probes, then the slower
   `legacy_drain_idle_interval` (a single 1-key probe).
2. Drain up to `legacy_drain_max_batches_per_tick` bounded batches via
   `drain_legacy_worker_queue_batch` (V1→V2 migrate + V1 delete in one txn each),
   stopping early on the first empty batch.
3. If the budget was spent without emptying, confirm with a cheap empty-range
   probe (`legacy_worker_queue_is_empty`).
4. Empty → increment `empty_streak`; straggler seen → reset `empty_streak` to 0
   (re-arm active cadence). On any migration, wake the tick so the new V2 due
   rows are picked up.

Convergence: new producers only ever write V2, so each batch strictly drains the
legacy layer toward empty; the drain never stops while old binaries may still
write, but its converged cost is one 1-key probe per idle interval. This is the
M5 straggler protection — see §II.8 M5.

### A3. Observation walk

One optimistic snapshot per visit, registered with the GC safepoint
(quarantine on failed rollback). Page over `list_tables` up to
`walk_tables_per_visit`; per table `get_schema`; collect
`{hnsw_pending, cic_transitional}`. Keep an in-memory per-(ck, db) resume
cursor; on process restart the walk restarts (convergence over cycles).
HNSW delta detection = existing `scan(delta_prefix, limit=1)` probe; frozen
indexes skipped via meta.

### A4. Trigger outbox

Producer (replaces collect-then-`tokio::spawn`):

1. Inside the firing DML's tenant transaction: for each activated async
   trigger, write `TriggerOutboxRow` (random id, `created_at_ms = now`).
   Atomic with the DML — a fired trigger can no longer be lost.
2. Post-commit fast path (best-effort): `enqueue_event` to the system queue
   (identity from the row), then delete the outbox row in a fenced tenant
   txn. Any failure → log; the sweep replays (A2.5b).

Delivery is at-least-once (crash between enqueue commit and outbox delete,
or between execution and queue delete, replays); trigger functions must
tolerate replay. Worker-disabled fleets accumulate outbox rows durably and
drain when execution returns.

### A5. CREATE DATABASE

1. Session txn: create database metadata (uncommitted).
2. `inventory::ensure_row_with_retry(sys, ck, db_id, 3)`; failure → roll
   back the session txn, fail the statement.
3. Commit the session txn.

Crash between 2 and 3 leaves an orphan row → removed by grace (A2.4).

### A6. DROP DATABASE

1. Session txn: drop database metadata **including the liveness key**;
   commit. (Fence armed: every fenced background commit now aborts.)
2. HNSW text-key scan-delete (also disarms in-flight merges via meta).
3. S3 prefix cleanup (best-effort, bounded pages).
4. `unsafe_destroy_range` on the binary data range.
5. `inventory::reap_queue_then_delete(sys, ck, db_id)` — on failure, the
   row is retained and the sweep retries via the missing-database protocol.
   The queue reap (`reap_db_queue_entries`) STREAMS its read phase: it fetches
   ONE bounded page of the per-db V2 identity index (1-byte values), deletes that
   page in its own bounded 2PC transaction, advances the cursor, and repeats —
   it never materializes the whole per-db backlog into one Vec, and no single
   transaction builds an oversized write/lock set. Deletes stay idempotent (a
   re-deleted key is a no-op), so a retry after a partial failure is safe; the
   reap still uses ONLY the dedicated per-db prefix index (no global scan) and
   reaps every task type for the db (issue #2628 item 1).
6. Finalize the dropping guard.

### A7. Claim hygiene

The worker GC loop scans claims in batches (`gc_batch_size`), decodes with
compat, and deletes rows with `lease_until_ms < now`. Because `acquire`
takes over expired claims directly (II.4), this pass is cosmetic cleanup of
claims whose due row vanished; it must never use any criterion other than
lease expiry.

## II.6 Configuration

New knobs (env → field, default, constraint):

| Env | Default | Meaning |
| --- | --- | --- |
| `DB9_WORKER_CLAIM_LEASE_SEC` | 60 | claim lease duration; min 10 |
| `DB9_WORKER_CLAIM_RENEW_SEC` | lease/3 | renewal tick; min 5, must be < lease/2 |
| `DB9_WORKER_SWEEP_CYCLE_INTERVAL_SEC` | 600 | pause between sweep cycles; `DB9_WORKER_HNSW_SWEEP_INTERVAL_SEC` accepted as deprecated alias |
| `DB9_WORKER_WALK_TABLES_PER_VISIT` | 256 | observation-walk page budget |
| `DB9_WORKER_OUTBOX_REPLAY_BATCH` | 256 | outbox rows replayed per visit |

Existing knobs unchanged: `DB9_WORKER_ENABLED`, `DB9_WORKER_POLL_MS`
(60000), `DB9_WORKER_MAX_CONCURRENT_JOBS` (32),
`DB9_WORKER_SWEEP_PAGE_INTERVAL_SEC` (30, min 1),
`DB9_WORKER_STORAGE_SCAN_INTERVAL_SEC` (1800), registry page size
(= max_concurrent_jobs), GC knobs. `DB9_WORKER_ORPHAN_TIMEOUT_SEC` (300) is
demoted to the legacy-claim compat window and deleted in the cleanup PR.

In-memory backoff (per replica): base = the owning kind's interval,
exponential shift capped at 2^5, reset on success — current shape retained.

## II.7 Metrics

All `db9_server_worker_*`, counters unless noted:

`claim_renewals_total`, `claim_renewal_failures_total`,
`claims_taken_over_total`, `expired_claims_reaped_total`,
`fence_aborts_total`, `inventory_repairs_total{layer=create|acquire|producer}`,
`inventory_repair_failures_total`, `alias_rows_migrated_total`,
`grace_deletions_total`, `identity_mismatch_skips_total`,
`outbox_fastpath_total`, `outbox_replayed_total`,
`sweep_cycles_completed_total`, `sweep_entries_total`,
`sweep_entries_skipped_total`, per-kind `sweep_kind_failures_total{kind}`,
`legacy_queue_drained_total` (V1 stragglers migrated to V2 by the convergent
drain A2.6; steady state stops incrementing once a rolling deploy completes),
gauge `hnsw_pending_indexes_observed` (full-cycle only).

Contract-violation counters (`fence_aborts`, `identity_mismatch_skips`,
`claims_taken_over`, `inventory_repairs{layer=acquire}`, `outbox_replayed`)
are the production alarms that a protocol is being violated upstream.

## II.8 Rolling-deploy migration

- **M1 claims.** New binaries write the lease field; decode falls back to
  the legacy layout (synthesized lease = `claimed_at + orphan_timeout`).
  Renewal refreshes `claimed_at`, so an OLD binary's age-based GC never
  reaps a renewed claim during the mixed window. After the fleet upgrades,
  delete the legacy decode path and `orphan_timeout_sec`.
- **M2 queue.** Due/index/payload formats unchanged; no migration.
- **M3 alias rows.** Converge via sweep migration (II.4); no one-shot job.
- **M4 bits.** Order: (1) sweep stops reading bits (probes unconditional);
  (2) producers switch `update_registry_task_types` → `ensure_row`;
  (3) struct field stays for serialization compat, marked diagnostic.
- **M5 legacy `_worker_queue_`.** Two-stage, CONVERGENT — not one-shot.
  - *Startup bulk migration.* Acquire `_wq_migration_lock`, drain V1 rows in
    bounded batches (`drain_legacy_worker_queue_batch`: write equivalent V2
    due/index/payload rows + delete migrated V1 keys in the SAME batch txn),
    then write `_wq_schema_version = 2`. After the version is V2, normal
    production dequeue and task/db-targeted operations are V2-only.
  - *Convergent background drain (A2.6).* The startup migration only converts
    rows present when this node latched the marker. During a rolling deploy an
    OLD (pre-V2) binary keeps writing V1 rows AFTER that point; the V2-only tick
    would never dequeue or reap them, stranding cron fires / bg DDL / bg SQL /
    auto-analyze. So the maintenance loop keeps draining stragglers in BOUNDED
    batches using the SAME `drain_legacy_worker_queue_batch` primitive, on the
    `legacy_drain_active_interval` cadence. Once the legacy queue is observed
    EMPTY for a grace window of `legacy_drain_grace_empty_sweeps` consecutive
    probes, the drain downshifts to a slower `legacy_drain_idle_interval` cadence
    that costs only a single 1-key empty-range probe. The drain NEVER stops —
    a straggler resets the streak and re-arms the active cadence — because an old
    binary may write a V1 row at any point in the mixed-version window. Queue
    task loss is more severe than the outbox window (M6), so there is NO
    accepted-loss caveat here: convergence is guaranteed as long as ≥1 fleet node
    runs worker execution.
  - *Bound (reconciles #2576).* The drain is a bounded background maintenance
    step (like GC), NEVER an O(global) scan on any per-enqueue/per-dequeue hot
    path. Each active sweep migrates at most `legacy_drain_max_batches_per_tick`
    page-sized batches; the converged steady state is one 1-key probe. The
    V2-only enqueue/dequeue hot paths never touch the legacy layer. The
    `_wq_schema_version = 2` marker records only that the startup bulk migration
    ran; it does NOT gate or stop the straggler drain.
  - *Behavioral coverage (T20).* The full `legacy_queue_drain_tick` orchestration
    — straggler migration → `scan_due_v2` visibility + V1 key removal, the
    `legacy_drain_max_batches_per_tick` per-tick budget cap, and the empty-streak
    re-arm to 0 when a backlog reappears — is asserted end to end against TiKV
    (not just the pure `legacy_drain_interval` cadence function).
- **M6 outbox.** New key family; old binaries during the deploy still use
  spawn-enqueue (old loss window persists until the fleet upgrades —
  accepted).
- Every phase is individually revertable: nothing deletes old state until
  the cleanup PR.

## II.9 Test matrix

Integration tests run against TiKV (existing `#[ignore]`-gated harness, CI
integration job). No `include_str!` source assertions.

The TiKV-backed worker-V2 `#[ignore]` tests are PROMOTED into the CI
`integration-tests` job (after `start-tikv`), run with
`PD_ENDPOINTS=127.0.0.1:2379 cargo test <filter> -- --ignored --nocapture`
following the fs9 precedent. The module filters
(`storage::tikv_store::worker::tests::`, `worker::engine::tests::`, the
database-liveness test, and the production-init migration test) select exactly
the worker/storage-worker surface; the default `cargo test` job is unchanged
(these stay `#[ignore]`-gated there). One engine test
(`takeover_before_finalize_skips_stale_finalize_and_preserves_due_row`) is
excluded from the CI step via `--skip`: it races a live `claim_and_execute`
against a manual takeover on a single-node playground and is inherently
timing-sensitive, so promoting it would flake the job. Its lease/takeover
contract is still covered green by `spawned_renewer_*`, `renew_lease_once_*`, and
the storage-layer `claim_blocks_*` / `renew_extends_*` tests. Source-string
`include_str!` guards for the
commit-adjacent lease fence and the `is_claim_cancelled_error` path are replaced
by behavioral tests T19/T20; the remaining S3-path source guards (retain on
uncertain commit; route uploads through the intent helper) are kept because CI
has no S3 client to drive them behaviorally.

| ID | Contract | Given / When / Then |
| --- | --- | --- |
| T1 | claim CAS | two tasks race `claim::acquire` on one identity → exactly one handle |
| T2 | lease renewal | acquire(lease=10s), renew for 30s → GC pass reaps nothing; stop renewing → after expiry GC reaps / acquire takes over |
| T3 | renewal loss cancels | delete the claim out from under a renewal loop → cancel fires before the next tenant commit |
| T4 | takeover | expired claim + live due row → second worker acquires and executes exactly once |
| T5 | fence | start a worker txn writing rows; DROP the database; worker commit → `DatabaseDropped`; destroyed range contains no keys afterwards |
| T6 | singleton no-churn | claimed StorageSizeScan + 5 sweep enqueues → `AlreadyPending`, nonce unchanged, completion deletes the row, no second scan |
| T7 | identity check | enqueue singleton, overwrite legitimately after completion-in-flight → old `complete` does not delete the newer row |
| T8 | bit independence | inventory row with `task_types=0` + journal entry + pending HNSW delta + Building index + cron job → all four discovered in one cycle |
| T9 | outbox crash replay | commit DML writing outbox row, skip fast path → sweep enqueues and the trigger runs; row deleted after |
| T10 | outbox at-least-once bound | fast path enqueues then crashes before outbox delete → exactly one extra replay, not unbounded |
| T11 | grace | row without database survives cycle 1, deleted after cycle 2, queue reaped before row delete |
| T12 | ordered cleanup | inject reap failure → registry row retained; next cycle retries and completes |
| T13 | alias migration | seed `DEFAULT` row + queue entries → one canonical row remains, alias queue rows reaped, db swept once |
| T14 | eviction bound | sweep over N≫page keyspaces with failures → ≤ page-limit idle tenant clients after each page |
| T15 | walk budget | db with tables > budget → visit reads ≤ budget tables; pending HNSW still found within k cycles |
| T16 | walk GC guard | observation walk snapshot is registered with the safepoint for its duration (runtime assertion, not source grep) |
| T17 | CREATE fail-closed | system store down → CREATE DATABASE fails; tenant has no database |
| T18 | CIC live work | Building index with claimed BgDdl task → repair skips; after lease expiry + takeover completes → state Ready |
| T19 | merge commit-adjacency (behavioral) | seed real meta+delta; run `execute_hnsw_merge` with a lease-cancel fuse that trips on the 2nd `bail_if_cancelled` (loop-top passes, commit-adjacent fires) → returns the canonical claim-cancelled error (`is_claim_cancelled_error` true); the batch did NOT commit (delta still present, graph_version unchanged, no graph blob); a loop-top-only fence would have already committed by then. (TiKV path; no S3 needed) |
| T20 | drain-tick orchestration | seed a V1 straggler AFTER the V2 marker; one `legacy_queue_drain_tick` migrates it (visible to `scan_due_v2`, V1 key gone, empty-streak advances); a backlog > one tick's `legacy_drain_max_batches_per_tick * migration_batch` ceiling is NOT emptied in one tick and re-arms the empty-streak to 0; repeated ticks converge |
| T21 | producer durability | with `WORKER_EXECUTION_ENABLED = false`, the HNSW-merge / auto-ANALYZE / async-trigger producer storage writes still enqueue a V2 row (visible via the identity index AND to `scan_due_v2`) — producer is decoupled from local consumer execution |
| T22 | streamed DROP reap | seed > one index page for a db; `scan_index_rows_page` returns ≤ one page per call (non-None cursor only after a FULL page); `reap_db_queue_entries` deletes ALL rows for the db (same count as collect-all) with another db untouched; re-reap is a no-op |

## II.10 PR plan

1. **PR-0 tests.** Land the II.9 harness; T1–T18 written, target-behavior
   tests `#[ignore]`d with the PR that un-ignores them named in a comment.
2. **PR-1 kernel claims.** `kernel/claim.rs` (+compat decode), engine tick
   switched to acquire/renew/complete, GC switched to lease expiry.
   Un-ignores T1–T4, T7.
3. **PR-2 fence.** `kernel/fence.rs`, call sites of II.4, DROP liveness-key
   ordering verified. Un-ignores T5, T18.
4. **PR-3 sweep + inventory.** `kernel/inventory.rs`, `kernel/queue.rs`,
   CanonicalKeyspace at all entry points; sweep: unconditional probes,
   shared walk, alias migration, singleton storage-scan/auto-analyze,
   cycle-interval rename. Un-ignores T6, T8, T11–T16.
5. **PR-4 outbox.** `triggers/outbox.rs`, producer txn write, fast path,
   sweep replay; spawn path removed. Un-ignores T9–T10.
6. **PR-5 cleanup.** Producer bit-writes → `ensure_row`; legacy claim
   decode + `orphan_timeout` removed (fleet-upgrade gated); API
   privatization; source-string tests deleted. Un-ignores T17 if not
   earlier.

Each PR is independently shippable and revertable; none changes an on-disk
format destructively.

## Changes from v1

1. Registry hints demoted to diagnostics; the no-false-negative hint class is
   eliminated by unconditional per-visit probes (rule 2). The
   `false_negative_allowed` protocol field is gone.
2. K4 rewritten: CAS claim acquisition, leases with renewal, expiry-only
   reaping, kernel-owned singleton nonce + in-transaction pending check, and
   an explicit delivery guarantee per protocol.
3. New K8 database liveness fence, generalizing the HNSW meta fence; DROP
   ordering updated accordingly.
4. K6 merges per-feature tenant scans into one budgeted observation walk.
5. Async triggers get a tenant-local outbox protocol (the only
   cross-domain-safe construction); best-effort spawn-enqueue is retired.
6. Multi-replica cost model and state-residency rule made explicit; page
   leases explicitly rejected with a revisit trigger.
7. CREATE DATABASE ordering decided (was an open question).
8. Anti-entropy repairers named and made contractual (was "repairable
   defect").
9. `CanonicalKeyspace` enforced at the type level on all kernel entry points.
10. Cluster homogeneity for `DB9_WORKER_ENABLED` declared as an explicit v1
    deployment constraint behind a single admission helper.
11. (v2.1) Part II added: normative on-disk formats, kernel API contracts
    with transaction recipes, algorithms A1–A7, configuration, metrics,
    rolling-deploy migration, test matrix T1–T18, and the PR plan.
12. (v2.1) Claim takeover on expired lease at `acquire` time (recovery no
    longer waits for the GC pass); renewal refreshes `claimed_at` for
    mixed-version GC safety; AutoAnalyze becomes a deterministic singleton.

## Adoption Order

Tests first, then mechanics, then demotion, then outbox — the concrete,
revertable slicing is normative in [II.10 PR plan](#ii10-pr-plan).

## Bottom Line

```text
always-on metadata store
+ one canonical inventory row (the only load-bearing durable object)
+ paged traversal with budgeted substeps and one shared observation walk
+ executable queue with CAS-acquired, leased, renewed claims
+ ordered cleanup
+ database liveness fence
```

Everything else is an outer protocol. The registry is powerful precisely
because it is narrow: it tells the worker where to look, and nothing else.
What work exists is decided by feature truth, probed unconditionally, executed
under a lease, and fenced against the database's own death.
