# Fix CIC Issues: Complete Index Consistency Architecture

## Context

PR #831 (`fix/821-create-index-concurrent-dml`) partially fixes `CREATE INDEX CONCURRENTLY` (CIC).  
This plan closes the remaining correctness gaps with one architecture: a shared unique-conflict resolver used by both DDL backfill and DML.

### Issues to close

- `P0` Phase 2 backfill can fail unique indexes before reconcile (`Duplicate entry` -> `Invalid`)
- `P0` Stale unique entries can cause false duplicate violations on later writes
- `P1` `Building -> WriteOnly` transition gap allows missed DML
- `P1` WriteOnly DML can hit stale Phase 1 entries
- `P1` `IndexState` ordinal compatibility risk from enum reorder
- `P2` Missing deterministic regression coverage for Building-window concurrency

## Design Principles

- Keep one source of truth for unique conflict resolution.
- Keep storage API stable (`create_index_entry` unchanged).
- Make CIC state transitions atomic with phase commits.
- Prefer explicit recovery over implicit hot-path mutation.

## Precondition

Wrong-order `IndexState` was introduced in PR #831 and is not merged/deployed.

## Final Architecture

```
create_index_entry (unchanged)
  -> on "Duplicate entry" for unique index
     -> resolve_unique_index_conflict (shared module)
        -> Idempotent | StaleReplaced | RealConflict
```

Shared resolver is used by:

1. CIC Phase 2 backfill
2. DML unique writes in `WriteOnly` and `Ready`
3. Reconcile helper logic

## Implementation Plan

### 1) `IndexState` compatibility and recovery

File: `src/worker/types.rs`

- Reorder enum to: `Ready, Building, Invalid, WriteOnly` (append-only safe for `Invalid`).
- Add backward-compat unit test using old enum serialization bytes.
- Do **not** apply `read_repair()` on normal schema reads.

File: `src/worker/engine.rs`

- Add startup recovery pass: any persisted `Building`/`WriteOnly` index from prior crash is marked `Invalid` and logged for rebuild.

### 2) Shared consistency module

Add file: `src/sql/index_consistency.rs`

Expose:

- `UniqueConflictResolution`
- `is_unique_duplicate_error(&Error) -> bool`
- `resolve_unique_index_conflict(...) -> Result<UniqueConflictResolution>`
- helper routines for stale-entry detection used by reconcile

Move all duplicate/stale logic here to avoid drift between DDL and DML.

### 3) Resolver semantics (bounded retry, no storage API change)

Resolver behavior:

1. Scan unique index for conflicting PK.
2. If no PK found, immediately retry `create_index_entry` (must reinsert, not just return success).
3. If conflicting PK equals new PK -> `Idempotent`.
4. Else fetch row by PK and evaluate stale conditions:
   - row missing
   - predicate no longer matches
   - current computed index values no longer match key
5. On stale -> delete old index entry + retry insert.
6. Bounded retry (max 2 attempts) to tolerate races.
7. Return `RealConflict` when conflict is genuine or retries are exhausted.

### 4) Backfill refactor with atomic state transitions

File: `src/sql/ddl.rs`

- Refactor `backfill_index_by_name(..., set_state_on_commit: Option<IndexState>)`.
- In backfill loops: on unique duplicate, call shared resolver.
- If `set_state_on_commit` is set, update index state in same transaction before commit.

### 5) DML hooks

File: `src/sql/dml.rs`

- On unique duplicate in index writes, call shared resolver for index state `WriteOnly` **and** `Ready`.
- If resolver returns `RealConflict`, preserve current `ON CONFLICT` and error behavior.

### 6) Reconcile unique index before `Ready`

File: `src/sql/index_consistency.rs` (or `src/sql/ddl.rs` if preferred)

- Implement `reconcile_unique_index(..., set_state_on_commit: Option<IndexState>)`.

Two-pass strategy:

1. Pass 1 (batched): scan + remove stale unique entries with txn rotation.
2. Pass 2 (short final txn, no rotation): re-verify, clean residual stale entries, atomically flip to `Ready`.

### 7) Worker CIC phase orchestration

File: `src/worker/engine.rs`

Flow:

1. Phase 1: backfill + atomic `WriteOnly`
2. Phase 2: catch-up backfill (resolver handles unique duplicates)
3. Phase 3 (unique only): reconcile + atomic `Ready`
4. Any phase error -> mark index `Invalid`

Remove separate non-atomic `update_index_state(WriteOnly/Ready)` calls.

## Files to Modify

- `src/worker/types.rs`
- `src/worker/engine.rs`
- `src/sql/ddl.rs`
- `src/sql/dml.rs`
- `src/sql/index_consistency.rs` (new)
- `tests/187_worker_cic.sql`
- `tests/187_worker_cic.assert`

## Test Plan

### Unit tests

- `IndexState` backward compatibility test.
- Resolver tests:
  - idempotent conflict
  - stale row missing
  - stale predicate false
  - stale key/value mismatch
  - genuine conflict
  - retry path

### SQL integration tests

Extend `tests/187_worker_cic.sql`:

1. `CREATE UNIQUE INDEX CONCURRENTLY` succeeds.
2. Stale-entry regression: delete old row value, run CIC, reinsertion correctness.

### Controlled concurrency test

Add ignored TiKV-backed test (engine or ddl tests) that forces DML into Building window and verifies:

- stale entry is cleaned
- real duplicate still fails
- index scan returns current row PK only

CI requirement: dedicated TiKV job runs `cargo test --ignored`.

## Acceptance Criteria

- `cargo build` succeeds.
- `cargo test` succeeds.
- TiKV CI job (`cargo test --ignored`) succeeds.
- No duplicate-resolution logic outside shared consistency module.
- Post-CIC behavior:
  - no false duplicates from stale unique entries
  - real duplicates still rejected
  - planner uses index only after `Ready`.

