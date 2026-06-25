# DB9 Transaction and Database Lifecycle Correctness Design

**Status**: Draft

Design for fixing the common correctness class behind:

- Database deletion racing with in-flight transactions.
- Database list/delete semantics not fencing already-open sessions.
- Stale snapshot writes overwriting newer committed data.
- Transaction mode mismatches around `READ ONLY`, `READ COMMITTED`, and `SERIALIZABLE`.
- Missing PostgreSQL-compatible transaction read-only session variables.

This is a design document, not a runtime fix. It must not be used to close
`#2754`, `#2755`, or related correctness issues until implementation PRs land
and the corresponding pgwire/API/FS harnesses pass.

This document is based on a comparison with TiDB and CockroachDB. The proposed DB9 design borrows TiDB's TiKV/SI write-conflict model and CockroachDB's clearer SQL contract, descriptor lifecycle, and retry taxonomy.

## Problem Statement

The reported issues are related, but they are not one bug with one mechanical
fix. They expose two orthogonal correctness gaps:

- write visibility under DB9's effective Snapshot Isolation, where DML can use
  a `start_ts` snapshot row image as the basis for a later mutation; and
- database lifecycle fencing, where old-epoch sessions, reads, streams, and
  commits can continue after delete/list has reported the database gone.

These should share common metadata and entry-point guards, but they must remain
separate subsystems with separate tests: current-read write correctness does not
prove lifecycle fencing, and lifecycle fencing does not prove stale-write
correctness.

The same database can currently be observed through multiple surfaces, such as pgwire, API SQL, filesystem/WebSocket sessions, and background work. If those surfaces do not share the same transaction contract and database lifecycle fence, DB9 can produce impossible timelines:

1. A database is deleted and disappears from `list`.
2. A pre-existing session still writes or commits into that database.
3. A stale transaction overwrites a newer committed value.
4. A user requests one transaction mode, but DB9 reports or executes another.

The target invariant is simple:

> Once DB9 reports a database as deleted or absent, no old session, old transaction, or in-flight write from that database epoch may later commit successfully.

For database lifecycle issues such as `#2754`, the invariant is broader than
commit success:

> Once DB9 reports a database as deleted or absent, no old-epoch pgwire query,
> API SQL request, FS/WebSocket operation, scalar `fs9_*` call, or streaming
> read may later return user data from that database.

### DB9 Storage Model Constraint

This design assumes a DB9 database is a `db_id` prefix inside a shared tenant
keyspace, not a standalone TiKV/PD keyspace. That means TiKV keyspace lifecycle
states are at the wrong granularity for these bugs: disabling or tombstoning a
whole keyspace cannot safely fence one DB9 database without affecting sibling
databases.

The lifecycle fence therefore has to live in DB9 metadata and DB9 admission
paths. Physical purge must also be prefix-scoped to `(keyspace_id, db_id)` so a
range delete for one dropped database cannot over-delete another database in the
same tenant keyspace.

## Reference Implementations

### TiDB

TiDB is the closest match for DB9's TiKV-backed transaction behavior.

Useful pieces to borrow:

- TiDB exposes MySQL `REPEATABLE READ`, but the documented behavior is Snapshot Isolation.
- Normal non-locking reads use the transaction start timestamp.
- Pessimistic DML and `SELECT FOR UPDATE` use a current-read timestamp, called `forUpdateTS` in the code.
- Write conflicts are checked during pessimistic lock acquisition and TiKV prewrite/commit, not only in SQL planning.
- TiDB DDL is online and stateful. Dropping a schema moves through metadata states before physical data deletion.
- TiDB Metadata Lock and schema-version validation prevent old transactions
  from committing across unsafe schema-version gaps.
- Physical deletion is asynchronous through delete ranges and GC.

Important TiDB code paths:

- `pkg/session/txnmanager.go`: chooses transaction context providers for optimistic, pessimistic RR, RC, and serializable-like modes.
- `pkg/sessiontxn/interface.go`: defines hooks such as statement start, statement retry, statement commit/rollback, read timestamp, and for-update timestamp.
- `pkg/sessiontxn/isolation/repeatable_read.go`: implements pessimistic RR/SI with `forUpdateTS`.
- `pkg/sessiontxn/isolation/readcommitted.go`: implements statement timestamp behavior for RC.
- `pkg/ddl/schema.go` and `pkg/ddl/delete_range.go`: implement schema drop state transitions and async GC delete ranges.
- TiDB Metadata Lock documentation: explains how DDL state changes wait for
  transactions on related metadata objects before advancing.

TiDB behavior to avoid copying directly:

- TiDB's "serializable" mode is Oracle-like SI in pessimistic mode, not full MySQL or PostgreSQL serializability.
- TiDB's treatment of `START TRANSACTION READ ONLY` is not strict enough for DB9's PostgreSQL-compatible surface.

### CockroachDB

CockroachDB is the better reference for SQL-facing transaction contracts and lifecycle correctness.

Useful pieces to borrow:

- Isolation levels are explicitly mapped to KV isolation behavior.
- Unsupported or weaker levels are upgraded or rejected according to a clear compatibility policy.
- `READ ONLY` is enforced in planning/execution. Mutations and row-level locking in read-only transactions fail with a read-only transaction error.
- Explicit `READ COMMITTED` transactions use statement-level savepoints and statement retry instead of silently retrying a whole user transaction.
- Drop database marks descriptors as dropped, removes namespace entries, and schedules GC jobs.
- Transaction retry errors carry structured information about whether the whole transaction must restart or only a statement can be retried.

Important CockroachDB code paths:

- `pkg/sql/sem/tree/txn.go`: maps SQL isolation levels to KV isolation levels.
- `pkg/sql/conn_executor_exec.go`: implements explicit READ COMMITTED statement retry using savepoints.
- `pkg/sql/opt/exec/execbuilder/relational.go`: rejects mutations in read-only transactions.
- `pkg/kv/kvpb/errors.go`: classifies transaction retry errors.
- `pkg/sql/drop_database.go`: marks databases dropped, removes namespace metadata, and creates drop jobs.
- `pkg/sql/schemachanger/scplan/internal/opgen`: models descriptor transition to `DROPPED` and `ABSENT`, with GC jobs as a separate step.

CockroachDB behavior to avoid copying initially:

- Full serializable isolation with refresh/retry is powerful but expensive. DB9 should not advertise full serializability until it implements the required machinery.

## Design Goals

