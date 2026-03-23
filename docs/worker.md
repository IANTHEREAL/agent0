> Architecture deep-dive: [docs/architecture/worker.md](architecture/worker.md)
> Contracts: [docs/sot/worker-cron.md](sot/worker-cron.md)

# Async Worker Engine

db9-server includes a built-in async worker engine that executes background tasks. All db9-server instances share a single global task queue stored in TiKV, competing for work via pessimistic transactions — no leader election or external dependencies required.

## Overview

```
db9-server instance 1            db9-server instance 2            db9-server instance N
┌──────────────────────┐     ┌──────────────────────┐     ┌──────────────────────┐
│  SQL handler (pgwire)│     │  SQL handler (pgwire)│     │  SQL handler (pgwire)│
│  WorkerEngine        │     │  WorkerEngine        │     │  WorkerEngine        │
│  WorkerGc            │     │  WorkerGc            │     │  WorkerGc            │
└─────────┬────────────┘     └─────────┬────────────┘     └─────────┬────────────┘
          │                            │                            │
          └────────────────────────────┼────────────────────────────┘
                                       │
                              ┌────────┴────────┐
                              │  TiKV Cluster    │
                              │                 │
                              │  Task Queue     │
                              │  Task Registry  │
                              │  Worker Claims  │
                              └─────────────────┘
```

The worker engine is **enabled by default** on every db9-server instance. Each instance polls the global queue, claims due tasks, and executes them in-process. Multiple instances naturally load-balance through TiKV transaction contention — if two workers try to claim the same task, only one succeeds.

## Task Types

The engine supports five task types, all sharing the same queue infrastructure:

| Task Type | Description | How It's Triggered |
|-----------|-------------|-------------------|
| **Cron** | Scheduled SQL execution | `cron.schedule()` (requires `pg_cron` extension) |
| **AsyncTrigger** | AFTER trigger async execution | Trigger fires on INSERT/UPDATE/DELETE |
| **AutoAnalyze** | Automatic statistics collection | Table modification count exceeds threshold |
| **BgDdl** | Background DDL operations | `CREATE INDEX CONCURRENTLY`, `REFRESH MATERIALIZED VIEW CONCURRENTLY` |
| **BgSql** | One-off background SQL | `SELECT pg_background_launch('...')` |

---

## Quick Start

The worker runs automatically when db9-server starts. No additional setup needed for basic usage.

```bash
# Start db9-server — worker is enabled by default
PD_ENDPOINTS=127.0.0.1:2379 cargo run

# Connect and use background features
psql -h 127.0.0.1 -p 5433 -U admin
```

To disable background task execution on a specific instance:

```bash
DB9_WORKER_ENABLED=false PD_ENDPOINTS=127.0.0.1:2379 cargo run
```

This disables cron / async-trigger / background-task execution on that node, but a SQL-serving db9 process still participates in GC safepoint coordination by publishing transaction liveness to the shared GC registry.

---

## Features

### 1. Cron Jobs

Schedule recurring SQL statements using the `pg_cron` extension. Jobs are persisted to TiKV and survive process restarts.

```sql
-- Enable pg_cron
CREATE EXTENSION pg_cron;

-- Schedule a job: run every 5 minutes
SELECT cron.schedule('cleanup', '*/5 * * * *', $$DELETE FROM logs WHERE created_at < NOW() - INTERVAL '7 days'$$);

-- Schedule a job: run nightly at 3 AM
SELECT cron.schedule('nightly_vacuum', '0 3 * * *', 'VACUUM');

-- List all jobs
SELECT * FROM cron.job ORDER BY jobid;

-- View execution history
SELECT * FROM cron.job_run_details ORDER BY runid DESC LIMIT 10;

-- Modify a job schedule
SELECT cron.alter_job(1, '0 4 * * *');

-- Disable a job (without deleting)
SELECT cron.alter_job(1, NULL, NULL, NULL, NULL, false);

-- Delete a job
SELECT cron.unschedule('cleanup');
SELECT cron.unschedule(1);  -- by ID

-- Remove pg_cron
DROP EXTENSION pg_cron;
```

**How it works**: When `cron.schedule()` is called, the job definition is stored in the tenant's keyspace and a queue entry is written with `next_fire_time` computed from the cron expression. The worker picks it up when the time comes, executes the SQL, records the result in `cron.job_run_details`, and enqueues the next occurrence.

**Cron expression format**: Standard 5-field cron (`minute hour day-of-month month day-of-week`).

```
*     *     *     *     *
│     │     │     │     │
│     │     │     │     └─ Day of week (0-7, Sun=0 or 7)
│     │     │     └─────── Month (1-12)
│     │     └───────────── Day of month (1-31)
│     └─────────────────── Hour (0-23)
└───────────────────────── Minute (0-59)
```

