# Testing Patterns

**Analysis Date:** 2026-03-17

## Test Layers Overview

The project has three distinct test layers, each with its own tooling:

1. **Rust unit tests** — `cargo test` (3,917 test functions across 278 files with `#[cfg(test)]`)
2. **SQL integration tests** — `python3 scripts/integration_test.py` (878 files in `tests/`, golden-file comparison via `psql`)
3. **ORM compatibility tests** — `cd orm-tests && npm test` (Vitest, TypeScript, 7 ORM frameworks)

## Rust Unit Tests

**Runner:** `cargo test` (Rust built-in test harness)

**Run Commands:**
```bash
cargo test                                          # Run all unit tests
cargo test <test_name>                              # Run single test by name
cargo test -- --nocapture                           # Show stdout during tests
PD_ENDPOINTS=127.0.0.1:2379 cargo test <name> -- --ignored --nocapture  # Run ignored (integration) tests
```

**Test File Organization:**

Two patterns are used:

**Pattern 1 — Inline `mod tests` at file bottom** (most common for small test suites):
```rust
// src/sql/operators/key_encoding.rs (end of file)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_values_key_canonicalizes_nan_payloads() { ... }
}
```

**Pattern 2 — Separate `tests.rs` sibling file** (used for larger test suites):
```
src/sql/analyzer/
├── mod.rs
├── catalog.rs
└── tests.rs          ← included via `#[cfg(test)] mod tests;` in mod.rs

src/sql/session/
├── mod.rs
├── settings.rs
├── transaction.rs
└── tests.rs

src/sql/operators/
├── mod.rs
├── hash_join/
│   └── tests.rs
└── window/
    └── tests.rs
```

Separate `tests.rs` files exist for:
- `src/sql/analyzer/tests.rs`
- `src/sql/ddl/tests.rs`
- `src/sql/planner/tests.rs`
- `src/sql/operators/tests.rs`
- `src/sql/operators/window/tests.rs`
- `src/sql/operators/hash_join/tests.rs`
- `src/sql/explain/tests.rs`
- `src/sql/plpgsql/tests.rs`
- `src/sql/session/tests.rs`
- `src/sql/udt/tests.rs`

**Test Naming:**
```rust
// snake_case, descriptive of what is tested and expected result
fn test_pool_creation() { ... }
fn test_tenant_handle_refcount() { ... }
fn encode_values_key_canonicalizes_nan_payloads() { ... }
fn constructor_uses_left_schema_as_output_schema() { ... }
fn test_session_settings_defaults_and_overrides() { ... }
```

No strict prefix required — both `test_` prefix and descriptive names without prefix are acceptable.

## Rust Test Structure

**Standard sync test:**
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_values_key_normalizes_negative_zero() {
        let key1 = encode_values_key(&[Value::Float64(-0.0)]);
        let key2 = encode_values_key(&[Value::Float64(0.0)]);
        assert_eq!(key1, key2);
    }
}
```

**Standard async test:**
```rust
#[tokio::test]
async fn test_pool_creation() {
    let pool = TikvClientPool::new_with_timeouts(...);
    // ...
    assert!(pool.active_tenant_count().await > 0);
}
```

**Assertion macros used:**
- `assert_eq!(actual, expected)` — most common
- `assert!(expr)` — boolean assertions
- `assert!(expr.is_ok())` / `assert!(expr.is_err())` — Result checks
- `assert_eq!(err.sqlstate(), "53200")` — SQLSTATE verification

## Mocking and Stubs

**No mocking framework is used.** Isolation is achieved through:

**1. `MockCatalog` in `src/sql/analyzer/catalog.rs`:**
A full in-memory `Catalog` trait implementation with a builder API. Used in all analyzer unit tests:
```rust
// src/sql/analyzer/tests.rs
fn test_catalog() -> MockCatalog {
    MockCatalog::builder()
        .table("users", vec![
            ("id", DataType::Int32, false),
            ("name", DataType::Text, true),
        ])
        .table("orders", vec![...])
        .build()
}
```

**2. `TikvStore::new_stub()` in `src/storage/tikv_store/mod.rs`:**
A process-level singleton stub with `client: None` — TiKV operations will fail at runtime but construction succeeds. Used for tests that need a `TikvStore` reference without a live TiKV cluster:
```rust
let store = TikvStore::new_stub();
```

