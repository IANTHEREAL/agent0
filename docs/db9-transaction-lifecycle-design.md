# DB9 Transaction and Database Lifecycle Correctness Design

## Status

Draft design for fixing the common correctness class behind:

- Database deletion racing with in-flight transactions.
- Database list/delete semantics not fencing already-open sessions.
- Stale snapshot writes overwriting newer committed data.
- Transaction mode mismatches around `READ ONLY`, `READ COMMITTED`, and `SERIALIZABLE`.
- Missing PostgreSQL-compatible transaction read-only session variables.

This document is based on a comparison with TiDB and CockroachDB. The proposed DB9 design borrows TiDB's TiKV/SI write-conflict model and CockroachDB's clearer SQL contract, descriptor lifecycle, and retry taxonomy.

## Problem Statement

The reported issues are not independent bugs. They point to one missing boundary in DB9:

> DB9 does not yet have one authoritative layer that owns transaction semantics, database lifecycle state, and write visibility checks across every entry point.

The same database can currently be observed through multiple surfaces, such as pgwire, API SQL, filesystem/WebSocket sessions, and background work. If those surfaces do not share the same transaction contract and database lifecycle fence, DB9 can produce impossible timelines:

1. A database is deleted and disappears from `list`.
2. A pre-existing session still writes or commits into that database.
3. A stale transaction overwrites a newer committed value.
4. A user requests one transaction mode, but DB9 reports or executes another.

The target invariant is simple:

> Once DB9 reports a database as deleted or absent, no old session, old transaction, or in-flight write from that database epoch may later commit successfully.

## Reference Implementations

### TiDB

TiDB is the closest match for DB9's TiKV-backed transaction behavior.

Useful pieces to borrow:

- TiDB exposes MySQL `REPEATABLE READ`, but the documented behavior is Snapshot Isolation.
- Normal non-locking reads use the transaction start timestamp.
- Pessimistic DML and `SELECT FOR UPDATE` use a current-read timestamp, called `forUpdateTS` in the code.
- Write conflicts are checked during pessimistic lock acquisition and TiKV prewrite/commit, not only in SQL planning.
- TiDB DDL is online and stateful. Dropping a schema moves through metadata states before physical data deletion.
- Physical deletion is asynchronous through delete ranges and GC.

Important TiDB code paths:

- `pkg/session/txnmanager.go`: chooses transaction context providers for optimistic, pessimistic RR, RC, and serializable-like modes.
- `pkg/sessiontxn/interface.go`: defines hooks such as statement start, statement retry, statement commit/rollback, read timestamp, and for-update timestamp.
- `pkg/sessiontxn/isolation/repeatable_read.go`: implements pessimistic RR/SI with `forUpdateTS`.
- `pkg/sessiontxn/isolation/readcommitted.go`: implements statement timestamp behavior for RC.
- `pkg/ddl/schema.go` and `pkg/ddl/delete_range.go`: implement schema drop state transitions and async GC delete ranges.

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
| `READ UNCOMMITTED` | Not supported | Reject, or upgrade to `READ COMMITTED` only after RC exists |
| `READ COMMITTED` | Not supported in phase 1 | Reject with clear error |
| `REPEATABLE READ` | Snapshot Isolation | Accept and report `REPEATABLE READ` |
| `SNAPSHOT` if exposed internally | Snapshot Isolation | Accept internally |
| `SERIALIZABLE` | Not implemented in phase 1 | Reject, or explicitly map to `REPEATABLE READ` only in compatibility mode |
| `READ ONLY` | Enforced transaction flag | Accept and enforce |
| `READ WRITE` | Normal write-capable transaction | Accept |

DB9 must not report `SERIALIZABLE` if it only provides SI. SI prevents lost updates when writes are checked correctly, but it does not prevent all write-skew anomalies.

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
| `FENCING` | Visible with `deleting` status, or hidden only if API explicitly models async delete |
| `DROPPED` | Hidden/absent |
| `PURGING` | Hidden/absent |
| `PURGED` | Hidden/absent |

