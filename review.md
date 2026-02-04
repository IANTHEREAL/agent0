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

---

# Review: Array Encoding / GIN Pattern Handling / TSVECTOR OIDs

Reviewer feedback (verbatim from reviewer model output) for a separate patch:

- [P1] Preserve NULL elements when encoding arrays in pgwire responses — `src/protocol/handler.rs:5767-5776`
  - In `encode_value()`, the `Value::Array` branch converts `Value::Null` into the string `"NULL"` and builds a `Vec<String>` for `DataRowEncoder`; pgwire’s array text encoding then quotes `NULL`-like strings, so queries like `SELECT ARRAY[1,NULL,2]` will be emitted as `{1,"NULL",2}` (NULL becomes a literal string), and nested arrays are stringified into `"{...}"` rather than `{{...}}`.
  - This is a behavioral regression vs the previous explicit array-literal encoder; consider encoding as `Vec<Option<...>>` (so NULL stays NULL) or reverting to an explicit array formatter that preserves NULL/nesting semantics.

- [P1] Don't fall back to tsquery tokens for invalid JSON GIN patterns — `src/sql/executor/select.rs:969-976`
  - In GIN scan token derivation, `Value::Text` patterns are treated as JSON if they parse, otherwise as tsquery tokens (`extract_tsquery_gin_tokens`); since `planner::extract_gin_contains_predicate()` evaluates RHS constants without schema, JSONB `@>` patterns commonly arrive as `Value::Text`, so an invalid JSON literal like `metadata @> 'foo'` will no longer raise an error and can silently return 0 rows when the (miscomputed) token probe yields no candidates (predicate never evaluated).
  - Token extraction should be driven by the indexed column type/operator (always error on invalid JSON for `@>`, parse arrays for array columns, treat text as tsquery only for `@@`), and `Value::Null` patterns should be short-circuited instead of falling back to a full scan.

- [P2] Map TSVECTOR/TSQUERY to correct pgwire OIDs (not TEXT) — `src/protocol/handler.rs:5371-5376`
  - `datatype_to_pgtype()` maps `DataType::Tsvector`/`DataType::Tsquery` to `Type::TEXT`, so RowDescriptions will advertise TEXT OIDs for TSVECTOR/TSQUERY columns and function results; this diverges from PostgreSQL and can break clients relying on OIDs/type introspection (and is inconsistent with the updated `information_schema` reporting).
  - Use `postgres_types::Type::TS_VECTOR` and `Type::TSQUERY` (and array variants if needed) instead of `Type::TEXT`.

- [P3] Fix nested-array hashing so GIN tokens include all elements — `src/sql/gin.rs:261-266`
  - In `hash_array_element()`, the `Value::Array(nested)` case overwrites `h` with each `hash_array_element(elem)` result, so if nested arrays ever occur (e.g., via `ARRAY[ARRAY[...]]`), the token hash effectively collapses to the last nested element’s hash and loses the parent context, causing collisions and incorrect GIN behavior.
  - If nested arrays are unsupported, reject them explicitly; otherwise fold each child hash into the parent hash instead of replacing it.
