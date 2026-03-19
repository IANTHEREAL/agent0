# db9-server

A PostgreSQL-compatible distributed SQL database built on TiKV.

A DATABASE FOR AI BY AI

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    PostgreSQL Clients                       │
│              (psql, pgcli, pg_dump, pg_restore)             │
└─────────────────────────────────────────────────────────────┘
                              │
                              │ PostgreSQL Wire Protocol (pgwire 0.28)
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                      db9-server Server                      │
│  ┌───────────────────────────────────────────────────────┐  │
│  │  Protocol: Simple Query, Extended Query, COPY         │  │
│  ├───────────────────────────────────────────────────────┤  │
│  │  SQL: Parser (sqlparser-rs) → Executor                │  │
│  ├───────────────────────────────────────────────────────┤  │
│  │  Storage: Key Encoding, Indexes, Transactions         │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                              │
                              │ gRPC (TiKV Pessimistic Transactions)
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                          TiKV                               │
│                Distributed KV Storage (Raft)                │
└─────────────────────────────────────────────────────────────┘
```

## Features

### SQL Support

| Category | Features |
|----------|----------|
| **DDL** | `CREATE TABLE`, `DROP TABLE`, `TRUNCATE`, `ALTER TABLE`, `CREATE INDEX`, `CREATE VIEW`, `DROP VIEW`, `CREATE MATERIALIZED VIEW`, `DROP MATERIALIZED VIEW`, `REFRESH MATERIALIZED VIEW`, `CREATE SCHEMA`, `DROP SCHEMA`, `CREATE SEQUENCE`, `DROP SEQUENCE`, `CREATE TYPE`, `DROP TYPE`, `CREATE EXTENSION`, `DROP EXTENSION`, `SHOW TABLES` |
| **DML** | `INSERT`, `UPDATE`, `DELETE` with `RETURNING`, `SELECT` with full `WHERE` support |
| **Queries** | `ORDER BY`, `LIMIT`, `OFFSET`, `DISTINCT`, `GROUP BY`, `HAVING`, `WITH ... AS` (CTEs), `WITH RECURSIVE` (Recursive CTEs) |
| **Joins** | `INNER JOIN`, `LEFT JOIN`, `RIGHT JOIN`, `FULL OUTER JOIN`, `CROSS JOIN`, `NATURAL JOIN` |
| **Aggregates** | `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` |
| **Window Functions** | `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LEAD`, `LAG`, `SUM/AVG/COUNT/MIN/MAX OVER` |
| **Expressions** | `+`, `-`, `*`, `/`, `%`, `\|\|`, `AND`, `OR`, `NOT`, comparisons |
| **Predicates** | `IN (...)`, `IN (SELECT ...)`, `EXISTS`, `BETWEEN`, `LIKE`, `ILIKE`, `IS NULL`, `IS NOT NULL`, Scalar Subqueries |
| **Functions** | String, Math, Date/Time, `CASE WHEN`, `CAST`, `COALESCE`, `NULLIF` |
| **Procedures** | `CREATE PROCEDURE`, `DROP PROCEDURE`, `CALL` |
| **Transactions** | `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`, `ROLLBACK TO`, `SELECT FOR UPDATE` |
| **COPY** | `COPY FROM stdin` for bulk loading, pg_restore compatible |

### Data Types

| Type | Aliases |
|------|---------|
| `BOOLEAN` | `BOOL` |
| `INTEGER` | `INT`, `INT4`, `SERIAL` |
| `BIGINT` | `INT8`, `BIGSERIAL` |
| `REAL` | `FLOAT4` |
| `DOUBLE PRECISION` | `FLOAT8` |
| `TEXT` | `VARCHAR`, `CHAR` |
| `BYTEA` | - |
| `TIMESTAMP` | `TIMESTAMPTZ` |
| `INTERVAL` | - |
| `UUID` | - |
| `JSON` | `JSONB` |
| `ENUM` | User-defined enum types |

### PostgreSQL Functions

```sql
-- String
UPPER, LOWER, LENGTH, CONCAT, LEFT, RIGHT, SUBSTRING, TRIM,
LPAD, RPAD, REPLACE, REVERSE, REPEAT, SPLIT_PART, INITCAP, POSITION