The key rule:

> DB9 must not return "absent" while an old epoch transaction can still later return `COMMIT OK`.

### Epoch Fence

Every session and transaction captures the current `db_epoch` when it starts. Every operation compares its captured epoch with the current database metadata.

If the current epoch differs, or state is not `ACTIVE`, the operation is fenced.

Required checks:

- session open
- transaction begin
- statement start
- DML planning
- DML execution
- lock acquisition
- before TiKV prewrite
- before entering commit barrier
- API filesystem mutation
- background job mutation

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

This produces a clean ordering:

- If commit permit wins first, delete waits or remains `FENCING`.
- If delete fence wins first, commit cannot prewrite and must fail.
- DB9 never returns both "database absent" and later "old commit succeeded".

### Distributed Permit Storage

The permit system must work across DB9 nodes.

Recommended options:

1. Store permits in a small internal metadata table with lease TTL and heartbeat.
2. Store per-node leases, where each node tracks local active commit permits and delete waits for all live node leases to acknowledge the new epoch.

Option 1 is simpler and more explicit. Option 2 is closer to TiDB schema-version waiting and CockroachDB descriptor lease behavior, but requires a reliable node liveness layer.

For phase 1, a durable permit table is the safer implementation.

Example permit row:

```text
permit_id
db_id
epoch
txn_id
node_id
start_ts
state              // ACTIVE, RESOLVED
heartbeat_at
expires_at
```

Delete may ignore expired permits only after confirming the owning node is dead or after resolving the underlying TiKV transaction outcome.

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

  wait_until_no_active_commit_permits(db_id, old_epoch, deadline)

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
  return 202 deleting
