# Unified Async Task Engine Design

## Status

- **Classification**: Historical
- **Phase**: Implemented (Phase 1)
- **Author**: db9-server team
- **Date**: 2026-02-18
- **Related files**: `src/worker/`, `src/cron/`, `src/sql/triggers/`, `src/sql/executor/`, `src/storage/tikv_store/`

---

## Problem Statement

db9-server currently has multiple independent async execution systems, each operating in isolation with the following issues:

### Current State Analysis

| System | Location | Problem |
|--------|----------|---------|
| **Cron** | `src/cron/worker.rs` | OnceLock singleton (line 506), full scan of all keyspaces every 60s, serial execution, cannot scale horizontally |
| **Async Trigger** | `src/sql/triggers/worker.rs` | Separate OnceLock (line 553), DashMap in-memory queue, lost on process restart, no persistence |
| **ANALYZE** | `src/sql/executor/core/analyze.rs` | Synchronous single-transaction (line 25), 10K batch streaming scan (line 316), blocks user queries |
| **CREATE INDEX** | `src/sql/ddl.rs:1208-1514` | Synchronous backfill, no CONCURRENTLY support, long-duration table lock |
| **REFRESH MATERIALIZED VIEW** | `src/sql/executor/procedure.rs:704` | Synchronous execution, no background option |

### Scale Assumptions

```
Total tenants:                  1,000,000
Tenants with cron enabled:         10,000   (1%)
Tenants with async triggers:       50,000   (5%)
Total cron jobs:                   20,000   (avg 2/tenant)
Total async triggers:             100,000   (avg 2/tenant)
Peak due jobs/minute:             200-500
Peak async triggers/second:        50-200
db9-server instances:                    10+
```

### Core Pain Points

1. **Multiple independent systems** — Cannot be uniformly managed, monitored, or scaled
2. **In-memory queues** — Tasks lost on process restart, no persistence guarantee
3. **Single point of failure** — OnceLock singletons cannot scale horizontally
4. **Inefficient scanning** — O(keyspace × db × job), unacceptable at million-tenant scale
5. **Blocking users** — ANALYZE, CREATE INDEX execute synchronously, impacting OLTP performance
6. **No priority** — All tasks treated equally, no way to differentiate paid tiers

---

## Design Overview

### Core Idea

**Unified Async Task Engine**: Use TiKV itself as the coordination layer with no external dependencies. All async tasks (cron, async triggers, auto-ANALYZE, CREATE INDEX CONCURRENTLY, background SQL) share a single queue + claim + execute mechanism.

```
                           ┌─────────────────────────┐
                           │      TiKV Cluster        │
                           │                         │
                           │  ┌───────────────────┐  │
                           │  │ Global Task Queue  │  │
                           │  │ (sorted by         │  │
                           │  │  next_fire_time)   │  │
                           │  └─────────┬─────────┘  │
                           │            │             │
                           │  ┌─────────┴─────────┐  │
                           │  │ Task Registry      │  │
                           │  │ (keyspace→tasks)   │  │
                           │  └───────────────────┘  │
                           └────────────┬────────────┘
                                        │
                    ┌────────────────────┬┴─────────────────────┐
                    │                    │                      │
              ┌─────┴─────┐       ┌─────┴─────┐       ┌─────┴─────┐
              │  Worker 1  │       │  Worker 2  │       │  Worker N  │
              │            │       │            │       │            │
              │ 1. scan    │       │ 1. scan    │       │ 1. scan    │
              │    queue   │       │    queue   │       │    queue   │
              │ 2. claim   │       │ 2. claim   │       │ 2. claim   │
              │    (txn)   │       │    (txn)   │       │    (txn)   │
              │ 3. execute │       │ 3. execute │       │ 3. execute │
              │    via     │       │    via     │       │    via     │
              │    pgwire  │       │    pgwire  │       │    pgwire  │
              └─────┬──────┘       └─────┬──────┘       └──────┬────┘
                    │                    │                     │
                    └────────────────────┼─────────────────────┘
                                         │ pgwire (SQL)
                    ┌────────────────────┼─────────────────────┐
                    │                    │                     │
              ┌─────┴─────┐       ┌─────┴─────┐       ┌───────┴───┐
              │ db9-server 1  │       │ db9-server 2  │       │ db9-server M  │
              │ (SQL only) │       │ (SQL only) │       │ (SQL only) │
              └────────────┘       └────────────┘       └───────────┘
```

