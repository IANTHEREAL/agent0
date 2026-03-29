# Design: Admin Session Management & Kill-Query

**Status**: Draft
**Priority**: P1 (operational readiness for multi-tenant production)
**Tracking Issue**: #2210
**Related**: #2209 (Layer 1 merged), #2206 (original feature request)

> **Draft / non-SoT note**
>
> This document is a design draft, not a current-behavior contract.
> Validate current behavior against `docs/sot/**`, `docs/ARCHITECTURE.md`,
> and the implementation under `src/**` before using it for product or
> compatibility decisions.

## Background & Motivation

db9-server serves millions of tenants. Operators need the ability to:

1. **Inspect** active sessions — who is connected, what query is running, how long.
2. **Cancel** a single in-flight query without dropping the connection (PostgreSQL `pg_cancel_backend` equivalent).
3. **Terminate** a single connection (PostgreSQL `pg_terminate_backend` equivalent).
4. **Terminate all** connections for a misbehaving tenant (stop-bleeding during incidents).

Without these capabilities, operators must restart server processes to clear stuck queries, affecting all tenants on that server.

## Goals

- Provide transport-agnostic internal primitives for list / cancel / terminate operations.
- Expose these via an authenticated admin API with audit logging.
- Support million-tenant scale without memory pressure.
- Cancel produces PostgreSQL-compatible SQLSTATE `57014` ("canceling statement due to user request"), severity `ERROR` (not `FATAL`).
- Cancelled connections remain reusable for subsequent queries.

## Non-Goals (v1)

- `cancel-all` by tenant — deferred to v2, not excluded. The per-query cancel primitive is non-destructive (57014, connection survives), so cancel-all is technically straightforward (iterate + cancel each). Deferred because terminate-all covers the primary stop-bleeding scenario, and v1 focuses on getting single-connection operations stable first. Can be added as a simple iteration over `list_sessions_by_tenant` + `cancel_query` once v1 is proven.
- Customer-facing self-service API (admin-only for v1).
- Multi-server routing (v1 assumes single server; multi-server addressed in v2).
- Query ID-based operations (connection ID is the stable identifier for v1).
- `pg_stat_activity` virtual table — important for PostgreSQL compatibility long-term, but deferred from the admin API scope. Should be tracked as a separate catalog compatibility item.

**Known v1 limitation — cancel propagation delay**: after `cancel_query` fires, the query may not terminate immediately if the executor is blocked on a synchronous TiKV operation with no async yield point. In that case, the query terminates when the current TiKV RPC completes or `statement_timeout` expires, whichever comes first. Admins should understand this latency and use `terminate` (connection kill) as escalation if cancel does not take effect promptly.

## Current State (Layer 1 — Merged in #2209)

### Internal Primitives (`src/admin/session_registry.rs`)

The `SessionRegistry` is a process-global, transport-agnostic registry:

```
SessionRegistry {
    sessions: DashMap<i64, Arc<SessionInfo>>,      // connection_id → info
    tenant_index: DashMap<String, DashSet<i64>>,   // tenant_id → {connection_ids}
    server_id: String,                              // hostname
}
```

**SessionInfo fields:**
- `connection_id` (i64): stable per-connection identifier
- `tenant_id`, `principal`, `database`, `peer_addr`: connection metadata
- `connected_at_epoch_ms`, `connected_at_mono`: timestamps
- `state` (AtomicSessionState): `idle` | `active` | `idle_in_transaction` | `idle_in_failed_transaction`
- `current_query` (RwLock\<String>): truncated to 1024 chars
- `query_start` (AtomicI64): epoch ms when current query started
- `cancel_token` (CancellationToken): connection-level
- `query_cancel` (RwLock\<Option\<CancellationToken>>): query-level child token

**Operations (already implemented & tested):**

