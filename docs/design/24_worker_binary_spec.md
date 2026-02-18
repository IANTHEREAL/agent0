# Phase 2 Worker Binary Specification

## Overview

This document specifies the pg-tikv worker binary — a separate, horizontally-scalable process that executes background tasks (Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql) by polling a task queue in TiKV and executing SQL via pgwire loopback to the pg-tikv server.

**Phase 2 Goal:** Decouple worker execution from the main pg-tikv process, enabling independent scaling and operational isolation.

---

## Section 1: Worker Binary Requirements

### Cargo.toml Binary Definition

Add a separate binary target to `Cargo.toml`:

```toml
[[bin]]
name = "pg-tikv-worker"
path = "src/worker/bin/main.rs"
```

### Command-Line Interface

The worker binary accepts the following CLI arguments:

```bash
pg-tikv-worker \
  --pd-endpoints 127.0.0.1:2379 \
  --pg-host localhost \
  --pg-port 5432 \
  --system-keyspace _sys_worker
```

| Argument | Type | Default | Description |
|----------|------|---------|-------------|
| `--pd-endpoints` | string | `127.0.0.1:2379` | Comma-separated TiKV PD endpoints for cluster discovery |
| `--pg-host` | string | `localhost` | pg-tikv server hostname for pgwire loopback |
| `--pg-port` | int | `5432` | pg-tikv server port for pgwire loopback |
| `--system-keyspace` | string | `_sys_worker` | System keyspace for task queue and registry |

### Module Reuse

The worker binary reuses existing pg-tikv infrastructure:

- **`src/worker/`** — Worker types, configuration, and engine:
  - `src/worker/types.rs` — Task, TaskStatus, TaskType (Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql)
  - `src/worker/config.rs` — WorkerConfig with poll interval, max concurrent jobs, statement timeout
  - `src/worker/engine.rs` — WorkerEngine with poll loop and semaphore-based concurrency control

- **`src/storage/tikv_store/worker.rs`** — TiKV CRUD operations:
  - Task registry: `register_task()`, `get_task()`, `list_tasks()`
  - Queue operations: `enqueue_task()`, `dequeue_task()`, `claim_task()`
  - Result storage: `store_result()`, `get_result()`, `delete_result()`
  - Orphan cleanup: `list_orphaned_claims()`, `release_claim()`
  - Batch operations: `gc_completed_tasks()`

### TiKV Connectivity

The worker connects directly to TiKV via the PD endpoints to:
- Poll the task queue in system keyspace `_sys_worker`
- Claim tasks atomically
- Store execution results
- Perform garbage collection on completed tasks

---

## Section 2: pgwire Loopback Execution

### Connection Model

The worker executes SQL by connecting back to the pg-tikv server via the PostgreSQL wire protocol (pgwire). This loopback design allows:
- Reuse of pg-tikv's SQL parser, analyzer, and executor
- Proper transaction isolation and error handling
- Audit trail via standard PostgreSQL logs

### Connection String

For each task execution, the worker establishes a pgwire connection using:

```
host={pg_host} port={pg_port} user={keyspace}.{username} dbname=postgres
```

**Example:**
```
host=localhost port=5432 user=_sys_worker.postgres dbname=postgres
```

Where:
- `{pg_host}` — pg-tikv server hostname (from `--pg-host`)
- `{pg_port}` — pg-tikv server port (from `--pg-port`)
- `{keyspace}` — system keyspace (from `--system-keyspace`, default `_sys_worker`)
- `{username}` — task owner or system user (e.g., `postgres`)

### Connection Pooling

The worker uses **`deadpool-postgres`** for connection pooling:

```rust
// Pseudocode
let pool = deadpool_postgres::Config {
    host: Some(pg_host.to_string()),
    port: Some(pg_port),
    user: Some(format!("{}.{}", keyspace, username)),
    dbname: Some("postgres".to_string()),
    max_size: 32,  // Configurable
    ..Default::default()
}
.create_pool(NoTls)?;
```

Benefits:
- Reuses connections across task executions
- Reduces connection overhead
- Enables concurrent task execution with bounded resource usage

### Statement Timeout

Each task execution respects a per-task statement timeout:

```rust
// Before executing task SQL
let timeout_ms = task.statement_timeout_ms.unwrap_or(300_000);  // Default 300s
client.execute(&format!("SET statement_timeout = {}", timeout_ms), &[]).await?;
client.execute(&task.sql, &[]).await?;
```

The timeout is:
- Configured per task in the task registry
- Defaults to 300 seconds (5 minutes)
- Enforced by pg-tikv's statement timeout mechanism
- Prevents runaway queries from blocking the worker

---

## Section 3: Deployment Model

### Kubernetes Architecture

