# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

The cloud-admin-portal is a web-based administration interface for managing pg-tikv multi-tenant database instances. It provides tenant (TiKV keyspace) management and user administration through a modern React frontend and FastAPI backend.

## Development Commands

### Development Environment

```bash
# Start both frontend and backend together (recommended)
./scripts/dev.sh

# Or start them separately:

# Backend only (from backend directory)
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
uvicorn app.main:app --reload --port 8090

# Frontend only (from frontend directory)
npm install
npm run dev
```

**Development URLs:**
- Frontend: http://localhost:5173
- Backend API: http://localhost:8090/api
- API Docs: http://localhost:8090/api/docs

### Testing

```bash
# Backend tests (from backend directory)
source .venv/bin/activate
pytest -v

# Frontend linting (from frontend directory)
npm run lint

# Frontend type checking
npx tsc --noEmit
```

### Building and Deployment

```bash
# Build frontend for production (from frontend directory)
npm run build

# Build Docker images for production
./scripts/build.sh

# Deploy with Docker Compose (from deploy directory)
cp .env.example .env
# Edit .env with your configuration
docker-compose up -d
```

## Architecture

### High-Level Architecture

```
┌─────────────┐        ┌──────────────┐        ┌──────────┐
│   Browser   │───────▶│  React SPA   │───────▶│  FastAPI │
│             │        │  (Frontend)  │        │ (Backend)│
└─────────────┘        └──────────────┘        └────┬─────┘
                                                     │
                       ┌─────────────────────────────┴────┐
                       │                                  │
                       ▼                                  ▼
                 ┌──────────┐                      ┌──────────┐
                 │  TiKV PD │                      │ pg-tikv  │
                 │(Keyspace)│                      │  Server  │
                 └──────────┘                      └──────────┘
```

### Backend Architecture (`backend/app/`)

**Core Components:**

- `main.py`: FastAPI application entry point with CORS middleware and lifespan management
- `config.py`: Pydantic settings loaded from environment variables with `PGTIKV_` prefix
- `session.py`: In-memory tenant session management (should be replaced with Redis for production)

**API Layer (`api/`):**
- `tenants.py`: Tenant CRUD operations and connection endpoints
- `users.py`: User management within tenants (requires tenant session)
- `system.py`: Health checks and API information
- All routes mounted under `/api` prefix

**Service Layer (`services/`):**
- `pd_client.py`: TiKV Placement Driver HTTP API client for keyspace management
- `pg_client.py`: pg-tikv PostgreSQL client using `psql` subprocess for SQL execution

**Models (`models/`):**
- Pydantic models for request/response validation

### Frontend Architecture (`frontend/src/`)

**Structure:**
- `api/`: API client functions using `fetch` with React Query hooks
- `components/`: Reusable UI components (built with shadcn/ui and Radix UI)
- `pages/`: Page-level components (TenantsPage, TenantDetailPage)
- `hooks/`: Custom React hooks (useTenantSession)
- `types/`: TypeScript type definitions

**Key Technologies:**
- React 18 with TypeScript
- React Router for navigation
- TanStack React Query for data fetching and caching
- React Hook Form with Zod validation
- Tailwind CSS with shadcn/ui components
- Vite for build tooling

### Authentication Model

The portal uses **per-tenant authentication** instead of global portal auth:

1. **Public Operations**: Tenant list/create/delete require no authentication
2. **Authenticated Operations**: User management requires a tenant session
   - Call `POST /api/tenants/{name}/connect` with admin credentials
   - Returns session ID stored in `sessionStorage` (frontend) and in-memory dict (backend)
   - Session ID included in `X-Tenant-Session` header for subsequent requests
   - Sessions expire after 1 hour (configurable via `PGTIKV_SESSION_TTL_HOURS`)

**Session Flow:**
```
Frontend                Backend                     pg-tikv
   │                       │                           │
   ├─ POST /connect ──────▶│                           │
   │  {admin_user,pass}    │                           │
   │                       ├─ psql connect test ──────▶│
   │                       │◀─ success ────────────────┤
   │◀─ {session_id} ───────┤                           │
   │                       │ (store session in memory) │
   │                       │                           │
   ├─ GET /users ─────────▶│                           │
   │  X-Tenant-Session: id │                           │
   │                       ├─ validate session         │
   │                       ├─ psql query ─────────────▶│
   │◀─ [users] ────────────┤                           │
```