1. Make DB9's transaction mode visible and truthful.
2. Prevent stale snapshot writes from overwriting newer commits.
3. Ensure database delete/list/open-session behavior follows one lifecycle model.
4. Ensure every user-facing entry point uses the same database lifecycle guard.
5. Keep physical cleanup asynchronous and separate from logical deletion.
6. Avoid hidden whole-transaction retries in explicit user transactions.
7. Provide PostgreSQL-compatible errors where DB9 exposes PostgreSQL-compatible SQL.

## Non-Goals

- Implement full CockroachDB-style serializable isolation in the first phase.
- Implement complete READ COMMITTED statement retry in the first phase.
- Make physical data deletion synchronous with `DROP DATABASE` or API delete.
- Preserve old sessions after a database has been logically deleted.

## Proposed DB9 Architecture

DB9 should introduce three shared components:

1. `TransactionContract`
2. `DatabaseLifecycle`
3. `CurrentReadWritePath`

All SQL/API/filesystem entry points must go through these components.

## 1. TransactionContract

### Supported Isolation Matrix

Initial recommended behavior:

| Requested mode | Effective DB9 behavior | Recommended response |
| --- | --- | --- |
| `READ UNCOMMITTED` | Snapshot Isolation | Accept for client compatibility; report effective `REPEATABLE READ` |
| `READ COMMITTED` | Snapshot Isolation | Accept for PostgreSQL client compatibility; report effective `REPEATABLE READ` |
| `REPEATABLE READ` | Snapshot Isolation | Accept and report `REPEATABLE READ` |
| `SNAPSHOT` if exposed internally | Snapshot Isolation | Accept internally |
| `SERIALIZABLE` | Full serializable is not implemented in phase 1 | Compatibility-downgrade to SI with a NOTICE and report effective `REPEATABLE READ`; strict mode may reject |
| `READ ONLY` | Enforced transaction flag | Accept and enforce |
| `READ WRITE` | Normal write-capable transaction | Accept |

DB9 must not report `SERIALIZABLE` if it only provides SI. SI prevents lost updates when writes are checked correctly, but it does not prevent all write-skew anomalies.

Current SoT note: `docs/sot/sql-engine.md` currently says requests for
`READ COMMITTED`, `READ UNCOMMITTED`, and `SERIALIZABLE` resolve to
`repeatable read`, while some code paths preserve the user-set display value
with warnings. The design should keep `READ COMMITTED` and `READ UNCOMMITTED`
compatible with PostgreSQL clients by upgrading them to DB9's effective SI
rather than rejecting them. `READ COMMITTED` is the PostgreSQL default isolation
level, so rejecting it can break client bootstrap paths even though SI is the
stronger execution mode.

The read-back rule must be:

> `SHOW transaction_isolation` returns the effective isolation level, not the
> last requested string.

For phase 1, that means `SHOW transaction_isolation` returns
`repeatable read` whenever the engine is executing SI. DB9 may keep a separate
diagnostic field or warning for the originally requested mode, but it must not
echo `serializable` while providing only SI.

### Transaction Context

Every transaction should carry:

```text
txn_id
db_id
keyspace_id
db_epoch
isolation
access_mode          // READ ONLY or READ WRITE
start_ts
statement_read_ts
for_update_ts
entry_point          // pgwire, api_sql, filesystem, background
explicit_txn         // true for BEGIN...COMMIT user transactions
commit_permit_id     // set only after entering commit barrier
```

### Read Timestamp Rules

For `REPEATABLE READ` / SI:

- Non-locking reads use `start_ts`.
- DML and `SELECT FOR UPDATE` acquire a fresh `for_update_ts`.
- The write path rechecks and locks the latest committed version before mutation.

For future `READ COMMITTED`:

- Each statement gets a new read timestamp.
- Each explicit transaction statement is protected by a statement savepoint.
- Only statement-local retry is allowed when the error is safe for partial retry.

### Read-Only Enforcement

DB9 should implement:

- `transaction_read_only`
- `default_transaction_read_only`
- `SET TRANSACTION READ ONLY`
- `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY`

When `READ ONLY` is active, these operations must fail:

- `INSERT`
- `UPDATE`
- `DELETE`
- `UPSERT`
- DDL
- `SELECT FOR UPDATE`
- `SELECT FOR SHARE` if DB9 supports it
- Any API/filesystem operation that mutates database state

Recommended SQLSTATE: `25006` (`read_only_sql_transaction`).

## 2. DatabaseLifecycle

### Database Metadata

DB9 should maintain durable metadata for each database:

```text
db_id
keyspace_id
name
epoch
state
created_at
fence_ts
drop_started_at
drop_completed_at
purge_job_id
```

Recommended states:

```text
ACTIVE
FENCING
DROPPED
PURGING
PURGED
```

State meaning:

- `ACTIVE`: new sessions, statements, and commits are allowed.
- `FENCING`: deletion has started. New work is rejected. Existing work must drain or be cancelled.
- `DROPPED`: logical deletion is complete. The database must not appear as usable or commit any old work.
- `PURGING`: physical cleanup is running asynchronously.
- `PURGED`: physical cleanup is complete.

### List Semantics

Preferred behavior:

| State | `list databases` behavior |
| --- | --- |
| `ACTIVE` | Visible |
| `FENCING` | Visible as `deleting`, or returned by the delete-operation status API as still deleting; never reported as absent |
| `DROPPED` | Hidden/absent |
| `PURGING` | Hidden/absent |
| `PURGED` | Hidden/absent |

The key rule:

> DB9 must not return "absent" while any old-epoch transaction, query, FS
> operation, stream, upload/download token, or background task can still return
> data or commit effects.

Product APIs may choose not to show `FENCING` databases in the normal
"active databases" list, but they must then expose an explicit asynchronous
delete status. They must not make `FENCING` indistinguishable from `DROPPED` or
never-created databases.

### Epoch Fence

Every session and transaction captures the current `db_epoch` when it starts. Every operation compares its captured epoch with the current database metadata.

If the current epoch differs, or state is not `ACTIVE`, the operation is fenced.

Required checks:

- session open
- transaction begin
- statement start
- pgwire `SELECT` execution and result streaming
- API SQL read execution and result streaming
- scalar SQL FS reads such as `fs9_read`, `fs9_read_bytea`, `fs9_exists`, and `fs9_size`
- FS/WebSocket `stat`, `batch_stat`, `readdir`, `readdir_recursive`, `read_file`, `read_file_at`, and `read_file_stream`
- FS/WebSocket and SQL FS mutations such as `write_file`, `write_file_at`,
  `begin_write_stream`, `batch_write`, `fs9_write`, and `fs9_write_at`