| Operation | Signature | Semantics |
|-----------|-----------|-----------|
| Register | `register(info: SessionInfo)` | Called post-auth, adds to sessions + tenant index |
| Unregister | `unregister(connection_id)` | Called on disconnect, cleans up empty tenant entries |
| Get | `get(connection_id) → Option<SessionSnapshot>` | Point lookup |
| List | `list(filter: &SessionFilter) → Vec<SessionSnapshot>` | Filtered, paginated listing |
| Cancel query | `cancel_query(connection_id) → Result<(), CancelError>` | Cancels child token → SQLSTATE 57014, connection survives |
| Terminate | `terminate(connection_id) → Result<(), CancelError>` | Cancels connection-level token → connection drops |
| Terminate all | `terminate_all(tenant_id) → TerminateAllResult` | Bulk kill all connections for a tenant |

**SessionFilter fields:**
- `tenant_id: Option<String>` — required unless `all_tenants` is true
- `all_tenants: bool` — capped at 1000 results
- `principal: Option<String>`, `state: Option<SessionState>`, `min_duration_ms: Option<i64>`
- `limit: usize`, `offset: usize` — pagination

**Scale characteristics:**
- DashMap provides O(1) sharded concurrent access.
- Memory is bounded by *active connections*, not total tenant count.
- Empty tenant entries are cleaned up on unregister (`remove_if` with emptiness re-check).
- Query text truncated to 1024 chars to bound memory.

**Cancel token threading (simple + extended query paths):**
- `begin_query_tracking` creates a child `CancellationToken` of the connection token, stored in both `SessionInfo.query_cancel` and `DynamicPgHandler.active_query_cancel`.
- Pre-executor cooperative check: `is_cancelled()` before entering executor (handles cancel-between-tracking-and-select race).
- `tokio::select! { biased; }` races executor against cancel token during execution.
- Both simple and extended query paths are wired.
- Handler-path regression tests verify 57014 + post-cancel reuse.

## Design: Layer 2 — Admin API

### Endpoints