The worker binary is deployed as a separate Kubernetes Deployment:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pg-tikv-worker
spec:
  replicas: 3  # Initial; scaled by HPA
  selector:
    matchLabels:
      app: pg-tikv-worker
  template:
    metadata:
      labels:
        app: pg-tikv-worker
    spec:
      containers:
      - name: worker
        image: pg-tikv:latest
        command: ["pg-tikv-worker"]
        args:
          - "--pd-endpoints"
          - "$(PD_ENDPOINTS)"
          - "--pg-host"
          - "$(PG_HOST)"
          - "--pg-port"
          - "$(PG_PORT)"
          - "--system-keyspace"
          - "$(SYSTEM_KEYSPACE)"
        env:
          - name: PD_ENDPOINTS
            value: "pd-0.pd:2379,pd-1.pd:2379,pd-2.pd:2379"
          - name: PG_HOST
            value: "pg-tikv-lb.default.svc.cluster.local"
          - name: PG_PORT
            value: "5432"
          - name: SYSTEM_KEYSPACE
            value: "_sys_worker"
          - name: PGTIKV_WORKER_POLL_MS
            value: "1000"
          - name: PGTIKV_WORKER_MAX_CONCURRENT_JOBS
            value: "32"
          - name: PGTIKV_WORKER_STATEMENT_TIMEOUT_MS
            value: "300000"
          - name: PGTIKV_WORKER_ORPHAN_TIMEOUT_SEC
            value: "3600"
          - name: PGTIKV_WORKER_GC_BATCH_SIZE
            value: "100"
        ports:
        - name: metrics
          containerPort: 9090
        livenessProbe:
          httpGet:
            path: /health
            port: metrics
          initialDelaySeconds: 30
          periodSeconds: 10
        readinessProbe:
          httpGet:
            path: /ready
            port: metrics
          initialDelaySeconds: 10
          periodSeconds: 5
```

### Environment Variables

The worker binary respects the following environment variables (inherited from Phase 1):

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `PGTIKV_WORKER_POLL_MS` | int | `1000` | Poll interval in milliseconds |
| `PGTIKV_WORKER_MAX_CONCURRENT_JOBS` | int | `32` | Max concurrent task executions |
| `PGTIKV_WORKER_STATEMENT_TIMEOUT_MS` | int | `300000` | Default statement timeout (5 min) |
| `PGTIKV_WORKER_ORPHAN_TIMEOUT_SEC` | int | `3600` | Timeout for orphaned claims (1 hour) |
| `PGTIKV_WORKER_GC_BATCH_SIZE` | int | `100` | Batch size for garbage collection |
| `PGTIKV_WORKER_SYSTEM_KEYSPACE` | string | `_sys_worker` | System keyspace for task queue |
| `PGTIKV_AUTO_ANALYZE_ENABLED` | bool | `true` | Enable auto-analyze background jobs |
| `PGTIKV_AUTO_ANALYZE_THRESHOLD` | int | `10000` | Row count threshold for auto-analyze |

**New variables (Phase 2):**

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `PGTIKV_WORKER_PG_HOST` | string | `localhost` | pg-tikv server hostname |
| `PGTIKV_WORKER_PG_PORT` | int | `5432` | pg-tikv server port |

### Health Check Endpoint

The worker exposes an HTTP health check endpoint on port 9090:

```
GET /health
GET /ready
GET /metrics
```

#### `/health` — Liveness Probe

Returns `200 OK` if the worker process is alive and the TiKV connection is healthy:

```json
{
  "status": "healthy",
  "uptime_seconds": 3600,
  "tikv_connected": true
}
```

#### `/ready` — Readiness Probe

Returns `200 OK` if the worker is ready to accept tasks:

```json
{
  "status": "ready",
  "queue_depth": 42,
  "active_jobs": 8
}
```

#### `/metrics` — Prometheus Metrics

Exposes WorkerMetrics as Prometheus-compatible metrics:

```
# HELP pg_tikv_worker_tasks_executed_total Total tasks executed
# TYPE pg_tikv_worker_tasks_executed_total counter
pg_tikv_worker_tasks_executed_total{status="ok"} 1250
pg_tikv_worker_tasks_executed_total{status="error"} 15

# HELP pg_tikv_worker_claims_total Total task claims
# TYPE pg_tikv_worker_claims_total counter
pg_tikv_worker_claims_total 1265

# HELP pg_tikv_worker_queue_depth Current queue depth
# TYPE pg_tikv_worker_queue_depth gauge
pg_tikv_worker_queue_depth 42

# HELP pg_tikv_worker_active_jobs Current active jobs
# TYPE pg_tikv_worker_active_jobs gauge
pg_tikv_worker_active_jobs 8
```

### Horizontal Pod Autoscaling

Deploy an HPA to scale workers based on queue depth:

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: pg-tikv-worker-hpa
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: pg-tikv-worker
  minReplicas: 2
  maxReplicas: 20
  metrics:
  - type: Pods
    pods:
      metricName: pg_tikv_worker_queue_depth
      targetAverageValue: "10"
```

