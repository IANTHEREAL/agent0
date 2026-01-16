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

## Known Issues

- executor.rs and expr.rs are large (~3500 lines each)
- Window functions sort entire result set (not streaming)

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