### Key Decisions

1. **Workers connect back to db9-server via pgwire** to execute SQL rather than operating on TiKV data directly. This guarantees full SQL semantics (transactions, triggers, privilege checks, search_path).
2. **TiKV pessimistic transactions for claims** provide a natural distributed lock — no additional leader election needed.
3. **A single engine serves all async task types**, differentiated by TaskType.

---

## Task Types

### TaskType Enum

```rust
enum TaskType {
    Cron,           // Scheduled SQL (cron.schedule)
    AsyncTrigger,   // AFTER trigger async execution
    AutoAnalyze,    // Automatic ANALYZE (table modification exceeds threshold)
    BgDdl,          // CREATE INDEX CONCURRENTLY, REFRESH MATERIALIZED VIEW
    BgSql,          // User-submitted one-off background tasks
}
```

### Enqueue Methods by Task Type

| TaskType | Enqueue Trigger | next_fire_time | Priority |
|----------|----------------|----------------|----------|
| **Cron** | `cron.schedule()` call | Computed from cron expression | Low |
| **AsyncTrigger** | Trigger fires | `now` | High |
| **AutoAnalyze** | Table modification count exceeds threshold | `now` | Medium |
| **BgDdl** | `CREATE INDEX CONCURRENTLY` / `REFRESH MATERIALIZED VIEW` | `now` | Medium |
| **BgSql** | `SELECT pg_background_launch(...)` | `now` | Low |

---

## Detailed Design

### 1. Global Task Registry

**Problem**: Currently requires scanning every keyspace and every database to discover which have tasks.

**Solution**: Maintain a global registry in a system keyspace (`_sys_worker`, not owned by any tenant).

#### Registry Key Layout

```
_worker_registry_{keyspace}_{db_id}  →  TaskRegistryEntry (bincode)
```

```rust
struct TaskRegistryEntry {
    keyspace: String,
    db_id: u64,
    task_types: u8,       // bitmask: 0x01=cron, 0x02=async_trigger, 0x04=auto_analyze, 0x08=bg_ddl, 0x10=bg_sql
    job_count: u32,       // hint for load balancing, does not need to be exact
    registered_at: i64,   // epoch ms
}
```

#### Registration / Deregistration Timing

| Operation | Action |
|-----------|--------|
| `CREATE EXTENSION pg_cron` | Write registry entry (`task_types \|= 0x01`) |
| `DROP EXTENSION pg_cron` | Clear cron bit; if `task_types == 0` delete entry |
| `cron.schedule()` | Update `job_count += 1` |
| `cron.unschedule()` | Update `job_count -= 1`; if `job_count == 0` clear cron bit |
| First async trigger created | Write registry entry (`task_types \|= 0x02`) |
| Last async trigger deleted | Clear async_trigger bit |
| Table first enables auto-ANALYZE | Write registry entry (`task_types \|= 0x04`) |
| `CREATE INDEX CONCURRENTLY` | Write registry entry (`task_types \|= 0x08`) |

#### Affected Files

- `src/sql/executor/cron.rs`: Write registry on schedule/unschedule
- `src/sql/triggers/enqueue.rs`: Write registry on first async trigger
- `src/sql/executor/core/analyze.rs`: Write registry when auto-ANALYZE enabled
- `src/sql/ddl.rs`: Write registry on CREATE INDEX CONCURRENTLY
- `src/extensions/mod.rs`: Write registry on CREATE/DROP EXTENSION
- `src/storage/tikv_store/worker.rs`: New registry read/write methods

