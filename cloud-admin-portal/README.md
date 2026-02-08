# pg-tikv Cloud Admin Portal

A modern web interface and CLI for managing pg-tikv multi-tenant database instances.

## Features

- **Tenant Management**: Create, view, disable, and remove database tenants with state machine (CREATING → ACTIVE → DISABLING → DISABLED)
- **User Management**: Manage users within each tenant with secure credential handling
- **Per-Tenant Observability**: Built-in metrics and query sampling via bootstrapped observer accounts
- **API Key Authentication**: Optional `X-API-Key` header for all endpoints
- **Background Reconciler**: Automatically recovers stuck CREATING/DISABLING tenants
- **Audit Logging**: All tenant/user operations logged with operator, timestamps, and metadata
- **CLI Tool**: `pgtikv-ctl` for command-line tenant and user management
- **Dual Database**: SQLite for development, PostgreSQL for production
- **Modern UI**: React frontend with shadcn/ui components

## Architecture

```
cloud-admin-portal/
├── backend/                 # Rust backend (axum + sqlx)
│   ├── src/
│   │   ├── api/             # axum handlers (tenants, users, system, audit)
│   │   ├── services/        # PD client, pg-tikv client, reconciler
│   │   ├── config.rs        # Env-based configuration
│   │   ├── db.rs            # sqlx AnyPool (SQLite/PostgreSQL)
│   │   ├── auth.rs          # API key + tenant session extractors
│   │   ├── session.rs       # In-memory session manager
│   │   ├── main.rs          # pgtikv-admin server binary
│   │   └── cli.rs           # pgtikv-ctl CLI binary
│   └── Cargo.toml
├── frontend/                # React TypeScript frontend
│   └── src/
├── deploy/                  # Docker Compose + nginx
│   ├── docker-compose.yml   # Production deployment
│   ├── docker-compose.dev.yml # Development (backend in Docker)
│   ├── nginx/nginx.conf     # Reverse proxy config
│   └── .env.example         # Environment template
└── scripts/                 # dev.sh, build.sh, deploy.sh
```

## Quick Start

### Build

```bash
cd backend
cargo build --release
```

Produces two binaries in `target/release/`:
- **`pgtikv-admin`** — HTTP API server
- **`pgtikv-ctl`** — CLI tool

### Run Server

```bash
# Minimal (SQLite, no auth)
./target/release/pgtikv-admin

# Production
PGTIKV_DATABASE_URL=postgres://user:pass@localhost/portal \
PGTIKV_PD_ENDPOINTS=10.0.0.1:2379 \
PGTIKV_API_KEYS=my-secret-key \
./target/release/pgtikv-admin
```

### CLI Usage

```bash
# List tenants
pgtikv-ctl tenants list

# Create tenant
pgtikv-ctl tenants create --admin-user admin

# Get tenant details
pgtikv-ctl tenants get <tenant_id>

# Connect to tenant (get session for user management)
pgtikv-ctl connect <tenant_id> --admin-user admin --admin-password secret

# List users (requires session)
pgtikv-ctl users list <tenant_id> --session <session_id>

# Create user
pgtikv-ctl users create <tenant_id> --username myuser --session <session_id>

# Health check
pgtikv-ctl health

# JSON output
pgtikv-ctl --json tenants list
```

### Frontend

```bash
cd frontend
npm install
npm run dev
```

**URLs:**
- Frontend: http://localhost:5173
- Backend API: http://localhost:8090/api

## Configuration

### Backend Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PGTIKV_PD_ENDPOINTS` | `127.0.0.1:2379` | TiKV PD addresses |
| `PGTIKV_PG_HOST` | `127.0.0.1` | pg-tikv server host (internal) |
| `PGTIKV_PG_PORT` | `5433` | pg-tikv server port (internal) |
| `PGTIKV_PG_PUBLIC_ENDPOINTS` | `127.0.0.1:5433` | Public pg-tikv endpoints for clients (comma-separated) |
| `PGTIKV_API_PORT` | `8090` | API server port |
| `PGTIKV_API_HOST` | `0.0.0.0` | API server bind address |
| `PGTIKV_DATABASE_URL` | `sqlite://data/portal.db?mode=rwc` | Metadata database (SQLite or PostgreSQL) |
| `PGTIKV_API_KEYS` | (empty) | Comma-separated API keys (empty = no auth) |
| `PGTIKV_CORS_ORIGINS` | `http://localhost:5173,http://localhost:3000` | Allowed CORS origins |
| `PGTIKV_RECONCILER_ENABLED` | `true` | Enable background reconciler |
| `PGTIKV_RECONCILER_INTERVAL_SECONDS` | `300` | Reconciler cycle interval |
| `PGTIKV_SESSION_TTL_HOURS` | `1` | Tenant session expiry |

### CLI Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PGTIKV_API_URL` | `http://localhost:8090/api` | API base URL |
| `PGTIKV_API_KEY` | (empty) | API key for authentication |

### Frontend Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `VITE_API_URL` | `/api` | Backend API base URL |

## API Endpoints