- presigned or tokenized FS upload/download operations such as
  `create_upload`, `presign_upload_part`, `complete_upload`, `abort_upload`,
  and `prepare_download`
- file-backed table functions and readers that call FS backends
- DML planning
- DML execution
- lock acquisition
- before TiKV prewrite
- before entering commit barrier
- API filesystem mutation
- background job mutation

### Read Fence, Commit Fence, and Old-Epoch Drain

Commit permits alone are not sufficient for database delete correctness.
`#2754` includes old pgwire reads and FS `stat`/`read` paths continuing after
delete/list-absent. However, DB9 should not add a durable metadata-table write
to every read operation just to model those reads.

The design should split the problem:

- read-facing operations use leased epoch checks and in-memory active-operation
  accounting on each live DB9 node; and
- commit-facing operations use a commit fence at the TiKV 2PC boundary.

Read-facing operations include pgwire `SELECT`, API SQL reads, scalar `fs9_*`
reads, FS/WebSocket read/stat/readdir calls, and streaming responses. They must:

- compare the captured `db_epoch` with current metadata at admission;
- re-check the epoch before returning each streaming chunk or before any
  externally visible publish point;
- register in node-local active-operation state under a node lease while they
  can still return old-epoch data; and
- acknowledge completion or cancellation before their node reports the old epoch
  drained.

`DROP DATABASE` / API delete may move `ACTIVE -> FENCING` while read operations
exist, but it must not move `FENCING -> DROPPED` until every live node has
acknowledged the new epoch and either drained or cancelled old-epoch reads.
Best-effort cancellation is not enough to mark the database absent. A node that
crashes, pauses, or partitions must be handled by the lease safety rules below:
the node self-fences before lease expiry, and the coordinator waits until
`lease_until + guard` before treating it as drained.

Commit-facing operations are narrower. A transaction that may cross TiKV
prewrite/commit needs a commit permit so delete can distinguish "already inside
the 2PC boundary" from "not yet allowed to commit." `DROPPED` requires:

```text
no live node lease can still return old-epoch data
no active old-epoch commit permits
no unresolved old-epoch TiKV 2PC outcomes
```

### Commit Permit

To solve the race where a transaction has already prewritten but delete starts before commit returns, DB9 needs a commit permit.

Protocol:

1. Before TiKV prewrite, the transaction calls `try_acquire_commit_permit(db_id, db_epoch, txn_id)`.
2. The permit acquisition atomically checks that database state is `ACTIVE` and epoch matches.
3. If the check fails, the transaction aborts before prewrite.
4. If the check succeeds, DB9 records a durable or lease-backed active commit permit.
5. `DROP DATABASE` / API delete first CASes `ACTIVE -> FENCING` and increments the epoch.
6. After the CAS, no new commit permits can be acquired for the old epoch.
7. Delete waits for existing old-epoch commit permits to finish, or returns an async `deleting` state.
8. The transaction releases the permit after TiKV commit or rollback is resolved.

The permit acquisition and the `ACTIVE -> FENCING` CAS must serialize on the
same metadata row in the same consistency domain. Otherwise a transaction can
acquire a permit against a stale `ACTIVE` read while delete concurrently wins the
fence CAS, and both sides can believe they have the ordering guarantee.

This produces a clean ordering:

- If commit permit wins first, delete waits or remains `FENCING`.
- If delete fence wins first, commit cannot prewrite and must fail.
- DB9 never returns both "database absent" and later "old commit succeeded".

### Distributed Fence Coordination

The fence system must work across DB9 nodes without making every read write a
metadata row.

Recommended phase-1 shape:

1. Do not introduce a global DB9 leader. This follows the existing worker/cron
   architecture: every SQL-serving node is a peer, and TiKV pessimistic
   transactions provide single-winner claims where needed.
2. Use one per-database, per-epoch drop coordinator. The coordinator is either
   the API node that starts delete or a worker that claims the drop job through
   TiKV. If it crashes, the claim lease expires and another node resumes the
   same `db_id + epoch` drop job.
3. Store durable or lease-backed commit permits for transactions that are about
   to enter TiKV prewrite/commit.
4. Store per-node leases for read and stream fencing. Each live node tracks its
   local active old-epoch operations in memory and reports when it has observed
   the new epoch and drained or cancelled old work.
5. Use durable per-operation rows only for exceptional operations that cannot be
   represented by a live node lease, such as an external publish path without
   a DB9 revalidation callback.

The node lease must be short-lived and independent from the GC safepoint
retention window:

```text
node_id
generation
lease_until
heartbeat_at
accepts_sql
```

Startup must publish the node lease before the process accepts SQL, API, or FS
requests. Graceful shutdown removes or marks the node lease inactive. Crash
recovery relies on `lease_until` expiry.

Each node also reports per-database drain state:

```text
db_id
epoch
node_id
generation
observed_state
active_old_epoch_ops
ack_at
```

Lease safety rules:

1. Lease timestamps must come from one authority, such as PD TSO or the
   metadata store's timestamp source, with an explicit bounded-skew assumption.
   DB9 must not compare lease deadlines using unrelated per-node wall clocks.
2. A node must self-fence before its own lease can expire. If it cannot prove
   its lease is still valid by `lease_until - guard`, it must stop accepting new
   SQL/API/FS work and abort in-flight old-epoch streams before emitting more
   data, even if it has not observed the database epoch bump.
3. The drop coordinator may treat an unresponsive node as drained only after
   `lease_until + guard`, where `guard` covers the maximum configured clock
   skew and the maximum in-flight response flush window. The node stops before
   lease expiry; the coordinator proceeds only after lease expiry plus the
   safety margin.
4. `generation` is a fencing token. Drain acknowledgements, local active-op
   reports, and commit-permit ownership must include the current generation.
   A stale-generation acknowledgement from a restarted or partitioned node must
   not count as drained.

This matches the useful failure mode from durable permits without putting a
durable write on every read hot path: if commit state is uncertain, delete
stays `FENCING`; if a read-serving node crashes or partitions, its
`lease_until + guard` deadline is when the coordinator may stop waiting for that
node.

Commit permits need a stronger rule than read leases. Delete must resolve the
authoritative TiKV 2PC outcome, such as the primary lock or commit record,
before ignoring or reclaiming an old-epoch commit permit. Node lease expiry can
tell DB9 that the owner is gone; it must not be the primary proof that a
prewritten transaction failed or committed.

### Delete Flow

Synchronous delete:

