---
name: e2e-up
description: "Start the full tipg e2e Docker Compose environment (7 services: postgres, pd, tikv, pg-tikv, fs9-meta, fs9-server, pgtikv-admin). Runs build, health wait, and smoke tests (db create / db sql / db sh against TiKV pagefs). Triggers on: e2e up, start e2e, 启动 e2e, deploy e2e, e2e environment."
---

# tipg E2E Environment

Spin up a complete local integration environment: PostgreSQL-compatible pg-tikv on TiKV storage, FS9 distributed filesystem (pagefs backed by TiKV), and the pgtikv-admin cloud portal backend with the `db9` CLI.

**Repo layout:**
- **tipg repo**: `~/lab/tipg` (pg-tikv source, admin portal, e2e configs)
- **fs9 repo**: configured via `FS9_REPO_PATH` in `.env` (typically `~/fs9`)
- **E2E dir**: `~/lab/tipg/deploy/e2e/`

---

## Quick Start

```bash
cd ~/lab/tipg/deploy/e2e
./setup.sh
```

The script handles everything: `.env` generation → build → health wait → smoke tests.

---

## Services & Ports

| Service | Port | Role |
|---------|------|------|
| postgres | 5432 | Metadata DB for pgtikv-admin |
| pd | 2379 | TiKV Placement Driver |
| tikv | 20160 | TiKV storage (API v2) |
| pg-tikv | 5433 | PostgreSQL-compatible frontend |
| fs9-meta | 9998 | FS9 metadata/auth service |
| fs9-server | 9999 | FS9 HTTP API (pagefs + TiKV) |
| pgtikv-admin | 8090 | Cloud admin API + db9 CLI |

---

## Prerequisites

- Docker Desktop (with Compose v2 plugin)
- The `fs9` repository cloned locally
- `openssl` (for secret generation — pre-installed on macOS/Linux)

No Rust toolchain needed on the host — all builds happen inside Docker.

---

## Step-by-Step (Manual)

### Step 0: Navigate to e2e directory

```bash
cd ~/lab/tipg/deploy/e2e
```

### Step 1: Create .env

If `.env` doesn't exist, copy from the example and fill in paths and secrets:

```bash
cp .env.example .env
```

Required fields in `.env`:

```bash
FS9_REPO_PATH=/path/to/your/fs9   # fs9 repository root
FS9_JWT_SECRET=$(openssl rand -hex 32)   # shared JWT secret for fs9-meta + fs9-server
FS9_META_KEY=$(openssl rand -hex 16)     # admin key for fs9-meta API
POSTGRES_USER=admin
POSTGRES_PASSWORD=admin
RUST_LOG=info
```

**Important**: `FS9_JWT_SECRET` and `FS9_META_KEY` must match across services. Once set, do not change them without wiping volumes.

### Step 2: Build and start

```bash
docker compose up -d --build
```

First build takes 5–15 minutes (compiles 4 Rust workspace crates including plugins). Subsequent builds are faster due to Docker layer caching.

To start without rebuilding:
```bash
docker compose up -d
```

### Step 3: Wait for all services to be healthy

```bash
docker compose ps
```

Wait until all show `healthy`. Or poll automatically:

```bash
# One-liner to wait for pgtikv-admin (last to start)
until docker compose exec pgtikv-admin curl -sf http://localhost:8090/api/health > /dev/null 2>&1; do
  echo "waiting..."; sleep 3
done && echo "Stack ready"
```

Typical startup times from cold:
- postgres: ~5s
- pd: ~10s
- tikv: ~20s (waits for pd)
- pg-tikv: ~30s (waits for tikv)
- fs9-meta: ~15s
- fs9-server: ~40s (waits for fs9-meta + tikv)
- pgtikv-admin: ~50s (waits for postgres + pg-tikv)

### Step 4: Smoke test