### Tenants

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/tenants` | List tenants (paginated, filterable) |
| POST | `/api/tenants` | Create new tenant |
| GET | `/api/tenants/{id}` | Get tenant details with endpoints |
| PUT | `/api/tenants/{id}` | Update tenant metadata (notes, tags) |
| DELETE | `/api/tenants/{id}` | Disable tenant |
| POST | `/api/tenants/{id}/remove` | Remove tenant (same as delete) |
| POST | `/api/tenants/{id}/connect` | Get tenant session |
| POST | `/api/tenants/{id}/query` | Execute SQL (requires session) |
| GET | `/api/tenants/{id}/observability` | Get metrics + query samples |
| POST | `/api/tenants/{id}/observability/bootstrap` | Bootstrap observer account |

### Users (requires `X-Tenant-Session` header)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/tenants/{id}/users` | List users |
| POST | `/api/tenants/{id}/users` | Create user |
| DELETE | `/api/tenants/{id}/users/{username}` | Delete user |
| POST | `/api/tenants/{id}/users/{username}/password` | Reset password |

### System

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/health` | Health check (includes PD status) |
| GET | `/api/info` | API version info |
| GET | `/api/audit-logs` | Query audit logs (filterable) |

## Authentication

### API Key Authentication

When `PGTIKV_API_KEYS` is set, all endpoints require an `X-API-Key` header:

```bash
curl -H "X-API-Key: my-secret" http://localhost:8090/api/tenants
```

### Per-Tenant Session

User management requires connecting to the tenant first:

1. `POST /api/tenants/{id}/connect` with `{ "admin_user": "admin", "admin_password": "..." }`
2. Returns `{ "session_id": "...", "expires_at": "..." }`
3. Include `X-Tenant-Session: <session_id>` header for user management APIs
4. Sessions expire after 1 hour (configurable via `PGTIKV_SESSION_TTL_HOURS`)

## Tenant State Machine

```
CREATING → ACTIVE (success) or CREATE_FAILED (failure)
ACTIVE → DISABLING → DISABLED (delete/remove)
ACTIVE → SUSPENDED (future)

Reconciler: CREATING(>10min) → check PD → ACTIVE or CREATE_FAILED
Reconciler: DISABLING(>10min) → DISABLED
```

## Development

### Backend (Rust)

```bash
cd backend
cargo check                         # Type check
cargo build --release               # Build both binaries (pgtikv-admin + pgtikv-ctl)
```

### Frontend

```bash
cd frontend
npm install
npx tsc --noEmit                    # Type check
npm run dev                         # Development server (http://localhost:5173)
npm run build                       # Production build
```

### Development Mode

Start frontend dev server with API proxy to backend:

```bash
# Terminal 1: Start backend
cd backend
cargo run

# Terminal 2: Start frontend (proxies /api to localhost:8090)
cd frontend
npm run dev
```

The Vite dev server proxies `/api` requests to the backend at `http://localhost:8090` (configurable via `VITE_BACKEND_URL`).

## Deployment

### Docker Compose (Production)

The `deploy/` directory provides a production-ready Docker Compose setup with nginx as reverse proxy.

```bash
# 1. Configure environment
cd deploy
cp .env.example .env
# Edit .env with your configuration (PD endpoints, pg-tikv host/port, etc.)

# 2. Build and start services
../scripts/build.sh                 # Build frontend + Docker images
docker-compose up -d                # Start all services
```

**Services:**

| Service | Description | Port |
|---------|-------------|------|
| `nginx` | Reverse proxy, serves frontend static files, proxies `/api` to backend | 80 (443 for HTTPS) |
| `backend` | Rust API server (`pgtikv-admin`) | 8080 (internal) |
| `frontend-builder` | Build stage that outputs static files to shared volume | - |

**Architecture:**
- nginx serves the pre-built React SPA from a shared Docker volume
- API requests to `/api/*` are proxied to the backend container
- SPA routing is handled via `try_files $uri $uri/ /index.html`
- Static assets (JS, CSS, images, fonts) are cached for 1 year with immutable headers

### Using deploy.sh

The `scripts/deploy.sh` script provides common Docker Compose operations:

```bash
./scripts/deploy.sh start           # Start services (default)
./scripts/deploy.sh stop            # Stop services
./scripts/deploy.sh restart         # Restart services
./scripts/deploy.sh status          # Show service status
./scripts/deploy.sh logs            # Tail logs (all services)
./scripts/deploy.sh logs backend    # Tail logs (specific service)
./scripts/deploy.sh build           # Build + start
```

### HTTPS Configuration

To enable HTTPS, uncomment the SSL server block in `deploy/nginx/nginx.conf` and mount your certificates:

```yaml
# In docker-compose.yml, uncomment:
volumes:
  - ./ssl:/etc/nginx/ssl:ro
```

Place `cert.pem` and `key.pem` in `deploy/ssl/`.

### Environment Variables (.env.example)

```bash
PD_ENDPOINTS=127.0.0.1:2379          # TiKV PD addresses
PG_HOST=127.0.0.1                    # pg-tikv host (internal backend connections)
PG_PORT=5433                         # pg-tikv port (internal backend connections)
PG_PUBLIC_ENDPOINTS=pg.example.com:5433  # Public endpoints for end-user connections
API_PORT=8080                        # Backend API port
```

## License

Apache 2.0