```text
delete_database(db):
  metadata_txn:
    row = get_database_for_update(db)
    if row.state != ACTIVE:
      return current_delete_status
    update row set state = FENCING, epoch = epoch + 1, fence_ts = now()

  publish_invalidation(db_id, new_epoch)
  cancel_or_close_sessions(db_id, old_epoch)
  wait_for_session_cancel_ack(db_id, old_epoch, deadline)

  wait_for_live_nodes_to_ack_epoch_or_drain_old_reads(db_id, old_epoch, deadline)
  wait_until_no_active_commit_permits(db_id, old_epoch, deadline)
  wait_until_no_unresolved_tikv_2pc_outcomes(db_id, old_epoch, deadline)

  if deadline reached:
    return 202 deleting

  metadata_txn:
    assert state == FENCING
    update row set state = DROPPED, drop_completed_at = now()
    enqueue_purge_job(db_id, keyspace_id)

  return 200 deleted
```

Asynchronous delete:

```text
delete_database(db):
  CAS ACTIVE -> FENCING, epoch++
  enqueue_drop_job(db_id, old_epoch)
  claim_drop_job_with_tikv_lease(db_id, old_epoch)
  return 202 deleting
```

The background drop job drains old-epoch work and then moves the database to
`DROPPED`. It must drain read fences, commit permits, and unresolved TiKV 2PC
outcomes.

### Session Invalidation

When a database enters `FENCING`:

- New sessions are rejected.
- Idle sessions are closed or marked invalid.
- In-flight statements receive cancellation and must acknowledge either
  completion or cleanup before the database can become `DROPPED`.
- In-flight transactions that have not acquired a commit permit are aborted on the next lifecycle check.
- In-flight transactions that already acquired a commit permit are allowed to resolve, while delete waits.
- In-flight read-only statements and FS reads are not allowed to keep returning
  data after `DROPPED`; they must either finish while the database is still
  `FENCING`, or be cancelled before `DROPPED`.

### Required FS and SQL Read Surfaces

The lifecycle guard must explicitly cover the current FS read surfaces, not only
mutations:

- `FsBackend::stat`
- `FsBackend::batch_stat`
- `FsBackend::readdir`
- `FsBackend::readdir_recursive`
- `FsBackend::read_file`
- `FsBackend::read_file_at`
- `FsBackend::read_file_stream`
- `FsBackend::write_file`
- `FsBackend::write_file_at`
- `FsBackend::begin_write_stream`
- `FsBackend::batch_write`
- presigned/tokenized upload and download methods, including
  `create_upload`, `presign_upload_part`, `complete_upload`, `abort_upload`,
  and `prepare_download`
- WebSocket handlers that call `session.backend.stat/read_file/read_file_at/read_file_stream`
- SQL scalar functions such as `fs9_read`, `fs9_read_bytea`, `fs9_read_at`,
  `fs9_read_at_bytea`, `fs9_exists`, `fs9_size`, `fs9_write`, and `fs9_write_at`
- SQL table functions or readers that acquire an FS backend and then call
  `stat`, `readdir`, or `read_file`

Presigned URLs and upload tokens need special treatment because DB9 may no
longer be in the data path after the token is issued. Issuance must be rejected
once the database enters `FENCING`. Tokens that were issued while the database
was `ACTIVE` must carry `db_id + epoch`, have a short bounded TTL, and either be
revalidated by a DB9 control-plane callback before object access or be bounded
by the issuing node's lease/accounting. DB9 must not allow user-chosen long
token TTLs to keep a database in `FENCING` indefinitely.

If DB9 cannot revoke or revalidate a token, the maximum token TTL becomes a hard
upper bound on how long delete may need to remain `FENCING`; rollout must first
cap that TTL to an operationally acceptable value.

## 3. CurrentReadWritePath

DB9 must prevent stale snapshot mutations from being applied blindly.

### SI Read and Write Rules

Under DB9 `REPEATABLE READ` / SI:

- Plain `SELECT` reads at `start_ts`.
- DML uses current read and locks before mutation.
- A transaction may read stale data, but it may not write over a newer committed version without detecting a conflict.

### Required Storage Preconditions

The `#2755` root cause is not merely "missing conflict plumbing." DB9 already
has write-conflict error plumbing, but DML can choose candidate rows from the
`start_ts` snapshot and later mutate based on that stale row image. The first
fix should make DML read and lock the current row image before mutation.

Phase 1 should use primitives DB9 can implement against the current TiKV client:

1. For `UPDATE`, `DELETE`, and the update arm of `UPSERT`, lock the latest
   committed row with a storage primitive whose conflict baseline is the
   transaction's own `txn.start_ts`, and detect whether the row's MVCC version
   changed after that snapshot. If it changed, return `40001` rather than
   recomputing from the newer row. The physical TiKV pessimistic lock may still
   advance its `for_update_ts` while taking the lock; that timestamp is not the
   stale-write comparison baseline.
2. For `INSERT`, `COPY`, and unique-index checks, use one storage-level
   lock/check primitive that behaves like `write_if_absent` for primary and
   unique keys. The existence check and the write precondition must be protected
   by the same pessimistic lock or equivalent TiKV conflict primitive.
3. Add the minimal storage API needed to make the stale-write decision, such as
   `lock_current_and_check_not_newer_than(key)`. The API must derive the
   conflict baseline from the transaction's own `txn.start_ts`; it must not
   accept a caller-supplied statement timestamp or `for_update_ts`. The
   implementation may use TiKV MVCC commit timestamps only if they are exposed
   in the same timestamp domain as `txn.start_ts`; otherwise it must use an
   equivalent storage-enforced conflict primitive.
4. When the primitive uses TiKV pessimistic locking with
   `allow_lock_with_conflict` / `WakeUpModeForceLock`, the client must remember
   the actual `for_update_ts` that TiKV locked for each key and send
   `PrewriteRequest.for_update_ts_constraints` for those mutations. This is the
   server-side fence that detects a replayed stale ForceLock request which
   re-locked the key after a newer commit. A prewrite that sees a different
   lock `for_update_ts` must fail rather than treating "same transaction has a
   pessimistic lock" as sufficient.

The storage primitive is a phase gate for `#2755`, not an implementation detail
to discover late. Before wiring the SQL DML paths, the implementation PR should
first prove that DB9 can lock a current row and determine whether its latest
committed version is newer than `txn.start_ts`, either by exposing TiKV MVCC
commit timestamps through the client or by using an equivalent storage
precondition.