```bash
# Create a database
docker compose exec -T pgtikv-admin \
  db9 --api-url http://localhost:8090/api db create --name hello

# Run a SQL query (capture the DB ID from the output above)
echo "SELECT 1 AS answer;" | \
  docker compose exec -T pgtikv-admin \
  db9 --api-url http://localhost:8090/api db sql <DB_ID>

# Write and read a file on TiKV pagefs
docker compose exec -T pgtikv-admin \
  db9 --api-url http://localhost:8090/api sh <DB_ID> \
  -c "echo hello_tikv > /test.txt && cat /test.txt"
```

Expected fs9-server log on first `db sh`:
```
[pagefs-tikv] Creating TiKV backend (txn), keyspace=Some("tipg_fs_<DB_ID>"), pd=["pd:2379"]
[pagefs] No superblock found, creating fresh filesystem
[pagefs] Created superblock and root inode
INFO fs9_server::api::handlers: Mounted pagefs from default config ns=<DB_ID> keyspace=tipg_fs_<DB_ID>
```

---

## Setup Script Options

```bash
./setup.sh                               # Full setup + smoke tests (default)
./setup.sh --skip-build                  # Start without rebuilding images
./setup.sh --smoke-only                  # Run smoke tests only (stack must already be running)
./setup.sh --smoke-only --multi-tenant-test  # Smoke + 6-test tenant isolation suite
./setup.sh --reset                       # Wipe all volumes + rebuild from scratch (DESTRUCTIVE)

# Inject a locally compiled binary instead of rebuilding the Docker image:
./setup.sh --skip-build \
  --binary=pgtikv-admin=../../cloud-admin-portal/backend/target/release/pgtikv-admin
./setup.sh --skip-build \
  --binary=pg-tikv=../../target/release/pg-tikv
# Multiple binaries in one call:
./setup.sh --skip-build \
  --binary=fs9-server=/path/to/fs9-server \
  --binary=fs9-meta=/path/to/fs9-meta
```

---

## Multi-Tenant Isolation Test

To verify that different customer accounts are properly isolated:

```bash
# Register two customers
curl -s -X POST http://localhost:8090/api/customer/register \
  -H "Content-Type: application/json" \
  -d '{"email":"alice@test.com","password":"alice_password123"}'

curl -s -X POST http://localhost:8090/api/customer/register \
  -H "Content-Type: application/json" \
  -d '{"email":"bob@test.com","password":"bob_password456"}'

# Login and create a DB for each customer
ALICE_TOKEN=$(curl -s -X POST http://localhost:8090/api/customer/login \
  -H "Content-Type: application/json" \
  -d '{"email":"alice@test.com","password":"alice_password123"}' | \
  python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")

ALICE_DB=$(curl -s -X POST http://localhost:8090/api/customer/databases \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $ALICE_TOKEN" \
  -d '{"name":"alice_db"}' | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")
```

To run `db9 sh` as a registered (non-anonymous) customer, temporarily replace the credentials file:

```bash
# Save Alice's token to db9 credentials file inside the container
docker compose exec -T pgtikv-admin bash -c "
  cat > /root/.db9/credentials << EOF
token = \"$ALICE_TOKEN\"
EOF
  db9 --api-url http://localhost:8090/api sh $ALICE_DB \
    -c 'echo alice_secret > /alice.txt && cat /alice.txt'
"
```

Isolation is enforced at the pgtikv-admin proxy layer: `GET /fs9/<db_id>/...` returns 404 if the Bearer token's customer_id does not own that database.

Verified isolation behavior:
- Alice can read/write her own DB's pagefs
- Bob can read/write his own DB's pagefs
- Alice gets HTTP 404 accessing Bob's DB
- Bob gets HTTP 404 accessing Alice's DB
- Each DB gets an isolated TiKV keyspace: `tipg_fs_<db_id>`

---

## Rebuilding Individual Services

When you change source code, rebuild only the affected service:

