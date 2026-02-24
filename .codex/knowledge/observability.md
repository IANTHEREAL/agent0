# Observability v1 (per-tenant, rolling 1h) — facts + code locations

## Goal (Phase 1)
- Low-overhead, in-memory observability for db9-server.
- Retention: **last 1 hour** only.
- Tenant isolation: metrics are keyed by tenant keyspace; tenant can only query its own metrics surface.
- Key metrics: QPS/TPS, p99/avg latency, active connections.
- Query samples: keep a bounded set of sampled SQL statements (slow + errors always sampled; plus probabilistic sampling).
- Exposed to `cloud-admin-portal` tenant detail page.

## Implementation overview (db9-server)

### Core module
- `src/observability.rs`
  - `ObservabilityRegistry`: per-tenant registry (keyed by keyspace string).
  - `TenantObservability`: per-tenant state:
    - `active_connections` gauge (atomic).
    - rolling 1h window (60×1min buckets) with atomic counters + latency histogram.
    - sampled statement ring buffer (Mutex, used only on sampling path).
  - Summary snapshot:
    - `snapshot_summary()` → QPS/TPS + avg/p99 latency over the rolling window.
  - Samples snapshot:
    - `snapshot_query_samples()` → grouped sampled statements (count + avg/p99/max + last_seen).

### Probes / instrumentation points
- Active connections:
  - `src/protocol/handler.rs`: `DynamicPgHandler::init_executor()` creates a per-tenant `ConnectionGuard` and stores it in the handler; `Drop` decrements.
- Statement latency + count:
  - `src/sql/executor.rs`: `Executor::execute()` measures each parsed statement (and also single-statement “fast paths” like `CREATE FUNCTION`) and calls `TenantObservability::record_statement(...)`.
- TPS (commit rate):
  - `src/sql/session.rs`: `Session::commit()` increments per-tenant commit counter on successful TiKV commit via `TenantObservability::record_commit()`.

### SQL surface (for portal / operators)
- `src/sql/executor_join.rs`: table-valued functions (resolved in `Executor::get_table_data`)
  - `SELECT * FROM _db9_sys_observability;` / `SELECT * FROM _db9_sys_observability();`
    - `window_seconds, statement_count, txn_commit_count, error_count, qps, tps, latency_avg_ms, latency_p99_ms, active_connections`
  - `SELECT * FROM _db9_sys_query_samples;` / `SELECT * FROM _db9_sys_query_samples();`
    - `query, sample_count, error_count, latency_avg_ms, latency_p99_ms, latency_max_ms, last_seen_ms_ago`

## Tenant isolation model
- Observability data is **in-memory only** in v1 (no TiKV writes).
- Metrics registry key = `effective_keyspace` chosen at auth time (`tenant.user` parsing in `src/protocol/handler.rs`).
- SQL surface has **no arguments** (no way to request another tenant’s metrics).

## Performance notes
- Hot-path updates: atomic increments + O(1) histogram bin increment.
- Locks: only on sampling path (slow/error/rare probabilistic hits).
- Time base: monotonic process uptime (no wall clock dependency).

## Configuration knobs (env)
- `DB9_OBS_ENABLED` (default `true`)
- `DB9_OBS_SAMPLE_EVERY` (default `1000`) — sample 1/N statements (slow/errors always sampled)
- `DB9_OBS_SLOW_MS` (default `200`) — always sample >= this latency
- `DB9_OBS_MAX_SAMPLE_EVENTS` (default `20000`) — per-tenant cap (still pruned to last 1h)
- `DB9_OBS_MAX_SAMPLE_GROUPS` (default `50`) — rows returned by `db9_query_samples()`
- `DB9_OBS_MAX_SQL_LEN` (default `512`) — stored sample SQL length after normalization

## cloud-admin-portal integration

### Backend
- `cloud-admin-portal/backend/app/api/tenants.py`
  - `POST /api/tenants/{name}/observability/bootstrap` (called after tenant creation)
    - Creates/rotates per-tenant observability account: `_db9_sys_observer`
    - Stores credentials in portal DB: `TenantDB.observability_user` / `TenantDB.observability_password`
  - `GET /api/tenants/{name}/observability` (no tenant session)
    - Queries db9-server using the stored observability credentials and returns structured JSON.
- `cloud-admin-portal/backend/app/services/pg_client.py`
  - `Db9Client` uses `pg8000` (pure Python, no `psql` subprocess); keeps the client minimal (no custom splitter/pool).

### Frontend
- `cloud-admin-portal/frontend/src/pages/TenantDetailPage.tsx`
  - Renders `TenantObservabilityCard`.
- `cloud-admin-portal/frontend/src/components/observability/TenantObservabilityCard.tsx`
  - Cards for QPS/TPS/p99/avg/connections + sampled statements table.
- `cloud-admin-portal/frontend/src/api/tenants.ts`
  - `useTenantObservability()` auto-refreshes every 5s, but stops polling on HTTP 409 (observability not bootstrapped).

## Readonly enforcement (db9-server)
- Observability account: `_db9_sys_observer`
- Enforcement point: `src/sql/executor.rs`
  - Allows only:
    - `SELECT ... FROM _db9_sys_observability()` / `_db9_sys_query_samples()` (no nested queries)
    - tableless `SELECT ...` (e.g., `SELECT 1`) (no nested queries)
    - harmless session/tx control (`SET ...`, `BEGIN/COMMIT/ROLLBACK/...`) as **no-op** for client compatibility
  - Skips `TenantObservability::record_statement(...)` for observer queries (avoid self-pollution from portal polling).
  - In autocommit, uses `ROLLBACK` instead of `COMMIT` for observer queries to avoid incrementing per-tenant commit counter (TPS) and reduce write-path overhead.
