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

## Lessons Learned (Volcano Operator Refactoring)

- When routing queries through the operator path, validate column existence BEFORE execution, not during. Use `validate_projection_columns()` to catch errors like `SELECT nonexistent_col FROM table` early, even when the table is empty.
- Never use `.unwrap_or(Value::Null)` for expression evaluation in projections - this silently converts errors to NULL values, causing `SELECT bad_col FROM table` to return empty results instead of proper errors.
- The `is_simple_operator_query()` function acts as a gatekeeper for the operator path. Keep it conservative - queries that don't match fall through to the legacy executor, ensuring incremental migration without breaking complex queries.
- Expression validation (`is_simple_projection_expr()`, `validate_projection_columns()`) must be recursive to handle nested expressions like `CAST(col AS INT)`, `CASE WHEN`, and binary operations.
- HAVING expressions reference computed aggregates, not raw data. When evaluating `HAVING COUNT(*) > 5`, the `COUNT(*)` is a reference to an already-computed column value, not a function to execute. Use a special evaluator (`eval_having_expr_for_operators`) that maps aggregate function calls to their column indices in the aggregated row.
- Match aggregates by function name AND arguments: `COUNT(*)` and `COUNT(id)` are different. Compare function name, stringified arguments, and DISTINCT flag.
- For DISTINCT queries, operator ordering is critical: `Scan → Project → Distinct → Sort → Limit`. Applying DISTINCT to full table rows (before projection) fails when the table has a primary key - all rows appear unique. Use `ProjectOperator` to narrow columns BEFORE `DistinctOperator`.
- See `WORK.md` for detailed Volcano refactoring progress and per-phase lessons.

## Lessons Learned (Type Inference Refactoring)

- Modular type systems should expose a **compatibility wrapper** that matches the old API signature. The new `src/sql/types/` module exports `infer_expr_type(expr, schema)` that internally creates `TypeContext::single(schema)` and `TypeInferrer::new(ctx)`.
- Use `OnceLock` for global registries (like `FunctionRegistry` with 150+ function signatures) to avoid repeated initialization.
- For multi-table (JOIN) scenarios, `TypeContext::join(left_alias, left, right_alias, right)` builds a column index that detects **ambiguous columns** (same name in multiple tables) and returns proper errors.
- When handling `CompoundIdentifier` (e.g., `table.column`), try multiple resolution strategies in order: (1) full name as single column, (2) qualified lookup, (3) last identifier only. This handles both JOIN schemas with "table.col" column names and regular qualified references.
- Type coercion rules (`unify_types`, `common_type`, `binary_op_result_type`) should be in a separate module (`coercion.rs`) for reuse in expression evaluation.
- Function signatures should capture: min/max args, return type resolution strategy (fixed, same-as-arg, first-non-null, custom), and flags for aggregate/window functions.
- Use `#[cfg(test)]` for re-exporting internal helpers (`global_registry`, `unify_types`) that are only needed by unit tests.

## Lessons Learned (Expression Evaluation Refactoring)

