# pg-tikv Cloud Admin Portal

A modern web interface for managing pg-tikv multi-tenant database instances.

## Features

- **JWT Authentication**: Secure API access with token-based authentication
- **Tenant Management**: Create, view, and disable database tenants
- **User Management**: Manage users within each tenant with secure credential handling
- **Modern UI**: Built with React, TypeScript, and shadcn/ui components
- **Production Ready**: Docker deployment with nginx reverse proxy

## Architecture

```
cloud-admin-portal/
├── backend/              # FastAPI Python backend
│   ├── app/
│   │   ├── api/          # REST API endpoints
│   │   ├── auth/         # JWT authentication
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
# Start both frontend and backend in development mode
./scripts/dev.sh

# Or start them separately:

# Backend (terminal 1)
cd backend
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
uvicorn app.main:app --reload --port 8080

# Frontend (terminal 2)
cd frontend
npm install
npm run dev
```

**URLs:**
- Frontend: http://localhost:5173
- Backend API: http://localhost:8080/api
- API Docs: http://localhost:8080/api/docs

### Production

```bash
# Build and start with Docker Compose
cd deploy
cp .env.example .env
# Edit .env with your configuration
docker-compose up -d
```

## Configuration

### Backend Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PGTIKV_PD_ENDPOINTS` | `127.0.0.1:2379` | TiKV PD addresses |
| `PGTIKV_PG_HOST` | `127.0.0.1` | pg-tikv server host |
| `PGTIKV_PG_PORT` | `5433` | pg-tikv server port |
| `PGTIKV_ADMIN_PASSWORD` | `admin` | Admin login password |
| `PGTIKV_JWT_SECRET` | (auto) | JWT signing secret |
| `PGTIKV_CORS_ORIGINS` | `["http://localhost:5173"]` | Allowed CORS origins |

### Frontend Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `VITE_API_URL` | `/api` | Backend API base URL |

## API Endpoints

### Authentication

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/auth/login` | Login with password |
| GET | `/api/auth/me` | Get current user info |

### Tenants

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/tenants` | List all tenants |
| POST | `/api/tenants` | Create new tenant |
| GET | `/api/tenants/{name}` | Get tenant details |
| DELETE | `/api/tenants/{name}` | Disable tenant |
| POST | `/api/tenants/{name}/connect` | Connect to tenant |

### Users

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

## Security

- All API endpoints (except `/api/auth/login`, `/api/health`, `/api/info`) require JWT authentication
- User management endpoints additionally require a tenant session (via `X-Tenant-Session` header)
- Passwords are never passed in URL query parameters
- CORS is restricted to configured origins only

## Development

### Quick Start (Recommended)

```bash
# Start both backend and frontend with one command
./scripts/dev.sh
```

This will:
- Create Python venv and install dependencies
- Install npm packages
- Start backend on port 8090
- Start frontend on port 5173

### Backend Tests

```bash
cd backend
source .venv/bin/activate
pytest -v
```

**Test Coverage**: 16 tests covering auth, system, and tenant endpoints.

### Frontend Build

```bash
cd frontend
npm run build     # Production build
npm run lint      # Lint check
npx tsc --noEmit  # Type check
```

## License

Apache 2.0
