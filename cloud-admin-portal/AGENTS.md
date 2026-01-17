# cloud-admin-portal Knowledge Base

Web-based admin interface for pg-tikv multi-tenant database. React frontend + FastAPI backend.

## Commands

```bash
# Development (starts both frontend + backend)
./scripts/dev.sh

# Backend only (from backend/)
uv sync                                    # Install deps
uv run uvicorn app.main:app --reload --port 8090

# Frontend only (from frontend/)
npm install
npm run dev

# Testing
cd backend && uv run pytest -v             # All backend tests
cd backend && uv run pytest tests/test_tenants.py -v  # Single file
cd backend && uv run pytest -k "test_list" # By name pattern

# Frontend
cd frontend && npm run lint                # ESLint
cd frontend && npx tsc --noEmit            # Type check
cd frontend && npm run build               # Production build
```

## Structure

```
cloud-admin-portal/
├── backend/                 # FastAPI Python backend
│   ├── app/
│   │   ├── api/             # REST endpoints (tenants, users, system, audit)
│   │   ├── models/          # Pydantic models
│   │   ├── services/        # Business logic (pd_client, pg_client)
│   │   ├── main.py          # App entry, CORS, lifespan
│   │   ├── config.py        # Pydantic settings (PGTIKV_ prefix)
│   │   └── session.py       # In-memory tenant session manager
│   └── tests/               # pytest tests
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

## Code Style

### Python (Backend)

- **Imports**: stdlib → third-party → local (relative with `.`)
- **Naming**: snake_case functions/vars, PascalCase classes
- **Types**: Use Pydantic models for request/response, type hints required
- **Async**: FastAPI endpoints are `async def`
- **Dependencies**: Use `Depends()` for DI (settings, db, clients)
- **Errors**: Raise `HTTPException` with proper status codes

```python
# Example pattern
from fastapi import APIRouter, Depends, HTTPException, status
from ..config import get_settings, Settings
from ..models import TenantCreate, TenantResponse

router = APIRouter()

@router.post("", response_model=TenantResponse, status_code=status.HTTP_201_CREATED)
async def create_tenant(
    request: TenantCreate,
    settings: Settings = Depends(get_settings),
):
    if error_condition:
        raise HTTPException(status_code=status.HTTP_409_CONFLICT, detail="message")
    return TenantResponse(...)
```

### TypeScript (Frontend)

- **Imports**: React → third-party → `@/` aliases → relative
- **Components**: Function components with explicit typing
- **State**: React Query for server state, useState for UI state
- **Forms**: react-hook-form + zod validation
- **UI**: shadcn/ui components from `@/components/ui/`
- **Path alias**: `@/*` → `./src/*`

```typescript
// Example pattern
import { useState } from "react"
import { useMutation, useQueryClient } from "@tanstack/react-query"
import { Button } from "@/components/ui/button"
import { apiRequest } from "@/api/client"
import type { Tenant } from "@/types"

export function MyComponent() {
  const [state, setState] = useState<string | null>(null)
  const { data, isLoading } = useTenants()
  // ...
}
```

## Testing

### Backend Tests

- Located in `backend/tests/`
- Use `pytest` with `pytest-asyncio`
- Fixtures in `conftest.py` - `client` fixture provides FastAPI TestClient
- Test classes grouped by endpoint: `TestListTenants`, `TestCreateTenant`

```python
class TestListTenants:
    def test_list_tenants(self, client):
        response = client.get("/api/tenants")
        assert response.status_code == 200
```

### Running Single Test

```bash
cd backend
uv run pytest tests/test_tenants.py::TestListTenants::test_list_tenants -v
```

## Key Patterns

### Tenant Session Flow

1. `POST /api/tenants/{name}/connect` with admin credentials
2. Returns `session_id` (stored in `sessionStorage`)
3. Include `X-Tenant-Session` header for user management APIs
4. Sessions expire after 1 hour (in-memory, lost on restart)

### API Client Pattern (Frontend)

```typescript
// api/client.ts - base fetcher with session handling
export async function apiRequest<T>(endpoint: string, options?: RequestInit): Promise<T>

// api/tenants.ts - React Query hooks
export function useTenants() {
  return useQuery({ queryKey: ["tenants"], queryFn: fetchTenants })
}
```

### Service Layer (Backend)

- `PDClient`: TiKV Placement Driver HTTP API (keyspace management)
- `PgTikvClient`: PostgreSQL client via `psql` subprocess
- `AuditService`: Logs tenant operations to database

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `PGTIKV_PD_ENDPOINTS` | `127.0.0.1:2379` | TiKV PD addresses |
| `PGTIKV_PG_HOST` | `127.0.0.1` | pg-tikv server host |
| `PGTIKV_PG_PORT` | `5433` | pg-tikv server port |
| `PGTIKV_API_PORT` | `8080` | Backend API port |
| `VITE_API_URL` | `/api` | Frontend API base URL |

## Anti-Patterns

- **Session persistence**: Current in-memory sessions lost on restart. For production multi-instance, use Redis.
- **psql subprocess**: `pg_client.py` uses subprocess, not psycopg2, due to pg-tikv auth quirks.
- **Keyspace deletion**: TiKV keyspaces can only be DISABLED, not deleted. Data remains.
- **Empty catch blocks**: Never `except: pass` - always handle or re-raise.
- **Type suppression**: Never use `# type: ignore` or `as any` without comment.

## URLs (Development)

- Frontend: http://localhost:5173
- Backend API: http://localhost:8090/api
- API Docs: http://localhost:8090/api/docs