-- Math  
ABS, CEIL, FLOOR, ROUND, TRUNC, SQRT, POWER, EXP, LN, LOG, SIGN, MOD, PI, RANDOM

-- Date/Time
NOW, CURRENT_TIMESTAMP, CURRENT_DATE, DATE_TRUNC, EXTRACT, TO_CHAR, AGE

-- Sequence
nextval, currval, setval

-- Other
COALESCE, NULLIF, GREATEST, LEAST, gen_random_uuid()
```

### Extensions

db9-server supports built-in extensions (compiled into the server binary) that can be enabled per-tenant:

| Extension | Description |
|-----------|-------------|
| `http` | HTTP client functions (`http_get`, `http_post`, etc.) |
| `pg_cron` | Distributed cron scheduler (pg_cron V2 compatible) |

```sql
CREATE EXTENSION http;
CREATE EXTENSION pg_cron;
```

See `docs/extensions.md` for details and security restrictions.

### Cron Jobs (pg_cron)

pg_cron V2-compatible distributed cron scheduler. Schedule SQL commands to run on a recurring basis.

```sql
CREATE EXTENSION pg_cron;

-- Schedule a job (returns job ID)
SELECT cron.schedule('nightly_vacuum', '0 3 * * *', 'VACUUM');
SELECT cron.schedule('*/5 * * * *', 'SELECT 1');

-- List jobs
SELECT * FROM cron.job ORDER BY jobid;

-- Modify a job (job_id, schedule, command, database, username, active)
SELECT cron.alter_job(1, '0 4 * * *');              -- change schedule
SELECT cron.alter_job(1, NULL, NULL, NULL, NULL, false);  -- disable

-- Delete a job
SELECT cron.unschedule('nightly_vacuum');  -- by name
SELECT cron.unschedule(1);                -- by ID

-- View execution history
SELECT * FROM cron.job_run_details ORDER BY runid DESC LIMIT 10;

DROP EXTENSION pg_cron;
```

### Async Worker Engine

db9-server includes a built-in async worker engine for background task execution. All instances share a global task queue in TiKV with automatic coordination — no leader election required.

| Feature | SQL |
|---------|-----|
| Cron jobs | `SELECT cron.schedule(...)` |
| Background index build | `CREATE INDEX CONCURRENTLY ...` |
| Background MV refresh | `REFRESH MATERIALIZED VIEW CONCURRENTLY ...` |
| Background SQL | `SELECT pg_background_launch('...')` |
| Auto-ANALYZE | Automatic (triggered by DML modification count) |

See [docs/worker.md](docs/worker.md) for configuration, deployment, and troubleshooting.

## Quick Start

```bash
# 1. Start TiKV
tiup playground --mode tikv-slim --kv.config deploy/e2e/config/tikv.toml

# 2. Start db9-server
PD_ENDPOINTS=127.0.0.1:2379 \
DB9_BOOTSTRAP_ADMIN_PASSWORD=admin \
cargo run

# 3. Connect
PGPASSWORD=admin psql -h 127.0.0.1 -p 5433 -U admin -d postgres
```

`db9-server` requires TiKV API v2. `deploy/e2e/config/tikv.toml` sets `api-version = 2`
and `enable-ttl = true`, which are required for local development.

### Example Session

```sql
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT UNIQUE,
    created_at TIMESTAMP DEFAULT NOW()
);

INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com');
INSERT INTO users (name, email) VALUES ('Bob', 'bob@example.com');

SELECT * FROM users WHERE name LIKE 'A%';
SELECT COUNT(*), DATE_TRUNC('day', created_at) FROM users GROUP BY DATE_TRUNC('day', created_at);