- When unifying duplicate code paths (`eval_expr` vs `eval_expr_join`), use a **trait-based context** (`EvalContext`) to abstract differences in column resolution while keeping the evaluation logic shared.
- For large refactorings (6000+ lines, 355 call sites), provide **bridge functions** (`eval_with_context`, `eval_with_join_context`) that maintain backward compatibility while enabling gradual migration.
- Converting a single-file module (`expr.rs`) to a directory (`expr/mod.rs` + submodules) requires no changes to callers since Rust treats both as equivalent.
- Expression test files should cover edge cases but **skip known pre-existing bugs** with comments (e.g., `TRUE::INTEGER` casting, unary `+` operator) rather than polluting the test with expected failures.
- The `EvalContext` trait needs methods for: `resolve_column(name)`, `resolve_compound_identifier(parts)`, `schema()`, `is_timestamptz(expr)`, and `column_type(expr)` to fully abstract single-table vs JOIN evaluation.
- For `JoinEvalContext`, column resolution requires case-insensitive fallback searches and special handling for ORM-style aliases (e.g., Sequelize's `table->association` patterns).
- Full code deduplication requires converting helper functions (`eval_function`, `eval_substring`, `eval_extract`, etc.) to be context-aware - this is a substantial undertaking best done incrementally.

## Lessons Learned (SQLAlchemy Compatibility)

- For ORM compatibility functions like `pg_type_is_visible(oid)`, a stub returning `true` is often sufficient since ORMs use these for introspection, not critical logic.
- pgwire re-exports `postgres_types::Type` which includes all PostgreSQL array types (`Type::INT4_ARRAY`, `Type::TEXT_ARRAY`, etc.) - no need to define custom OIDs.
- When `datatype_to_pgtype()` returns `Type::TEXT` for arrays, Python drivers (psycopg2/asyncpg) receive strings instead of lists. The fix is mapping `DataType::Array(inner)` to the corresponding `Type::*_ARRAY` based on the element type.
- For trigger validation, pre-check the trigger body for unsupported functions (like FTS functions `to_tsvector`, `plainto_tsquery`) before execution. This gives clearer error messages than runtime failures.
- The `UNSUPPORTED_FTS_FUNCTIONS` list in `triggers.rs` should be case-insensitive and check both the function body text and parsed AST for comprehensive coverage.

## Lessons Learned (GIN Index for ARRAY)

- Extending GIN indexes to support ARRAY columns requires changes in 4 places: `gin.rs` (token extraction), `ddl.rs` + `dml.rs` (index maintenance), `planner.rs` (query optimization), and `executor/select.rs` (scan execution).
- The `supported_gin_index_column()` function signature changed from `Option<usize>` to `Option<(usize, bool)>` where the bool indicates whether it's an ARRAY column (vs JSON/JSONB).
- ARRAY GIN tokens use a different prefix ('A') than JSON tokens to avoid hash collisions between array elements and JSON key-values.
- The planner's `choose_gin_access_path()` must verify the column type matches the GIN-compatible types before selecting the index scan path.
- PostgreSQL overloads `@>` for both JSONB containment and ARRAY containment; the same `JsonOperator::AtArrow` AST node is used for both, distinguished by operand types at runtime.

## Lessons Learned (Full-Text Search MVP)

- sqlparser-rs parses the `@@` operator as `Expr::JsonAccess` with `JsonOperator::AtAt`, not as a `BinaryOperator`. Handle it in `eval_json_access()` before the JSON/array paths.
- For FTS boolean validation, add `JsonOperator::AtAt` to the match arms in both `boolean.rs` and `evaluator.rs` to allow `@@` in WHERE clauses.
- MVP FTS implementation uses simple space-based tokenization with lowercasing; no stemming or language-specific processing. Store tsvector as `'word':posA 'word2':pos2A` format.
- `ts_match()` in `src/sql/fts.rs` handles both `Value::Tsvector`/`Value::Tsquery` and `Value::Text` for flexibility with different evaluation paths.
- For tsquery matching, support both `&` (AND) and `|` (OR) operators; default to AND semantics when no operator present.
- `ts_rank()` returns a simple ratio: (matched_terms / total_query_terms). Production systems need TF-IDF or more sophisticated ranking.
- Function registration goes in `src/sql/expr/functions/fts.rs`, delegating to core logic in `src/sql/fts.rs` to keep function wrappers thin.

## Lessons Learned (FTS Performance Optimization)

- The `@@` operator is parsed as `BinaryOperator::PGCustomBinaryOperator(["@@"])`, NOT as `JsonOperator::AtAt` (though some code paths also emit `AtAt`). Handle both in planner/executor.
- TSVECTOR/TSQUERY types must be added to THREE separate type conversion functions: `convert_data_type()` in helpers.rs, `resolve_column_data_type()` in ddl.rs, and `sql_datatype_to_internal()` in types/infer.rs. Missing any one causes column type to default to TEXT, breaking GIN index selection.
- GIN token prefix 'T' for tsvector tokens distinguishes them from JSON ('J') and ARRAY ('A') tokens to avoid hash collisions.
- For O(n+m) FTS matching, return `HashSet<String>` from `extract_tsvector_words()` instead of `Vec<String>`. This eliminates `Vec::contains()` O(n) lookups per query term.
- The `GinColumnType` enum (`Json`, `Array`, `Tsvector`) provides clean dispatch in DML/DDL for index maintenance. Prefer explicit enum variants over boolean flags.
- Column type MUST be `DataType::Tsvector` (not `DataType::Text`) for the planner's `choose_gin_access_path()` to select GIN index scan. If columns show as TEXT in schema, GIN index won't be used.
- For planner GIN predicate extraction, handle both `Expr::BinaryOp` with `PGCustomBinaryOperator(["@@"])` and `Expr::JsonAccess` with `JsonOperator::AtAt` to catch all parser output variations.
- Binary format storage for tsvector (Sprint 5.3) is a P1 optimization - text format works correctly and can be optimized later based on profiling.
