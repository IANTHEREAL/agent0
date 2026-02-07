# cloud-admin-portal Knowledge Base

Web-based admin interface and CLI for pg-tikv multi-tenant database. React frontend + Rust backend (axum).

## Commands

```bash
# Rust backend (from backend-rs/)
cd backend-rs && cargo build --release     # Build both binaries
cd backend-rs && cargo check               # Type check only
./backend-rs/target/release/pgtikv-admin   # Run server (default: SQLite, port 8090)
./backend-rs/target/release/pgtikv-ctl     # CLI tool

# Python backend (legacy, from backend/)
cd backend && uv sync                      # Install deps
cd backend && uv run uvicorn app.main:app --reload --port 8090
cd backend && uv run pytest -v             # All backend tests (10 tests)

# Frontend (from frontend/)
npm install
npm run dev
cd frontend && npx tsc --noEmit            # Type check
cd frontend && npm run build               # Production build
```

## Structure

```
cloud-admin-portal/
├── backend-rs/              # Rust backend (production)
│   ├── src/
│   │   ├── api/             # axum handlers
│   │   │   ├── mod.rs       # Router with all routes
│   │   │   ├── tenants.rs   # Tenant CRUD, connect, query, observability
│   │   │   ├── users.rs     # User management (list, create, delete, reset-password)
│   │   │   ├── system.rs    # Health + info endpoints
│   │   │   └── audit.rs     # Audit log query
│   │   ├── services/        # External service clients
│   │   │   ├── pd_client.rs # TiKV PD HTTP API (keyspace management)
│   │   │   ├── pg_client.rs # pg-tikv connection via tokio-postgres
│   │   │   └── reconciler.rs# Background stuck-tenant recovery
│   │   ├── lib.rs           # Module declarations + AppState
│   │   ├── config.rs        # Env-based config (PGTIKV_ prefix)
│   │   ├── db.rs            # sqlx AnyPool queries (SQLite/PostgreSQL)
│   │   ├── models.rs        # serde request/response/DB row types
│   │   ├── error.rs         # AppError → axum response
│   │   ├── auth.rs          # API key extractor + tenant session extractor
│   │   ├── session.rs       # In-memory RwLock<HashMap> session manager
│   │   ├── main.rs          # pgtikv-admin server binary
│   │   └── cli.rs           # pgtikv-ctl CLI binary (clap)
│   └── Cargo.toml
├── backend/                 # Python backend (legacy)
│   ├── app/
│   │   ├── api/             # FastAPI endpoints
│   │   ├── models/          # Pydantic + SQLAlchemy models
│   │   ├── services/        # Business logic
│   │   ├── main.py          # App entry
│   │   ├── config.py        # Settings
│   │   ├── auth.py          # API key dependency
│   │   └── cli.py           # pgtikv-ctl (Python, stdlib only)
│   └── tests/               # pytest tests (10 passing)
├── frontend/                # React TypeScript frontend
│   └── src/
│       ├── api/             # API client + React Query hooks
│       ├── components/      # UI components (shadcn/ui based)
│       ├── pages/           # Page components
│       ├── hooks/           # Custom hooks (useTenantSession)
│       └── types/           # TypeScript types
├── deploy/                  # Docker Compose + nginx
└── scripts/                 # dev.sh, build.sh
```

## Where to Look

| Task | Location |
|------|----------|
| Add API endpoint | `backend-rs/src/api/mod.rs` (route) + corresponding handler file |
| Add tenant handler | `backend-rs/src/api/tenants.rs` |
| Add user handler | `backend-rs/src/api/users.rs` |
| Add DB query | `backend-rs/src/db.rs` |
| Add request/response type | `backend-rs/src/models.rs` |
| Change config | `backend-rs/src/config.rs` |
| Change PD client | `backend-rs/src/services/pd_client.rs` |
| Change pg-tikv client | `backend-rs/src/services/pg_client.rs` |
| Change reconciler | `backend-rs/src/services/reconciler.rs` |
| Change CLI commands | `backend-rs/src/cli.rs` |
| Change frontend types | `frontend/src/types/index.ts` |
| Change frontend API hooks | `frontend/src/api/tenants.ts` |

## Code Style

### Rust (Backend)

- **Framework**: axum 0.7 with tower-http CORS
- **Database**: sqlx 0.8 with AnyPool (SQLite + PostgreSQL)
- **Async**: tokio runtime, all handlers are `async`
- **Auth**: `ApiKeyAuth` axum extractor (FromRequestParts), `TenantSessionExtractor` manual from HeaderMap
- **Errors**: `AppError` with `IntoResponse` impl, `From<sqlx::Error>` and `From<reqwest::Error>`
- **SQL**: Use `$1, $2, ...` placeholders; `adapt_sql()` converts to `?` for SQLite
- **IDs**: TEXT primary keys (tenant IDs = 12-char alphanumeric, credentials/audit = UUID v4)
- **Dates**: ISO 8601 TEXT strings (not native datetime columns)