```bash
# Rebuild pgtikv-admin (cloud-admin-portal backend)
docker compose up -d --build --no-deps pgtikv-admin

# Rebuild pg-tikv (tipg database engine)
docker compose up -d --build --no-deps pg-tikv

# Rebuild fs9-server (after changes to ~/fs9/server/ or ~/fs9/plugins/)
docker compose up -d --build --no-deps fs9-server

# Rebuild fs9-meta (after changes to ~/fs9/meta/)
docker compose up -d --build --no-deps fs9-meta
```

---

## Useful Commands

```bash
# Tail all logs
docker compose logs -f

# Tail a specific service
docker compose logs -f fs9-server
docker compose logs -f pgtikv-admin

# Open psql directly on pg-tikv
PGPASSWORD=admin psql -h 127.0.0.1 -p 5433 -U admin -d postgres

# db9 list databases
docker compose exec -T pgtikv-admin \
  db9 --api-url http://localhost:8090/api db list

# Test fs9-meta admin API (run from within container to bypass host proxy)
docker compose exec -T pgtikv-admin bash -c \
  'curl -s -H "x-fs9-meta-key: $FS9_META_KEY" http://fs9-meta:9998/api/v1/admin/namespaces'

# Test fs9-server health
curl -s http://localhost:9999/health

# Test pgtikv-admin API
curl -s http://localhost:8090/api/health

# Direct fs9-server access (from within pgtikv-admin container, with fs9 JWT)
ALICE_JWT=$(docker compose exec -T postgres psql -U admin -d pgtikv_admin -t -c \
  "SELECT trim(password_plain) FROM tenant_credentials WHERE tenant_id='<DB_ID>' AND credential_type='fs9_token'" | tr -d ' \n')
docker compose exec -T pgtikv-admin bash -c \
  "curl -s -H 'Authorization: Bearer $ALICE_JWT' http://fs9-server:9999/<DB_ID>/api/v1/stat?path=/"
```

---

## Stop / Reset

```bash
# Stop all services (keep volumes)
docker compose down

# Stop and wipe all data volumes (DESTRUCTIVE — loses all databases)
docker compose down -v

# Full reset + rebuild from scratch
./setup.sh --reset
```

---

## Architecture: How It All Connects

```
User
 │
 ▼
db9 CLI ──── HTTP ──────► pgtikv-admin :8090
                                │
                ┌───────────────┼───────────────┐
                │               │               │
                ▼               ▼               ▼
          pg-tikv :5433   fs9-meta :9998   fs9-server :9999
                │                               │
                ▼                               ▼
            TiKV :20160 ◄─── pd :2379 ──► TiKV :20160
```

**db9 sh flow:**
1. `db9 sh <db_id>` strips `/api` suffix from api-url, calls pgtikv-admin at `/fs9/<db_id>/...`
2. pgtikv-admin verifies customer owns the DB (`/api/customer/databases` ownership check), fetches stored fs9 JWT
3. Proxies request to `http://fs9-server:9999/<db_id>/api/v1/...` (replacing Authorization header with fs9 JWT)
4. fs9-server validates JWT via fs9-meta, resolves namespace
5. On first access: auto-provisions namespace in fs9-meta + mounts pagefs plugin with TiKV keyspace `tipg_fs_<db_id>`
6. File operations persisted to TiKV

**db9 db create flow:**
1. Creates tenant record in postgres
2. Connects to pg-tikv, bootstraps admin user in the new keyspace
3. Creates namespace in fs9-meta (`POST /api/v1/admin/namespaces`)
4. Creates user in the namespace, generates fs9 JWT (`POST /api/v1/admin/tokens`)
5. Stores fs9 JWT as credential in postgres (keyed by `tenant_id + customer_id`)

**URL routing note:**
- pgtikv-admin API routes: `http://localhost:8090/api/...`
- pgtikv-admin fs9 proxy: `http://localhost:8090/fs9/<db_id>/...` (NOT under `/api/`)
- db9 CLI derives fs9 URL by stripping `/api` suffix: `http://localhost:8090` + `/fs9/<db_id>`

