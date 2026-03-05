---
name: e2e-up
description: "Legacy notes for the retired 7-service db9 e2e stack. The current in-repo setup uses deploy/e2e/setup.sh for a 6-service stack without db9-admin."
---

# db9 E2E Environment

> Legacy note: the in-repo `cloud-admin-portal` / `db9-admin` target has been removed. For the current repo, use `deploy/e2e/setup.sh`, which starts the 6-service stack (`postgres`, `pd`, `tikv`, `db9-server`, `fs9-meta`, `fs9-server`) and runs direct SQL + fs9 smoke tests. The remainder of this document describes the retired setup.

Spin up a complete local integration environment: PostgreSQL-compatible db9-server on TiKV storage, FS9 distributed filesystem (pagefs backed by TiKV), and the db9-admin cloud portal backend with the `db9` CLI.

**Repo layout:**
- **db9 repo**: `~/lab/db9` (db9-server source, admin portal, e2e configs)
- **fs9 repo**: configured via `FS9_REPO_PATH` in `.env` (typically `~/fs9`)
- **E2E dir**: `~/lab/db9/deploy/e2e/`

---

## Quick Start

```bash
cd ~/lab/db9/deploy/e2e
./setup.sh
```

The script handles everything: `.env` generation → build → health wait → smoke tests.

---

## Services & Ports

| Service | Port | Role |
|---------|------|------|
| postgres | 5432 | Metadata DB for db9-admin |
| pd | 2379 | TiKV Placement Driver |
| tikv | 20160 | TiKV storage (API v2) |
| db9-server | 5433 | PostgreSQL-compatible frontend |
| fs9-meta | 9998 | FS9 metadata/auth service |
| fs9-server | 9999 | FS9 HTTP API (pagefs + TiKV) |
| db9-admin | 8090 | Cloud admin API + db9 CLI |

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
cd ~/lab/db9/deploy/e2e
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
# One-liner to wait for db9-admin (last to start)
until docker compose exec db9-admin curl -sf http://localhost:8090/api/health > /dev/null 2>&1; do
  echo "waiting..."; sleep 3
done && echo "Stack ready"
```

Typical startup times from cold:
- postgres: ~5s
- pd: ~10s
- tikv: ~20s (waits for pd)
- db9-server: ~30s (waits for tikv)
- fs9-meta: ~15s
- fs9-server: ~40s (waits for fs9-meta + tikv)
- db9-admin: ~50s (waits for postgres + db9-server)

### Step 4: Smoke test

```bash
# Create a database
docker compose exec -T db9-admin \
  db9 --api-url http://localhost:8090/api db create --name hello

# Run a SQL query (capture the DB ID from the output above)
echo "SELECT 1 AS answer;" | \
  docker compose exec -T db9-admin \
  db9 --api-url http://localhost:8090/api db sql <DB_ID>

# Write and read a file on TiKV pagefs
docker compose exec -T db9-admin \
  db9 --api-url http://localhost:8090/api sh <DB_ID> \
  -c "echo hello_tikv > /test.txt && cat /test.txt"
```

Expected fs9-server log on first `db sh`:
```
[pagefs-tikv] Creating TiKV backend (txn), keyspace=Some("db9_fs_<DB_ID>"), pd=["pd:2379"]
[pagefs] No superblock found, creating fresh filesystem
[pagefs] Created superblock and root inode
INFO fs9_server::api::handlers: Mounted pagefs from default config ns=<DB_ID> keyspace=db9_fs_<DB_ID>
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
  --binary=db9-admin=../../cloud-admin-portal/backend/target/release/db9-admin
./setup.sh --skip-build \
  --binary=db9-server=../../target/release/db9-server
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
docker compose exec -T db9-admin bash -c "
  cat > /root/.db9/credentials << EOF
token = \"$ALICE_TOKEN\"
EOF
  db9 --api-url http://localhost:8090/api sh $ALICE_DB \
    -c 'echo alice_secret > /alice.txt && cat /alice.txt'
"
```

Isolation is enforced at the db9-admin proxy layer: `GET /fs9/<db_id>/...` returns 404 if the Bearer token's customer_id does not own that database.

Verified isolation behavior:
- Alice can read/write her own DB's pagefs
- Bob can read/write his own DB's pagefs
- Alice gets HTTP 404 accessing Bob's DB
- Bob gets HTTP 404 accessing Alice's DB
- Each DB gets an isolated TiKV keyspace: `db9_fs_<db_id>`

---

## Rebuilding Individual Services

When you change source code, rebuild only the affected service:

```bash
# Rebuild db9-admin (cloud-admin-portal backend)
docker compose up -d --build --no-deps db9-admin

# Rebuild db9-server (db9 database engine)
docker compose up -d --build --no-deps db9-server

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
docker compose logs -f db9-admin