**3. Inline `TestOp` structs for `PhysicalOperator`:**
When testing operators that require child operators, a minimal `TestOp` struct implementing `PhysicalOperator` is defined inline in the test module:
```rust
#[derive(Debug)]
struct TestOp {
    schema: TableSchema,
}

#[async_trait]
impl PhysicalOperator for TestOp {
    fn schema(&self) -> &TableSchema { &self.schema }
    async fn open(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> { Ok(()) }
    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> { Ok(None) }
    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> { Ok(()) }
    fn name(&self) -> &'static str { "TestOp" }
}
```

**4. `#[cfg(test)]` constructor overloads:**
Production structs expose test-only constructors gated with `#[cfg(test)]` that bypass expensive initialization:
```rust
// src/pool.rs
#[cfg(test)]
pub(crate) fn new_with_rate_limit(qps_limit: u64) -> Self { ... }

#[cfg(test)]
pub(crate) fn new_with_limits(qps_limit: u64, memory_quota_bytes: usize) -> Self { ... }

// src/sql/operators/scan.rs
#[cfg(test)]
pub fn new(schema: TableSchema) -> Self { ... }
```

**5. `with_context()` async helper for extension tests (`src/extensions/context.rs`):**
```rust
#[cfg(test)]
pub(crate) async fn with_context<R>(
    is_superuser: bool,
    tenant_keyspace: &str,
    future: impl Future<Output = R>,
) -> R { ... }
```

## Test Fixtures

**Schema fixtures — inline builder functions:**
```rust
fn make_schema(name: &str, cols: &[(&str, DataType, bool)]) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        table_id: 1,
        columns: cols.iter().map(|(n, dt, nullable)| ColumnDef {
            name: (*n).to_string(),
            data_type: dt.clone(),
            nullable: *nullable,
            primary_key: false, unique: false, is_serial: false,
            default_expr: None, generation_expr: None,
            generation_expr_authorized_by: None, collation: None,
        }).collect(),
        version: 1,
        pk_constraint_name: None, pk_indices: vec![],
        indexes: vec![], check_constraints: vec![], foreign_keys: vec![],
        owner: String::new(), rls_enabled: false, rls_force: false, from_alias: None,
    }
}
```

No shared fixture files or factory crates — fixtures are defined inline in each test module.

## SQL Integration Tests

**Location:** `tests/` (878 total files)

**How it works:**
- Each test is a pair of files: `{name}.sql` (input) + one of `.expected`, `.errors`, or `.assert` (validation)
- `scripts/integration_test.py` runs `psql` against a live db9-server, captures output, and compares with the golden file
- Tests are numbered with a prefix (e.g., `01_ddl_basic.sql`, `100_create_database.sql`, `1878_http_scalar_contract.sql`)

**Validation modes (use exactly one per test):**

| File type | Description |
|-----------|-------------|
| `.expected` | Full psql output match (aligned or unaligned format auto-detected) |
| `.errors` | Output must contain this substring (used to verify error messages) |
| `.assert` | Output must contain this exact column+value pair (key=value format) |

**Priority:** `.expected` > `.errors` > `.assert` — only one file per test.

**Example — `.expected` test (`tests/01_ddl_basic.sql`):**
```sql
-- 3. Create duplicate (should fail)
CREATE TABLE t_ddl (id INT PRIMARY KEY);
```
`tests/01_ddl_basic.expected`:
```
psql:/tmp/db9-tests/tests/01_ddl_basic.sql:11: ERROR:  relation "t_ddl" already exists
id|val
1|a
2|b
(2 rows)
```

**Example — `.errors` test (`tests/108_boolean_text_where.errors`):**
```
WHERE
```
The test passes if `WHERE` appears somewhere in the error output.

**Example — `.assert` test (`tests/106_join_precedence_issue13.assert`):**
```
issue13_cnt
2
```
The test passes if the query output contains `issue13_cnt` followed by `2`.

**Determinism rules (enforced by convention):**
- Always use `ORDER BY` for any query whose output order is non-deterministic
- No random-dependent assertions
- `NULL` display: prefer `-P null=NULL` (unaligned mode) — validated in `scripts/integration_test.py`

**Golden file update rule (from CLAUDE.md):**
Before changing any `.expected`, `.errors`, or `.assert` file, run the corresponding `.sql` against real PostgreSQL 17.7 and verify the new expected output matches PG's actual output. No guessing.

## Regression Gate

**Location:** `scripts/regression_gate.sh` + `scripts/regression_gate.list`

