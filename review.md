# Review: Async AFTER Trigger Queue (Phase 1 MVP)

This repository now supports asynchronous `AFTER ... FOR EACH ROW` triggers via an internal TiKV-backed queue plus an in-process background worker.

## What’s Implemented

- **Queue + DLQ storage**: `_sys_tq_<id>` and `_sys_tq_dlq_<id>` keys (see `src/sql/trigger_queue.rs:1`).
- **Enqueue semantics**: AFTER-trigger events are written **in the same TiKV transaction as the DML**, so rollback does not enqueue (`src/sql/trigger_worker.rs:319`, called from `src/sql/executor_dml_ops.rs:203`).
- **Worker**: fair per-tenant batch claiming, retries, DLQ, orphan recovery, and DLQ retention GC (`src/sql/trigger_worker.rs:232`).
- **System introspection**:
  - `_pgtikv_sys_trigger_queue_stats()` (`src/sql/executor_join.rs:264`)
  - `_pgtikv_sys_trigger_dlq()` (`src/sql/executor_join.rs:463`)
- **`pg_sleep(seconds)`** (async, tableless `SELECT`) to make integration testing deterministic (`src/sql/executor.rs:1092`).
- **Integration test**: `tests/86_async_triggers.sql:1` validates async AFTER trigger execution.

## Key Correctness Points

### 1) Tenant isolation is preserved

Queue/DLQ keys deliberately use global-looking prefixes (`_sys_tq_`, `_sys_tq_dlq_`), but **they are still tenant-isolated** because pg-tikv creates a separate TiKV client per keyspace (`Config::with_keyspace(...)`). All keys are automatically prefixed by the TiKV client, so different tenants never share the same physical keys.

### 2) Event ID uniqueness across nodes

Event IDs are time-ordered and include a node identifier (10 bits) plus a per-ms sequence (12 bits). For multi-node deployments, set `PGTIKV_TRIGGER_NODE_ID` uniquely per node to eliminate any chance of collision.

When `PGTIKV_TRIGGER_NODE_ID` is not set, a best-effort derived node id is used (hostname/pid/random), but explicit configuration is strongly recommended for production clusters (`src/sql/trigger_queue.rs:41`).

### 3) AFTER trigger semantics match the design intent

- Trigger execution happens in a separate transaction from the original DML, so failures do **not** roll back user writes.
- Enqueue is atomic with the original DML transaction (eventual trigger consistency).

## Known Limitations / Follow-ups

- **Trigger body parsing is line/semicolon based** (`src/sql/trigger_worker.rs:741`), so complex PL/pgSQL (multi-line statements, semicolons in strings, etc.) may not work. This matches the existing BEFORE-trigger executor’s simplicity.
- **`search_path` context**: worker executes with `search_path = ["public"]` (not captured from the originating session).
- **Queue depth quota is best-effort**: `current_depth` is in-memory and resets on restart; it’s a fast backpressure guard, not a durable limit.
- **Long-running triggers vs orphan timeout**: if a trigger execution exceeds `PGTIKV_TRIGGER_ORPHAN_TIMEOUT_SEC`, orphan recovery can re-queue and cause duplicates.
- **Prometheus metrics** from the design doc are not wired (there is no metrics exporter pipeline in the current codebase); the sys table functions are implemented instead.
