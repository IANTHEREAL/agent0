# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

pg-tikv is a PostgreSQL-compatible distributed SQL database built on TiKV. It implements the PostgreSQL wire protocol using pgwire and translates SQL queries into TiKV key-value operations with full transaction support.

## Development Commands

### Building and Running

```bash
# Build the project
cargo build

# Run the server (requires TiKV running on 127.0.0.1:2379)
cargo run

# Start TiKV first (required dependency)
tiup playground --mode tikv-slim

# Run with custom configuration
PG_PORT=5433 PG_KEYSPACE=myapp cargo run

# Connect using psql
psql -h 127.0.0.1 -p 5433 -d postgres
```

### Testing

```bash
# Run unit tests (184 tests)
cargo test

# Run integration tests (requires server to be running)
python3 scripts/integration_test.py

# Run single SQL test file
python3 scripts/integration_test.py tests/01_ddl_basic.sql

# Run all tests in a directory
python3 scripts/integration_test.py tests/

# Run integration tests with existing server (skip setup)
python3 scripts/integration_test.py --no-setup --port 5433

# Run ORM compatibility tests (requires server running)
cd orm-tests && npm test

# Run specific ORM test suite
cd orm-tests && npm test -- --grep "TypeORM"

# Run tests with automatic cleanup script
./run_tests.sh
```

### Linting and Formatting

```bash
# Format code
cargo fmt

# Run clippy for lints
cargo clippy

# Check without building
cargo check
```

## Architecture

### High-Level Data Flow

```
PostgreSQL Client (psql/ORM)
        ↓ (PostgreSQL Wire Protocol - pgwire)
Protocol Handler (handler.rs)
        ↓ (SQL String)
SQL Parser (sqlparser-rs)
        ↓ (AST)
Executor (executor.rs)
        ↓ (Key-Value Operations)
Storage Layer (tikv_store.rs + encoding.rs)
        ↓ (gRPC)
TiKV (Distributed KV Store)
```

### Core Components

**Protocol Layer** (`src/protocol/handler.rs`):
- Implements pgwire protocol handlers: `SimpleQueryHandler`, `ExtendedQueryHandler`, `CopyHandler`
- Handles authentication with multi-tenant support (username format: `tenant.user` or `tenant:user`)
- Manages per-connection sessions and COPY protocol state
- Parses tenant from username to determine TiKV keyspace

**SQL Layer** (`src/sql/`):
- `executor.rs`: Main query execution engine. Dispatches to specialized modules (DDL, DML, query). Handles transaction coordination with auto-commit logic.
- `session.rs`: Transaction state management. Wraps TiKV pessimistic transactions with PostgreSQL semantics (BEGIN/COMMIT/ROLLBACK).
- `parser.rs`: SQL parsing using sqlparser-rs with PostgreSQL dialect
- `expr.rs`: Expression evaluation (arithmetic, comparisons, functions, subqueries)
- `aggregate.rs`: Aggregation functions (COUNT, SUM, AVG, MIN, MAX)
- `window.rs`: Window function evaluation (ROW_NUMBER, RANK, LAG/LEAD, etc.)
- `planner.rs`: Query optimization including index scan selection
- `helpers.rs`: Utility functions extracted from executor to reduce file size
- `information_schema.rs`: Virtual tables for schema introspection (tables, columns, views)

**Storage Layer** (`src/storage/`):
- `tikv_store.rs`: TiKV client wrapper with keyspace support for multi-tenancy
- `encoding.rs`: Key-value encoding scheme (see Key Layout below)
- `pool.rs`: Connection pooling for TiKV clients

**Type System** (`src/types/mod.rs`):
- `Value`: Runtime value representation (Int, Text, Bool, Timestamp, UUID, Json, etc.)
- `Row`: Collection of values
- `TableSchema`: Table metadata with columns, constraints, indexes
- `DataType`: SQL type definitions with PostgreSQL aliases

### Key Storage Layout