The comparison baseline is always the transaction snapshot timestamp,
`txn.start_ts`. It must not use `for_update_ts`, statement time, wall-clock time,
or "time of mutation." `for_update_ts` is only the current-read/lock timestamp;
using it as the conflict baseline would miss commits that happened after the
transaction began but before the lock was acquired.

The phase-1 design should not introduce DB9-maintained per-row or per-index
version tokens. They would require migration/backfill and create a second source
of truth beside TiKV MVCC. Prefer read-latest-under-lock and storage-enforced
preconditions.

Phase 1A chooses PostgreSQL-compatible SI semantics for the pgwire/API SQL
surface: `UPDATE`, `DELETE`, and the update arm of `UPSERT` fail with `40001`
when their target row was changed or deleted after the transaction snapshot.
They must not silently recompute from the newer row image. TiDB-style
current-read recompute is an implementation reference for locking, but it is not
the SQL contract DB9 should expose for `REPEATABLE READ`.

### DML Protocol

For `UPDATE` and the update arm of `UPSERT`:

1. Identify candidate keys from the query plan.
2. For each target row, read and lock the latest committed row image through
   `lock_current_and_check_not_newer_than(key)` or the equivalent
   storage primitive.
3. The stale-write check must compare the locked row's latest committed version
   against `txn.start_ts`. It must not compare against a freshly acquired
   `for_update_ts`, even if the physical TiKV pessimistic lock advances its
   `for_update_ts` while taking the lock.
4. If the locked row is missing or has an MVCC version newer than the
   transaction snapshot, return `40001`.
5. Re-evaluate the mutation predicate and RLS visibility only after the stale
   version check has passed.
6. Compute `SET` expressions and generated/index values from the snapshot row
   version that is still proven current for this transaction.
7. Buffer the write.
8. If the lifecycle fence slice is implemented, acquire a database commit
   permit before prewrite.
9. Let TiKV prewrite/commit perform final MVCC conflict checks.

The implementation must not rely on the old snapshot row collected by planning
or scanning after the target key has been selected unless storage has proven
that this row version is still current for the transaction.

### DELETE-Specific Requirements

DELETE needs a separate implementation requirement because the current analyzed
path collects `rows_to_delete` from an earlier snapshot and later batch-deletes
keys.

The fix must update `src/sql/executor/dml_analyzed/delete.rs` so that, after
candidate PKs are collected and before FK handling, trigger `RETURNING`, HNSW
cleanup, or `txn_batch_mutate_mixed`:

1. Re-fetch every candidate PK through the locked current-read storage API.
2. If a candidate row is missing but existed in the old snapshot, return `40001`
   for the stale delete conflict.
3. If the current row has an MVCC version newer than `txn.start_ts`,
   return `40001` rather than deleting a newer row image.
4. Re-evaluate the DELETE predicate, RLS visibility, and `DELETE USING`
   predicate only after the stale version check has passed.
5. Build `stmt_deleting_pks`, FK context, delete key collection, trigger
   payloads, and `RETURNING` rows from the row version that is still proven
   current for this transaction.

For `INSERT` with unique keys:

1. Lock/check the primary-key data key through the shared unique-key primitive.
2. Lock/check every non-null unique index key through the same primitive.
3. If a committed row exists, return `23505`.
4. If an uncommitted conflicting intent exists, wait or return retryable conflict according to lock policy.
5. Only then buffer row and index writes.

The current INSERT/unique paths that must move from plain snapshot reads to
locked current-read/versioned checks include:

- `src/storage/tikv_store/tables.rs::insert`, which currently checks the primary
  key with `txn.get`.
- `src/storage/tikv_store/tables.rs::insert_batch`, which currently prefetches
  primary keys with `txn.batch_get`.
- `src/storage/tikv_store/indexes.rs::create_index_entry`, which currently
  checks unique index keys with `txn.get`.
- `src/storage/tikv_store/indexes.rs::create_index_entries_batch`, which
  currently checks unique index keys with `txn.batch_get`.
- `src/sql/index_consistency.rs::resolve_unique_index_conflict`, which currently
  validates stale unique entries through `scan_index` and `batch_get_rows`.

These helpers should share one storage-level unique-key lock/check primitive so
single-row INSERT, batch INSERT, COPY, UPDATE unique checks, and UPSERT cannot
drift apart.

### Lock Ordering and Deadlock Policy

Moving more paths to current-read locks increases the need for deterministic
lock ordering. Implementations should collect the full mutation key set where
possible and acquire locks in a stable order:

1. database lifecycle fence / commit permit
2. table row data keys
3. primary-key and unique-index keys
4. secondary index keys and auxiliary metadata keys
5. external-object publish markers

Paths that cannot pre-collect keys, such as trigger-driven cascades or streaming
writes, must document their retry/deadlock policy and must not hide whole
explicit-transaction retries from the client.

For blind writes:

- Avoid blind put/delete for user-visible SQL mutations.
- If a blind write is unavoidable internally, attach an existence/version precondition.

### Execution Closure Rules (KISS)

The implementation should stay simple by making correctness a property of a few
shared entry points, not by relying on every caller to remember every rule.

1. User-data writes have only two approved paths:
   - `lock_current_row_or_40001(txn, row_key)` for `UPDATE`, `DELETE`, and the
     update arm of `UPSERT`.
   - `insert_if_absent_current(txn, key)` for primary keys, unique indexes,
     `INSERT`, `COPY`, batch insert, and unique-index cleanup.

   Callers must not make stale-write decisions from `start_ts` snapshot helpers
   such as `scan_index` or `batch_get_rows` after they have observed a unique
   conflict.

2. Database lifecycle has one admission shape:
   `admit_db_operation(db_id, kind)`, where `kind` is only `Read`, `Write`, or
   `Stream`.
   - `Read` checks the database is still active and binds the current epoch.
   - `Write` checks the database is still active and reaches the commit-permit
     path before TiKV prewrite/commit.
   - `Stream` binds the epoch at open and re-checks before each emitted or
     accepted chunk.

3. Every wait has one deadline source. Any retry loop, lock wait, lifecycle
   fence wait, background-completion wait, drain wait, lease wait, or
   post-commit wait must receive the remaining statement/retry deadline. A naked
   `sleep` loop is only valid for background maintenance that is not holding a
   user request.

4. SQL dispatch has one classification before execution: `NoDb`, `Read`,
   `Write`, `TxnControl`, or `AdminWrite`. AST SQL, prepared SQL, raw SQL,
   passthrough utilities, `COPY`, FS-backed SQL operations, and WebSocket/API
   entry points are not exceptions. `READ ONLY` rejects `Write` and
   `AdminWrite`; PostgreSQL no-transaction-block rules such as `ALTER SYSTEM`
   are checked before passthrough execution.

