# Quick Start Guide

## Prerequisites

- Rust 1.70+ with Cargo
- TiUP (TiKV package manager)
- PostgreSQL client (psql)

### Install TiUP

```bash
curl --proto '=https' --tlsv1.2 -sSf https://tiup-mirrors.pingcap.com/install.sh | sh
source ~/.bashrc
```

### Install PostgreSQL Client

```bash
# Ubuntu/Debian
sudo apt-get install postgresql-client

# macOS
brew install libpq
```

## Starting TiKV

db9-server uses TiKV keyspaces for isolation. Your TiKV cluster must run with API v2 enabled (`storage.api-version = 2`).

Create a config file `/tmp/tikv.toml`:

```toml
[storage]
api-version = 2
enable-ttl = true
```

Start with config:

```bash
tiup playground --mode tikv-slim --kv.config /tmp/tikv.toml
```

Note the PD endpoint from the output (often `127.0.0.1:2379`). If it differs, set `PD_ENDPOINTS` when starting db9-server.

### Create Additional Keyspaces (Multi-Tenancy)

The default keyspace is provided by TiKV/PD; you only need to create extra tenant keyspaces (e.g. `tenant_a`, `tenant_b`).

```bash
# Replace 127.0.0.1:2379 if your PD endpoint differs.
curl -sS -X POST http://127.0.0.1:2379/pd/api/v2/keyspaces \
  -H 'Content-Type: application/json' \
  -d '{"name":"tenant_a"}'

curl -sS -X POST http://127.0.0.1:2379/pd/api/v2/keyspaces \
  -H 'Content-Type: application/json' \
  -d '{"name":"tenant_b"}'
```

## Building db9-server

```bash
git clone https://github.com/c4pt0r/db9.git
cd db9
cargo build --release --locked
```

## Running db9-server

### Basic Start

```bash
DB9_BOOTSTRAP_ADMIN_PASSWORD=admin ./target/release/db9-server
```

### With Custom Configuration

```bash
PD_ENDPOINTS=127.0.0.1:2379 \
PG_PORT=5433 \
PG_KEYSPACE=default \
DB9_BOOTSTRAP_ADMIN_PASSWORD=admin \
./target/release/db9-server
```

## Security Note

db9-server is **secure-by-default**:
- By default, db9-server binds to `127.0.0.1:${PG_PORT}`.
- The first superuser is bootstrapped only when you explicitly set `DB9_BOOTSTRAP_ADMIN_PASSWORD` (and optionally `DB9_BOOTSTRAP_ADMIN_USER`).
- Non-loopback binds without TLS are refused unless you explicitly opt into insecure mode (`DB9_INSECURE=1` or `DB9_DEV=1`).

For production-like usage, enable TLS (`PG_TLS_CERT` + `PG_TLS_KEY`), set `PG_REQUIRE_TLS=1`, and use a strong bootstrap password. See `docs/authentication.md` and `docs/release-notes-v0.1.0.md`.

## Connecting

### Basic Connection

```bash
pg_isready -h 127.0.0.1 -p 5433
psql -h 127.0.0.1 -p 5433 -U admin -d postgres -c "SELECT 1;"
# Or open an interactive shell:
psql -h 127.0.0.1 -p 5433 -U admin -d postgres
# Password: (the value you used for `DB9_BOOTSTRAP_ADMIN_PASSWORD`)
```

### Multi-Tenant Connection

```bash
# Connect to tenant_a keyspace
psql -h 127.0.0.1 -p 5433 -U tenant_a.admin -d postgres
# Password: (bootstrapped per keyspace)

# Connect to tenant_b keyspace
psql -h 127.0.0.1 -p 5433 -U tenant_b.admin -d postgres
# Password: (bootstrapped per keyspace)
```

## First Steps

```sql
-- Create a table
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT UNIQUE,
    created_at TIMESTAMP DEFAULT NOW()
);

-- Insert data
INSERT INTO users (name, email) VALUES 
    ('Alice', 'alice@example.com'),
    ('Bob', 'bob@example.com');

-- Query data
SELECT * FROM users WHERE name LIKE 'A%';

-- Create index
CREATE INDEX idx_users_email ON users (email);

-- Show tables
SHOW TABLES;
```

## Extensions (HTTP)

db9-server supports built-in extensions that can be enabled per-tenant. The `http` extension provides Supabase-style HTTP table functions under the `extensions` schema:

```sql
-- Requires SUPERUSER
CREATE EXTENSION http;

SELECT status, content_type, content
FROM extensions.http_get('https://example.com');
```

## Using Transactions

```sql
BEGIN;
INSERT INTO users (name, email) VALUES ('Charlie', 'charlie@example.com');
UPDATE users SET name = 'Charles' WHERE name = 'Charlie';
COMMIT;

-- Or rollback
BEGIN;
DELETE FROM users WHERE id = 1;
ROLLBACK;  -- Changes discarded
```

## Creating Users

```sql
-- Create a read-only user
CREATE ROLE reader WITH PASSWORD 'secret' LOGIN;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO reader;

-- Create an admin user
CREATE ROLE app_admin WITH PASSWORD 'admin123' LOGIN SUPERUSER;

-- Connect as new user
-- psql -h 127.0.0.1 -p 5433 -U tenant_a.reader
```

## Loading Existing Data

db9-server supports pg_restore for loading PostgreSQL dumps:

```bash
pg_restore -h 127.0.0.1 -p 5433 -U admin -d postgres \
    --no-owner --no-privileges ./backup/
```

## Next Steps

- [SQL Reference](./sql-reference.md) - Complete SQL syntax reference
- [Extensions](./extensions.md) - Built-in extensions and the HTTP extension
- [Multi-Tenancy](./multi-tenancy.md) - Keyspace isolation and routing
- [Authentication](./authentication.md) - User management and RBAC
- [Configuration](./configuration.md) - Environment variables and options