Examples: `*/5 * * * *` (every 5 min), `0 3 * * *` (daily 3 AM), `0 0 * * 0` (weekly Sunday midnight).

---

### 2. CREATE INDEX CONCURRENTLY

Build indexes in the background without blocking DML operations.

```sql
-- Synchronous (blocks until complete — traditional behavior)
CREATE INDEX idx_users_email ON users (email);

-- Concurrent (returns immediately, backfill runs in background)
CREATE INDEX CONCURRENTLY idx_users_name ON users (name);
```

**How it works**:

1. **Phase 1 (synchronous)**: The index metadata is registered with `state = Building`. The command returns immediately to the user.
2. **Phase 2 (async)**: A `BgDdl` task is enqueued. The worker picks it up and backfills the index by scanning all existing rows.
3. **Completion**: The index state is set to `Ready`. Queries now use the index.

**Index states**:

| State | Meaning | Queries use it? |
|-------|---------|----------------|
| `Ready` | Fully built and usable | Yes |
| `Building` | Backfill in progress | No (skipped by planner) |
| `Invalid` | Backfill failed | No (needs rebuild) |

**If backfill fails**: The index is marked `Invalid`. Drop it and recreate:

```sql
DROP INDEX idx_users_name;
CREATE INDEX CONCURRENTLY idx_users_name ON users (name);
```

> **Note**: The current CIC implementation uses a simplified one-pass backfill. It does not implement full PostgreSQL CIC semantics (two scans + wait for concurrent transactions). This means there is a brief window during backfill where concurrent DML may not be reflected in the index. See Issue #821 for details. For critical use cases, prefer non-concurrent `CREATE INDEX` which guarantees full consistency.

---

### 3. REFRESH MATERIALIZED VIEW CONCURRENTLY

Refresh materialized views in the background.

```sql
-- Create a materialized view
CREATE MATERIALIZED VIEW sales_summary AS
  SELECT region, SUM(amount) as total
  FROM orders
  GROUP BY region;

-- Synchronous refresh (blocks until complete)
REFRESH MATERIALIZED VIEW sales_summary;

-- Concurrent refresh (returns immediately, runs in background)
REFRESH MATERIALIZED VIEW CONCURRENTLY sales_summary;
```

**How it works**: The `CONCURRENTLY` keyword enqueues a `BgDdl` task. The worker executes `REFRESH MATERIALIZED VIEW sales_summary` (without CONCURRENTLY) in the background.

---

### 4. Background SQL (`pg_background_launch` / `pg_background_result`)

Submit arbitrary SQL for background execution and retrieve results later.

```sql
-- Launch a background task (returns a task_id)
SELECT pg_background_launch('INSERT INTO audit_log SELECT * FROM temp_audit');
-- Returns: 1739836800000  (task_id, a bigint)

-- Check task status
SELECT pg_background_result(1739836800000);
-- Returns: 'pending'   (still in queue)
-- Returns: 'OK'        (completed successfully)
-- Returns: 'ERROR: ...' (failed with error message)
-- Returns: 'not found'  (no such task)
```

**Use cases**:
- Long-running data migrations
- Bulk inserts/updates that shouldn't block the client
- Fire-and-forget administrative tasks

**Behavior**:
- `pg_background_launch(sql TEXT)` → Returns `BIGINT` task_id
- `pg_background_result(task_id BIGINT)` → Returns `TEXT` status
- The task executes under the identity of the calling user
- Results persist until GC cleans them up

---

### 5. Auto-ANALYZE

Tables are automatically analyzed when modification counts exceed a threshold, keeping query planner statistics fresh without manual intervention.

**How it works**:

1. Every INSERT, UPDATE, and DELETE increments an in-memory modification counter for the affected table.
2. When the counter exceeds `50 + 0.1 × estimated_row_count`, an `AutoAnalyze` task is enqueued.
3. The worker executes `ANALYZE "table_name"` in the background.
4. The counter resets after enqueue to prevent duplicate tasks.

**Example**: A table with 10,000 rows triggers auto-ANALYZE after ~1,050 modifications (50 + 0.1 × 10000).

This feature is **enabled by default**. To disable:

```bash
DB9_AUTO_ANALYZE_ENABLED=false PD_ENDPOINTS=127.0.0.1:2379 cargo run
```

---

## Configuration

All settings are controlled via environment variables. Every setting has a sensible default — no configuration is required for typical deployments.

