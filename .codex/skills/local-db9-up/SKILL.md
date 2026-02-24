---
name: local-db9-up
description: "Start a local db9 (db9-server) instance in sandbox: start TiKV via scripts/tikv_admin.py, build with cargo, run db9-server, then smoke-test with pg_isready + a simple SQL (SELECT 1)."
---

# Local db9 up (sandbox)

Run everything from the repo root (example: `cd /path/to/db9`).

## Prerequisites

Required:

- `uv` (runs `scripts/tikv_admin.py`) — install: `curl -LsSf https://astral.sh/uv/install.sh | sh`
- `cargo` (Rust toolchain) — install via `rustup` (`https://rustup.rs`)
- `tiup` (starts local TiKV) — install: `curl --proto '=https' --tlsv1.2 -sSf https://tiup-mirrors.pingcap.com/install.sh | sh`
- `psql` + `pg_isready` (PostgreSQL client) — Ubuntu/Debian: `sudo apt-get install postgresql-client`

Helpful for debugging port conflicts:

- `ss` (preferred) — Ubuntu/Debian: `sudo apt-get install iproute2`
- `netstat` (alternative) — Ubuntu/Debian: `sudo apt-get install net-tools`
- `lsof` — Ubuntu/Debian: `sudo apt-get install lsof`

## Start TiKV (persistent dev cluster)

```bash
uv run scripts/tikv_admin.py start --name dev --persistent
```

Copy the `PD_ENDPOINTS=...` line from the output (example: `127.0.0.1:2379`).

If you already started it (or forgot the port), print it again:

```bash
uv run scripts/tikv_admin.py status --name dev
```

## Build db9 (db9-server) (confirm it compiles)

```bash
cargo build --release
```

## Start db9-server

```bash
PD_ENDPOINTS=127.0.0.1:<pd_port> \
PG_PORT=5433 \
./target/release/db9-server
```

If you want to run it in the background (so you can smoke-test in the same terminal):

```bash
PD_ENDPOINTS=127.0.0.1:<pd_port> \
PG_PORT=5433 \
./target/release/db9-server > /tmp/db9-server.log 2>&1 & echo $! > /tmp/db9-server.pid
```

Optional:

```bash
PG_KEYSPACE=default ...
```

TLS (optional):

```bash
PG_TLS_CERT=/path/server.crt \
PG_TLS_KEY=/path/server.key \
./target/release/db9-server
```

## Verify + connect

```bash
pg_isready -h 127.0.0.1 -p 5433
PGPASSWORD=admin psql -h 127.0.0.1 -p 5433 -U admin -d postgres
```

## Smoke test (simple SQL)

```bash
PGPASSWORD=admin psql -h 127.0.0.1 -p 5433 -U admin -d postgres -v ON_ERROR_STOP=1 -c "SELECT 1;"
```

Default credentials (per keyspace): `admin / admin`.

## Conflict handling (common)

### TiKV cluster already running / want a clean cluster

- If `start` prints "already running": that's OK; reuse it.
- If you need a clean state (DESTRUCTIVE):

```bash
uv run scripts/tikv_admin.py stop --name dev
uv run scripts/tikv_admin.py clean --name dev
uv run scripts/tikv_admin.py start --name dev --persistent
```

### `Address already in use` on port 5433

- Reuse the existing db9-server if it’s already listening:
  - Check listener (pick one): `ss -ltn | grep -F ':5433'` OR `netstat -ltn 2>/dev/null | grep -F ':5433'` OR `lsof -nP -iTCP:5433 -sTCP:LISTEN`
- Or pick a different port:

```bash
PG_PORT=5434 PD_ENDPOINTS=127.0.0.1:<pd_port> ./target/release/db9-server
pg_isready -h 127.0.0.1 -p 5434
```

## Stop

- Stop `db9-server`: `Ctrl-C` in the server terminal
- Stop TiKV cluster:

```bash
uv run scripts/tikv_admin.py stop --name dev
```
