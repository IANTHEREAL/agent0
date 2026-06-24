# Observability Metrics Reference

db9-server installs a global `metrics-exporter-prometheus` recorder at startup and exposes Prometheus text format at:

```text
GET /internal/metrics
```

The listener address defaults to `0.0.0.0:9102` and can be overridden with `--metrics-addr` or `DB9_METRICS_ADDR`. The endpoint is disabled when the configured metrics address uses port `0`, for example `DB9_METRICS_ADDR=:0`. Metrics are process-local; counters reset on process restart. SQL-visible per-tenant summaries remain available through `_DB9_SYS_OBSERVABILITY` and `_DB9_SYS_QUERY_SAMPLES`, but this document describes the Prometheus surface.

## Naming and Labels

- Metric names use the `db9_` prefix.
- Durations use seconds and end with `_seconds`.
- Counters end with `_total` unless the existing name predates this convention.
- High-cardinality labels such as raw SQL, file path, S3 key, or table name must not be added.
- Tenant scoped metrics use `keyspace` only where the runtime path already has a stable keyspace.

## Server and Query Metrics

| Metric | Type | Labels | Source | Status | Meaning |
|---|---:|---|---|---|---|
| `db9_server_start_time_seconds` | Gauge | none | `src/main.rs` | Emitted | Process start Unix timestamp. |
| `db9_server_build_info` | Gauge | version/build labels in code | `src/main.rs` | Emitted | Build metadata exposed as a constant gauge. |
| `db9_server_connections_accepted_total` | Counter | none | `src/main.rs` | Emitted | Accepted pgwire connections. |
| `db9_server_connections_rejected_total` | Counter | none | `src/main.rs` | Emitted | Connections rejected by admission control. |
| `db9_server_connections_active` | Gauge | none | `src/main.rs` | Emitted | Currently active pgwire connections. |
| `db9_server_connection_errors_total` | Counter | none | `src/main.rs` | Emitted | Connection handler errors. |
| `db9_server_query_duration_seconds` | Histogram | none | `src/observability.rs` | Emitted | Statement execution latency. |
| `db9_server_statements_total` | Counter | none | `src/observability.rs` | Emitted | Statements executed. |
| `db9_server_query_errors_total` | Counter | none | `src/observability.rs` | Emitted | Failed statements. |
| `db9_server_write_conflict_retries_total` | Counter | none | `src/observability.rs` | Emitted | Write-conflict retry attempts. |
| `db9_server_expensive_queries_total` | Counter | none | `src/pool.rs` | Emitted | Statements crossing the expensive memory threshold. |
| `db9_database_activity_flushed_total` | Counter | none | `src/database_activity_connector.rs` | Emitted | Database activity records flushed to the external connector. |
| `db9_database_activity_dropped_total` | Counter | `reason` | `src/database_activity_connector.rs` | Emitted | Database activity records dropped before external delivery. |

## Runtime and Resource Metrics

| Metric | Type | Labels | Source | Status | Meaning |
|---|---:|---|---|---|---|
| `db9_tokio_worker_threads` | Gauge | none | `src/main.rs` | Emitted | Tokio worker thread count. |
| `db9_tokio_worker_busy_ratio` | Gauge | `worker` | `src/runtime_metrics.rs` | Emitted | Per-worker busy ratio. |
| `db9_tokio_worker_park_total` | Counter | `worker` | `src/runtime_metrics.rs` | Emitted | Worker park count. |
| `db9_tokio_worker_local_queue_depth` | Gauge | `worker` | `src/runtime_metrics.rs` | Emitted | Per-worker local queue depth. |
| `db9_tokio_global_queue_depth` | Gauge | none | `src/runtime_metrics.rs` | Emitted | Tokio global queue depth. |
| `db9_tokio_blocking_queue_depth` | Gauge | none | `src/runtime_metrics.rs` | Emitted | Tokio blocking queue depth. |
| `db9_tokio_num_blocking_threads` | Gauge | none | `src/runtime_metrics.rs` | Emitted | Blocking thread count. |
| `db9_tokio_num_idle_blocking_threads` | Gauge | none | `src/runtime_metrics.rs` | Emitted | Idle blocking thread count. |
| `db9_tokio_sampler_lag_microseconds_total` | Counter | none | `src/runtime_metrics.rs` | Emitted | Runtime sampler lag. |
| `db9_cgroup_cpu_usage_usec_total` | Counter | none | `src/runtime_metrics.rs` | Emitted | cgroup CPU usage. |
| `db9_cgroup_cpu_throttled_usec_total` | Counter | none | `src/runtime_metrics.rs` | Emitted | cgroup throttled CPU time. |
| `db9_cgroup_cpu_nr_throttled_total` | Counter | none | `src/runtime_metrics.rs` | Emitted | cgroup throttling events. |
| `db9_cgroup_cpu_nr_periods_total` | Counter | none | `src/runtime_metrics.rs` | Emitted | cgroup CPU periods. |
| `db9_cgroup_cpu_quota_usec` | Gauge | none | `src/runtime_metrics.rs` | Emitted | cgroup CPU quota, or `-1` when unavailable. |