These rules are intentionally small. When a new surface is added, the reviewer
should only need to ask four questions: how is it classified, which lifecycle
admission does it use, which write primitive does it use, and which deadline
bounds its waits?

### Mutation Guard Choke Point

The runtime implementation should centralize stale-write and lifecycle
enforcement at the mutation choke point instead of relying on a permanent manual
audit of every caller. Keep the mechanism small:

- user table rows and indexes must go through the two approved user-data write
  paths above;
- internal metadata writes may use the existing transaction wrappers when they
  are already behind lifecycle/admission checks; and
- direct `txn.put`, `txn.delete`, `txn.insert`, or external-object publish
  helpers are private escape hatches, not ordinary user-data mutation APIs.

The one-time migration audit should include:

- SQL DML and COPY
- DDL and schema/data backfill
- cron, async trigger, HNSW merge/GC, auto-analyze, and worker tasks
- FS scalar functions and WebSocket operations
- HNSW/S3 external object cleanup and publish paths
- migration and repair jobs

After migration, raw mutation primitives should be private or restricted enough
that new code cannot bypass the guard accidentally.

### Operational Correctness Requirements

Lifecycle correctness also depends on the order in which DB9 resolves storage
and background work:

- A fenced-but-prewritten transaction outcome must be resolved before DB9 runs
  the physical range delete for that database prefix.
- Background readers and writers such as analyze, HNSW merge/GC, backup,
  repair, and async trigger workers must check database state and epoch before
  touching a `DROPPED` database prefix.
- Purge must be a prefixed range delete under `(keyspace_id, db_id)` and must
  not scan or delete sibling database prefixes in the shared tenant keyspace.

### What This Fixes

This prevents:

- stale `UPDATE` overwriting newer values
- stale `DELETE` deleting a row that changed after the transaction began
- stale `INSERT` violating a unique constraint
- stale `UPSERT` choosing insert/update based on old visibility

It does not claim to prevent all write skew. That requires full serializable isolation and is out of scope for phase 1.

## Error Contract

Recommended SQL/API mapping:

| Condition | SQLSTATE | API shape |
| --- | --- | --- |
| stale write / serialization conflict | `40001` | `409 conflict`, retryable |
| read-only transaction mutation | `25006` | `400 bad request` or `409 invalid transaction mode` |
| unique violation | `23505` | `409 conflict` |
| database does not exist | `3D000` | `404 not found` |
| database is already `DROPPED` / absent | `3D000` | `404 not found` |
| current transaction/session is fenced by database delete | `25000` or a DB9-specific class-25 invalid-transaction-state code | `409 deleting` or `423 locked` |
| connection is actually terminated by server shutdown/drop cleanup | connection-level `08xxx`/`57P01` only when teardown is real | reconnect required |

`57P01` is PostgreSQL `admin_shutdown`; clients often treat it as fatal for the
connection or pool entry. DB9 should not use it for an ordinary per-statement
database fence on a connection that can otherwise keep running.

Explicit transactions should surface errors to the client. DB9 should not secretly retry the whole transaction.

Autocommit statements may be retried internally if:

- the statement is idempotent from the user's perspective
- no result has been streamed to the client
- the transaction has not crossed a non-retryable boundary

## Test Plan

### Lifecycle Tests

1. Open pgwire transaction, pause after TiKV prewrite, delete database concurrently.
   - Expected: delete remains `FENCING` / returns async status until commit resolves, or commit fails before prewrite if fence wins.
   - Forbidden: list reports absent and old commit later returns success.

2. Open API SQL session, delete database, then execute another statement on old session.
   - Expected: statement fails with fenced/deleted database error.

3. Open filesystem/WebSocket session, delete database, then attempt mutation.
   - Expected: mutation fails through the same lifecycle guard.

4. Delete database while idle sessions exist.
   - Expected: sessions are invalidated or closed; new work cannot start.

5. Delete database while long read-only query runs.
   - Expected: either delete waits/returns async deleting, or query is cancelled before `DROPPED`.

6. Delete database while an old pgwire `SELECT` is paused before returning rows.
   - Expected: delete remains `FENCING` / async until the SELECT finishes or
     cancellation is acknowledged.
   - Forbidden: list reports absent while the old SELECT later returns data.

7. Delete database while an old FS/WebSocket `stat`, `read_file`,
   `read_file_at`, or `read_file_stream` is in progress.
   - Expected: delete remains `FENCING` / async until the read finishes or is
     cancelled with acknowledged cleanup.
   - Forbidden: list reports absent while the old FS operation later returns file metadata or bytes.

8. Delete database, observe list-absent, then call SQL scalar `fs9_read` /
   `fs9_read_bytea` / `fs9_exists` / `fs9_size` from an old session.
   - Expected: fenced/deleted database error, not stale file data.

9. Delete database, observe list-absent, then read through file-backed SQL table
   functions or readers that call `stat`, `readdir`, or `read_file`.
   - Expected: fenced/deleted database error, not stale file data.

10. Issue an old-epoch presigned download/upload token, delete the database, and
    attempt to use the token after list-absent.
    - Expected: token is expired/rejected, or delete remains `FENCING` until
      the token can no longer access old data.

11. Run the same lifecycle races with two DB9 nodes.
    - Expected: node-local caches and session registries observe the epoch
      change; one node cannot mark `DROPPED` while another node is still
      returning old-epoch data.

12. Pause or partition a node so it cannot refresh its lifecycle lease while it
    is holding an old-epoch stream.
    - Expected: the paused/partitioned node self-fences at
      `lease_until - guard` and stops returning data; the drop coordinator does
      not treat it as drained until `lease_until + guard`.
    - Forbidden: coordinator marks `DROPPED` while the paused node can still
      emit old-epoch data.

### Stale Write Tests

1. T1 reads row at start_ts. T2 updates and commits. T1 updates same row.
   - Expected: T1 fails with `40001` / lock conflict. It must not write a value
     computed from its stale snapshot, and it must not silently recompute from
     T2's newer row image under `REPEATABLE READ`.

2. T1 reads row. T2 deletes and commits. T1 updates/deletes same row.
   - Expected: T1 fails with `40001` / lock conflict and must not resurrect
     stale data.

3. T1 sees unique key absent. T2 inserts and commits. T1 inserts same unique key.
   - Expected: T1 fails with `23505` or retryable conflict.

4. T1 stale UPSERT races with T2 insert/update.
   - Expected: UPSERT decision is based on current unique-key state, not the old snapshot.

