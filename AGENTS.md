# pg-tikv Knowledge Base

**Generated:** 2025-01-13  
**Commit:** 2f4793f  
**Branch:** master

## Overview

PostgreSQL-compatible distributed SQL database on TiKV. Implements pgwire protocol, translates SQL→KV operations.

## Structure

```
pg-tikv/
├── src/
│   ├── sql/          # SQL parsing, execution, optimization (16 files, ~15k lines)
│   ├── protocol/     # pgwire handlers, type mapping (2 files)
│   ├── storage/      # TiKV client, key encoding (3 files)
│   ├── auth/         # RBAC, password hashing (3 files)
│   ├── types/        # Value, Row, TableSchema, DataType
│   ├── pool.rs       # TiKV client pool per keyspace
│   ├── tls.rs        # TLS config
│   └── main.rs       # Server entry point
├── tests/            # 38 SQL integration tests
├── orm-tests/        # TypeORM, Prisma, Sequelize compatibility
└── scripts/          # integration_test.py
```

## Where to Look

| Task | Location |
|------|----------|
| Add SQL function | `src/sql/expr.rs` → `eval_function()` |
| Add SQL statement | `src/sql/executor.rs` → `execute_statement_on_txn()` |
| Fix type inference | `src/sql/helpers.rs` → `infer_expr_type()` |
| Add PostgreSQL type | `src/protocol/handler.rs` → `datatype_to_pgtype()` |
| Change key encoding | `src/storage/encoding.rs` |
| Add constraint | `src/sql/executor.rs` (enforced at execution time) |
| Multi-tenancy | `src/pool.rs` + username parsing in handler.rs |

## Data Flow

```
psql/ORM → pgwire (handler.rs) → SQL Parser → Executor → TiKV Store → TiKV
```

## Commands

```bash
cargo build                              # Build
cargo test                               # Unit tests (~190)
PD_ENDPOINTS=127.0.0.1:2379 cargo run    # Run server
python3 scripts/integration_test.py     # Integration tests (requires running server)
cd orm-tests && npm test                  # ORM compatibility (600+ tests)
```

## Quick Start Testing

### Automated Full Test Suite

```bash
./run_tests.sh
```

This script automatically:
1. Starts a fresh TiKV cluster
2. Builds pg-tikv in release mode
3. Runs integration tests
4. Runs ORM tests
5. Cleans up the cluster on exit

### Manual Workflow

```bash
# 1. Start TiKV cluster (persistent mode)
uv run scripts/tikv_admin.py start --name dev --persistent

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

## TiKV Cluster Management

Use `scripts/tikv_admin.py` to manage local TiKV test clusters:

```bash
# Start a cluster (persistent mode - for development)
uv run scripts/tikv_admin.py start --name dev --persistent

# Start with specific PD port
uv run scripts/tikv_admin.py start --name dev --pd-port 2379 --persistent

# List all managed clusters
uv run scripts/tikv_admin.py list

# Stop a specific cluster
uv run scripts/tikv_admin.py stop --name dev

# Clean cluster data
uv run scripts/tikv_admin.py clean --name dev
```

### Why Use tikv_admin.py?

1. **API v2 mode**: pg-tikv requires TiKV API v2 for keyspace support. The script auto-generates required config.
2. **Port discovery**: tiup playground assigns random ports. The script extracts and reports them.
3. **Cluster tracking**: Manages cluster metadata in `~/.pg-tikv/clusters/`

### Cluster Data Location

```
~/.pg-tikv/clusters/dev/
├── cluster.json      # Cluster metadata (ports, PID, mode)
├── data/             # TiKV data directory
├── playground.log    # tiup playground output
└── tikv.toml         # TiKV configuration (API v2)
```

## Design Principles

### Multi-Tenancy Isolation (CRITICAL)

**All persistent data MUST be isolated per keyspace (tenant).** This is a fundamental architectural invariant.

#### What MUST be per-keyspace:
- All `_sys_*` metadata keys (schemas, views, sequences, procedures, types)
- All user data (`t_*` table rows, `i_*` index entries)
- All auth data (`_sys_user_*`, `_sys_role_*`)
- Any future statistics/optimizer data (e.g., `_sys_stats_*`)

#### What MAY be global (process-level only):
- TikvClientPool (in-memory connection cache)
- Server configuration (PD endpoints, TLS, port)
- Logging/tracing

#### Key Prefixes (all keyspace-isolated):
| Prefix | Purpose |
|--------|---------|
| `_sys_next_table_id` | Table ID counter |
| `_sys_schema_{table}` | Table definitions |
| `_sys_schemadef_{schema}` | Schema catalog |
| `_sys_view_{name}` | View definitions |
| `_sys_matview_{name}` | Materialized views |
| `_sys_proc_{name}` | Stored procedures |
| `_sys_type_{name}` | User-defined types |
| `_sys_seqdef_{name}` | Sequence definitions |
| `_sys_seq_{id}` | Sequence current values |
| `_sys_user_{name}` | User credentials |
| `_sys_role_{name}` | Role definitions |
| `t_{table_id}_{pk}` | Table row data |
| `i_{table_id}_{idx}_{vals}` | Index entries |

#### Anti-Pattern: NEVER store cross-tenant data
```rust
// WRONG: Global key without keyspace
let key = b"_global_config_xxx";

