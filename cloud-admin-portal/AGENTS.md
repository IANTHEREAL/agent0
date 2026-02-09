# cloud-admin-portal Knowledge Base

**Generated:** 2026-02-08
**Commit:** 0fd997f
**Branch:** master

Web-based admin interface and CLI for pg-tikv multi-tenant database. React 18 frontend + Rust backend (axum 0.7).

## Commands

```bash
# Rust backend (from backend/)
cd backend && cargo build --release     # Build both binaries
cd backend && cargo check               # Type check only
./backend/target/release/pgtikv-admin   # Run server (default: SQLite, port 8090)
./backend/target/release/pgtikv-ctl     # CLI tool

# Frontend (from frontend/)
cd frontend && npm install
cd frontend && npm run dev              # Dev server (port 5173)
cd frontend && npx tsc --noEmit         # Type check
cd frontend && npm run build            # Production build (tsc + vite)

# Full dev stack
./scripts/dev.sh                        # Starts backend + frontend together

# Production
./scripts/build.sh                      # Build backend (release) + frontend + Docker images
./scripts/deploy.sh start|stop|restart|status|logs
```

## Structure

```
cloud-admin-portal/
├── backend/                 # Rust backend (axum + sqlx)
│   ├── src/
│   │   ├── api/             # axum route handlers
│   │   │   ├── mod.rs       # Router with all routes
│   │   │   ├── tenants.rs   # Tenant CRUD, connect, query, observability
│   │   │   ├── users.rs     # User management (list, create, delete, reset-password)
│   │   │   ├── system.rs    # Health + info endpoints
│   │   │   └── audit.rs     # Audit log query
│   │   ├── services/        # External service clients
│   │   │   ├── pd_client.rs # TiKV PD HTTP API (keyspace management)
│   │   │   ├── pg_client.rs # pg-tikv connection via tokio-postgres
│   │   │   └── reconciler.rs# Background stuck-tenant recovery + keyspace sync
│   │   ├── lib.rs           # Module declarations, constants, AppState
│   │   ├── config.rs        # Env-based config (PGTIKV_ prefix)
│   │   ├── db.rs            # sqlx AnyPool queries (SQLite/PostgreSQL)
│   │   ├── models.rs        # serde request/response/DB row types
│   │   ├── error.rs         # AppError → axum response, From<sqlx/reqwest>
│   │   ├── auth.rs          # ApiKeyAuth extractor + TenantSessionExtractor
│   │   ├── crypto.rs        # AES-256-GCM credential encryption (aes-gcm)
│   │   ├── session.rs       # In-memory RwLock<HashMap> session manager
│   │   ├── main.rs          # pgtikv-admin server binary
│   │   └── cli.rs           # pgtikv-ctl CLI binary (clap)
│   ├── app/                 # [DEAD] Legacy Python backend — only .pyc artifacts remain
│   ├── Cargo.toml
│   └── Dockerfile           # Multi-stage: rust:1.83-bookworm → debian:bookworm-slim
├── frontend/                # React TypeScript frontend
│   └── src/
│       ├── api/             # API client + React Query hooks
│       │   ├── client.ts    # Base fetch wrapper with session injection
│       │   ├── tenants.ts   # Tenant CRUD/connect/query/observability hooks
│       │   └── users.ts     # User management hooks
│       ├── components/
│       │   ├── ui/          # shadcn/ui primitives (button, dialog, toast, etc.)
│       │   ├── tenants/     # CreateTenantDialog, EditTenantMetadataDialog
│       │   ├── users/       # CreateUserDialog
│       │   ├── sql/         # SqlEditor
│       │   ├── observability/ # TenantObservabilityCard
│       │   ├── layout/      # AppLayout, TenantLayout (with Outlet)
│       │   └── common/      # CredentialsModal, ConfirmDialog
│       ├── contexts/        # TenantSessionContext (connect/disconnect state)
│       ├── hooks/           # useSortableData (generic table sorting)
│       ├── pages/           # TenantsPage, TenantDetailPage, SqlEditorPage
│       ├── types/           # TypeScript interfaces (index.ts)
│       └── App.tsx          # Routes: / → /tenants, /tenants/:id, /tenants/:id/sql
├── deploy/                  # Docker Compose + nginx
│   ├── docker-compose.yml   # nginx + backend + frontend-builder
│   ├── docker-compose.dev.yml
│   ├── nginx/nginx.conf     # Reverse proxy: static SPA + /api → backend:8090
│   └── .env.example
└── scripts/                 # dev.sh, build.sh, deploy.sh
```

