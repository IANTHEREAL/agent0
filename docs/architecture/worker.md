# Worker Engine & Cron Scheduler Architecture

> **Contracts**: See [docs/sot/worker-cron.md](../sot/worker-cron.md) for normative specifications.
> **User guide**: See [docs/worker.md](../worker.md) for setup instructions, examples, and troubleshooting.

## Worker Engine (`src/worker/`)

Unified async task engine. All db9-server instances share a global task queue in TiKV — no leader election, natural load balancing via pessimistic transaction contention.

```
Startup (main.rs):
    → WorkerConfig::from_env()
    → init_system_store(pd_addrs) → TikvStore for _sys_worker keyspace
    → tokio::spawn(WorkerEngine::run())   # task processing loop
    → tokio::spawn(WorkerGc::run())       # orphan recovery + DLQ cleanup
```

### Task Types

| Task Type | Description | Trigger |
|-----------|-------------|---------|
| Cron | Scheduled SQL execution | `cron.schedule()` |
| AsyncTrigger | AFTER trigger async execution | Trigger fires on INSERT/UPDATE/DELETE |
| AutoAnalyze | Automatic statistics collection | Table modification count exceeds threshold |
| BgDdl | Background DDL operations | `CREATE INDEX CONCURRENTLY`, `REFRESH MATERIALIZED VIEW CONCURRENTLY` |
| BgSql | One-off background SQL | `pg_background_launch('...')` |

### File Map

| File | Purpose |
|------|---------|
| `engine.rs` | Core worker loop: claim tasks, execute, manage state |
| `types.rs` | Task types, claims, execution context, retry logic |
| `config.rs` | Worker configuration (enabled, polling intervals, system keyspace) |
| `gc.rs` | Garbage collector for completed tasks and orphaned claims |
| `metrics.rs` | Worker metrics tracking (tasks/second, latency, errors) |

## Cron Scheduler (`src/cron/`)

pg_cron-compatible cron job scheduling integrated with the worker engine.

| File | Purpose |
|------|---------|
| `parser.rs` | PostgreSQL cron expression parser (min/hour/day/month/dow) |
| `types.rs` | Cron job types, state, metadata |
| `config.rs` | Cron configuration (interval, concurrency) |
| `worker.rs` | Cron worker task processing |
| `process_list.rs` | pg_cron-compatible virtual table for job inspection |