The regression gate is the fast CI gate (target: <5 min). It runs a curated subset of SQL tests, Python multi-session tests, and ORM tests.

**`regression_gate.list` format:**
```
[sql]
tests/01_ddl_basic.sql
tests/02_dml_crud.sql
...

[python]
tests/advisory_locks_savepoint.py
tests/hnsw_large_liveness_coverage.py
...

[orm]
orm-tests/pg-client/
orm-tests/typeorm/connection.test.ts
```

**Python multi-session tests** (in `tests/*.py`): standalone scripts for concurrency and multi-connection scenarios (advisory locks, background tasks, HNSW liveness). Run with `--dsn` argument.

## ORM Compatibility Tests

**Framework:** Vitest 1.x (TypeScript)

**Config:** `orm-tests/vitest.config.ts`
- `globals: true`, `environment: node`
- `testTimeout: 30000` (30s per test), `hookTimeout: 30000`
- Sequential execution (`pool: forks`, `singleFork: true`) — avoids connection conflicts
- Output: `test-results.json` (JSON reporter)

**Run Commands:**
```bash
cd orm-tests && npm test              # Run all ORM tests
npm run test:typeorm                  # TypeORM only
npm run test:prisma                   # Prisma only
npm run test:sequelize                # Sequelize only
npm run test:knex                     # Knex only
npm run test:kysely                   # Kysely only
npm run test:drizzle                  # Drizzle only
npm run test:connection               # All connection tests
npm run test:schema                   # All schema tests
npm run test:crud                     # All CRUD tests
npm run test:transactions             # All transaction tests
```

**ORM frameworks tested:**
- TypeORM (primary, most tests) — `orm-tests/typeorm/`
- Prisma — `orm-tests/prisma/`
- Sequelize — `orm-tests/sequelize/`
- Knex — `orm-tests/knex/`
- Kysely — `orm-tests/kysely/`
- Drizzle — `orm-tests/drizzle/`
- Raw `pg` client — `orm-tests/pg-client/`

**Test structure (TypeORM example):**
```typescript
// orm-tests/typeorm/connection.test.ts
import { describe, it, expect, afterAll, beforeAll } from 'vitest';

describe('TypeORM Connection & Protocol Compatibility [db9-server]', () => {
  describe('connection establishment', () => {
    it('should establish connection via pg driver', async () => {
      const ds = createDataSource();
      await ds.initialize();
      expect(ds.isInitialized).toBe(true);
      await ds.destroy();
    });
  });
});
```

## CI Pipeline

**Defined in:** `.github/workflows/ci.yml`

**Jobs (run order):**
1. `build-release` — `cargo build --release`, uploads binary artifact
2. `lint` (parallel) — `cargo fmt -- --check` + `cargo clippy --workspace --all-targets -- -D warnings`
3. `unit-tests` (parallel) — `cargo test`
4. `regression-gate` (needs build) — `scripts/regression_gate.sh --skip-unit --skip-build`
5. `integration-tests` (needs build) — full SQL integration tests + ORM tests

**Lint scripts run in governance-lint workflow:**
- `scripts/lint_dead_code.sh` — enforces `#[allow(dead_code)]` single-line + justification tag
- `scripts/lint_error_masking.sh` — bans `unwrap_or(DataType::Text)` without `// INTENTIONAL:` comment

## Coverage

**Requirements:** No enforced coverage threshold.

**Vitest coverage available:**
```bash
cd orm-tests && npx vitest run --coverage  # provider: v8, reporters: text/json/html
```

**Rust coverage:** Not configured in CI. `scripts/coverage_line.sh` exists as a local helper but is not part of CI.

## Test Types

**Rust unit tests:**
- Scope: individual functions, expression evaluation, encoding/decoding, schema operations, session settings, pool lifecycle
- No live TiKV needed — use `TikvStore::new_stub()` or `MockCatalog`
- Async tests use `#[tokio::test]`

**SQL integration tests:**
- Scope: full end-to-end SQL execution through psql against a live db9-server + TiKV cluster
- Validate PostgreSQL protocol compatibility, SQL semantics, error message text and SQLSTATE codes

**Python multi-session tests:**
- Scope: concurrency (advisory locks, savepoints), background task behavior, HNSW liveness
- Require a live db9-server + TiKV cluster

**ORM compatibility tests:**
- Scope: ORM framework compatibility, connection pooling, DDL/DML via ORM, transactions, relation mapping
- Require a live db9-server + TiKV cluster

---

*Testing analysis: 2026-03-17*