-- CTE example
WITH active_users AS (
    SELECT * FROM users WHERE created_at > NOW() - INTERVAL '7 days'
)
SELECT * FROM active_users ORDER BY name;

-- Subquery examples
SELECT * FROM users WHERE id IN (SELECT id FROM users WHERE name LIKE 'A%');
SELECT id, name, (SELECT COUNT(*) FROM users) as total_users FROM users;
SELECT * FROM users WHERE id = (SELECT MIN(id) FROM users);

-- View example
CREATE VIEW recent_users AS SELECT * FROM users WHERE created_at > NOW() - INTERVAL '30 days';
SELECT * FROM recent_users WHERE name LIKE 'A%';

-- Window function examples
SELECT id, name, ROW_NUMBER() OVER (ORDER BY created_at) as rn FROM users;
SELECT id, name, RANK() OVER (ORDER BY name) as rank FROM users;
SELECT id, name, SUM(id) OVER (ORDER BY id) as running_total FROM users;
SELECT id, name, LAG(name) OVER (ORDER BY id) as prev_name FROM users;
SELECT id, name, LEAD(name, 1, 'N/A') OVER (ORDER BY id) as next_name FROM users;

-- RETURNING clause
UPDATE users SET name = 'Robert' WHERE name = 'Bob' RETURNING *;
DELETE FROM users WHERE id = 1 RETURNING id, name;

-- Sequences
CREATE SEQUENCE order_seq START 1000;
SELECT nextval('order_seq');
SELECT currval('order_seq');
SELECT setval('order_seq', 2000);

-- Schemas
CREATE SCHEMA sales;
CREATE TABLE sales.orders (id SERIAL PRIMARY KEY, total INT);
SELECT * FROM sales.orders;
SET search_path TO sales, public;

-- User-defined enum types
CREATE TYPE status AS ENUM ('pending', 'active', 'completed');
CREATE TABLE tasks (id SERIAL PRIMARY KEY, state status);
INSERT INTO tasks (state) VALUES ('active');
```

### Restore a PostgreSQL Dump

```bash
pg_restore -h 127.0.0.1 -p 5433 -d postgres --no-owner --no-privileges ./backup/
```

## Configuration

| Environment Variable | Default | Description |
|---------------------|---------|-------------|
| `PD_ENDPOINTS` | `127.0.0.1:2379` | TiKV PD endpoints |
| `PG_PORT` | `5433` | PostgreSQL protocol port |
| `DB9_TOKIO_STACK_MB` | `8` | Tokio worker thread stack size (MB) |
| `PG_KEYSPACE` | `default` | Default TiKV keyspace for multi-tenancy |
| `PG_TLS_CERT` | (empty) | Path to TLS certificate file |
| `PG_TLS_KEY` | (empty) | Path to TLS private key file |

`DB9_TOKIO_STACK_MB` controls worker thread stack size for the async runtime. The default `8` MiB is chosen to safely handle deep analyzed-path subquery execution (especially catalog-heavy queries) without stack overflows. Increase it (for example to `16` or `32`) if your workload includes unusually deep nested query shapes.

**Authentication**: Password authentication is always enabled via AuthManager. Each tenant has its own users stored in TiKV. Default admin user is created on bootstrap with password "admin".

**Multi-tenancy**: Use `tenant.user` or `tenant:user` format to specify keyspace per connection (e.g., `myapp.admin` connects to keyspace `myapp` as user `admin`).

## Constraints

| Constraint | Status | Notes |
|------------|--------|-------|
| PRIMARY KEY | ✅ | Single and composite keys |
| NOT NULL | ✅ | Enforced on INSERT/UPDATE |
| UNIQUE | ✅ | With auto-index creation |
| CHECK | ✅ | Column-level constraints |
| FOREIGN KEY | ✅ | Full referential integrity |
| DEFAULT | ✅ | Including expressions like `NOW()` |

### Foreign Key Actions

| Action | ON DELETE | ON UPDATE |
|--------|-----------|-----------|
| CASCADE | ✅ | ✅ |
| SET NULL | ✅ | ✅ |
| SET DEFAULT | ✅ | ✅ |
| RESTRICT | ✅ | ✅ |
| NO ACTION | ✅ | ✅ |

## Stored Procedures

Basic stored procedure support with parameter passing:

```sql
-- Create a procedure
CREATE PROCEDURE update_prices(p_factor INT)
AS BEGIN
UPDATE products SET price = price * p_factor
END;