## Worker, HNSW, and Storage Metrics

| Metric | Type | Labels | Source | Status | Meaning |
|---|---:|---|---|---|---|
| `db9_server_worker_tasks_total` | Counter | `task_type`, `result` | `src/worker/metrics.rs` | Emitted | Worker task completions by type and result. |
| `db9_server_worker_claim_attempts_total` | Counter | none | `src/worker/metrics.rs` | Emitted | Worker claim attempts. |
| `db9_server_worker_claim_successes_total` | Counter | none | `src/worker/metrics.rs` | Emitted | Successful worker claims. |
| `db9_server_worker_queue_depth` | Gauge | none | `src/worker/metrics.rs` | Emitted | Due tasks seen on last worker tick. |
| `db9_server_worker_active_jobs` | Gauge | none | `src/worker/metrics.rs` | Emitted | Active worker jobs on last tick. |
| `db9_server_worker_sweep_cycles_completed_total` | Counter | none | `src/worker/engine.rs` | Emitted | Registry sweep cycles completed. |
| `db9_server_worker_sweep_entries_total` | Counter | none | `src/worker/engine.rs` | Emitted | Registry sweep entries processed. |
| `db9_server_worker_sweep_entries_skipped_total` | Counter | none | `src/worker/engine.rs` | Emitted | Registry sweep entries skipped. |
| `db9_server_worker_legacy_queue_drained_total` | Counter | none | `src/worker/engine.rs` | Emitted | Legacy worker queue rows migrated into V2. |
| `db9_server_worker_storage_pd_region_stats_total` | Counter | result labels in code | `src/worker/engine.rs` | Emitted | Storage PD region-stat worker outcomes. |
| `db9_server_gc_safepoint_version` | Gauge | none | `src/worker/gc.rs` | Emitted | Last reported TiKV GC safepoint version. |
| `db9_server_gc_safepoint_advance_total` | Counter | `result` | `src/worker/gc.rs` | Emitted | GC safepoint advancement attempts. |
| `db9_server_hnsw_pending_indexes` | Gauge | none | `src/worker/metrics.rs` | Emitted | HNSW pending indexes sampled on worker tick. |
| `db9_server_hnsw_pending_indexes_observed` | Gauge | none | `src/worker/engine.rs` | Emitted | HNSW dirty indexes observed by sweep. |
| `db9_server_hnsw_sweep_enqueued_total` | Counter | none | `src/worker/engine.rs` | Emitted | HNSW merge tasks enqueued by sweep. |
| `db9_server_hnsw_sweep_enqueue_errors_total` | Counter | none | `src/worker/engine.rs` | Emitted | HNSW sweep enqueue failures. |
| `db9_server_hnsw_scan_deltas_applied_total` | Counter | none | `src/sql/operators/hnsw_scan.rs` | Emitted | HNSW deltas applied during query-time scans. |
| `db9_hnsw_s3_operations_total` | Counter | `operation`, `result` | `src/sql/hnsw/s3.rs` | Emitted | HNSW S3 GET/PUT outcomes. |
| `db9_hnsw_s3_operation_duration_seconds` | Histogram | `operation`, `result` | `src/sql/hnsw/s3.rs` | Emitted | HNSW S3 GET/PUT latency. |
| `db9_server_storage_memory_commits_total` | Counter | none | `src/storage/memory.rs` | Emitted | In-memory storage commit count. |
| `db9_server_storage_memory_write_conflicts_total` | Counter | none | `src/storage/memory.rs` | Emitted | In-memory storage write conflicts. |
| `db9_server_storage_memory_rollbacks_total` | Counter | none | `src/storage/memory.rs` | Emitted | In-memory storage rollback count. |
| `db9_server_storage_memory_active_txns` | Gauge | none | `src/storage/memory.rs` | Emitted | Active in-memory transactions. |
| `db9_server_storage_memory_keyspace_bytes` | Gauge | none | `src/storage/memory.rs` | Emitted | Approximate in-memory keyspace bytes. |
| `db9_server_storage_memory_lock_acquired_total` | Counter | `outcome` | `src/storage/memory.rs` | Emitted | In-memory storage lock acquisition outcomes. |
| `db9_server_storage_memory_lock_wait_seconds` | Histogram | none | `src/storage/memory.rs` | Emitted | In-memory storage lock wait latency. |