#### Scan Cost

Worker tick only needs **one prefix scan** `_worker_registry_` → gets the list of all keyspaces with tasks. 10K entries ≈ 1MB, single scan < 50ms.

---

### 2. Pre-computed Next-Fire-Time Index

**Problem**: Currently each tick parses cron expressions for every job and checks `is_due()`.

**Solution**: Each task maintains a `next_fire_time`, written to a globally ordered queue. Tick only does a range scan for `fire_time <= now`.

#### Queue Key Layout

```
_worker_queue_{next_fire_time_ms}_{keyspace}_{db_id}_{task_id}  →  TaskQueueEntry (bincode)
```

`next_fire_time_ms` uses **big-endian i64** for natural sort order.

```rust
struct TaskQueueEntry {
    keyspace: String,
    db_id: u64,
    task_id: i64,           // job_id / trigger_id / table_id / index_id
    task_type: TaskType,    // Cron | AsyncTrigger | AutoAnalyze | BgDdl | BgSql
    command: String,        // SQL to execute
    username: String,       // execution identity
    schedule: Option<String>, // cron expression (Cron type only)
    priority: u8,           // 0-255; lower numeric values execute first
}
```

#### Write Timing

| Operation | Action |
|-----------|--------|
| `cron.schedule()` | Compute next_fire_time, write to queue |
| Job execution completes | Compute next next_fire_time, write new queue entry, delete old entry |
| `cron.alter_job()` modifies schedule | Delete old entry, compute new next_fire_time, write new entry |
| `cron.unschedule()` | Delete queue entry |
| Async trigger fires | Write queue entry (`next_fire_time = now`) |
| Table modification count exceeds threshold | Write queue entry (`next_fire_time = now`) |
| `CREATE INDEX CONCURRENTLY` | Write queue entry (`next_fire_time = now`) |
| `REFRESH MATERIALIZED VIEW CONCURRENTLY` | Write queue entry (`next_fire_time = now`) |

#### Tick Flow (New)

```
every tick_interval:
  now = current_time_ms()
  entries = range_scan("_worker_queue_" .. "_worker_queue_{now}")
  // Returns only entries with fire_time <= now, O(due_jobs)
  // Sort by priority
  for entry in entries:
    spawn_claim_and_execute(entry)
```

#### Scan Cost

Peak 500 due jobs/minute + 50-200 async triggers/second → each scan returns at most ~1000 entries, < 500KB.

#### Affected Files

- `src/storage/tikv_store/worker.rs`: New queue read/write methods
- `src/sql/executor/cron.rs`: Maintain queue on schedule/unschedule/alter_job
- `src/sql/triggers/enqueue.rs`: Write to queue on trigger fire
- `src/sql/executor/core/analyze.rs`: Write to queue on auto-ANALYZE
- `src/sql/ddl.rs`: Write to queue on CREATE INDEX CONCURRENTLY
- `src/cron/worker.rs`: Change tick to queue range scan

---

### 3. Multi-Instance Work Stealing

**Problem**: Single-instance singleton cannot scale horizontally.

**Solution**: All worker instances are equal peers competing for claims via TiKV pessimistic transactions.

#### Claim Mechanism

```
_worker_claim_{keyspace}_{db_id}_{task_id}_{fire_time_min}  →  WorkerClaim
```

```rust
struct WorkerClaim {
    worker_id: String,     // instance identifier (hostname:pid or UUID)
    claimed_at: i64,       // epoch ms
    task_type: TaskType,   // used by GC to differentiate
}
```

#### Competition Flow

```rust
async fn try_claim(txn, entry) -> bool {
    let key = claim_key(entry.keyspace, entry.db_id, entry.task_id, fire_minute);
    if txn.get(key).await?.is_some() {
        return false;  // Already claimed by another worker
    }
    txn.put(key, serialize(WorkerClaim { ... }));
    true  // TiKV pessimistic transaction guarantees only one writer succeeds
}
```