-- Call the procedure
CALL update_prices(2);

-- Drop the procedure
DROP PROCEDURE update_prices;
```

**Supported**: `CREATE PROCEDURE`, `DROP PROCEDURE`, `CALL`, parameters with type-aware substitution.

**Limitations**: No OUT/INOUT parameters, no control flow (IF/WHILE), no exception handling.

## Schemas

PostgreSQL-style schema support with `search_path`:

```sql
-- Create schema
CREATE SCHEMA myapp;

-- Create objects in schema
CREATE TABLE myapp.users (id SERIAL PRIMARY KEY, name TEXT);
CREATE SEQUENCE myapp.order_seq;

-- Query with qualified names
SELECT * FROM myapp.users;

-- Set search path for unqualified names
SET search_path TO myapp, public;
SELECT * FROM users;  -- Resolves to myapp.users

-- Default search_path is 'public'
```

**Supported**: `CREATE SCHEMA`, `DROP SCHEMA`, schema-qualified table/view/sequence names, `SET search_path`, `SHOW search_path`.

## Sequences

PostgreSQL-compatible sequences:

```sql
-- Standalone sequences
CREATE SEQUENCE order_seq START 1000 INCREMENT 1;
SELECT nextval('order_seq');  -- 1000
SELECT nextval('order_seq');  -- 1001
SELECT currval('order_seq');  -- 1001 (last value in session)
SELECT setval('order_seq', 5000);
SELECT setval('order_seq', 5000, false);  -- Next nextval returns 5000
DROP SEQUENCE order_seq;

-- Implicit sequences (SERIAL columns)
CREATE TABLE orders (id SERIAL PRIMARY KEY);
INSERT INTO orders DEFAULT VALUES;  -- id = 1
SELECT currval('orders_id_seq');    -- Auto-created sequence
```

**Options**: `START`, `INCREMENT`, `MINVALUE`, `MAXVALUE`, `CYCLE`/`NO CYCLE`.

## User-Defined Types

PostgreSQL-style enum types:

```sql
-- Create enum type
CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');

-- Use in table
CREATE TABLE people (
    name TEXT,
    current_mood mood
);

INSERT INTO people VALUES ('Alice', 'happy');
SELECT * FROM people WHERE current_mood = 'happy';

-- Drop type
DROP TYPE mood;
```

## Project Structure

```
src/
├── main.rs              # TCP server entry point
├── protocol/
│   ├── mod.rs
│   └── handler.rs       # pgwire handlers (query, COPY)
├── sql/
│   ├── mod.rs
│   ├── parser.rs        # SQL parsing
│   ├── executor.rs      # Query execution
│   ├── expr.rs          # Expression evaluation
│   ├── aggregate.rs     # Aggregation functions
│   ├── session.rs       # Transaction management
│   ├── names.rs         # Schema-aware name resolution
│   ├── sequences.rs     # Sequence operations
│   ├── information_schema.rs  # information_schema virtual tables
│   └── result.rs        # Result types
├── storage/
│   ├── mod.rs
│   ├── encoding.rs      # Key/value encoding
│   └── tikv_store.rs    # TiKV client wrapper
└── types/
    └── mod.rs           # Value, Row, Schema types