# Open psql directly on db9-server
PGPASSWORD=admin psql -h 127.0.0.1 -p 5433 -U admin -d postgres

# db9 list databases
docker compose exec -T db9-admin \
  db9 --api-url http://localhost:8090/api db list

# Test fs9-meta admin API (run from within container to bypass host proxy)
docker compose exec -T db9-admin bash -c \
  'curl -s -H "x-fs9-meta-key: $FS9_META_KEY" http://fs9-meta:9998/api/v1/admin/namespaces'

# Test fs9-server health
curl -s http://localhost:9999/health

# Test db9-admin API
curl -s http://localhost:8090/api/health

# Direct fs9-server access (from within db9-admin container, with fs9 JWT)
ALICE_JWT=$(docker compose exec -T postgres psql -U admin -d db9_admin -t -c \
  "SELECT trim(password_plain) FROM tenant_credentials WHERE tenant_id='<DB_ID>' AND credential_type='fs9_token'" | tr -d ' \n')
docker compose exec -T db9-admin bash -c \
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
db9 CLI ──── HTTP ──────► db9-admin :8090
                                │
                ┌───────────────┼───────────────┐
                │               │               │
                ▼               ▼               ▼
          db9-server :5433   fs9-meta :9998   fs9-server :9999
                │                               │
                ▼                               ▼
            TiKV :20160 ◄─── pd :2379 ──► TiKV :20160
```

**db9 sh flow:**
1. `db9 sh <db_id>` strips `/api` suffix from api-url, calls db9-admin at `/fs9/<db_id>/...`
2. db9-admin verifies customer owns the DB (`/api/customer/databases` ownership check), fetches stored fs9 JWT
3. Proxies request to `http://fs9-server:9999/<db_id>/api/v1/...` (replacing Authorization header with fs9 JWT)
4. fs9-server validates JWT via fs9-meta, resolves namespace
5. On first access: auto-provisions namespace in fs9-meta + mounts pagefs plugin with TiKV keyspace `db9_fs_<db_id>`
6. File operations persisted to TiKV

**db9 db create flow:**
1. Creates tenant record in postgres
2. Connects to db9-server, bootstraps admin user in the new keyspace
3. Creates namespace in fs9-meta (`POST /api/v1/admin/namespaces`)
4. Creates user in the namespace, generates fs9 JWT (`POST /api/v1/admin/tokens`)
5. Stores fs9 JWT as credential in postgres (keyed by `tenant_id + customer_id`)

**URL routing note:**
- db9-admin API routes: `http://localhost:8090/api/...`
- db9-admin fs9 proxy: `http://localhost:8090/fs9/<db_id>/...` (NOT under `/api/`)
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
- Each tenant gets isolated keyspace: `db9_fs_<db_id>`
- Expected log: `[pagefs-tikv] Creating TiKV backend (txn), keyspace=Some("db9_fs_<db_id>")`

### Multi-Tenant Auth Isolation
- fs9 credentials (JWT) are stored per `(tenant_id, credential_type, customer_id)` in postgres
- db9-admin's `/fs9/<db_id>/...` proxy checks that the requesting customer owns the DB
- The anonymous `db9` session (default in container) can only access DBs it created
- To access a registered customer's DB via `db9 sh`, write their token to `/root/.db9/credentials`
- Format: `token = "<128-char-hex-token>"`

### Host Proxy Interference
- If running `curl localhost:9998` from the host fails with 502, a system HTTP proxy may be intercepting
- Run API tests from inside the Docker container: `docker compose exec -T db9-admin bash -c "curl ..."`
- Or use `curl --noproxy '*'` (may still fail if service not accessible from host)

### fs9 JWT Generation
- db9-admin calls `POST /api/v1/admin/tokens` on fs9-meta with `user_id`
- fs9-meta signs JWT with `FS9_JWT_SECRET`
- fs9-server validates via `POST /api/v1/tokens/validate` (not local verification)
- `FS9_JWT_SECRET` env var in db9-admin config is present but unused — signing is done by fs9-meta

---

### First Run: Expect Slow Context Transfer

The Docker build sends the entire repo as build context. For fs9, this includes `target/` (~17GB) if no `.dockerignore` exists. **Always ensure `~/fs9/.dockerignore` contains at minimum:**

```
target/
.git/
```

Without this, the `COPY . .` step in `Dockerfile.server-e2e` takes 2+ minutes just to transfer context. With `.dockerignore`, it drops to seconds.

### Host Port Conflicts

The e2e stack binds ports **5433** (db9-server), **8090** (db9-admin), and **9999** (fs9-server) on the host. If you're running local dev instances of these services, Docker will fail with `address already in use`.

**Before running `./setup.sh` or `docker compose up -d`:**