---

## Lessons Learned (Development History)

### fs9-meta API Paths
- All namespace/token admin operations use `/api/v1/admin/` prefix
- Public token endpoints: `/api/v1/tokens/validate` and `/api/v1/tokens/refresh`
- **No mounts API exists** in fs9-meta — fs9-server mounts pagefs directly in-process using `default_pagefs` config

### NamespaceInfo Deserialization
- fs9-meta returns `{id, name, description, created_at, user_count}` — **no `status` field**
- The `status` field in `NamespaceInfo` struct must use `#[serde(default = "default_status_active")]`
- Without this, all namespace lookups fail with "error decoding response body"

### pagefs Auto-Provisioning
- fs9-server creates each tenant's pagefs mount on first access (lazy provisioning)
- Uses `DefaultPagefsConfig` from `fs9-server.yaml` to build TiKV config
- Each tenant gets isolated keyspace: `tipg_fs_<db_id>`
- Expected log: `[pagefs-tikv] Creating TiKV backend (txn), keyspace=Some("tipg_fs_<db_id>")`

### Multi-Tenant Auth Isolation
- fs9 credentials (JWT) are stored per `(tenant_id, credential_type, customer_id)` in postgres
- pgtikv-admin's `/fs9/<db_id>/...` proxy checks that the requesting customer owns the DB
- The anonymous `db9` session (default in container) can only access DBs it created
- To access a registered customer's DB via `db9 sh`, write their token to `/root/.db9/credentials`
- Format: `token = "<128-char-hex-token>"`

### Host Proxy Interference
- If running `curl localhost:9998` from the host fails with 502, a system HTTP proxy may be intercepting
- Run API tests from inside the Docker container: `docker compose exec -T pgtikv-admin bash -c "curl ..."`
- Or use `curl --noproxy '*'` (may still fail if service not accessible from host)

### fs9 JWT Generation
- pgtikv-admin calls `POST /api/v1/admin/tokens` on fs9-meta with `user_id`
- fs9-meta signs JWT with `FS9_JWT_SECRET`
- fs9-server validates via `POST /api/v1/tokens/validate` (not local verification)
- `FS9_JWT_SECRET` env var in pgtikv-admin config is present but unused — signing is done by fs9-meta

---

### First Run: Expect Slow Context Transfer

The Docker build sends the entire repo as build context. For fs9, this includes `target/` (~17GB) if no `.dockerignore` exists. **Always ensure `~/fs9/.dockerignore` contains at minimum:**

```
target/
.git/
```

Without this, the `COPY . .` step in `Dockerfile.server-e2e` takes 2+ minutes just to transfer context. With `.dockerignore`, it drops to seconds.

### Host Port Conflicts

The e2e stack binds ports **5433** (pg-tikv), **8090** (pgtikv-admin), and **9999** (fs9-server) on the host. If you're running local dev instances of these services, Docker will fail with `address already in use`.

**Before running `./setup.sh` or `docker compose up -d`:**

```bash
# Check for conflicts
ss -tlnp | grep -E '5433|8090|9999'

# Kill local processes if needed
kill <pid>
```

The script may succeed partially (e.g., 6/7 services up) if only one port conflicts. Fix the conflict and run `docker compose up -d` again — it's idempotent.

### Container Recreate Loses db9 Credentials

db9 stores credentials at `/root/.db9/credentials` inside the pgtikv-admin container. When the container is recreated (e.g., after `docker compose build pgtikv-admin && docker compose up -d`), credentials are lost.

**After rebuilding pgtikv-admin, always re-login:**

```bash
docker compose exec pgtikv-admin db9 --api-url http://localhost:8090/api login
```

User accounts persist (stored in postgres volume), so no need to re-register.

### Missing `Dockerfile.server-e2e` in fs9

The `docker-compose.yml` references `docker/Dockerfile.server-e2e` in the fs9 repo. This file may not exist in a fresh fs9 clone. The tipg repo ships a reference copy at `deploy/e2e/Dockerfile.fs9-server`.