Multiple workers scan the same batch of due tasks simultaneously, each attempting to claim. TiKV pessimistic transactions guarantee only one writer successfully commits per key; others automatically abort. **No leader election needed.**

#### Load Balancing

Each worker maintains a local `active_jobs` counter (AtomicU32). When `active_jobs >= max_concurrent_jobs`, it skips claiming for the current tick, letting other workers pick up work.

No complex sharding or consistent hashing needed — random competition + backpressure is sufficient for even distribution across 10-50 workers.

#### Affected Files

- `src/cron/worker.rs`: Remove `OnceLock` singleton, switch to configurable startup
- `src/cron/config.rs`: Add `worker_id` configuration
- `src/sql/triggers/worker.rs`: Same refactoring

---

### 4. Concurrent Execution

**Problem**: Due tasks execute serially.

**Solution**: `tokio::JoinSet` + `Semaphore` for concurrency control.

```rust
let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs)); // default 32
let mut join_set = JoinSet::new();

for entry in due_entries {
    let permit = semaphore.clone().acquire_owned().await?;
    join_set.spawn(async move {
        let _permit = permit;  // Hold permit until job completes
        claim_and_execute(entry).await
    });
}

// Wait for all jobs to complete (or timeout)
while let Some(result) = join_set.join_next().await {
    handle_result(result);
}
```

#### Execution Path: Connect Back to db9-server via pgwire

```rust
async fn execute_task_sql(entry: &TaskQueueEntry) -> Result<()> {
    // Connect to the corresponding tenant's db9-server
    let connstr = format!(
        "host={} port={} user={}.{} password={} dbname=postgres",
        pg_host, pg_port, entry.keyspace, entry.username, service_password
    );
    let (client, conn) = tokio_postgres::connect(&connstr, NoTls).await?;
    tokio::spawn(conn);

    // Set execution timeout (configurable)
    client.execute("SET statement_timeout = ?", &[&config.statement_timeout_ms]).await?;

    client.simple_query(&entry.command).await?;
    Ok(())
}
```

**Why pgwire instead of operating on TiKV directly**:
- Guarantees full SQL semantics (transactions, triggers, privilege checks, search_path)
- Worker doesn't need to understand db9-server internal state — purely stateless
- Can use standard connection pool (deadpool-postgres) to manage connections
- Supports statement_timeout, search_path, and other SQL parameters

#### Affected Files

- `src/cron/worker.rs`: Change execution path from `Executor::execute_statement_on_txn` to pgwire connection
- `Cargo.toml`: Add crate if worker is a separate binary

---

### 5. Claim Lifecycle

**Problem**: Claim keys accumulate indefinitely.

**Solution**: Dual cleanup — immediate deletion on execution completion + GC as safety net.

#### Normal Flow

```
Task completes → write run_details → delete claim key → write next queue entry (Cron only)
```

A successful task execution immediately deletes its claim key — no accumulation.

#### Abnormal Flow (Orphans)

Worker crash or task timeout → claim key remains. GC handles it:

```
GC every 10 minutes:
  scan _worker_claim_ prefix
  for claim in claims:
    if claim.claimed_at < now - orphan_timeout:
      delete claim
      // Do NOT re-enqueue — wait for next fire time to trigger naturally (Cron)
      // Or mark as failed (AsyncTrigger/BgDdl/BgSql)
```

#### Cost Estimate

Under normal conditions, claim keys live < 1 minute. Peak 1000 tasks/min → at most 1000 concurrent claim keys. GC only handles abnormal residuals.

---

### 6. GC Optimization

**Problem**: Current GC does a full scan of all keyspaces.

**Solution**:

1. **Use registry**: GC only scans keyspaces listed in `_worker_registry_`
2. **Batch processing**: Each GC cycle processes a batch (e.g., 100 keyspaces), continues next cycle
3. **Jitter**: Each worker's GC timer adds random offset (0-60s) to avoid simultaneous execution
4. **In-place run retention**: Run data stored as `_sys_cron_run_{db_id}_{run_id}` with `run_id` monotonically increasing. Keeping the last N entries only requires scan + delete prefix.