```

The background drop job drains permits and then moves the database to `DROPPED`.

### Session Invalidation

When a database enters `FENCING`:

- New sessions are rejected.
- Idle sessions are closed or marked invalid.
- In-flight statements receive cancellation where possible.
- In-flight transactions that have not acquired a commit permit are aborted on the next lifecycle check.
- In-flight transactions that already acquired a commit permit are allowed to resolve, while delete waits.

## 3. CurrentReadWritePath

DB9 must prevent stale snapshot mutations from being applied blindly.

### SI Read and Write Rules

Under DB9 `REPEATABLE READ` / SI:

- Plain `SELECT` reads at `start_ts`.
- DML uses current read and locks before mutation.
- A transaction may read stale data, but it may not write over a newer committed version without detecting a conflict.

### DML Protocol

For `UPDATE`, `DELETE`, and `UPSERT`:

1. Identify candidate keys from the query plan.
2. Acquire `for_update_ts` greater than or equal to the latest known timestamp.
3. For each target row, read the latest committed version at `for_update_ts`.
4. Check that the row still exists and still satisfies the mutation predicate where required.
5. Acquire a pessimistic lock or equivalent TiKV lock on the row key.
6. If latest committed version has `commit_ts > txn.start_ts` and the mutation conflicts, return `40001`.
7. Buffer the write.
8. Before prewrite, acquire a database commit permit.
9. Let TiKV prewrite/commit perform final MVCC conflict checks.

For `INSERT` with unique keys:

1. Current-read the unique index key.
2. Lock the unique index key.
3. If a committed row exists, return `23505`.
4. If an uncommitted conflicting intent exists, wait or return retryable conflict according to lock policy.
5. Only then buffer row and index writes.

For blind writes:

- Avoid blind put/delete for user-visible SQL mutations.
- If a blind write is unavoidable internally, attach an existence/version precondition.

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
| database is deleting/fenced | `57P01` or DB9-specific SQL error | `409 deleting` or `423 locked` |
| session invalidated by database drop | connection-level error | reconnect required |

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

### Stale Write Tests

1. T1 reads row at start_ts. T2 updates and commits. T1 updates same row.
   - Expected: T1 fails with `40001` or lock/write conflict.

2. T1 reads row. T2 deletes and commits. T1 updates/deletes same row.
   - Expected: T1 detects missing/newer version and fails or affects zero rows according to SQL semantics, but must not resurrect stale data.

3. T1 sees unique key absent. T2 inserts and commits. T1 inserts same unique key.
   - Expected: T1 fails with `23505` or retryable conflict.

4. T1 stale UPSERT races with T2 insert/update.
   - Expected: UPSERT decision is based on current unique-key state, not the old snapshot.

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
   - Expected phase 1: reject or explicitly report effective downgraded mode. Never silently behave as SI while reporting serializable.

6. `BEGIN ISOLATION LEVEL READ COMMITTED`
   - Expected phase 1: reject unless statement-level RC has been implemented.

### Harness Non-Vacuity Tests

For every concurrency test:

- Confirm both racing operations actually reached the intended failpoint/barrier.
- Confirm the row/database existed before the race.
- Confirm postconditions from a fresh session.
- Confirm error code, not just error text.
- Run through pgwire and API SQL when both surfaces exist.

## Rollout Plan

### Phase 1: SQL Contract and Read-Only Enforcement

- Add `transaction_read_only` and `default_transaction_read_only`.
- Reject unsupported isolation levels, or expose an explicit compatibility mode.
- Enforce read-only checks in every mutation path.
- Add error-code stable tests.

### Phase 2: Database Lifecycle Fence

- Add database `state` and `epoch`.
- Add shared lifecycle guard for all entry points.
- Add commit permit acquisition before TiKV prewrite.
- Change delete/list semantics to respect `FENCING` vs `DROPPED`.
- Add session invalidation on epoch change.

### Phase 3: Current-Read Write Path

- Add `for_update_ts` to transaction context.
- Route DML through current-read and lock/check paths.
- Add unique-key current-read checks.
- Ensure TiKV prewrite sees expected conflict metadata.
- Remove or restrict blind user-visible writes.

### Phase 4: Optional READ COMMITTED

- Add statement read timestamps.
- Add statement savepoints for explicit transactions.
- Add partial retry classification.
- Keep full transaction retry disabled for explicit user transactions.

### Phase 5: Optional Full Serializable

- Only advertise `SERIALIZABLE` after implementing serializable conflict detection, timestamp refresh, and retry semantics.
- Until then, keep `SERIALIZABLE` rejected or explicitly mapped to SI with truthful reporting.

## Open Questions

1. Should API delete be synchronous by default, or should it return `202 deleting` whenever active old-epoch work exists?
2. Which SQLSTATE should DB9 use for "database is deleting" on an existing session?
3. Does DB9 already have a node-liveness or lease layer that can replace a durable permit table?
4. Should `FENCING` databases be visible in list with status `deleting`, or hidden behind an async operation API?
5. Are filesystem/WebSocket operations currently routed through the same SQL transaction layer, or do they need a separate lifecycle guard adapter?

## Recommended Decisions

1. Make delete asynchronous when old-epoch work exists. This is the safest user-facing behavior.
2. Do not report a database as absent until it reaches `DROPPED`.
3. Use a durable permit table for phase 1 commit fencing.
4. Reject `READ COMMITTED` and `SERIALIZABLE` in strict mode until they are actually implemented.
5. Implement `READ ONLY` enforcement before changing isolation behavior.
6. Use TiDB's SI/current-read model for writes and CockroachDB's descriptor/drop contract for database lifecycle.

## Sources

- TiDB transaction isolation: https://docs.pingcap.com/tidb/stable/transaction-isolation-levels/
- TiDB pessimistic transactions: https://docs.pingcap.com/tidb/stable/pessimistic-transaction/
- TiDB online DDL design: https://github.com/pingcap/tidb/blob/master/docs/design/2018-10-08-online-DDL.md
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