## fs9 Metrics

| Metric | Type | Labels | Source | Status | Meaning |
|---|---:|---|---|---|---|
| `db9_upload_sha256_seconds` | Histogram | none | `src/extensions/fs/ws/mod.rs` | Emitted | SHA-256 hot-path upload timing. |
| `db9_upload_mpsc_send_seconds` | Histogram | none | `src/extensions/fs/grpc/client.rs` | Emitted | Bounded upload channel send timing into the gRPC `WriteParts` stream. |
| `db9_fs9_glob_truncations_total` | Counter | `keyspace`, `mode` | `src/extensions/fs/table_function.rs`, `src/extensions/fs/glob_stream.rs` | Emitted | Glob scans stopped by byte budget. |
| `db9_fs9_read_budget_rejections_total` | Counter | `keyspace`, `operation` | `src/sql/expr/functions/fs9.rs` | Emitted | SQL fs9 read calls rejected by the global read budget. |
| `db9_fs9_juicefs_lifecycle_total` | Counter | `tenant_id`, `result` | `src/extensions/fs/grpc/admin.rs` | Emitted when fs-plane v2 is compiled | JuiceFS volume lifecycle materialization outcomes. `tenant_id` is the auth tenant id used for fs-plane tokens and volume names, not the db9 keyspace string. |
| `db9_fs9_redis_event_queue_depth` | Gauge | none | `src/extensions/fs/redis_events.rs` | Emitted | Pending fs9 Redis persistence events. |
| `db9_fs9_gc_backoff_failures` | Gauge | `keyspace` | `src/extensions/fs/embedded/pagefs.rs` | Retained embedded-only | Consecutive embedded PageFS background maintenance failures. Current production fs9 routing is JuiceFS-only, so this series is not emitted there. |
| `db9_fs9_gc_backoff_seconds` | Gauge | `keyspace` | `src/extensions/fs/embedded/pagefs.rs` | Retained embedded-only | Current embedded PageFS maintenance backoff delay. Current production fs9 routing is JuiceFS-only, so this series is not emitted there. |
| `db9_fs9_stats_worker_scans_total` | Counter | `result` | `src/extensions/fs/stats_worker.rs` | Emitted | fs9 stats worker scan outcomes. |
| `db9_fs9_stats_worker_scan_duration_seconds` | Histogram | `result` | `src/extensions/fs/stats_worker.rs` | Emitted | fs9 stats worker scan latency. |
| `db9_fs9_stats_worker_staleness_seconds` | Gauge | `result` | `src/extensions/fs/stats_worker.rs` | Emitted | Cached fs9 stats age after each scan attempt. |
| `db9_fs9_pd_lifecycle_probe_total` | Counter | `keyspace`, `result` | `src/extensions/fs/backend.rs` | Emitted | PD lifecycle probe outcomes before JuiceFS initialization. |
| `db9_fs9_operation_duration_seconds` | Histogram | `keyspace`, `backend`, `operation`, `result` | `src/extensions/fs/normalizing.rs` | Emitted | fs9 backend operation latency. `keyspace` comes from backend construction, so SQL and WebSocket paths use the tenant that opened the backend. Batch operations use `ok`, `partial`, or `err`. |
| `db9_fs9_orphan_inodes` | Gauge | `keyspace` | `src/metrics.rs` helper only | Planned | Orphan inode count. No runtime source currently returns this count. |

## Async Trigger Metrics

| Metric | Type | Labels | Source | Status | Meaning |
|---|---:|---|---|---|---|
| `db9_trigger_queue_depth` | Gauge | `keyspace` | `src/worker/mod.rs`, `src/worker/engine.rs`, `src/sql/executor/core/mod.rs`, `src/sql/executor/table_utils/mod.rs` | Emitted | Pending plus processing async trigger tasks for the tenant. Enqueue/finalization/tick refreshes are tenant-rate-limited to avoid storage scans on every task transition; idle sampler state is pruned after the retention window; `_DB9_SYS_TRIGGER_QUEUE_STATS` reads still compute an exact point-in-time value. |
| `db9_trigger_events_total` | Counter | `keyspace`, `event` | `src/worker/engine.rs` | Emitted | Async trigger task completions and failures. |
| `db9_trigger_execution_duration_seconds` | Histogram | `keyspace`, `result` | `src/metrics.rs` helper only | Planned | Trigger execution latency. The current worker path does not expose stable low-cardinality trigger timing at this boundary. |
| `db9_trigger_gc_runs_total` | Counter | `keyspace` | `src/metrics.rs` helper only | Planned | Trigger queue GC runs. No dedicated trigger GC path is wired yet. |