// CORRECT: All keys go through TikvStore which applies keyspace prefix
let key = store.key(&encode_schema_key(table_name));
```

When TiKV client is created with `Config::default().with_keyspace(ks)`, all keys are automatically prefixed with the keyspace. Tenant A's `_sys_schema_users` and Tenant B's `_sys_schema_users` are completely separate keys in TiKV.

## Anti-Patterns

- **NEVER** suppress type errors with `as any`, `@ts-ignore`
- **AVOID** `eval_expr` vs `eval_expr_join` confusion (single table vs JOIN context)
- **NEVER** store data outside keyspace isolation (see Design Principles above)

## Integration Testing

### Test Framework

Tests are in `tests/` directory. Run with `./run_tests.sh` or `python3 scripts/integration_test.py`.

| File Type | Purpose |
|-----------|---------|
| `.sql` | Test input |
| `.expected` | Expected output (exact match) |
| `.assert` | Partial match (each line must appear in output) |
| `.errors` | Expected error patterns |
| `_setup.sql` | Pre-test setup |
| `_load.py` | Data loading script |

### Writing Deterministic Tests

**Problem**: TiKV row order is non-deterministic. Tests must not depend on insertion order.

#### Always add ORDER BY
```sql
-- BAD: order depends on storage
SELECT * FROM users;

-- GOOD: deterministic order
SELECT * FROM users ORDER BY id;
```

#### Use fixed values for dynamic data
```sql
-- BAD: changes every run
CREATE TABLE t (created_at TIMESTAMP DEFAULT NOW());

-- GOOD: fixed timestamp
CREATE TABLE t (created_at TIMESTAMP DEFAULT '2024-01-15 10:00:00');
```

#### Avoid selecting random/dynamic columns
```sql
-- BAD: UUID is random
SELECT * FROM users;  -- includes uuid column

-- GOOD: select specific columns
SELECT name, email FROM users ORDER BY name;

-- For UUID validation, check format not value
SELECT LENGTH(gen_random_uuid()::text) AS uuid_length;  -- always 36
```

#### Handle materialized view internal columns
```sql
-- BAD: _mv_rowid is internal and may appear
SELECT * FROM my_mv;

-- GOOD: explicit columns
SELECT col1, col2 FROM my_mv ORDER BY col1;
```

### Test File Priority (IMPORTANT)

The test framework processes files in this priority order:

1. **`.expected`** - If exists, does exact match and returns immediately
2. **`.errors`** - Only checked if no `.expected` and output contains errors
3. **`.assert`** - Only checked if no `.expected`

**Rule: Never have both `.expected` AND `.assert`/`.errors` for the same test.**
- If `.expected` exists, `.assert` and `.errors` are ignored
- Choose ONE validation method per test

### When to Use Each File Type

| Scenario | Use |
|----------|-----|
| Exact output match (including expected errors) | `.expected` |
| Dynamic execution time (EXPLAIN ANALYZE) | `.assert` |
| No `.expected`, need to allow specific errors | `.errors` |
| Non-deterministic order (last resort) | `.expected` with `# unordered` first line |

### Test File Examples

**Stable test (.expected)**:
```sql
-- test.sql
SELECT id, name FROM users ORDER BY id;
```
```
-- test.expected
id|name
1|Alice
2|Bob
(2 rows)
```

**Dynamic output (.assert)**:
```
-- Only for truly dynamic content like execution time
Seq Scan on users
Filter: (active = true)
Actual Rows:
Execution Time:
```

**Known errors (.errors)**:
```
-- One pattern per line
Unsupported function in JOIN: count
```

### Common Pitfalls