## Where to Look

| Task | Location |
|------|----------|
| Add API endpoint | `backend/src/api/mod.rs` (route) + handler file |
| Add tenant handler | `backend/src/api/tenants.rs` |
| Add user handler | `backend/src/api/users.rs` |
| Add DB query | `backend/src/db.rs` |
| Add request/response type | `backend/src/models.rs` |
| Change config | `backend/src/config.rs` (add field + `from_env()`) |
| Change PD client | `backend/src/services/pd_client.rs` |
| Change pg-tikv client | `backend/src/services/pg_client.rs` |
| Change reconciler | `backend/src/services/reconciler.rs` |
| Change CLI commands | `backend/src/cli.rs` |
| Add credential encryption | `backend/src/crypto.rs` |
| Change frontend types | `frontend/src/types/index.ts` |
| Change frontend API hooks | `frontend/src/api/tenants.ts` or `users.ts` |
| Change API client/auth | `frontend/src/api/client.ts` |
| Add frontend route | `frontend/src/App.tsx` |
| Change tenant session UX | `frontend/src/contexts/TenantSessionContext.tsx` |
| Add UI component | `frontend/src/components/ui/` (shadcn/ui style) |

## Code Style

### Rust (Backend)

- **Framework**: axum 0.7 with tower-http CORS
- **Database**: sqlx 0.8 with AnyPool (SQLite + PostgreSQL)
- **Async**: tokio runtime, all handlers are `async`
- **Auth**: `ApiKeyAuth` axum extractor (FromRequestParts), `TenantSessionExtractor` manual from HeaderMap
- **Errors**: `AppError` with `IntoResponse` impl, `From<sqlx::Error>` and `From<reqwest::Error>`
- **SQL**: Use `$1, $2, ...` placeholders; `adapt_sql()` in `db.rs` converts to `?` for SQLite
- **IDs**: TEXT primary keys (tenant IDs = 12-char alphanumeric, credentials/audit = UUID v4)
- **Dates**: ISO 8601 TEXT strings (not native datetime columns)
- **Constants**: `lib.rs` defines `KEYSPACE_PREFIX`, `TENANT_ID_LEN`, `DEFAULT_ADMIN_USER`, tenant state strings

```rust
// Handler signature pattern
pub async fn list_tenants(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Query(params): Query<ListTenantsParams>,
) -> Result<Json<TenantListResponse>, AppError> {
    let (tenants, total) = db::list_tenants(&state.db, page, size, ...).await?;
    Ok(Json(TenantListResponse { items, total, page, size }))
}
```

### TypeScript (Frontend)

- **Imports**: React → third-party → `@/` aliases → relative
- **Components**: Function components with explicit typing
- **State**: React Query (`@tanstack/react-query`) for server state, `useState` for UI, Context for session
- **UI**: shadcn/ui components from `@/components/ui/`
- **Path alias**: `@/*` → `./src/*`
- **Session**: `sessionStorage` (per-tab, cleared on tab close) via `TenantSessionContext`
- **API client**: `apiRequest<T>()` in `client.ts` auto-injects `X-Tenant-Session` from sessionStorage

## Testing

```bash
cd backend && cargo check               # Type check (no test suite beyond crypto.rs)
cd frontend && npx tsc --noEmit         # Type check
cd frontend && npm run build            # Production build validates
```

**Note**: Only `crypto.rs` has unit tests (`#[cfg(test)]`). No integration test suite for the Rust backend. Legacy Python tests under `backend/tests/` reference the defunct FastAPI backend — ignore them.

## Key Patterns

### Tenant State Machine

```
CREATING → ACTIVE (success) or CREATE_FAILED (failure)
ACTIVE → DISABLING → DISABLED (delete/remove)
ACTIVE → SUSPENDED (future)

Reconciler: CREATING(>10min) → check PD → ACTIVE or CREATE_FAILED
Reconciler: DISABLING(>10min) → DISABLED
Reconciler (sync mode): imports PD keyspaces as tenants when PGTIKV_RECONCILER_SYNC_KEYSPACES=true
```

### Tenant Session Flow

1. `POST /api/tenants/{id}/connect` with `{ "admin_user", "admin_password" }`
2. Backend validates credentials via `tokio-postgres` → returns `{ "session_id", "expires_at" }`
3. Frontend stores in `sessionStorage` via `TenantSessionContext`
4. `apiRequest()` auto-attaches `X-Tenant-Session` header for matching tenant routes
5. Sessions expire after 1 hour (in-memory, lost on restart)

