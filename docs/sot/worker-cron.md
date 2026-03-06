# worker-cron — Background task engine and cron scheduling

## Scope
- Worker task lifecycle: enqueue, claim, execute, complete, and garbage collection.
- Cron scheduling and pg_cron-compatible catalog surfaces.
- Worker coordination across db9 instances, including HNSW merge discovery/sweep.

## Non-goals
- General SQL execution semantics (authoritative: `./sql-engine.md`).
- Key layout details for worker/cron persistence (authoritative: `./storage-format.md`).
- RBAC policy semantics (authoritative: `./auth-rbac.md`).

## External Contracts
- **[Stable] Task claiming is single-winner**
  - Worker claims use pessimistic transactions so competing workers cannot both acquire the same task.
  - Evidence: `src/worker/engine.rs`, `src/storage/tikv_store/worker.rs`.

- **[Stable] No leader election**
  - Any db9 instance with worker support enabled can participate; there is no coordinator-only node role.
  - Evidence: `src/worker/engine.rs`, `src/worker/gc.rs`.

- **[Stable] System-keyspace queue with configurable keyspace name**
  - Background task metadata lives in the worker system keyspace, which defaults to `_sys_worker` and is configurable via `DB9_WORKER_SYSTEM_KEYSPACE`.
  - Evidence: `src/worker/config.rs`, `src/worker/mod.rs`, `src/storage/tikv_store/mod.rs`.

- **[Stable] Shipped task types**
  - The current task model includes `Cron`, `AsyncTrigger`, `AutoAnalyze`, `BgDdl`, `BgSql`, and `HnswMerge`.
  - Evidence: `src/worker/types.rs`, `src/worker/engine.rs`.

- **[Stable] HNSW sweep is independent from regular GC**
  - Worker GC runs two timer loops:
    - orphan-claim / cron cleanup;
    - HNSW delta backlog sweep that discovers pending merges and enqueues `HnswMerge` tasks.
  - A long HNSW sweep MUST NOT block normal claim/orphan GC cadence.
  - Evidence: `src/worker/gc.rs`.

- **[Stable] Background execution identity**
  - Background jobs execute using the stored user identity associated with the task/registry entry, not an anonymous superuser bypass.
  - Evidence: `src/worker/engine.rs`, `src/cron/types.rs`.

- **[Stable] Cron expressions use pg_cron-style 5-field syntax**
  - Current cron scheduling uses `minute hour day-of-month month day-of-week`.
  - Evidence: `src/cron/parser.rs`, `tests/170_cron_basic.sql`, `tests/178_cron_expressions.sql`.

- **[Experimental] Auto-ANALYZE threshold formula**
  - Current auto-analyze policy uses `threshold + 0.1 * estimated_row_count` with default threshold `50`.
  - Evidence: `src/worker/config.rs`, `tests/189_worker_auto_analyze.sql`.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`.

## Entrypoints
- `src/worker/engine.rs`
- `src/worker/types.rs`
- `src/worker/config.rs`
- `src/worker/gc.rs`
- `src/worker/metrics.rs`
- `src/cron/parser.rs`
- `src/cron/types.rs`
- `src/cron/config.rs`
- `src/cron/worker.rs`
- `src/cron/process_list.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/ci.yml/regression-gate`, `ci:.github/workflows/ci.yml/integration-tests`
- Local reproduce (typical):
  - `./scripts/regression_gate.sh`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/53_trigger_execution.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/170_cron_basic.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/185_worker_cron_queue.sql`

## Change Management
- Any change to worker task types, claiming rules, GC behavior, cron scheduling, or HNSW background merge behavior MUST update this document and the corresponding `docs/sot/modules.yaml` entry.
- Breaking changes require DR/ADR per #368 rules.
- Reference: https://github.com/c4pt0r/db9/issues/368