5. DELETE-specific stale path: T1 collects `rows_to_delete`; T2 updates the same
   row and commits; T1 flushes batch delete keys.
   - Expected: T1 re-fetches under lock before key collection and fails with
     `40001`; it must not delete T2's newer row.

6. INSERT primary-key stale path: T1 sees PK absent; T2 inserts and commits; T1
   single-row insert checks the PK.
   - Expected: T1 checks the PK through the locked current-read path and fails
     with `23505` or retryable conflict.

7. Batch INSERT/COPY stale path: T1 batches PK/unique checks; T2 inserts a
   conflicting row before T1 flushes mutations.
   - Expected: batch PK and unique checks use locked current-read/precondition
     behavior and surface `23505` or retryable conflict.

8. Unique-index stale cleanup path: T1 hits a duplicate index entry and calls
   `resolve_unique_index_conflict`; T2 concurrently changes the referenced row.
   - Expected: conflict resolution revalidates via the shared locked
     current-read / unique-key primitive before deleting or replacing any unique
     index entry.

9. COPY stale uniqueness path: T1 streams COPY rows and batches PK/unique checks;
   T2 inserts a conflicting key before one COPY batch flushes.
   - Expected: the COPY batch uses the same locked unique-key primitive as
     INSERT and surfaces `23505` or retryable conflict.

### Transaction Contract Tests

1. `SET TRANSACTION READ ONLY; INSERT ...`
   - Expected: `25006`.

2. `START TRANSACTION READ ONLY; SELECT FOR UPDATE ...`
   - Expected: `25006`.

3. `SHOW transaction_read_only`
   - Expected: reflects current effective transaction mode.

4. `SET default_transaction_read_only = on`
   - Expected: new transactions default to read-only.

5. `BEGIN ISOLATION LEVEL SERIALIZABLE`
   - Expected phase 1: downgrade with NOTICE and
     `SHOW transaction_isolation = repeatable read`; strict mode may reject.
     Never silently behave as SI while reporting `serializable`.

6. `BEGIN ISOLATION LEVEL READ COMMITTED`
   - Expected phase 1: accept for compatibility, execute as effective SI, and
     `SHOW transaction_isolation = repeatable read`.

7. `BEGIN ISOLATION LEVEL READ UNCOMMITTED`
   - Expected phase 1: accept for compatibility, execute as effective SI, and
     `SHOW transaction_isolation = repeatable read`.

### Harness Non-Vacuity Tests

For every concurrency test:

- Confirm both racing operations actually reached the intended failpoint/barrier.
- Confirm the row/database existed before the race.
- Confirm postconditions from a fresh session.
- Confirm error code, not just error text.
- Run through pgwire and API SQL when both surfaces exist.

### Required Failpoint Inventory

The implementation PRs must add or identify failpoints for every concurrency
claim before a test is considered non-vacuous:

- transaction paused after TiKV prewrite and before commit resolution;
- delete paused after `ACTIVE -> FENCING` and before `DROPPED`;
- SELECT/API SQL response paused after execution and before final row/result
  delivery;
- FS/WebSocket `stat`, `read_file`, `read_file_at`, and stream delivery paused
  before response;
- DELETE paused after candidate collection and before FK/trigger/RETURNING/key
  collection;
- INSERT/COPY paused after PK/unique absence check and before mutation flush;
- unique-index conflict cleanup paused before deleting or replacing stale index
  entries; and
- node lifecycle lease refresh paused while an old-epoch stream tries to emit
  more data; and
- two-node epoch propagation paused so one node has observed `FENCING` while
  another still has old-epoch work.

Every failpoint-based test must assert that both sides reached their intended
barrier before releasing either side.

### Issue Closure Conditions

- `#2763` closes only when the paused-after-prewrite race proves that delete
  remains `FENCING` until the commit resolves, or that commit fails before
  prewrite if the fence wins first.
- `#2755` closes only when UPDATE, DELETE, INSERT, UPSERT, COPY, batch unique
  checks, and unique-index cleanup all pass the stale-write tests through
  pgwire and API SQL where both surfaces exist.
- `#2754` closes only when pgwire, API SQL, scalar `fs9_*`, file-backed SQL
  readers, FS/WebSocket reads and writes, FS streams, presigned/tokenized
  upload/download paths, background workers, and the two-node lifecycle race all
  obey the `FENCING -> DROPPED` contract.

Partial phase-1 lifecycle work may close narrower implementation tasks, but it
must not close `#2754` until every listed surface is fenced.

## Rollout Plan

### Phase 1A: P0 Current-Read Write Path

This phase addresses `#2755`. It should land no later than lifecycle fencing
because stale writes are silent data corruption and are independent of
delete/list semantics.

- First land a storage-probe PR that proves DB9 can lock a current row and
  compare its latest committed version against `txn.start_ts`, or can enforce
  an equivalent write precondition.
- Add `for_update_ts` to transaction context.
- Add the minimal storage primitive needed to detect whether a locked current
  row has an MVCC version newer than `txn.start_ts`.
- Carry per-key pessimistic-lock `for_update_ts` expectations into TiKV
  prewrite through `for_update_ts_constraints`, including remapping those
  constraint indexes after region/request sharding.
- Route UPDATE, DELETE, UPSERT, INSERT, batch INSERT, COPY, and unique-index
  cleanup through locked current-read / shared unique-key check paths.
- Return `40001` for UPDATE, DELETE, and UPSERT update-arm targets changed
  after the transaction snapshot; do not silently recompute from the newer row
  image under `REPEATABLE READ`.
- Update DELETE to re-fetch current rows before FK/trigger/RETURNING/key
  collection.
- Add storage-enforced `write_if_absent` / unique-key precondition behavior for
  primary and unique indexes.
- Centralize mutation guard proof requirements at the mutation choke point.
- Remove or restrict blind user-visible writes.
- Add stale-write death-condition tests across pgwire and API SQL, including a
  racer that commits in `(txn.start_ts, lock_acquisition_ts]`; the older
  transaction's UPDATE, DELETE, and UPSERT update-arm paths must return `40001`.

### Phase 1B: P0 Lifecycle Minimum Slice

This phase addresses the core of `#2763` and the transactional subset of
`#2754`. It is not enough to close all of `#2754`.

- Add database `state` and `epoch`.
- Add statement-start lifecycle guard for pgwire and API SQL transactional
  paths.
- Add commit permit acquisition before TiKV prewrite.
- Serialize commit permit acquisition and `ACTIVE -> FENCING` on the same
  metadata row.