### Observability Flow

1. `POST /api/tenants/{id}/observability/bootstrap` with admin credentials
2. Creates `_pgtikv_sys_observer` user in pg-tikv, stores encrypted credential in `tenant_credentials`
3. `GET /api/tenants/{id}/observability` uses stored observer credentials to query pg-tikv
4. Frontend polls every 5s, stops on 409 (not bootstrapped)

### Credential Encryption

When `PGTIKV_CREDENTIAL_KEY` is set (base64-encoded 32-byte key):
- Observer passwords encrypted via AES-256-GCM before storage in `tenant_credentials`
- `crypto.rs` handles encrypt/decrypt with random 12-byte nonce

### Service Layer

- `PdClient`: TiKV Placement Driver HTTP API (keyspace CRUD, health, list)
- `PgClient`: pg-tikv connection via `tokio-postgres` (user management, SQL execution, observability)
- `Reconciler`: Background tokio task — recovers stuck tenants + optional keyspace sync

### Two Binaries

| Binary | Entry | Purpose |
|--------|-------|---------|
| `pgtikv-admin` | `src/main.rs` | HTTP API server (axum, tokio) |
| `pgtikv-ctl` | `src/cli.rs` | CLI tool (clap, reqwest) |

### AppState

```rust
pub struct AppState {
    pub db: AnyPool,           // sqlx (SQLite or PostgreSQL)
    pub config: Arc<Config>,
    pub sessions: Arc<SessionManager>,
    pub http_client: reqwest::Client,
}
```

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `PGTIKV_PD_ENDPOINTS` | `127.0.0.1:2379` | TiKV PD addresses (also reads `PD_ENDPOINTS`) |
| `PGTIKV_PG_HOST` | `127.0.0.1` | pg-tikv server host (internal) |
| `PGTIKV_PG_PORT` | `5433` | pg-tikv server port (internal) |
| `PGTIKV_PG_PUBLIC_ENDPOINTS` | `127.0.0.1:5433` | Public endpoints shown to users (comma-separated) |
| `PGTIKV_API_PORT` | `8090` | Backend API port |
| `PGTIKV_API_HOST` | `0.0.0.0` | Backend bind address |
| `PGTIKV_DATABASE_URL` | `sqlite://data/portal.db?mode=rwc` | Metadata DB (sqlite:// or postgres://) |
| `PGTIKV_API_KEYS` | (empty) | Comma-separated API keys (empty = no auth) |
| `PGTIKV_CORS_ORIGINS` | `http://localhost:5173,http://localhost:3000` | CORS origins |
| `PGTIKV_RECONCILER_ENABLED` | `true` | Enable background reconciler |
| `PGTIKV_RECONCILER_INTERVAL_SECONDS` | `300` | Reconciler cycle interval |
| `PGTIKV_RECONCILER_SYNC_KEYSPACES` | `false` | Auto-import PD keyspaces as tenants |
| `PGTIKV_SESSION_TTL_HOURS` | `1` | Session expiry |
| `PGTIKV_AUDIT_RETENTION_DAYS` | `90` | Audit log retention |
| `PGTIKV_CREDENTIAL_KEY` | (empty) | Base64 AES-256 key for credential encryption |
| `PGTIKV_API_URL` | `http://localhost:8090/api` | CLI: API base URL |
| `PGTIKV_API_KEY` | (empty) | CLI: API key |
| `VITE_API_URL` | `/api` | Frontend API base URL |

## Anti-Patterns

- **No `backend-rs/`**: Rust backend lives at `backend/` directly. Old references to `backend-rs/` are wrong.
- **Dead Python code**: `backend/app/` contains only `.pyc` artifacts from the replaced FastAPI backend. Do NOT reference or modify.
- **Session persistence**: In-memory sessions lost on restart. For multi-instance production, needs shared store.
- **Keyspace deletion**: TiKV keyspaces can only be DISABLED, not deleted. Data remains.
- **Type suppression**: Never use `as any`, `@ts-ignore` in frontend or `unsafe` in Rust.
- **No `unwrap()`/`expect()` in handlers** — use `?` or proper `AppError` construction.
- **No sqlx compile-time macros** (`query!`, `query_as!`) — incompatible with AnyPool.
- **No Python tests**: Legacy pytest tests under `backend/tests/` reference dead FastAPI code — ignore them.

## URLs (Development)

- Frontend: http://localhost:5173
- Backend API: http://localhost:8090/api