| Variable | Default | Description |
|----------|---------|-------------|
| `DB9_WORKER_ENABLED` | `true` | Enable background task execution on this instance. `false` does not disable GC registry participation for a SQL-serving node. |
| `DB9_WORKER_POLL_MS` | `60000` | Queue poll interval in milliseconds (minimum 100). |
| `DB9_WORKER_MAX_CONCURRENT_JOBS` | `32` | Max tasks executing concurrently per instance. |
| `DB9_WORKER_ID` | `{hostname}:{pid}` | Worker claim/logging identifier. Auto-generated if not set. It is not the GC registry identity. |
| `DB9_WORKER_STATEMENT_TIMEOUT_MS` | `300000` | Whole-task timeout for non-cron worker SQL (5 minutes). `0` disables the timeout. |
| `DB9_CRON_JOB_TIMEOUT_MS` | `1800000` | Whole-job timeout for cron execution (30 minutes). `0` disables the timeout. |
| `DB9_WORKER_ORPHAN_TIMEOUT_SEC` | `300` | Seconds before an uncompleted claim is considered orphaned (5 minutes). |
| `DB9_WORKER_GC_BATCH_SIZE` | `100` | Number of keyspaces processed per GC cycle. |
| `DB9_WORKER_SYSTEM_KEYSPACE` | `_sys_worker` | TiKV keyspace for global worker state (rarely needs changing). |
| `DB9_AUTO_ANALYZE_ENABLED` | `true` | Enable automatic ANALYZE on modified tables. |
| `DB9_AUTO_ANALYZE_THRESHOLD` | `50` | Base threshold for auto-ANALYZE (formula: threshold + 0.1 × row_count). |
| `DB9_GC_SAFEPOINT_ENABLED` | `true` | Enable PD GC safepoint advancement. |
| `DB9_GC_SAFEPOINT_INTERVAL_SEC` | `300` | GC safepoint publish/advance interval (5 minutes). Must stay below `DB9_GC_LIFE_TIME_SEC` on every SQL-serving node, even if local safepoint advancement is disabled. |
| `DB9_GC_LIFE_TIME_SEC` | `86400` | Time-based MVCC retention window (24 hours). Active transactions are protected by direct registry tracking; GC registry heartbeats older than this window are treated as stale and reaped. |

### Deployment Scenarios

**Single instance** (development / small production):

```bash
# Default: worker enabled, everything works out of the box
PD_ENDPOINTS=127.0.0.1:2379 cargo run
```

**Multi-instance** (production):

```bash
# Instance 1: SQL + Worker
PD_ENDPOINTS=pd1:2379,pd2:2379,pd3:2379 DB9_WORKER_ID=worker-1 cargo run

# Instance 2: SQL + Worker
PD_ENDPOINTS=pd1:2379,pd2:2379,pd3:2379 DB9_WORKER_ID=worker-2 cargo run

# Instance 3: no background task execution on this node
PD_ENDPOINTS=pd1:2379,pd2:2379,pd3:2379 DB9_WORKER_ENABLED=false cargo run
```

**High-throughput cron** (many scheduled jobs):

```bash
# Increase concurrency and poll frequency
DB9_WORKER_MAX_CONCURRENT_JOBS=64 \
DB9_WORKER_POLL_MS=60000 \
PD_ENDPOINTS=pd1:2379 cargo run
```

---

## How Multi-Instance Works

When multiple db9-server instances have the worker enabled, they coordinate automatically through TiKV:

```
             ┌─────────────────────────────────────────────────┐
             │                  Task Queue (TiKV)               │
             │                                                 │
             │  [fire_time=1000] task A  ◄── claimed by W1     │
             │  [fire_time=1001] task B  ◄── claimed by W2     │
             │  [fire_time=1002] task C  ◄── claimed by W1     │
             │  [fire_time=2000] task D  ◄── not yet due       │
             └─────────────────────────────────────────────────┘

  Worker 1 (W1)                              Worker 2 (W2)
  ┌─────────────────────┐                   ┌─────────────────────┐
  │ 1. scan queue       │                   │ 1. scan queue       │
  │ 2. try claim task A │──── TiKV txn ────│ 2. try claim task A │
  │    ✅ wins (commits) │                   │    ❌ loses (aborts)  │
  │ 3. execute task A   │                   │ 3. try claim task B │
  │                     │                   │    ✅ wins           │
  │ 4. try claim task C │                   │ 4. execute task B   │
  │    ✅ wins           │                   │                     │
  │ 5. execute task C   │                   │                     │
  └─────────────────────┘                   └─────────────────────┘
```

**Key properties**:
- **No leader**: All instances are equal peers
- **No duplicate execution**: TiKV pessimistic transactions guarantee exactly-one claim per task
- **Automatic load balancing**: Each instance stops claiming when it reaches `max_concurrent_jobs`
- **Fault tolerant**: If a worker crashes, its orphaned claims are cleaned by GC within `orphan_timeout_sec` (default 5 minutes), and the tasks are re-triggered on their next fire time

---

## Garbage Collection