| Problem | Solution |
|---------|----------|
| Row order changes between runs | Add `ORDER BY` |
| Timestamp changes | Use fixed `DEFAULT '2024-01-15 10:00:00'` |
| UUID values differ | Don't SELECT uuid columns, or verify length |
| GROUP BY order unstable | Add `ORDER BY` on grouping columns |
| STRING_AGG order | Use `STRING_AGG(col, ',' ORDER BY ...)` |
| MV shows `_mv_rowid` | SELECT explicit columns |
| EXPLAIN costs differ | Use `.assert` for structure only |

### Adding New SQL File Tests

1. Create `tests/NN_feature_name.sql` (NN = next available number)
2. Run against **real PostgreSQL** (not pg-tikv) to generate expected output:
   ```bash
   psql -h localhost -U postgres -f tests/NN_feature_name.sql > tests/NN_feature_name.expected 2>&1
   ```
3. Run the same test against pg-tikv and compare results

### Handling Test Discrepancies (IMPORTANT)

**When pg-tikv output differs from real PostgreSQL:**

⚠️ **DO NOT immediately modify the test or expected file to make it pass.**

Instead, follow this process:

1. **Analyze the difference** - Determine which category it falls into:
   - **Bug in pg-tikv**: pg-tikv behavior is incorrect and should be fixed
   - **Intentional difference**: pg-tikv has different but valid behavior (e.g., precision, format)
   - **Missing feature**: pg-tikv doesn't support this feature yet
   - **Test issue**: The test itself has problems (non-deterministic, etc.)

2. **Document the finding** - Add to `WORK.md` with:
   - What the difference is
   - Expected (PostgreSQL) vs Actual (pg-tikv)
   - Your analysis of the root cause

3. **Ask for decision** - Let the administrator decide:
   - Fix the bug in pg-tikv?
   - Accept the difference and update `.expected`?
   - Mark as known limitation?
   - Defer to future work?

**Example workflow:**
```
# Found: pg-tikv returns '2024-01-15 10:30:00.000000' 
#        PostgreSQL returns '2024-01-15 10:30:00'
# Analysis: pg-tikv always shows 6 decimal places for timestamps
# Question: Should we fix timestamp formatting or accept this difference?
```

**Never silently change `.expected` files to hide compatibility issues.**

### Debugging Test Failures

```bash
# Check pg-tikv logs (if using run_tests.sh)
cat /tmp/pgtikv-test.log

# Run single test with verbose output
python3 scripts/integration_test.py --dsn $PG_DSN --verbose tests/18_window_functions.sql

# Connect directly with psql
PGPASSWORD=admin psql -h 127.0.0.1 -p 5433 -U admin -d postgres

# Run specific ORM test suite
cd orm-tests && npm test -- --grep "TypeORM"
```

## Known Issues

- executor.rs and expr.rs are large (~3500 lines each)
- Window functions sort entire result set (not streaming)
- `count()` in JOIN context not supported (use `.errors` file)

## TODO

- [ ] `ALTER INDEX ... RENAME TO` - Currently a stub that returns success without actually renaming the index. Need to implement: find index in table schema, update index name, persist updated schema to TiKV.

## Environment Variables

| Var | Default | Purpose |
|-----|---------|---------|
| `PD_ENDPOINTS` | 127.0.0.1:2379 | TiKV PD address |
| `PG_PORT` | 5433 | Listen port |
| `PG_KEYSPACE` | (none) | Default TiKV keyspace |
| `PGTIKV_HTTP_ALLOW_INSECURE` | false | Allow HTTP extension to make insecure http:// requests (port 80) |
| `PGTIKV_OBS_ENABLED` | true | Enable in-memory observability |
| `PGTIKV_OBS_SAMPLE_EVERY` | 1000 | Sample 1 out of N statements (plus always sample slow/errors) |
| `PGTIKV_OBS_SLOW_MS` | 200 | Always sample statements slower than this |
| `PGTIKV_OBS_MAX_SAMPLE_EVENTS` | 20000 | Max sampled events kept per tenant (still pruned to last 1h) |
| `PGTIKV_OBS_MAX_SAMPLE_GROUPS` | 50 | Max grouped sampled statements returned |
| `PGTIKV_OBS_MAX_SQL_LEN` | 512 | Max SQL length stored in samples (after normalization) |

## Child AGENTS.md

- `src/sql/AGENTS.md` - SQL execution details
- `src/protocol/AGENTS.md` - Wire protocol details
- `src/storage/AGENTS.md` - Storage layer details

## Lessons Learned (Observability v1)