**Note**: the `/admin/...` paths below are *logical endpoint shapes* describing the API surface. They do not imply a server-hosted HTTP surface — the actual hosting (server-direct, backend proxy, or both) depends on the entrypoint decision in the "Open Design Decision" section below.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/admin/sessions` | List sessions (requires `tenant_id` or `all_tenants=true`) |
| `POST` | `/admin/sessions/:id/cancel` | Cancel current query (57014) |
| `POST` | `/admin/sessions/:id/terminate` | Kill connection |
| `POST` | `/admin/tenants/:id/terminate-all` | Bulk terminate by tenant |

#### `GET /admin/sessions`

Query parameters:
- `tenant_id` (string, required unless `all_tenants=true`)
- `all_tenants` (bool, default false) — capped at 1000 results
- `principal` (string, optional)
- `state` (string, optional): `idle`, `active`, `idle_in_transaction`, `idle_in_failed_transaction`
- `min_duration_ms` (i64, optional)
- `limit` (int, default 100)
- `offset` (int, default 0)

**Fail-closed rule**: if neither `tenant_id` nor `all_tenants=true` is provided, return 400. No implicit global listing.

Response:
```json
{
  "sessions": [
    {
      "connection_id": 12345,
      "tenant_id": "acme-corp",
      "principal": "app_user",
      "database": "postgres",
      "peer_addr": "10.0.1.42:54321",
      "connected_at_epoch_ms": 1711670400000,
      "state": "active",
      "current_query": "SELECT * FROM large_table WHERE ...",
      "query_start_epoch_ms": 1711670410000,
      "duration_ms": 120000,
      "query_duration_ms": 10000,
      "server_id": "db9-server-0"
    }
  ],
  "has_more": true
}
```

**Pagination note**: the response uses `has_more: bool` instead of an exact `total` count. Computing exact counts across DashMap shards with filters is expensive at scale. Callers paginate by incrementing `offset` until `has_more` is `false`. The `all_tenants` cap of 1000 means at most 1000 results per page; if `has_more` is `true`, the caller should paginate with `offset` to retrieve more.

#### `POST /admin/sessions/:id/cancel`

Request body (optional):
```json
{
  "reason": "slow query blocking other tenants"
}
```

Response:
```json
{
  "connection_id": 12345,
  "result": "cancelled",
  "query_was": "SELECT * FROM large_table WHERE ..."
}
```

Error cases:
- 404: connection not found
- 409: no active query to cancel

#### `POST /admin/sessions/:id/terminate`

Same request/response shape as cancel, with `"result": "terminated"`.

#### `POST /admin/tenants/:id/terminate-all`

Response:
```json
{
  "tenant_id": "acme-corp",
  "requested": 15,
  "terminated": 14,
  "already_closed": 1
}
```

**Fail-closed rule**: `:id` must be a non-empty tenant identifier. No wildcard / empty tenant.

### Operation Semantics & Race Conditions

#### Cancel (`POST /admin/sessions/:id/cancel`)

- **Query already finished**: returns 409 (`no_active_query`). The `SessionInfo.query_cancel` is `None` after `end_query`, so cancel is a no-op. Idempotent: calling cancel twice on the same query returns 409 on the second call.
- **Query not yet started** (session idle): returns 409 (`no_active_query`).
- **Race: cancel arrives between `begin_query_tracking` and executor entry**: handled by the pre-executor cooperative `is_cancelled()` check. The query returns 57014 before touching the executor.
- **Race: cancel arrives during execution**: handled by `tokio::select!` racing the executor against the cancel token. Query returns 57014.
- **Race: cancel arrives after execution but before response sent**: cancel token is already consumed; cancel returns 409. Client receives normal result.

#### Terminate (`POST /admin/sessions/:id/terminate`)

- **Idle connection**: cancels the connection-level `CancellationToken`. The next pgwire operation on that connection will see the cancelled token and disconnect. Returns `"terminated"`.
- **Active query**: cancels the connection-level token, which also cancels any child (query-level) token. The running query aborts with FATAL, connection drops. Returns `"terminated"`.
- **Already disconnected** (unregistered): returns 404. The session is removed from the registry on disconnect.
- **Idempotent**: calling terminate on an already-cancelled token is safe (CancellationToken::cancel is idempotent). Returns `"terminated"` again if the session entry still exists.

#### Terminate-all (`POST /admin/tenants/:id/terminate-all`)

- **Partial success model**: iterates all connections for the tenant. Each connection is independently terminated. Returns `{ requested, terminated, already_closed }`.
  - `terminated`: connection was live and cancel token was fired.
  - `already_closed`: connection was in the tenant index but no longer in the sessions map (race with natural disconnect).
- **Empty tenant** (no connections): returns `{ requested: 0, terminated: 0, already_closed: 0 }`. This is not an error.
- **Concurrent new connections**: connections that register after the iteration starts are not terminated. This is by design — the operator can call terminate-all again if needed.

### Server Targeting (v1 Constraint)

**v1 assumes a single server instance.** This is an explicit architectural constraint, not an implicit assumption:

- All admin API calls target the local server's `SessionRegistry`.
- There is no cross-server routing, discovery, or aggregation.
- The `server_id` field in `SessionSnapshot` identifies which server a session belongs to (hostname), but v1 does not use it for routing.

**Implications for backend proxy (Option A/C):**
- v1: backend sends admin requests to a single, configured server endpoint.
- If multiple servers exist, the backend must know which server to target. v1 does NOT solve this — the operator must know the topology.
- v2 path: backend maintains a `connection_id → server` mapping (populated via server heartbeats or connection registration callbacks). Admin API routes to the correct server automatically.

**What this means for the API contract:**
- Responses include `server_id` so clients know which server they're talking to.
- `GET /admin/sessions` only returns sessions from the local server.
- Documentation must state: "v1 is single-server scoped. Multi-server admin requires targeting each server independently."

### Audit Logging

All write operations (cancel, terminate, terminate-all) MUST emit a structured audit log entry:

```
AuditEntry {
    admin_actor: String,       // identity of the admin caller
    action: String,            // "cancel_query" | "terminate_session" | "terminate_all"
    target_tenant_id: String,
    target_connection_ids: Vec<i64>,
    server_id: String,
    result: String,            // "success" | "not_found" | "no_active_query"
    reason: Option<String>,    // optional reason from request
    timestamp_epoch_ms: i64,
}
```

v1 storage: structured log (tracing) at `INFO` level with all fields. Future: persist to audit table.

### Open Design Decision: Management Entrypoint & Auth

The external entrypoint for the admin API is **not yet decided**. Three options are under consideration:

#### Option A: Backend Admin Proxy

```
Admin CLI / Dashboard → Backend Admin API → (internal RPC) → Server Control Primitive
```

- **Auth**: reuse existing `DB9_API_KEYS` mechanism on backend.
- **Server exposure**: internal-only endpoint, no public admin surface on server.
- **Pro**: single management entry point, no new auth surface.
- **Con**: depends on backend availability — if backend is down, cannot kill queries.
- **Backend→Server auth**: needs concrete mechanism (see below).

#### Option B: Server-Direct Admin API

```
Admin CLI / Dashboard → Server Admin API (separate port or path)
```

- **Auth**: server-local secret (e.g. `SERVER_ADMIN_SECRET` env var).
- **Pro**: works when backend is unavailable (break-glass).
- **Con**: second auth surface, secret distribution/rotation, new network exposure.

#### Option C: Both, Staged

```
Primary:     Admin CLI → Backend Admin API → Server Internal RPC
Break-glass: Admin CLI → Server Local Admin (localhost-only / unix socket)
```

- **v1**: backend proxy as primary entrypoint.
- **v1 optional**: localhost-only break-glass (no public TCP port).
- **v2**: full break-glass with proper secret management.
- **Pro**: most complete operability model.
- **Con**: more implementation scope.

**Decision needed from @EdHuang** before implementation starts.

### Backend → Server Auth (if Option A or C)

If the backend proxies to the server, the internal RPC channel needs authentication. Options:

| Mechanism | Description | Complexity |
|-----------|-------------|------------|
| Shared service secret | Backend and server share a secret via env var; backend sends it as bearer token on internal calls | Low |
| mTLS | Server only accepts internal control calls from clients with a specific CA-signed cert | Medium |
| HMAC-signed requests | Backend signs each request with a shared key; server verifies signature + timestamp | Medium |

v1 recommendation: shared service secret (simplest, sufficient for internal-only traffic within the same cluster). Upgrade to mTLS in v2 if needed.

## Implementation Steps (after design is locked)

### Common work (independent of entrypoint choice)

1. **Audit logging module** — `src/admin/audit.rs`, structured tracing for all write operations with the defined schema.
2. **Server internal control service** — `src/admin/control.rs`, transport-agnostic service layer that maps logical operations (list, cancel, terminate, terminate-all) to `SessionRegistry` calls + audit logging. This is the shared core regardless of how it's exposed.
3. **Logical API contract** — endpoint schemas, error codes, pagination behavior (shared between all entrypoint options).
4. **Integration tests** — test the control service directly (no transport dependency).
5. **Documentation** — admin runbook.

### If Option A (backend proxy) or Option C (staged both)

6. **Server: internal control endpoint** — expose control service via internal-only HTTP/RPC endpoint (not public).
7. **Server: internal auth** — implement chosen backend→server auth on the internal endpoint.
8. **Backend: admin API endpoints** — public admin endpoints that proxy to server internal endpoint.
9. **Backend: integration tests** — end-to-end through backend proxy.

### If Option B (server-direct) or Option C (break-glass)

10. **Server: admin HTTP surface** — expose control service via server-hosted admin port or path prefix.
11. **Server: admin auth middleware** — implement chosen server-direct auth mechanism.
12. **Server: admin surface tests** — end-to-end through server-direct path.

## Test Plan

### Unit Tests (`cargo test`)

- Auth middleware rejects unauthenticated requests.
- Auth middleware accepts valid credentials.
- Audit log entries contain all required fields.
- Fail-closed: GET /admin/sessions without tenant_id returns 400.
- Fail-closed: terminate-all with empty tenant_id returns 400.

### Integration Tests

- Full flow: connect N sessions → list via admin API → cancel one → verify 57014 on client → verify session still active → run follow-up query successfully.
- Terminate flow: connect → terminate via admin API → verify client disconnect.
- Terminate-all flow: connect N sessions for tenant A and M for tenant B → terminate-all tenant A → verify all A disconnected, all B unaffected.
- Pagination: connect many sessions → list with limit/offset → verify correct pages.
- Filter: list by state, principal, min_duration_ms.

### Scale Tests

- Register/unregister 100K sessions, verify no memory leak in tenant index.
- Concurrent cancel + terminate + list operations, verify no deadlock.
