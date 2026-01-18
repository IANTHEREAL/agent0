# pg-tikv Cloud Admin Portal

A modern web interface for managing pg-tikv multi-tenant database instances.

## Features

- **Tenant Management**: Create, view, and disable database tenants
- **User Management**: Manage users within each tenant with secure credential handling
- **Per-Tenant Authentication**: No global portal auth - authenticate per tenant when needed
- **Modern UI**: Built with React, TypeScript, and shadcn/ui components
- **Production Ready**: Docker deployment with nginx reverse proxy

## Architecture

```
cloud-admin-portal/
├── backend/              # FastAPI Python backend
│   ├── app/
│   │   ├── api/          # REST API endpoints
│   │   ├── models/       # Pydantic models
│   │   └── services/     # Business logic
│   └── Dockerfile
├── frontend/             # React TypeScript frontend
│   ├── src/
│   │   ├── api/          # API client hooks
│   │   ├── components/   # React components
│   │   ├── hooks/        # Custom hooks
│   │   └── pages/        # Page components
│   └── Dockerfile
├── deploy/               # Deployment configuration
│   ├── docker-compose.yml
│   └── nginx/
└── scripts/              # Helper scripts
```

## Quick Start

### Development

```bash
# Start both frontend and backend with one command
./scripts/dev.sh
```

Or start them separately:

```bash
# Backend (terminal 1)
cd backend
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
uvicorn app.main:app --reload --port 8090

# Frontend (terminal 2)
cd frontend
npm install
npm run dev
```

**URLs:**
- Frontend: http://localhost:5173
- Backend API: http://localhost:8090/api
- API Docs: http://localhost:8090/api/docs

### Production

```bash
cd deploy
cp .env.example .env
docker-compose up -d
```

## Configuration

### Backend Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PGTIKV_PD_ENDPOINTS` | `127.0.0.1:2379` | TiKV PD addresses |
| `PGTIKV_PG_HOST` | `127.0.0.1` | pg-tikv server host (internal) |
| `PGTIKV_PG_PORT` | `5433` | pg-tikv server port (internal) |
| `PGTIKV_PG_PUBLIC_ENDPOINTS` | `127.0.0.1:5433` | Public pg-tikv endpoints for clients (comma-separated) |
| `PGTIKV_API_PORT` | `8080` | API server port |
| `PGTIKV_CORS_ORIGINS` | `["http://localhost:5173"]` | Allowed CORS origins |

**Multi-Endpoint Configuration for Load Balancing:**

The portal supports multiple public endpoints for load balancing scenarios:

```bash
# Single endpoint (default)
PGTIKV_PG_PUBLIC_ENDPOINTS=pg.example.com:5433

# Multiple endpoints for load balancing
PGTIKV_PG_PUBLIC_ENDPOINTS=pg1.example.com:5433,pg2.example.com:5433,pg3.example.com:5433

# With regions (configure in production deployment)
# Endpoints can be tagged with region info for geographic routing
```

The portal will display all configured endpoints to users with:
- Endpoint type (primary/replica/load_balancer)
- Priority ranking
- Connection commands for each endpoint
- Region information (if configured)

### Frontend Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `VITE_API_URL` | `/api` | Backend API base URL |

## API Endpoints

### Tenants

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/tenants` | List all tenants |
| POST | `/api/tenants` | Create new tenant |
| GET | `/api/tenants/{name}` | Get tenant details |
| DELETE | `/api/tenants/{name}` | Disable tenant |
| POST | `/api/tenants/{name}/connect` | Connect to tenant (get session) |

### Users (requires tenant session)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/tenants/{name}/users` | List users |
| POST | `/api/tenants/{name}/users` | Create user |
| DELETE | `/api/tenants/{name}/users/{user}` | Delete user |
| POST | `/api/tenants/{name}/users/{user}/password` | Reset password |

### System

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/health` | Health check |
| GET | `/api/info` | API information |

## Authentication Model

This portal uses **per-tenant authentication** instead of a global portal login:

1. **Tenant list/create/delete**: No authentication required
2. **User management**: Requires connecting to the tenant first
   - Call `POST /api/tenants/{name}/connect` with tenant admin credentials
   - Returns a session ID valid for 1 hour
   - Include session ID in `X-Tenant-Session` header for user operations

This design ensures that only users with valid tenant credentials can manage that tenant's users.

## Development

### Backend Tests

```bash
cd backend
source .venv/bin/activate
pytest -v
```

### Frontend Build

```bash
cd frontend
npm run build
npx tsc --noEmit
```

## License

Apache 2.0