```bash
# Check for conflicts
ss -tlnp | grep -E '5433|8090|9999'

# Kill local processes if needed
kill <pid>
```

The script may succeed partially (e.g., 6/7 services up) if only one port conflicts. Fix the conflict and run `docker compose up -d` again — it's idempotent.

### Container Recreate Loses db9 Credentials

db9 stores credentials at `/root/.db9/credentials` inside the db9-admin container. When the container is recreated (e.g., after `docker compose build db9-admin && docker compose up -d`), credentials are lost.

**After rebuilding db9-admin, always re-login:**

```bash
docker compose exec db9-admin db9 --api-url http://localhost:8090/api login
```

User accounts persist (stored in postgres volume), so no need to re-register.

### Missing `Dockerfile.server-e2e` in fs9

The `docker-compose.yml` references `docker/Dockerfile.server-e2e` in the fs9 repo. This file may not exist in a fresh fs9 clone. The db9 repo ships a reference copy at `deploy/e2e/Dockerfile.fs9-server`.

```bash
# If fs9 is missing the Dockerfile:
cp ~/lab/db9/deploy/e2e/Dockerfile.fs9-server ~/fs9/docker/Dockerfile.server-e2e
```

### Docker Network DNS After Partial Restarts

If a container is recreated after port conflicts (e.g., `db9-admin` fails on first run, succeeds on retry), it may lose Docker DNS resolution to other containers (error: `failed to lookup address information: Temporary failure in name resolution`).

**Fix: full restart to recreate the network cleanly:**

```bash
docker compose down && docker compose up -d
```

### Interactive db9 Commands in Docker

`db9 register` and `db9 login` require interactive TTY input (email/password prompts). They won't work with `docker compose exec -T` (no TTY).

**Use tmux or a separate terminal:**

```bash
# Interactive — works
docker compose exec db9-admin db9 --api-url http://localhost:8090/api register

# Non-interactive — fails with "No such device or address"
docker compose exec -T db9-admin db9 --api-url http://localhost:8090/api register
```

For CI/scripting, use the HTTP API directly with `curl` (see Multi-Tenant Isolation Test section).

### Rebuilding a Single Service (e.g., sh9 change)

When you modify source in one crate (e.g., `sh9` in fs9 repo), you only need to rebuild the affected image:

```bash
# sh9 is bundled in db9-admin
docker compose build db9-admin
docker compose up -d db9-admin

# db9-server engine change
docker compose build db9-server
docker compose up -d db9-server
```

With `.dockerignore` in place, incremental rebuilds take ~30-60s (Docker layer cache reuses everything except the changed crate).

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `tikv` exits immediately | pd not ready yet | Health check handles it; wait longer |
| `db create` returns 500 | db9-server bootstrap failure | Check db9-server logs; ensure `DB9_BOOTSTRAP_ADMIN_USER` matches admin portal's user |
| `db sh` returns "Namespace not found" | fs9-server can't reach fs9-meta | Check fs9-server logs; verify `FS9_META_KEY` matches in .env |
| fs9-server WARN "error decoding response body" | `NamespaceInfo` struct missing `status` field | Rebuild fs9-server (fix: add `#[serde(default)]` to `status` in `meta_client.rs`) |
| `db sh` returns 404 for file write | DB owned by different customer than active db9 session | Write correct customer token to `/root/.db9/credentials` in container |
| `db sh` shows local files instead of pagefs | fs9 JWT expired or proxy not working | Check db9-admin has `FS9_SERVER_URL` env; check fs9-server logs for "Mounted pagefs" |
| `curl localhost:9998` returns 502 from host | Host HTTP proxy intercepting localhost traffic | Run from inside container: `docker compose exec -T db9-admin bash -c "curl http://fs9-meta:9998/..."` |
| Build fails with cargo error | Rust toolchain inside Docker too old | `docker buildx prune -f` then retry |
| Port already in use | Local dev process occupying the port | `ss -tlnp \| grep <port>` then `kill <pid>` |
| Build context transfer takes minutes | fs9 missing `.dockerignore`, sending `target/` dir | Add `.dockerignore` with `target/` and `.git/` to fs9 repo root |
| db9-admin DNS resolution failure | Container recreated outside clean network cycle | `docker compose down && docker compose up -d` |
| `db9` says "Not logged in" after rebuild | Container recreation wiped `/root/.db9/credentials` | Re-run `docker compose exec db9-admin db9 ... login` |
| Volumes have stale data after code change | DB schema changed | `docker compose down -v && ./setup.sh` |
| `Dockerfile.server-e2e` not found | fs9 repo missing the e2e Dockerfile | Copy from `deploy/e2e/Dockerfile.fs9-server` to `~/fs9/docker/Dockerfile.server-e2e` |