```
System keys:
  _sys_next_table_id                     → u64 (auto-increment)
  _sys_schema_{table_name}               → TableSchema (bincode serialized)
  _sys_view_{view_name}                  → View definition SQL
  _sys_matview_{matview_name}            → Materialized view definition
  _sys_proc_{procedure_name}             → Stored procedure definition

Table data:
  t_{table_id}_{pk_values}               → Row (bincode serialized)

Indexes:
  i_{table_id}_{index_id}_{index_values} → pk_values (unique index)
  i_{table_id}_{index_id}_{index_values}_{pk} → empty (non-unique index)
```

Primary keys are encoded using lexicographic ordering to enable range scans. Multi-column PKs are concatenated with delimiters.

### Transaction Model

- Uses TiKV pessimistic transactions for ACID guarantees
- Auto-commit mode: Each statement runs in its own transaction unless within explicit BEGIN/COMMIT block
- Session-scoped transaction state managed in `Session` struct
- Support for SELECT FOR UPDATE (pessimistic locking)

### Identifier Handling

- Identifiers are case-folded to lowercase following PostgreSQL conventions (unless quoted)
- Table names, column names, and object names normalized in parser.rs
- Schema names default to "public" when omitted
- Quoted identifiers preserve case

### Constraint Enforcement

All constraints are enforced at execution time in `executor.rs`:
- PRIMARY KEY: Checked on INSERT, enforced via unique key encoding
- UNIQUE: Auto-creates secondary index, enforced on INSERT/UPDATE
- NOT NULL: Validated before row write
- CHECK: Evaluated as boolean expression per row
- FOREIGN KEY: Full referential integrity with CASCADE/SET NULL/SET DEFAULT/RESTRICT actions
- DEFAULT: Evaluated during INSERT if column omitted

### Multi-Tenancy

Multi-tenancy is implemented using TiKV's native keyspace API:

- **Keyspace** (`PG_KEYSPACE` or `tenant.user` login): Each tenant gets a separate TiKV keyspace, providing true physical isolation with separate resource management.
- Username format: `tenant.user` or `tenant:user` extracts the keyspace from the username (e.g., `myapp.admin` → keyspace=`myapp`, user=`admin`)
- Default keyspace can be set via `PG_KEYSPACE` environment variable
- Each keyspace has its own TiKV client connection (pooled in `pool.rs`)

## Development Patterns

### Adding a New SQL Feature

1. Parse: Extend `parse_sql()` in `parser.rs` if needed (most features use sqlparser-rs directly)
2. Execute: Add handling in `executor.rs` `execute_statement_on_txn()` or relevant sub-module (ddl.rs, dml.rs, query.rs)
3. Storage: Update key encoding in `encoding.rs` if new metadata structures are needed
4. Test: Add SQL test file in `tests/` directory with `.sql` extension

### Adding a New Function

Built-in functions are implemented in `expr.rs` in the `eval_function()` method:
1. Add function name to match statement
2. Implement function logic using `Value` enum operations
3. Handle NULL values appropriately
4. Add test cases in `tests/13_pg_functions.sql`

### Adding a New Data Type

1. Add variant to `DataType` enum in `types/mod.rs`
2. Add variant to `Value` enum with serialization support
3. Update parser in `infer_data_type()` helper for SQL type name mapping
4. Update `value_to_sql_expr()` for value literal conversion
5. Update comparison and arithmetic operations in `expr.rs` if needed

### Test Structure

Integration test files (`tests/*.sql`) are plain SQL scripts that run sequentially. The test runner (`scripts/integration_test.py`) executes each file against a running server. Tests should be idempotent when possible (use IF EXISTS, DROP before CREATE).

ORM tests (`orm-tests/`) verify compatibility with real-world TypeScript ORMs including TypeORM, Prisma, Sequelize, Knex, and Drizzle.

## Important Notes

- The project uses `sqlparser-rs` in visitor mode - AST traversal, not compilation
- TiKV client uses pessimistic transaction mode exclusively
- The server is single-binary with no external dependencies except TiKV/PD
- Connection pooling is handled in `pool.rs` using lazy initialization
- Extended Query protocol (prepared statements) is fully supported via pgwire
- COPY protocol support enables `pg_restore` compatibility for bulk data loading
- Window functions require post-processing after initial query execution (implemented as a separate evaluation pass)