```rust
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
- **State**: React Query for server state, useState for UI state
- **UI**: shadcn/ui components from `@/components/ui/`
- **Path alias**: `@/*` → `./src/*`

## Testing

### Python Backend Tests (Legacy)

```bash
cd backend && uv run pytest -v                                              # All 10 tests
cd backend && uv run pytest tests/test_tenants.py::TestListTenants -v       # Single class
```

### Frontend

```bash
cd frontend && npx tsc --noEmit    # Type check
cd frontend && npm run build       # Production build
```

### Rust Backend

```bash
cd backend-rs && cargo check       # Type check
cd backend-rs && cargo build       # Debug build
```

## Key Patterns

### Tenant State Machine

```
CREATING → ACTIVE (success) or CREATE_FAILED (failure)
ACTIVE → DISABLING → DISABLED (delete/remove)
ACTIVE → SUSPENDED (future)

Reconciler: CREATING(>10min) → check PD → ACTIVE or CREATE_FAILED
Reconciler: DISABLING(>10min) → DISABLED
```

### Tenant Session Flow

1. `POST /api/tenants/{id}/connect` with `{ "admin_user", "admin_password" }`
2. Returns `{ "session_id", "expires_at" }`
3. Include `X-Tenant-Session: <session_id>` header for user management APIs
4. Sessions expire after 1 hour (in-memory, lost on restart)

### Observability Flow

1. `POST /api/tenants/{id}/observability/bootstrap` with admin credentials
2. Creates `_pgtikv_sys_observer` user in pg-tikv, stores credential in `tenant_credentials` table
3. `GET /api/tenants/{id}/observability` uses stored observer credentials to query pg-tikv

### Service Layer (Rust)

- `PdClient`: TiKV Placement Driver HTTP API (keyspace CRUD, health)
- `PgClient`: pg-tikv connection via `tokio-postgres` (user management, SQL execution, observability)
- `Reconciler`: Background task recovering stuck tenants

### Two Binaries

| Binary | Entry | Purpose |
|--------|-------|---------|
| `pgtikv-admin` | `src/main.rs` | HTTP API server (axum, tokio) |
| `pgtikv-ctl` | `src/cli.rs` | CLI tool (clap, reqwest) |

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `PGTIKV_PD_ENDPOINTS` | `127.0.0.1:2379` | TiKV PD addresses |
| `PGTIKV_PG_HOST` | `127.0.0.1` | pg-tikv server host |
| `PGTIKV_PG_PORT` | `5433` | pg-tikv server port |
| `PGTIKV_PG_PUBLIC_ENDPOINTS` | `127.0.0.1:5433` | Public endpoints shown to users |
| `PGTIKV_API_PORT` | `8090` | Backend API port |
| `PGTIKV_API_HOST` | `0.0.0.0` | Backend bind address |
| `PGTIKV_DATABASE_URL` | `sqlite://data/portal.db?mode=rwc` | Metadata DB (sqlite:// or postgres://) |
| `PGTIKV_API_KEYS` | (empty) | Comma-separated API keys |
| `PGTIKV_CORS_ORIGINS` | `http://localhost:5173,http://localhost:3000` | CORS origins |
| `PGTIKV_RECONCILER_ENABLED` | `true` | Enable background reconciler |
| `PGTIKV_RECONCILER_INTERVAL_SECONDS` | `300` | Reconciler cycle interval |
| `PGTIKV_SESSION_TTL_HOURS` | `1` | Session expiry |
| `PGTIKV_API_URL` | `http://localhost:8090/api` | CLI: API base URL |
| `PGTIKV_API_KEY` | (empty) | CLI: API key |
| `VITE_API_URL` | `/api` | Frontend API base URL |

## Anti-Patterns

- **Session persistence**: In-memory sessions lost on restart. For multi-instance production, needs shared session store.
- **Keyspace deletion**: TiKV keyspaces can only be DISABLED, not deleted. Data remains.
- **Type suppression**: Never use `as any`, `@ts-ignore` in frontend or `unsafe` in Rust.
- **No `unwrap()`/`expect()` in production code paths** — use `?` or proper error handling.
- **No sqlx compile-time macros** (`query!`, `query_as!`) — incompatible with AnyPool.
- **Python LSP errors**: All `Column[str]` errors in Python files are pre-existing SQLAlchemy/pyright noise — do NOT fix.

## URLs (Development)

- Frontend: http://localhost:5173
- Backend API: http://localhost:8090/api