```rust
async fn gc_tick(&self, cursor: &mut Option<String>) {
    let batch = scan_registry(cursor, 100);
    for entry in batch {
        gc_keyspace(entry.keyspace, entry.db_id).await;
    }
    if batch.is_empty() {
        *cursor = None;  // Restart from beginning
    }
}
```

---

### 7. Auto-ANALYZE Design

**Problem**: ANALYZE executes synchronously, blocking user queries.

**Solution**:

1. **Modification counter**: Each table maintains a `mod_since_analyze` counter (atomic operation)
2. **Threshold check**: Trigger when `mod_since_analyze > 50 + 0.1 × reltuples`
3. **Async enqueue**: Write queue entry (`next_fire_time = now`, `task_type = AutoAnalyze`)
4. **Background execution**: Worker executes `ANALYZE table_name`

#### Affected Files

- `src/sql/executor/dml_analyzed.rs`: Increment `mod_since_analyze` on INSERT/UPDATE/DELETE
- `src/sql/executor/core/analyze.rs`: Reset counter after ANALYZE completes
- `src/sql/stats.rs`: Add `mod_since_analyze` field to TableStatsCache

---

### 8. CREATE INDEX CONCURRENTLY Design

**Problem**: CREATE INDEX synchronous backfill causes long-duration table locks.

**Solution**: Two-phase execution

#### Phase 1: Synchronous (Schema Registration)

```sql
CREATE INDEX CONCURRENTLY idx_name ON table_name (col);
```

1. Create index metadata (`_sys_index_{db_id}_{index_id}`)
2. Set `index_state = BUILDING`
3. Return success to user

#### Phase 2: Asynchronous (Background Backfill)

1. Worker picks up `BgDdl` task from queue
2. Executes `REINDEX INDEX idx_name` or equivalent backfill logic
3. On completion, sets `index_state = READY`

#### New Types

```rust
enum IndexState {
    Building,   // Backfill in progress
    Ready,      // Usable
    Invalid,    // Needs rebuild
}
```

#### Affected Files

- `src/sql/ddl.rs`: Change CREATE INDEX CONCURRENTLY to two-phase
- `src/sql/catalog/`: Index virtual tables return `index_state`
- `src/sql/executor/core/index.rs`: New backfill logic

---

### 9. Complete Key Layout

#### System Keyspace (`_sys_worker`, cross-tenant)

```
_worker_registry_{keyspace}_{db_id}                              → TaskRegistryEntry
_worker_queue_{next_fire_time_ms}_{keyspace}_{db_id}_{task_id}   → TaskQueueEntry
_worker_claim_{keyspace}_{db_id}_{task_id}_{fire_time_min}       → WorkerClaim
```

#### Tenant Keyspace (unchanged)

```
_sys_cron_enabled_{db_id}                     → [1]
_sys_cron_job_{db_id}_{job_id}                → CronJob
_sys_cron_run_{db_id}_{run_id}                → CronRun
_sys_next_cron_job_id_{db_id}                 → i64
_sys_next_cron_run_id_{db_id}                 → i64
```

Note: The `_sys_cron_claim_` prefix is **deprecated** — claims are migrated to the system keyspace.

---

## Deployment Model

### Phase 1: In-Process Worker (Current Architecture Improvement)

```
db9-server process
├── SQL handler (pgwire)
├── WorkerEngine (queue scan + concurrent execution)
└── WorkerGc (orphan claim cleanup + run retention)
```

- No process split, minimal changes
- Still uses in-process `Executor` for SQL execution
- `list_all_keyspaces()` replaced with registry scan
- Concurrent execution added
- Trigger queue migrated from in-memory to TiKV
- Controlled by `DB9_WORKER_ENABLED` environment variable (default: `true`)

### Phase 2: Standalone Worker Microservice