The worker GC runs automatically on every instance with the worker enabled:

- **Interval**: Every 10 minutes (with random jitter to avoid thundering herd)
- **Orphan cleanup**: Deletes claims older than `orphan_timeout_sec`. This handles worker crashes — if a worker claims a task but crashes before completing, the claim is cleaned up and the task will be picked up on its next fire time.
- **No manual intervention needed**

## GC Safepoint Coordination

Every SQL-serving db9 process publishes a heartbeat plus the minimum start timestamp of its currently active transactions to the shared worker system keyspace. The safepoint advancer then computes:

`min(time_based_gc_life_time, oldest_live_transaction_start_ts - 1)`

This means GC safety is based on real transaction liveness, not on worker timeout guesses. `DB9_WORKER_STATEMENT_TIMEOUT_MS` and `DB9_CRON_JOB_TIMEOUT_MS` still limit task runtime, but they are no longer inputs to safepoint calculation.

Heartbeat rows whose `updated_at_version` ages past `DB9_GC_LIFE_TIME_SEC` are treated as stale, ignored for safepoint calculation, and automatically reaped from `_sys_worker`.
Because the publisher is unconditional, `DB9_GC_SAFEPOINT_INTERVAL_SEC` must remain smaller than `DB9_GC_LIFE_TIME_SEC` on every SQL-serving node, not only on nodes that advance the safepoint.

---

## Monitoring

The worker engine exposes in-memory metrics. These can be observed via structured logging (tracing).

### Log Output

The worker emits structured logs at key points:

```
INFO WorkerEngine starting (poll_ms=60000, max_concurrent=32)
INFO Worker task completed: keyspace=myapp db_id=1 task_id=100 type=Cron
WARN Worker task failed: keyspace=myapp db_id=1 task_id=200 type=BgDdl error=...
WARN GC: cleaned orphan claim worker=host1:1234 type=Cron claimed_at=...
INFO GC: cleaned 3 orphan claims
```

### Internal Metrics

Available programmatically (atomic counters):

| Metric | Type | Description |
|--------|------|-------------|
| `tasks_executed_ok` | Counter | Total successful task executions |
| `tasks_executed_err` | Counter | Total failed task executions |
| `{type}_executed_ok` | Counter | Success count by type (cron, async_trigger, auto_analyze, bg_ddl, bg_sql) |
| `{type}_executed_err` | Counter | Error count by type |
| `claim_attempts` | Counter | Total claim attempts |
| `claim_successes` | Counter | Successful claims (claim_attempts - claim_successes = contention) |
| `last_tick_queue_depth` | Gauge | Number of due tasks found in last poll |
| `last_tick_active_jobs` | Gauge | Number of tasks actively executing at last poll |

---

## Troubleshooting

### Task not executing

1. **Is the worker enabled?** Check that `DB9_WORKER_ENABLED` is not set to `false`.
2. **Is TiKV reachable?** The worker needs TiKV to read the queue. Check logs for connection errors.
3. **Is the task due?** Cron tasks only execute at their scheduled time. Check `cron.job` for schedule and `cron.job_run_details` for history.
4. **Is `max_concurrent_jobs` reached?** If all slots are occupied, the worker skips claiming until a slot opens.

### Cron job shows "failed" in run details

Check `cron.job_run_details` for the error message:

```sql
SELECT runid, status, return_message, start_time, end_time
FROM cron.job_run_details
WHERE jobid = 1
ORDER BY runid DESC
LIMIT 5;
```

Common causes:
- SQL syntax error in the job command
- Table or object doesn't exist
- Permission denied
- Whole-task timeout exceeded (`DB9_WORKER_STATEMENT_TIMEOUT_MS`)

### Index stuck in "Building" state

If `CREATE INDEX CONCURRENTLY` was issued but the index never reaches `Ready`:

```sql
-- Check index state via pg_catalog
SELECT indexname, indexdef FROM pg_indexes WHERE tablename = 'your_table';
```

Possible causes:
- Worker is disabled on all instances
- Backfill failed (index marked `Invalid`)
- Worker crashed during backfill (GC will clean up, but you need to retry)

Fix: Drop and recreate the index.

```sql
DROP INDEX idx_name;
CREATE INDEX CONCURRENTLY idx_name ON your_table (column);
```

### pg_background_launch returns error

- `"worker engine not available"`: Background task execution is disabled on this instance (`DB9_WORKER_ENABLED=false`). Connect to an instance with the worker enabled, or enable it.

---

## Design Document

For the full technical design including key layouts, claim mechanism, migration plan, and failure modes, see:

- [Design: Unified Async Task Engine](design/23_unified_async_task_engine.md)
- [Design: Worker Binary Spec (Phase 2)](design/24_worker_binary_spec.md)