### Multi-Tenancy Implementation

Each tenant maps to a TiKV keyspace:

1. **Tenant Creation**: Creates a keyspace via PD HTTP API (`POST /pd/api/v2/keyspaces`)
2. **Tenant Isolation**: Each keyspace provides physical isolation in TiKV
3. **User Connection**: Uses `tenant.user` format for PostgreSQL connections (e.g., `myapp.admin`)

### Configuration

**Backend Environment Variables (prefix: `PGTIKV_`):**
- `PD_ENDPOINTS`: TiKV PD addresses (default: `127.0.0.1:2379`)
- `PG_HOST`: pg-tikv server host for internal backend connections (default: `127.0.0.1`)
- `PG_PORT`: pg-tikv server port for internal backend connections (default: `5433`)
- `PG_PUBLIC_ENDPOINTS`: Public pg-tikv endpoints for end-user connections, comma-separated (default: `127.0.0.1:5433`)
  - Single endpoint: `pg.example.com:5433`
  - Multiple endpoints for load balancing: `pg1.example.com:5433,pg2.example.com:5433,pg3.example.com:5433`
- `API_PORT`: Backend API port (default: `8080`)
- `CORS_ORIGINS`: Allowed CORS origins (default: `["http://localhost:5173", "http://localhost:3000"]`)
- `SESSION_TTL_HOURS`: Session validity period (default: `1`)
- `DEBUG`: Enable debug mode (default: `false`)

**Public Endpoints Model:**

The portal supports an extensible endpoint model for displaying connection information to users:

- **Endpoint**: Structured model with host, port, type, region, priority, description, and enabled status
- **Endpoint Types**: `primary`, `replica`, `load_balancer`
- **Priority**: Higher values are displayed first (default: 100, decreasing by 10 for each additional endpoint)
- **Region**: Optional geographic region/availability zone for routing
- **Load Balancing**: Multiple endpoints are automatically tagged as `load_balancer` type

The frontend displays all configured endpoints with:
- Visual type badges (primary/replica/load_balancer)
- Individual connection commands for each endpoint
- Priority-based ordering
- Copy-to-clipboard functionality

**Frontend Environment Variables:**
- `VITE_API_URL`: Backend API base URL (default: `/api`)

### Production Deployment

The `deploy/` directory contains Docker Compose configuration with:

1. **nginx**: Reverse proxy serving frontend static files and proxying `/api` to backend
2. **backend**: FastAPI application in Python container
3. **frontend-builder**: Build stage container that outputs static files to shared volume

The frontend is built once and served by nginx, while the backend runs as a separate service.

## Development Patterns

### Adding a New API Endpoint

1. Define Pydantic models in `backend/app/models/`
2. Implement endpoint handler in appropriate router (`api/tenants.py` or `api/users.py`)
3. Add frontend API client function in `frontend/src/api/`
4. Create React Query hook for data fetching
5. Use the hook in UI components

### Adding a New Frontend Page

1. Create page component in `frontend/src/pages/`
2. Add route in `frontend/src/App.tsx`
3. Create necessary API hooks in `frontend/src/api/`
4. Build UI using shadcn/ui components from `frontend/src/components/ui/`

### Working with Tenant Sessions

Always validate tenant sessions in user management endpoints:

```python
from ..session import session_manager

async def some_user_endpoint(
    tenant_name: str,
    session_id: str = Header(None, alias="X-Tenant-Session")
):
    session = session_manager.validate_session(session_id, tenant_name)
    if not session:
        raise HTTPException(401, "Invalid or expired session")
    # Use session.admin_user and session.admin_password for pg-tikv operations
```

## Important Notes

- The session manager is in-memory and will lose sessions on restart. For production with multiple backend instances, replace with Redis.
- The `pg_client.py` uses `psql` subprocess instead of psycopg2 to avoid dependency issues with pg-tikv's custom authentication.
- Frontend uses `sessionStorage` (not `localStorage`) for session IDs, so sessions are tab-specific and cleared when tab closes.
- TiKV keyspaces cannot be fully deleted, only disabled. The "delete tenant" operation sets state to `DISABLED`.
- All tenant names must be valid TiKV keyspace names (alphanumeric and underscore).