## batch_write_atomic Metrics

| Metric | Type | Labels | Source | Status | Meaning |
|---|---:|---|---|---|---|
| `db9_batch_write_atomic_requests_total` | Counter | `result` | `src/extensions/fs/ws/handler.rs` | Emitted | `batch_write_atomic` request outcomes. |
| `db9_batch_write_atomic_files_total` | Counter | none | `src/extensions/fs/ws/handler.rs` | Emitted | Files accepted into validated `batch_write_atomic` requests. |
| `db9_batch_write_atomic_subgroup_duration_seconds` | Histogram | none | `src/extensions/fs/embedded/pagefs/write_impl.rs` | Retained embedded-only | Per-subgroup embedded PageFS atomic commit latency. Current JuiceFS/gRPC backend does not support grouped atomic writes yet, so production `batch_write_atomic` requests do not emit this series. |
| `db9_batch_write_atomic_errors_total` | Counter | `code` | `src/extensions/fs/ws/handler.rs` | Emitted | Request-level validation, unsupported-backend, or whole-request execution error categories. |
| `db9_batch_write_atomic_entry_errors_total` | Counter | `code` | `src/extensions/fs/ws/handler.rs` | Emitted | Per-entry execution failure categories returned inside a validated `batch_write_atomic` response. |

## Plan Cache and Optimization Metrics

| Metric | Type | Labels | Source | Status | Meaning |
|---|---:|---|---|---|---|
| `db9_plan_cache_events_total` | Counter | `event` | `src/sql/executor/core/dispatch/prepared.rs`, `src/sql/executor/core/plan_cache.rs` | Emitted | Prepared plan cache hit, miss, promote, insert, invalidation, eviction, and ineligible events. |
| `db9_plan_cache_memory_bytes` | Gauge | none | `src/metrics.rs` helper only | Planned | Plan cache memory usage. No reliable entry-size estimator exists yet. |
| `db9_engine_rows_total` | Counter | `kind` | `src/metrics.rs` helper only | Planned | Standardized scanned, decoded, and fetched-base row counters. Existing operators do not yet report all categories consistently. |
| `db9_first_row_latency_seconds` | Histogram | none | `src/metrics.rs` helper only | Planned | First-row latency for streaming and remote plans. |
| `db9_statement_memory_bytes` | Gauge | `kind` | `src/metrics.rs` helper only | Planned | Peak statement memory gauges. Expensive-query logging exists separately. |
| `db9_operator_memory_bytes` | Gauge | `operator` | `src/metrics.rs` helper only | Planned | Sort and aggregate memory gauges. |
| `db9_remote_rejects_total` | Counter | `reason` | `src/metrics.rs` helper only | Planned | Remote execution rejection reasons. |
| `db9_streaming_mode_total` | Counter | `mode` | `src/metrics.rs` helper only | Planned | DB9 streaming mode selection. |

## Spill Metrics

External spill execution is not implemented yet. The helper names below are reserved so future spill work uses one Prometheus vocabulary.

| Metric | Type | Labels | Source | Status | Meaning |
|---|---:|---|---|---|---|
| `db9_spill_bytes_total` | Counter | none | `src/metrics.rs` helper only | Planned | Bytes spilled to temporary storage. |
| `db9_spill_runs_total` | Counter | none | `src/metrics.rs` helper only | Planned | Spill run count. |
| `db9_spill_passes_total` | Counter | none | `src/metrics.rs` helper only | Planned | Spill merge/pass count. |
| `db9_spill_cleanup_failures_total` | Counter | none | `src/metrics.rs` helper only | Planned | Spill cleanup failures. |

## In-Memory SQL Observability

The following are not Prometheus metrics. They are SQL-visible, per-tenant, in-memory snapshots:

```sql
SELECT * FROM _db9_sys_observability();
SELECT * FROM _db9_sys_query_samples();
```

`_DB9_SYS_OBSERVABILITY` exposes rolling-window statement count, transaction commit count, error count, QPS, TPS, average latency, p99 latency, and active connections. `_DB9_SYS_QUERY_SAMPLES` exposes grouped sampled query latency and error data. These surfaces are intentionally tenant-scoped and have no argument for reading another tenant's data.