```
db9-server process       ← SQL only
worker process(es)    ← separate binary, independently scalable
```

- Workers connect back to db9-server via pgwire
- Workers can use Kubernetes HPA to auto-scale based on queue depth
- db9-server no longer embeds worker (set `DB9_WORKER_ENABLED=false`)
- Design spec: `docs/design/24_worker_binary_spec.md`

### Phase 3: Full Unified Engine

```
TaskQueueEntry.task_type:
  Cron          → Scheduled SQL
  AsyncTrigger  → AFTER trigger async execution
  AutoAnalyze   → Automatic ANALYZE
  BgDdl         → CREATE INDEX CONCURRENTLY / REFRESH MATERIALIZED VIEW
  BgSql         → User-submitted one-off background tasks
```

Same queue + claim + execute mechanism; different task_types only differ in enqueue method.

---

## Migration Plan (Zero Downtime)

### Step 1: Add Registry + Queue (Dual-Write)

1. Deploy new version of db9-server
2. `cron.schedule()` writes to both tenant keyspace job **and** system keyspace queue entry
3. Worker still scans from `list_all_keyspaces()` (old path), **also** scans from queue (new path)
4. Dedup from both sources (same claim key)

### Step 2: Backfill Existing Data

Background task scans all keyspaces (one-time), writing existing cron jobs to registry + queue:

```sql
-- Admin manually triggers or runs automatically
SELECT _sys_backfill_cron_registry();
```

### Step 3: Switch to New Path

1. Confirm queue jobs cover all tenants (compare registry entry count vs old-path scan count)
2. Set `DB9_CRON_USE_QUEUE=true`, worker reads only from queue
3. Observe for 1-2 days to confirm stability

### Step 4: Clean Up Old Path

1. Remove `list_all_keyspaces()` scanning logic
2. Remove `_sys_cron_claim_` prefix (migrated to system keyspace)
3. Retain tenant keyspace job/run data (cron.job / cron.job_run_details virtual tables still need them)

---

## Configuration

| Environment Variable | Default | Description |
|---------------------|---------|-------------|
| `DB9_WORKER_ENABLED` | `true` | Enable in-process worker |
| `DB9_WORKER_POLL_MS` | `60000` | Tick interval |
| `DB9_WORKER_MAX_CONCURRENT_JOBS` | `32` | Max concurrent tasks per worker |
| `DB9_WORKER_ID` | `{hostname}:{pid}` | Worker instance identifier |
| `DB9_WORKER_STATEMENT_TIMEOUT_MS` | `300000` | Per-task execution timeout (5 minutes) |
| `DB9_WORKER_ORPHAN_TIMEOUT_SEC` | `300` | Orphan claim timeout |
| `DB9_WORKER_GC_BATCH_SIZE` | `100` | Keyspaces processed per GC cycle |
| `DB9_WORKER_PG_HOST` | (same as db9-server) | Phase 2: db9-server address for worker connection |
| `DB9_WORKER_PG_PORT` | (same as db9-server) | Phase 2: db9-server port for worker connection |
| `DB9_AUTO_ANALYZE_ENABLED` | `true` | Enable automatic ANALYZE |
| `DB9_AUTO_ANALYZE_THRESHOLD` | `50` | Auto-ANALYZE base threshold |

---

## Failure Modes

| Failure | Impact | Recovery |
|---------|--------|----------|
| Worker crash | Claimed tasks won't execute | Orphan GC cleans claim after 5 min; next fire time re-enqueues automatically |
| TiKV partition | Worker can't scan queue or claim | Tick fails, warn log; resumes automatically after TiKV recovers |
| db9-server unavailable | Worker claims successfully but SQL fails | run_details records failed; retries on next fire time |
| Queue entry lost | Task no longer triggers | Periodic reconcile: compare registry keyspaces with tenant keyspace jobs, backfill missing queue entries |
| Clock skew | Task fires early or late | Claim's `fire_time_min` uses minute-level truncation, tolerates ±30s |
| All workers offline | No tasks execute | Queue entries persist in TiKV; workers catch up when they come back |