- Change delete/list semantics to respect `FENCING` vs `DROPPED`.
- Add session invalidation on epoch change.
- Resolve fenced old-epoch TiKV 2PC outcomes before physical purge.

### Phase 1C: Lifecycle Surface Completion

This phase completes `#2754` across the non-transactional and streaming
surfaces.

- Add leased epoch read fences and node-local active-operation accounting for
  pgwire result streaming, API SQL streaming, FS/WebSocket reads, scalar
  `fs9_*`, file-backed table readers, and background workers.
- Fence presigned/tokenized upload and download issuance once the database is
  `FENCING`.
- Bind issued external tokens to `db_id + epoch` and bounded TTL/revalidation.
- Add two-node epoch propagation tests.
- Only close `#2754` after every issue closure condition passes.

### Phase 2: SQL Contract and Read-Only Enforcement

- Keep `READ COMMITTED` and `READ UNCOMMITTED` accepted as effective SI for
  PostgreSQL client compatibility.
- Ensure `SHOW transaction_isolation` reports the effective level.
- Add `transaction_read_only` and `default_transaction_read_only`.
- Enforce read-only checks in every mutation path.
- Add error-code stable tests.

### Phase 3: Optional READ COMMITTED

- Add statement read timestamps.
- Add statement savepoints for explicit transactions.
- Add partial retry classification.
- Keep full transaction retry disabled for explicit user transactions.

### Phase 4: Optional Full Serializable

- Only advertise `SERIALIZABLE` after implementing serializable conflict detection, timestamp refresh, and retry semantics.
- Until then, compatibility-downgrade `SERIALIZABLE` to SI with a NOTICE and
  truthful effective-level reporting; strict mode may reject.

## Open Questions

1. Should API delete be synchronous by default, or should it return `202 deleting` whenever active old-epoch work exists?
2. Which API should expose `FENCING` / deleting status when the normal active
   database list omits deleting databases?
3. Are filesystem/WebSocket operations currently routed through the same SQL transaction layer, or do they need a separate lifecycle guard adapter?
4. Should SQL `DROP DATABASE` keep PostgreSQL-like behavior and fail while
   other sessions are connected, while API delete uses asynchronous fencing?
5. What maximum TTL and revalidation path should DB9 require for old-epoch
   presigned upload/download tokens?
6. What admin command and metrics should expose databases stuck in `FENCING`
   because of live old-epoch operations, commit permits, or unresolved TiKV
   outcomes?

## Recommended Decisions

1. Make delete asynchronous when old-epoch work exists. This is the safest user-facing behavior.
2. Do not report a database as absent until it reaches `DROPPED`.
3. Use leased epoch read fences for read paths and durable or lease-backed
   commit permits only at the TiKV 2PC boundary.
4. Do not introduce a global DB9 leader. Use short TTL node leases for
   liveness/drain and a per-database, per-epoch drop coordinator claimed through
   TiKV.
5. Use PostgreSQL-compatible SI conflict semantics for `UPDATE`, `DELETE`, and
   the update arm of `UPSERT`: if the target row changed after the transaction
   snapshot, return `40001` rather than recomputing from the newer row image.
6. Treat stale-write current-read fixes and lifecycle commit fencing as parallel
   P0 slices; do not defer `#2755` behind SQL compatibility work.
7. Accept `READ COMMITTED` and `READ UNCOMMITTED` as effective SI for client
   compatibility; make `SHOW` report the effective level.
8. Implement `READ ONLY` enforcement after the P0 stale-write and lifecycle
   minimum slices unless it is needed by the same code path.
9. Borrow TiDB's current-read locking mechanics for writes, TiDB Metadata Lock /
   schema validation as the lifecycle analogy, and CockroachDB's
   descriptor/drop contract for user-facing lifecycle semantics.
10. Prefer storage preconditions and shared unique-key lock/check primitives over
   DB9-maintained per-row version tokens.
11. Bind any external FS token or presigned URL to `db_id + epoch` and a bounded
   TTL before allowing `DROPPED` to mean "no old data can still be accessed."
12. Add observability before rollout: live old-epoch operations by
   database/epoch/node, commit permits, oldest lease age, FENCING duration,
   cancel-ack latency, and unresolved TiKV transaction outcomes.

## Sources

- TiDB transaction isolation: https://docs.pingcap.com/tidb/stable/transaction-isolation-levels/
- TiDB pessimistic transactions: https://docs.pingcap.com/tidb/stable/pessimistic-transaction/
- TiDB online DDL design: https://github.com/pingcap/tidb/blob/master/docs/design/2018-10-08-online-DDL.md
- TiDB Metadata Lock: https://docs.pingcap.com/tidb/stable/metadata-lock/
- TiDB transaction manager code: https://github.com/pingcap/tidb/blob/master/pkg/session/txnmanager.go
- TiDB repeatable-read provider: https://github.com/pingcap/tidb/blob/master/pkg/sessiontxn/isolation/repeatable_read.go
- TiDB read-committed provider: https://github.com/pingcap/tidb/blob/master/pkg/sessiontxn/isolation/readcommitted.go
- TiDB drop schema code: https://github.com/pingcap/tidb/blob/master/pkg/ddl/schema.go
- TiDB delete range code: https://github.com/pingcap/tidb/blob/master/pkg/ddl/delete_range.go
- CockroachDB transactions: https://www.cockroachlabs.com/docs/stable/transactions
- CockroachDB transaction layer: https://www.cockroachlabs.com/docs/stable/architecture/transaction-layer
- CockroachDB online schema changes: https://www.cockroachlabs.com/docs/stable/online-schema-changes
- CockroachDB DROP DATABASE: https://www.cockroachlabs.com/docs/stable/drop-database
- CockroachDB READ COMMITTED RFC: https://github.com/cockroachdb/cockroach/blob/master/docs/RFCS/20230122_read_committed_isolation.md
- CockroachDB transaction mode mapping: https://github.com/cockroachdb/cockroach/blob/master/pkg/sql/sem/tree/txn.go
- CockroachDB executor retry path: https://github.com/cockroachdb/cockroach/blob/master/pkg/sql/conn_executor_exec.go
- CockroachDB read-only enforcement: https://github.com/cockroachdb/cockroach/blob/master/pkg/sql/opt/exec/execbuilder/relational.go
- CockroachDB retry error taxonomy: https://github.com/cockroachdb/cockroach/blob/master/pkg/kv/kvpb/errors.go
- CockroachDB drop database implementation: https://github.com/cockroachdb/cockroach/blob/master/pkg/sql/drop_database.go
