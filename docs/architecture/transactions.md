# Transaction & Session Management Architecture

> **Contracts**: See [docs/sot/sql-engine.md](../sot/sql-engine.md) for autocommit retry and trigger execution contracts.
> **Contracts**: See [docs/sot/storage-format.md](../sot/storage-format.md) for transaction primitives and isolation guarantees.

## Session State (`src/sql/session/`)

Per-connection state (session/ module: mod.rs, settings.rs, transaction.rs):
- Current database, search path, timezone
- Transaction state (idle, active, failed)
- Sequence value cache (NEXTVAL/CURRVAL)
- GUC settings (SET/SHOW/RESET)
- Statement timeout

Task-local session context (`src/session_context.rs`):
- Timezone scoping (per-connection, tokio task-local)
- Max sort bytes (per-connection memory limit)
- Search path isolation

## Transaction Flow

```
Autocommit (single statement):
    begin() → execute_statement() → commit()
    On conflict: retry up to 10x with exponential backoff

Explicit transaction:
    BEGIN → execute_statements... → COMMIT/ROLLBACK
    SAVEPOINT/RELEASE/ROLLBACK TO for partial rollback
    No automatic retry — application handles conflicts
```

## Trigger Execution

```
BEFORE triggers: inline during DML execution (src/sql/triggers/)
    → Body compiled and cached in TriggerCache (cache.rs)
    → Evaluated synchronously before row modification (before.rs)
    → Row reference substitution (rewrite.rs)

AFTER triggers: deferred to worker engine (src/worker/)
    → Queued during DML execution (triggers/enqueue.rs)
    → Dispatched by WorkerEngine as AsyncTrigger task type
    → Executed in background (triggers/execute.rs)
```

## Runtime Stack Budget

db9-server runs on Tokio multi-thread workers. Certain valid SQL shapes (for example, scalar subqueries over `pg_catalog` views) can produce deep async call chains during analyzed-path execution.

Runtime contract:
- `DB9_TOKIO_STACK_MB` configures Tokio worker thread stack size.
- Default is `8` MiB (`src/main.rs`).
- The setting is operational only: it does not change SQL semantics or planner/executor logic.
- Increase this value (`16`/`32`) for workloads with unusually deep nested query trees or heavy catalog introspection.