---

## Monitoring Metrics

Exposes the following Prometheus metrics:

```
# Gauge
db9_server_worker_queue_depth{task_type}           # Queue depth
db9_server_worker_active_jobs{worker_id}           # Active job count
db9_server_worker_claim_success_rate{task_type}    # Claim success rate

# Counter
db9_server_worker_tasks_executed_total{task_type, status}  # Execution total
db9_server_worker_tasks_failed_total{task_type, reason}    # Failure total

# Histogram
db9_server_worker_task_duration_seconds{task_type}  # Execution duration
db9_server_worker_queue_latency_seconds{task_type}  # Queue latency
```

---

## Test Plan

### Existing Coverage (Unchanged)

- `src/cron/parser.rs` (24 tests) — Cron expression parsing
- `src/cron/types.rs` (5 tests) — Serialization round-trips
- `tests/170_cron_basic.sql` etc. (11 tests) — SQL regression

### New Tests: Phase 1

#### Unit Tests

- TaskRegistryEntry / TaskQueueEntry / WorkerClaim bincode round-trips
- Queue key ordering (big-endian i64)
- next_fire_time computation (various cron expressions)
- Claim dedup (minute-level truncation)
- Concurrency control (Semaphore limits)

#### Storage Tests

- Registry CRUD (requires TiKV)
- Queue CRUD (requires TiKV)
- Claim competition (requires TiKV)

#### SQL Integration Tests

- `tests/185_worker_cron_queue.sql` — Cron queue integration
- `tests/186_worker_bg_sql.sql` — Background SQL execution
- `tests/187_worker_cic.sql` — CREATE INDEX CONCURRENTLY
- `tests/188_worker_refresh_mv.sql` — REFRESH MATERIALIZED VIEW CONCURRENTLY
- `tests/189_worker_auto_analyze.sql` — Auto-ANALYZE

### New Tests: Phase 2

- E2E: Worker process executes tasks
- Multi-worker competition: Verify dedup
- Worker crash recovery: Orphan GC cleanup

### New Tests: Phase 3

- Async trigger enqueue
- Auto-ANALYZE enqueue
- CREATE INDEX CONCURRENTLY two-phase
- Background job enqueue

---

## Implementation Priority

### P0 (Must Have)

1. Task registry + queue infrastructure
2. Worker claim mechanism (TiKV pessimistic transactions)
3. Cron migration to new queue
4. Concurrent execution (Semaphore)

### P1 (Important)

1. Async trigger migration to TiKV queue
2. Auto-ANALYZE implementation
3. GC optimization (registry scan)
4. Monitoring metrics

### P2 (Optional)

1. CREATE INDEX CONCURRENTLY
2. REFRESH MATERIALIZED VIEW CONCURRENTLY
3. Background job API
4. Priority queue

---

## Future Work

- **Priority queue**: Different task priorities per tenant (paid tiers). Can be implemented by adding a priority field to the queue key.
- **Execution timeout**: Per-task SQL execution timeout. Requires setting `statement_timeout` on the pgwire connection.
- **Cross-region**: Worker affinity — prefer claiming tasks in the same region to reduce cross-region SQL execution latency.
- **Task dependencies**: Support dependencies between tasks (e.g., update statistics after ANALYZE completes).
- **Retry strategy**: Configurable retry count and backoff strategy.

---

## Summary

The unified async task engine achieves efficient, scalable async task execution through:

1. **Global queue**: TiKV itself serves as the coordination layer — no external dependencies
2. **Work stealing**: Multiple workers compete via pessimistic transactions — no leader election
3. **Concurrent execution**: Semaphore controls concurrency — fully utilizes resources
4. **Persistence**: All tasks persisted to TiKV — no data loss on process restart
5. **Extensibility**: Supports multiple task types with unified management and monitoring

TiKV itself is the best coordination layer.