- Lowest-intrusion hooks: `src/sql/executor.rs` per-statement timing + `src/sql/session.rs::commit()` for TPS (actual TiKV commits).
- Per-tenant active connections works best as a `Drop`-based guard stored in the per-connection `DynamicPgHandler` (`src/protocol/handler.rs`).
- Expose to tooling via table functions in `src/sql/executor_join.rs` (`_pgtikv_sys_observability` / `_pgtikv_sys_query_samples`), avoiding new wire-protocol/admin endpoints.
- Admin portal parsing constraint: avoid `|` and newlines in sampled SQL (portal parses pipe-delimited rows); normalize/truncate at sample time.
- Portal UX/security: bootstrap a per-tenant low-privilege observability account (`_pgtikv_sys_observer`) during tenant creation and use it for metrics queries (avoid storing/typing admin password for dashboards).
- Server-side guardrail: enforce `_pgtikv_sys_observer` can only query `_pgtikv_sys_observability()` / `_pgtikv_sys_query_samples()` in `src/sql/executor.rs`, and exclude these queries from sampling + autocommit commit counting.
- Portal backend DB client: use `pg8000` (pure Python) with a minimal wrapper (no `psql` subprocess, no custom splitter/pool) to avoid interactive password prompts and keep the portal code easy to maintain.
- Portal frontend: stop polling `/observability` on HTTP 409 (not bootstrapped) to avoid access-log spam; show a clear “need bootstrap” message instead.

## Lessons Learned (Dify pg_dump Compatibility)

- `pg_dump` emits SQL that sqlparser-rs doesn’t fully cover (e.g. `COMMENT ON EXTENSION`, flexible `CREATE SEQUENCE` option order); prefer tight, purpose-built pre-parsers/normalizers and executor fast-paths over large grammar forks.
- When validating compatibility via a real dump, add an integration test derived from the dump and include explicit cleanup to keep the shared test database clean for subsequent suites (ORM tests).
- For metadata-centric features (owners, extensions, dependencies), ensure virtual catalogs expose stable identifiers (e.g., `pg_class`/`pg_proc` OIDs) so ORMs can join reliably, while keeping all persisted metadata keyspace-isolated via the `_sys_*` keys + `TikvStore` key prefixing.
- For `COMMENT ON`, store comments in dedicated `_sys_comment_*` keys (avoid inflating frequently-read schema blobs), and expose them through `pg_catalog.pg_description` using PostgreSQL’s stable `classoid` constants so ORMs can join/filter correctly.
- Aggregate queries without `GROUP BY` must return a single row even for empty inputs; ensure JOIN and non-JOIN aggregation code paths share this behavior (caught by `COUNT(*)` over an empty join).
- For session-level compatibility (`SET`, `SHOW`, `set_config`), prefer executor-level fast-paths that update `Session` state over adding side-effectful hooks into expression evaluation (keeps the hot path stable).
- Keep per-session GUC storage compact and allocation-free by default; only allocate when a setting is explicitly changed by the client.
- Integration tests that manipulate schemas should do explicit object cleanup (drop tables then schemas) instead of relying on unsupported `... CASCADE` behavior.
- For `statement_timeout`, a per-statement `tokio::time::timeout` wrapper is a low-intrusion enforcement mechanism; use a typed error marker so the executor can reliably detect timeouts and abort open transactions.
- Integration runner detail: unaligned-mode tests concatenate `stdout` + `stderr`, so `ERROR:` lines may appear at the end of `.out`; write `.expected` ordering accordingly when asserting errors.
- For `current_setting()` / `set_config()` compatibility, prefer a tableless-`SELECT` executor fast-path that reads/writes `SessionSettings`, avoiding threading session context through the expression evaluator.
- `.expected` files are strict exact-match outputs; avoid accidental trailing blank lines when updating them.
- Driver/tool compatibility often depends on small introspection surfaces (`server_version_num`, `TimeZone`, `application_name`) and common cast patterns (`current_setting(...)::int`); handle these in fast-path logic and keep pgwire `ParameterStatus` consistent with SQL-level readbacks.
- Some client libraries probe for extension-provided types (e.g. `hstore`) at connect time; a metadata-only shim in `pg_catalog.pg_type` (with a valid `typarray` link) can unblock startup without implementing full type semantics.
- The integration suite shares a single database for all `.sql` files; tests that `CREATE EXTENSION`/persist metadata must `DROP ...` at the end to avoid breaking later expectations (e.g. `pg_extension` contents).
- For wire-protocol compatibility, never return `EmptyQueryResponse` for a successfully executed utility statement (e.g. `BEGIN`/`COMMIT`/`SAVEPOINT`/`SET`): libpq-based clients surface it as `PGRES_EMPTY_QUERY` and may treat it as an error; use `CommandComplete`, and for transaction boundaries use pgwire `TransactionStart`/`TransactionEnd` so `ReadyForQuery` carries the right transaction status.
