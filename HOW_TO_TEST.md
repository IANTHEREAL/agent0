# Testing pg-tikv

This document describes how to run tests for pg-tikv.

## Prerequisites

- **Rust toolchain**: `rustup` with stable Rust
- **TiUP**: TiKV cluster management tool
- **Node.js 20+**: For ORM tests
- **psql**: PostgreSQL client (for integration tests)
- **uv** (recommended): Python package runner for scripts

Install TiUP:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://tiup-mirrors.pingcap.com/install.sh | sh
```

## Quick Start

### Automated Full Test Suite

The easiest way to run all tests:

```bash
./run_tests.sh
```

This script automatically:
1. Starts a fresh TiKV cluster
2. Builds pg-tikv in release mode
3. Runs integration tests (6 built-in tests)
4. Runs ORM tests (600+ tests)
5. Cleans up the cluster on exit

### Manual Workflow

For development, you may prefer to keep the TiKV cluster running:

```bash
# 1. Start TiKV cluster (persistent mode)
uv run scripts/tikv_admin.py start --name dev --persistent

# Output shows PD endpoint, e.g.:
#   PD_ENDPOINTS=127.0.0.1:2379

# 2. Start pg-tikv (use the port from step 1)
PD_ENDPOINTS=127.0.0.1:2379 PG_PORT=5433 cargo run --release

# 3. Run tests (in another terminal)
export PG_DSN=postgres://admin:admin@127.0.0.1:5433/postgres

# Integration tests
python3 scripts/integration_test.py --dsn $PG_DSN

# Or run specific SQL test files
python3 scripts/integration_test.py --dsn $PG_DSN tests/01_ddl_basic.sql
python3 scripts/integration_test.py --dsn $PG_DSN tests/

# ORM tests
cd orm-tests && npm test

# 4. When done, stop the cluster
uv run scripts/tikv_admin.py stop --name dev
```

## Test Types

### 1. Unit Tests (Rust)

```bash
cargo test
```

~184 tests covering:
- Key encoding/decoding
- Expression evaluation
- SQL parsing helpers
- Type conversions

No external dependencies required.

### 2. Integration Tests (SQL)

```bash
python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres
```

**Built-in tests** (run when no files specified):
- Basic connection
- DDL operations (CREATE/DROP/ALTER TABLE)
- DML operations (INSERT/UPDATE/DELETE)
- Transactions (BEGIN/COMMIT/ROLLBACK)
- JSON operations
- Advanced queries (JOINs, subqueries, GROUP BY)

**SQL file tests** (in `tests/` directory):
```bash
# Run all SQL tests
python3 scripts/integration_test.py --dsn $PG_DSN tests/

# Run specific test
python3 scripts/integration_test.py --dsn $PG_DSN tests/18_window_functions.sql
```

### 3. ORM Tests (TypeScript)

```bash
cd orm-tests
npm install        # First time only
npx prisma generate  # First time only
npm test
```

600+ tests covering 7 ORMs:
| ORM | Tests | Coverage |
|-----|-------|----------|
| pg (node-postgres) | 60+ | Connection, CRUD, transactions |
| TypeORM | 147 | Entities, relations, migrations |
| Prisma | 89 | Client, transactions, relations |
| Sequelize | 87 | Models, associations, hooks |
| Knex.js | 97 | Query builder, migrations |
| Drizzle | 75 | Schema, queries, relations |
| Kysely | 60+ | Type-safe queries, transactions |

## TiKV Cluster Management

### Recommended: Use tikv_admin.py

We recommend using `scripts/tikv_admin.py` to manage local TiKV test clusters. This script wraps `tiup playground` and provides:

- **Automatic API v2 configuration**: Enables TiKV keyspace support required by pg-tikv
- **Multi-cluster management**: Run multiple isolated clusters simultaneously
- **Port tracking**: Automatically detects and reports assigned ports
- **Clean lifecycle management**: Proper startup, shutdown, and cleanup

### Why Not Raw tiup playground?

While you can use `tiup playground --mode tikv-slim` directly, `tikv_admin.py` handles several pg-tikv-specific requirements:

1. **API v2 mode**: pg-tikv requires TiKV API v2 for keyspace/multi-tenant support. The script auto-generates the required `tikv.toml`:
   ```toml
   [storage]
   api-version = 2
   enable-ttl = true
   ```

2. **Port discovery**: tiup playground assigns random ports. The script extracts and reports them:
   ```
   PD_ENDPOINTS=127.0.0.1:2379
   ```

3. **Cluster tracking**: Manages cluster metadata in `~/.pg-tikv/clusters/` for easy listing and cleanup.

### tikv_admin.py Commands

```bash
# Start a cluster (one-time mode - for CI/automated tests)
uv run scripts/tikv_admin.py start --name test

# Start a cluster (persistent mode - for development)
uv run scripts/tikv_admin.py start --name dev --persistent

# Start with specific PD port
uv run scripts/tikv_admin.py start --name dev --pd-port 2379 --persistent

# List all managed clusters
uv run scripts/tikv_admin.py list

# Check cluster status
uv run scripts/tikv_admin.py status --name dev

# Stop a specific cluster
uv run scripts/tikv_admin.py stop --name dev

# Stop all clusters
uv run scripts/tikv_admin.py stop --all

# Clean cluster data (stops if running, removes data)
uv run scripts/tikv_admin.py clean --name dev