tests/                   # SQL integration tests
orm-tests/               # ORM compatibility tests (pg, TypeORM, Sequelize, Knex, Drizzle, Kysely)
```

## ORM Compatibility

db9-server is tested against popular TypeScript/JavaScript ORMs:

| ORM | Tests | Status |
|-----|-------|--------|
| **pg** (node-postgres) | 60+ | ✅ All passing |
| TypeORM | 147 | ✅ All passing |
| Prisma | 89 | ✅ All passing |
| Sequelize | 87 | ✅ All passing |
| Knex.js | 97 | ✅ All passing |
| Drizzle | 75 | ✅ All passing |
| **Kysely** | 60+ | ✅ All passing |

**Total: 600+ ORM tests passing**

Features tested include:
- Connection pooling and error handling
- CRUD operations (INSERT, SELECT, UPDATE, DELETE)
- Transactions (BEGIN, COMMIT, ROLLBACK, isolation levels)
- Relations (foreign keys, JOINs, eager loading)
- Advanced queries (window functions, CTEs, subqueries, views)
- JSONB operations
- Schema introspection via `information_schema`

See [orm-tests/README.md](orm-tests/README.md) for details.

## Tests

```bash
# Fast, deterministic regression gate (recommended; <5min typical)
# - Starts TiKV + db9-server by default
# - Uses `scripts/regression_gate.list` as the single source of truth
bash scripts/regression_gate.sh

# Reuse an existing running db9-server instance
bash scripts/regression_gate.sh --dsn "$PG_DSN"

# Tier-2 E2E suites (app-like smoke tests; requires running db9-server)
PG_DSN=postgres://admin:<password>@127.0.0.1:5433/postgres bash scripts/e2e_tests.sh sqlalchemy_smoke

# Full automated test suite (slower; broader coverage)
./run_tests.sh

# Unit tests (184 tests)
cargo test

# Integration tests (requires running server)
python3 scripts/integration_test.py

# ORM tests (requires running server)
cd orm-tests && npm test

# Go/GORM smoke test (requires running server + Go)
export PG_DSN="postgres://admin:<password>@127.0.0.1:5433/postgres?sslmode=disable"
(cd e2e/gorm_smoke && go test ./... -count=1)
# Or via the runner:
bash scripts/e2e_tests.sh gorm_smoke
```

Notable integration workloads:
- **Dify-lite compatibility gate**: `tests/96_dify_schema.sql` (schema restore smoke) + `tests/127_dify_lite_workload.sql` (minimal deterministic Dify query/DDL workload).

| Test Suite | Coverage |
|------------|----------|
| DDL | CREATE, DROP, ALTER, TRUNCATE, Views, Schemas |
| DML | INSERT, UPDATE, DELETE, SELECT, RETURNING |
| Transactions | BEGIN, COMMIT, ROLLBACK, SAVEPOINT, SELECT FOR UPDATE |
| Queries | WHERE, ORDER BY, LIMIT, GROUP BY, HAVING, JOIN, CTEs |
| Subqueries | IN (SELECT ...), EXISTS, NOT EXISTS, Scalar Subqueries |
| Window Functions | ROW_NUMBER, RANK, DENSE_RANK, LEAD, LAG, SUM/AVG/COUNT OVER |
| Functions | String, Math, Date, CASE, CAST, Sequences |
| Indexes | CREATE INDEX, Index Scan optimization |
| Types | UUID, INTERVAL, TIMESTAMP, JSONB, ENUM |
| Compatibility | COPY protocol, pg_restore, Extended Query, ORMs |

## db9 CLI

`db9` is the customer-facing CLI for managing databases on db9-server. The legacy in-repo CLI source was removed from this repository; the maintained control-plane/CLI code now lives in `db9-backend`.

### Install

Build `db9` from the `db9-backend` repository.

### Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `DB9_API_URL` | `http://localhost:8090/api` | API endpoint (or use `--api-url`) |

Credentials are stored in `~/.db9/credentials` after login.

### Quick Start

