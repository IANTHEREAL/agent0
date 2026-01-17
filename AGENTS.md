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
cargo test                               # Unit tests (191)
PD_ENDPOINTS=127.0.0.1:2379 cargo run    # Run server
python3 scripts/integration_test.py     # Integration tests (requires running server)
cd orm-tests && npm test                  # ORM compatibility
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

### When to Use Each File Type

| Scenario | Use |
|----------|-----|
| Exact output match | `.expected` |
| Dynamic execution time (EXPLAIN ANALYZE) | `.assert` |
| Known unsupported features | `.errors` |
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

## Known Issues

- executor.rs and expr.rs are large (~3500 lines each)
- Window functions sort entire result set (not streaming)
- `count()` in JOIN context not supported (use `.errors` file)

## Environment Variables

| Var | Default | Purpose |
|-----|---------|---------|
| `PD_ENDPOINTS` | 127.0.0.1:2379 | TiKV PD address |
| `PG_PORT` | 5433 | Listen port |
| `PG_KEYSPACE` | (none) | Default TiKV keyspace |

## Child AGENTS.md

- `src/sql/AGENTS.md` - SQL execution details
- `src/protocol/AGENTS.md` - Wire protocol details
- `src/storage/AGENTS.md` - Storage layer details
