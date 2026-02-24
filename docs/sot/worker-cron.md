# Worker & Cron Engine Contracts

## Scope

- Async task engine lifecycle: claim, execute, complete, GC.
- Task types: Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql.
- Cron expression parsing and scheduling (pg_cron-compatible).
- Worker coordination across multiple db9-server instances.

## Non-goals

- SQL execution semantics (authoritative: [sql-engine](./sql-engine.md)).
- Storage key layout (authoritative: [storage-format](./storage-format.md)).
- Auth policy (authoritative: [auth-rbac](./auth-rbac.md)).

## Contracts (MUST)

- **Exactly-once task execution**: Task claiming MUST use pessimistic TiKV transactions. If two workers attempt to claim the same task, exactly one MUST succeed and the other MUST abort.
- **No leader election**: All db9-server instances with the worker enabled MUST be equal peers. There MUST be no coordinator or leader node.
- **Orphan recovery**: Uncompleted claims older than `orphan_timeout_sec` MUST be cleaned by the GC cycle. Orphaned tasks MUST be re-triggered on their next fire time.
- **Task queue keyspace**: All worker state (queue, claims, results, registry) MUST be stored in the `_sys_worker` system keyspace.
- **Cron expression format**: Cron jobs MUST use pg_cron-compatible 5-field expressions (`minute hour day-of-month month day-of-week`).
- **Task execution identity**: Background tasks MUST execute under the identity of the user who enqueued them.
- **Concurrent job limit**: Each instance MUST stop claiming new tasks when `max_concurrent_jobs` is reached.

## Experimental

- **Auto-ANALYZE threshold formula**: `threshold + 0.1 × estimated_row_count` (default threshold = 50). This formula MAY change based on production workload feedback.

## Configuration

This module MUST NOT redefine config keys. Relevant keys are defined exactly once in [ops-config](./ops-config.md):
- `DB9_WORKER_ENABLED`, `DB9_WORKER_POLL_MS`, `DB9_WORKER_MAX_CONCURRENT_JOBS`
- `DB9_WORKER_ID`, `DB9_WORKER_STATEMENT_TIMEOUT_MS`, `DB9_WORKER_ORPHAN_TIMEOUT_SEC`
- `DB9_WORKER_GC_BATCH_SIZE`, `DB9_WORKER_SYSTEM_KEYSPACE`
- `DB9_AUTO_ANALYZE_ENABLED`, `DB9_AUTO_ANALYZE_THRESHOLD`

## Entrypoints

- `src/worker/engine.rs` — Core worker loop: claim tasks, execute, manage state
- `src/worker/types.rs` — Task types, claims, execution context, retry logic
- `src/worker/config.rs` — Worker configuration
- `src/worker/gc.rs` — Garbage collector for completed tasks and orphaned claims
- `src/worker/metrics.rs` — Worker metrics tracking
- `src/cron/parser.rs` — PostgreSQL cron expression parser
- `src/cron/types.rs` — Cron job types, state, metadata
- `src/cron/config.rs` — Cron configuration
- `src/cron/worker.rs` — Cron worker task processing
- `src/cron/process_list.rs` — pg_cron-compatible virtual table for job inspection

## Verification (Gates)

- `ci:.github/workflows/regression-gate.yml/regression-gate`
- `ci:.github/workflows/orm-tests.yml/test`
- `cmd:python3 scripts/integration_test.py --dsn "$PG_DSN" tests/`

## Change Management

Any change to worker/cron semantics (task claiming, GC behavior, cron scheduling) MUST update this document and the corresponding module entry in `docs/sot/modules.yaml`. Breaking changes require DR/ADR per #368 rules.
