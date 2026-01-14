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

## Anti-Patterns

- **NEVER** use `u32::MAX` for TiKV scan limit → causes overflow (use `SCAN_LIMIT` constant)
- **NEVER** suppress type errors with `as any`, `@ts-ignore`
- **AVOID** `eval_expr` vs `eval_expr_join` confusion (single table vs JOIN context)

## Known Issues

- tikv/client-rust#514: scan overflow when buffer has deletes (workaround in place)
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