```bash
# 1. Register and login
db9 register
db9 login

# 2. Create a database
db9 db create --name myapp
# → Database ID: x9y8z7w6v5u4
# → Admin password: aB3kL9mP2xQr
# → Connection: psql -h 127.0.0.1 -p 5433 -U x9y8z7w6v5u4.admin

# 3. Run SQL
db9 db sql x9y8z7w6v5u4 -q "CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT)"
db9 db sql x9y8z7w6v5u4 -q "SELECT * FROM users"

# 4. Inspect performance
db9 db inspect x9y8z7w6v5u4 tables
db9 db inspect x9y8z7w6v5u4 slow-queries

# 5. Dump schema
db9 db dump x9y8z7w6v5u4 --ddl-only
```

### Command Reference

```
db9
├── register                          # Create account
├── login                             # Login (stores token in ~/.db9/)
├── logout                            # Remove stored credentials
├── db
│   ├── create --name <name>          # Create database
│   ├── list                          # List databases
│   ├── status <id>                   # Database details
│   ├── delete <id>                   # Delete database
│   ├── reset-password <id>           # Reset admin password
│   ├── connect <id>                  # Show connection string
│   ├── sql <id> -q <sql> | -f <file># Execute SQL
│   ├── seed <id> <file>              # Run seed SQL file
│   ├── dump <id> [--ddl-only]        # Export schema/data as SQL
│   ├── users <id> list|create|delete # Manage database users
│   ├── inspect <id> <subcommand>     # Observability (see below)
│   ├── cron <id> <subcommand>        # Cron job management (see below)
│   └── branch create|list|delete     # Database branching
├── gen types <id> --lang ts|python   # Generate type definitions
├── migration
│   ├── new <name>                    # Create migration file
│   ├── list                          # List local migrations
│   ├── up <id>                       # Apply pending migrations
│   └── status <id>                   # Show applied vs pending
├── token show|create|list|revoke     # API token management
└── completion bash|zsh|fish          # Shell completions
```

#### Inspect Subcommands

```bash
db9 db inspect <id>              # Summary dashboard (TPS, latency, connections)
db9 db inspect <id> queries      # Query samples with latency stats
db9 db inspect <id> report       # Combined summary + queries
db9 db inspect <id> schemas      # List schemas
db9 db inspect <id> tables       # List tables with row counts
db9 db inspect <id> indexes      # List indexes
db9 db inspect <id> slow-queries # Slow queries sorted by p99 latency
```

#### Cron Subcommands

```bash
db9 db cron <id> list                                           # List all cron jobs
db9 db cron <id> create '*/5 * * * *' 'SELECT 1' --name my_job # Create with name
db9 db cron <id> create '0 * * * *' 'VACUUM'                   # Create without name
db9 db cron <id> enable my_job                                  # Enable (by name or ID)
db9 db cron <id> disable 1                                      # Disable (by name or ID)
db9 db cron <id> history --job my_job --limit 50                # Execution history
db9 db cron <id> delete my_job                                  # Delete (by name or ID)
db9 db cron <id> status                                         # Overview of all jobs
db9 db cron <id> status my_job                                  # Detailed status of specific job
```

### Examples

```bash
# Run SQL from file
db9 db sql x9y8z7w6v5u4 -f ./init.sql

# Generate TypeScript types from schema
db9 gen types x9y8z7w6v5u4 --lang typescript --schema public

# Create and apply migrations
db9 migration new add_users_table
# → Created: migrations/20260212_add_users_table.sql
# Edit the file, then apply:
db9 migration up x9y8z7w6v5u4

# Branch a database (schema copy for dev/test)
db9 db branch create x9y8z7w6v5u4 --name feature-auth
db9 db branch list x9y8z7w6v5u4

# Output as JSON (for scripting)
db9 --json db list
db9 --output csv db list
```

Use `db9 --help` or `db9 <command> --help` for full option details.

## Admin Portal

The legacy in-repo admin portal was removed from this repository. Use the maintained control-plane in `db9-backend`.

## License

Apache 2.0