# Clean all clusters
uv run scripts/tikv_admin.py clean --all
```

### Cluster Modes

| Mode | Flag | Behavior | Use Case |
|------|------|----------|----------|
| **One-time** | (default) | Caller manages lifecycle | CI, automated tests, `run_tests.sh` |
| **Persistent** | `--persistent` | Keeps running until explicitly stopped | Development, manual testing |

### Example: Development Workflow

```bash
# Terminal 1: Start TiKV cluster (runs in background)
uv run scripts/tikv_admin.py start --name dev --persistent
# Output:
#   [INFO] Starting TiKV cluster 'dev' (persistent mode)...
#   [INFO] Cluster 'dev' is ready!
#   [INFO]   PD endpoint: 127.0.0.1:2379
#   [INFO]   TiKV: 127.0.0.1:20160
#   
#   Cluster 'dev' is ready.
#     PD_ENDPOINTS=127.0.0.1:2379

# Terminal 1: Start pg-tikv
PD_ENDPOINTS=127.0.0.1:2379 cargo run --release

# Terminal 2: Run tests
export PG_DSN=postgres://admin:admin@127.0.0.1:5433/postgres
python3 scripts/integration_test.py --dsn $PG_DSN

# When done (any terminal)
uv run scripts/tikv_admin.py stop --name dev
```

### Example: CI Workflow

```bash
# run_tests.sh uses one-time mode
CLUSTER_NAME="test-$(date +%s)"
uv run scripts/tikv_admin.py start --name "$CLUSTER_NAME"

# ... run tests ...

# Cleanup on exit (handled by trap in run_tests.sh)
uv run scripts/tikv_admin.py stop --name "$CLUSTER_NAME"
uv run scripts/tikv_admin.py clean --name "$CLUSTER_NAME"
```

### Cluster Data Location

All cluster data is stored in `~/.pg-tikv/clusters/<name>/`:

```
~/.pg-tikv/clusters/dev/
├── cluster.json      # Cluster metadata (ports, PID, mode)
├── data/             # TiKV data directory
├── playground.log    # tiup playground output
└── tikv.toml         # TiKV configuration (API v2)
```

### Troubleshooting

**Cluster won't start:**
```bash
# Check if tiup is installed
which tiup

# Check for port conflicts
uv run scripts/tikv_admin.py list
lsof -i :2379

# View startup logs
cat ~/.pg-tikv/clusters/<name>/playground.log
```

**Orphaned clusters after crash:**
```bash
# List all clusters (shows status)
uv run scripts/tikv_admin.py list

# Force cleanup
uv run scripts/tikv_admin.py clean --all
```

## Test File Conventions

SQL test files in `tests/`:

| File | Purpose |
|------|---------|
| `NN_name.sql` | Main test file (NN = order number) |
| `NN_name.expected` | Expected output (optional, for exact matching) |
| `NN_name.errors` | Expected error patterns (optional) |
| `NN_name.out` | Actual output (auto-generated) |
| `NN_name_setup.sql` | Setup script (run before main test) |
| `NN_name_load.py` | Data loading script (optional) |

**Test result logic:**
1. If `.expected` exists: Compare output exactly
2. If `.errors` exists: Allow listed errors, fail on unexpected ones
3. Otherwise: Pass if no SQL errors detected

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `PG_DSN` | `postgres://admin:admin@127.0.0.1:5433/postgres` | Connection string |
| `PD_ENDPOINTS` | `127.0.0.1:2379` | TiKV PD endpoint |
| `PG_PORT` | `5433` | pg-tikv listen port |
| `PG_USER` | `admin` | Database user |
| `PG_PASSWORD` | `admin` | Database password |

## CI/CD

GitHub Actions workflow (`.github/workflows/orm-tests.yml`):

```yaml
# Triggered on push/PR to main/master
# Steps:
#   1. Install tiup
#   2. Build pg-tikv
#   3. Run cargo test
#   4. Start TiKV cluster
#   5. Start pg-tikv
#   6. Run integration tests
#   7. Run ORM tests
#   8. Upload test results
```

## Debugging Test Failures

### Check pg-tikv logs
```bash
# If using run_tests.sh
cat /tmp/pgtikv-test.log

# If running manually, logs go to stdout
```

### Check TiKV cluster logs
```bash
cat ~/.pg-tikv/clusters/<name>/playground.log
```

### Run single test with verbose output
```bash
python3 scripts/integration_test.py --dsn $PG_DSN --verbose tests/18_window_functions.sql
```

### Connect directly with psql
```bash
PGPASSWORD=admin psql -h 127.0.0.1 -p 5433 -U admin -d postgres
```

### Run specific ORM test suite
```bash
cd orm-tests
npm test -- --grep "TypeORM"
npm test -- typeorm/crud.test.ts
```

## Known Limitations

Some ORM tests may fail due to pg-tikv limitations (not test infrastructure issues):

| Feature | Status | Affected Tests |
|---------|--------|----------------|
| `SAVEPOINT` | Not supported | Some transaction rollback tests |
| `DISTINCT ON` | Parser crash | Specific Prisma queries |
| `pg_indexes` | Not implemented | Schema introspection tests |
| Extended Query params | Partial | Some Prisma parameterized queries |

These are tracked as pg-tikv feature gaps, not test failures.

## Coverage Reports

### Rust Coverage
```bash
# Using cargo-llvm-cov (recommended)
cargo install cargo-llvm-cov
cargo llvm-cov --html
open target/llvm-cov/html/index.html

# Using cargo-tarpaulin (alternative)
cargo install cargo-tarpaulin
cargo tarpaulin --out Html
```

### ORM Test Coverage
```bash
cd orm-tests
npm test -- --coverage
# Reports in orm-tests/coverage/
```