```bash
# If fs9 is missing the Dockerfile:
cp ~/lab/tipg/deploy/e2e/Dockerfile.fs9-server ~/fs9/docker/Dockerfile.server-e2e
```

### Docker Network DNS After Partial Restarts

If a container is recreated after port conflicts (e.g., `pgtikv-admin` fails on first run, succeeds on retry), it may lose Docker DNS resolution to other containers (error: `failed to lookup address information: Temporary failure in name resolution`).

**Fix: full restart to recreate the network cleanly:**

```bash
docker compose down && docker compose up -d
```

### Interactive db9 Commands in Docker

`db9 register` and `db9 login` require interactive TTY input (email/password prompts). They won't work with `docker compose exec -T` (no TTY).

**Use tmux or a separate terminal:**

```bash
# Interactive — works
docker compose exec pgtikv-admin db9 --api-url http://localhost:8090/api register

# Non-interactive — fails with "No such device or address"
docker compose exec -T pgtikv-admin db9 --api-url http://localhost:8090/api register
```

For CI/scripting, use the HTTP API directly with `curl` (see Multi-Tenant Isolation Test section).

### Rebuilding a Single Service (e.g., sh9 change)

When you modify source in one crate (e.g., `sh9` in fs9 repo), you only need to rebuild the affected image:

```bash
# sh9 is bundled in pgtikv-admin
docker compose build pgtikv-admin
docker compose up -d pgtikv-admin

# pg-tikv engine change
docker compose build pg-tikv
docker compose up -d pg-tikv
```

With `.dockerignore` in place, incremental rebuilds take ~30-60s (Docker layer cache reuses everything except the changed crate).

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `tikv` exits immediately | pd not ready yet | Health check handles it; wait longer |
| `db create` returns 500 | pg-tikv bootstrap failure | Check pg-tikv logs; ensure `PGTIKV_BOOTSTRAP_ADMIN_USER` matches admin portal's user |
| `db sh` returns "Namespace not found" | fs9-server can't reach fs9-meta | Check fs9-server logs; verify `FS9_META_KEY` matches in .env |
| fs9-server WARN "error decoding response body" | `NamespaceInfo` struct missing `status` field | Rebuild fs9-server (fix: add `#[serde(default)]` to `status` in `meta_client.rs`) |
| `db sh` returns 404 for file write | DB owned by different customer than active db9 session | Write correct customer token to `/root/.db9/credentials` in container |
| `db sh` shows local files instead of pagefs | fs9 JWT expired or proxy not working | Check pgtikv-admin has `FS9_SERVER_URL` env; check fs9-server logs for "Mounted pagefs" |
| `curl localhost:9998` returns 502 from host | Host HTTP proxy intercepting localhost traffic | Run from inside container: `docker compose exec -T pgtikv-admin bash -c "curl http://fs9-meta:9998/..."` |
| Build fails with cargo error | Rust toolchain inside Docker too old | `docker buildx prune -f` then retry |
| Port already in use | Local dev process occupying the port | `ss -tlnp \| grep <port>` then `kill <pid>` |
| Build context transfer takes minutes | fs9 missing `.dockerignore`, sending `target/` dir | Add `.dockerignore` with `target/` and `.git/` to fs9 repo root |
| pgtikv-admin DNS resolution failure | Container recreated outside clean network cycle | `docker compose down && docker compose up -d` |
| `db9` says "Not logged in" after rebuild | Container recreation wiped `/root/.db9/credentials` | Re-run `docker compose exec pgtikv-admin db9 ... login` |
| Volumes have stale data after code change | DB schema changed | `docker compose down -v && ./setup.sh` |
| `Dockerfile.server-e2e` not found | fs9 repo missing the e2e Dockerfile | Copy from `deploy/e2e/Dockerfile.fs9-server` to `~/fs9/docker/Dockerfile.server-e2e` |
