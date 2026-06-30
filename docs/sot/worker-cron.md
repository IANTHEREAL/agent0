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

- **[Stable] Active worker executor lease**
  - There is no dedicated coordinator-only node role: any worker-enabled db9 instance may hold the active executor lease, and another instance may take over after the lease expires.
  - Only the active executor scans/drains the worker queues, runs registry sweep maintenance, and performs worker-only GC cleanup. Standby worker-enabled SQL nodes continue serving SQL and remain eligible for takeover.
  - The executor lease does not replace per-task worker claims; task claims remain the execution-level single-winner fence.
  - The executor lease MUST NOT gate GC registry publisher participation, which remains unconditional for every SQL-serving process.
  - Evidence: `src/worker/executor_lease.rs`, `src/worker/engine.rs`, `src/worker/gc.rs`, `src/storage/tikv_store/worker.rs`.

- **[Stable] System-keyspace queue with configurable keyspace name**
  - Background task metadata lives in the worker system keyspace, which defaults to `_sys_worker` and is configurable via `DB9_WORKER_SYSTEM_KEYSPACE`.
  - Evidence: `src/worker/config.rs`, `src/worker/mod.rs`, `src/storage/tikv_store/mod.rs`.

- **[Stable] GC registry participation is unconditional for SQL-serving processes**
  - Every db9 process that accepts SQL connections MUST publish its GC registry heartbeat and local `min_start_ts`, even when `DB9_WORKER_ENABLED=false`.
  - Startup MUST publish the local GC registry row before the pgwire listener accepts traffic; the periodic publisher then maintains that row on a fixed cadence.
  - `DB9_WORKER_ENABLED` gates background task execution only; it does not opt a SQL-serving node out of safepoint coordination.
  - Therefore the shared GC publish interval MUST remain below `gc_life_time` for every SQL-serving node, not just for safepoint advancers.
  - Evidence: `src/main.rs`, `src/worker/gc.rs`, `src/worker/mod.rs`.

- **[Stable] GC safepoint protection is driven by real transaction liveness**
  - The GC registry publishes only `updated_at_version` and optional `min_start_ts`.
  - The safepoint advancer computes `min(time_based_gc_life_time, min_live_instance_min_start_ts - 1)`.
  - Worker SQL task timeouts are execution limits only; they are not safepoint inputs.
  - Graceful shutdown MUST stop the local GC loops and delete the process's own GC registry row; crash recovery relies on stale-row reaping.
  - GC registry rows whose heartbeat ages past `gc_life_time` MUST be ignored for safepoint calculation and reaped from the shared registry.
  - Evidence: `src/worker/active_txn_registry.rs`, `src/worker/gc.rs`, `src/worker/engine.rs`.

- **[Stable] Shipped task types**
  - The current task model includes `Cron`, `AsyncTrigger`, `AutoAnalyze`, `BgDdl`, `BgSql`, `HnswMerge`, and `StorageSizeScan`.
  - Evidence: `src/worker/types.rs`, `src/worker/engine.rs`.

- **[Stable] StorageSizeScan derived state**
  - Storage-size accounting uses the derived-state path; there is no legacy V2 execution fallback.
  - The derived path is owned by a worker-system state row plus capacity token; progress advances only after tenant storage stats and the applied marker commit.
  - Any leftover V2 `StorageSizeScan` row is treated only as a compatibility input and is executed through the derived state machine before normal queue cleanup removes it.
  - Evidence: `src/worker/config.rs`, `src/worker/types.rs`, `src/worker/engine.rs`.

- **[Stable] HNSW sweep is independent from regular GC**
  - Worker GC runs two timer loops:
    - orphan-claim / cron cleanup;
    - HNSW delta backlog sweep that discovers pending merges and enqueues `HnswMerge` tasks.
  - A long HNSW sweep MUST NOT block normal claim/orphan GC cadence.
  - Failed `HnswMerge` work is retried by moving the same deterministic V2 descriptor to a future due time with per-index exponential backoff; dirty markers remain the recovery truth.
  - Evidence: `src/worker/gc.rs`, `src/worker/engine.rs`, `src/worker/types.rs`.

- **[Stable] DDL journal recovery is bounded**
  - Registry sweep scans DDL journal entries by page.
  - Large CREATE INDEX / CTAS orphan ranges are cleaned one range-delete batch per journal entry visit; the tenant-local journal entry remains the durable resume point until final metadata cleanup succeeds.
  - Evidence: `src/worker/engine.rs`, `src/storage/tikv_store/ddl_journal.rs`.

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
- Any change to worker task types, claiming rules, GC behavior, cron scheduling, StorageSizeScan derived behavior, or HNSW background merge behavior MUST update this document and the corresponding `docs/sot/modules.yaml` entry.
- Breaking changes require DR/ADR per #368 rules.
- Reference: https://github.com/c4pt0r/db9/issues/368