---

## Section 4: Migration from Phase 1 → Phase 2

### Migration Steps

The transition from Phase 1 (embedded worker) to Phase 2 (separate binary) is **zero-downtime** and **data-preserving**:

#### Step 1: Disable Embedded Worker

On the pg-tikv server, set:

```bash
export PGTIKV_WORKER_ENABLED=false
```

This disables the embedded worker loop in pg-tikv. Existing tasks remain in the queue.

**Verification:**
```sql
SELECT COUNT(*) FROM _sys_worker.task_queue WHERE status = 'pending';
```

#### Step 2: Deploy Worker Binary

Deploy the pg-tikv-worker binary as a separate Kubernetes Deployment (see Section 3).

Configure it to point at the pg-tikv server:

```bash
pg-tikv-worker \
  --pd-endpoints pd-0.pd:2379,pd-1.pd:2379,pd-2.pd:2379 \
  --pg-host pg-tikv-lb.default.svc.cluster.local \
  --pg-port 5432 \
  --system-keyspace _sys_worker
```

**Verification:**
```bash
kubectl logs -f deployment/pg-tikv-worker
# Should show: "Worker started, polling queue..."
```

#### Step 3: Verify Task Execution

Monitor the worker metrics to confirm tasks are being claimed and executed:

```bash
curl http://pg-tikv-worker:9090/metrics | grep pg_tikv_worker_tasks_executed_total
```

Expected output:
```
pg_tikv_worker_tasks_executed_total{status="ok"} 100
pg_tikv_worker_tasks_executed_total{status="error"} 0
```

#### Step 4: Cleanup (Optional)

Once the worker binary is stable and all pending tasks have been processed, you may:
- Remove the `PGTIKV_WORKER_ENABLED` configuration from pg-tikv
- Archive old task execution logs
- Update documentation to reference the worker binary

### Data Preservation

**No data migration is required.** The worker binary uses the same system keyspace (`_sys_worker`) and task schema as Phase 1:

- Task registry: `_sys_worker.task_registry`
- Task queue: `_sys_worker.task_queue`
- Task results: `_sys_worker.task_results`
- Claims: `_sys_worker.task_claims`

All existing tasks, results, and claims remain accessible and are processed by the new worker binary without modification.

### Rollback Plan

If the worker binary encounters issues:

1. **Scale down the worker binary:**
   ```bash
   kubectl scale deployment pg-tikv-worker --replicas=0
   ```

2. **Re-enable the embedded worker in pg-tikv:**
   ```bash
   export PGTIKV_WORKER_ENABLED=true
   kubectl rollout restart deployment pg-tikv
   ```

3. **Verify task processing resumes:**
   ```bash
   curl http://pg-tikv:9090/metrics | grep pg_tikv_worker_tasks_executed_total
   ```

The embedded worker will resume processing tasks from the queue without data loss.

---

## Appendix: Task Types

The worker binary executes the following task types:

| Type | Source | Execution | Example |
|------|--------|-----------|---------|
| **Cron** | Cron registry | Periodic SQL execution | `VACUUM ANALYZE` every hour |
| **AsyncTrigger** | Trigger queue | Async trigger body execution | Fire AFTER trigger asynchronously |
| **AutoAnalyze** | Auto-analyze threshold | Column statistics collection | `ANALYZE table_name` when row count exceeds threshold |
| **BgDdl** | DDL queue | Background DDL execution | `CREATE INDEX CONCURRENTLY` |
| **BgSql** | Application queue | Background SQL execution | User-submitted background jobs |

---

## Appendix: WorkerMetrics

The worker maintains atomic counters exposed via `/metrics`:

```rust
pub struct WorkerMetrics {
    pub tasks_executed_ok: AtomicU64,
    pub tasks_executed_err: AtomicU64,
    pub claims_total: AtomicU64,
    pub queue_depth: AtomicU64,
    pub active_jobs: AtomicU64,
}
```

These metrics are used for:
- Health checks (liveness/readiness probes)
- Horizontal Pod Autoscaling (queue depth)
- Observability and alerting (Prometheus)

---

## Appendix: TriggerWorker Separation

**Important:** The `TriggerWorker` (async trigger execution) remains **separate** from the worker binary in Phase 2.

- **TriggerWorker** — Embedded in pg-tikv, executes AFTER triggers asynchronously
- **Worker Binary** — Separate process, executes background tasks (Cron, AutoAnalyze, BgDdl, BgSql)

This separation allows:
- Triggers to execute with low latency (same process as SQL execution)
- Background tasks to scale independently
- Clear operational boundaries

---

**Document Version:** 1.0  
**Last Updated:** 2026-02-17  
**Status:** Phase 2 Specification
