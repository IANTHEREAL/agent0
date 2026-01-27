# pg-tikv

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
│                      pg-tikv Server                         │
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

pg-tikv supports built-in extensions (compiled into the server binary) that can be enabled per-tenant:

```sql
-- Requires SUPERUSER
CREATE EXTENSION http;

-- Supabase-style table functions under the `extensions` schema
SELECT status, content_type, content
FROM extensions.http_get('https://example.com');
```

See `docs/extensions.md` for details and security restrictions.

## Quick Start

```bash
# 1. Start TiKV
tiup playground --mode tikv-slim

# 2. Start pg-tikv
cargo run

# 3. Connect
psql -h 127.0.0.1 -p 5433 -d postgres
```

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
| `PGTIKV_TOKIO_STACK_MB` | `4` | Tokio worker thread stack size (MB) |
| `PG_KEYSPACE` | `default` | Default TiKV keyspace for multi-tenancy |
| `PG_TLS_CERT` | (empty) | Path to TLS certificate file |
| `PG_TLS_KEY` | (empty) | Path to TLS private key file |

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

pg-tikv is tested against popular TypeScript/JavaScript ORMs:

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
# Unit tests (184 tests)
cargo test

# Integration tests (requires running server)
python3 scripts/integration_test.py

# ORM tests (requires running server)
cd orm-tests && npm test
```

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

## Admin Portal

A web-based admin portal is available for managing multi-tenant deployments:

```bash
cd cloud-admin-portal
./scripts/dev.sh
```

Features:
- Tenant management (create, view, disable keyspaces)
- User management per tenant (requires tenant credentials)
- Health monitoring
- Modern React UI with shadcn/ui

See [cloud-admin-portal/README.md](cloud-admin-portal/README.md) for details.

## License

Apache 2.0
