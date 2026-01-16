# Work: DATE Type (`DATE` without time zone)

## Feature Request

Implement `DATE` type described in `docs/design/09_date_type.md`.

### MVP (P0)
- DDL: support `DATE` column type.
- Storage: represent as `DataType::Date` + `Value::Date(i32)` (days since 1970-01-01; append-only enum variants).
- DML: parse/validate inserts/updates from `'YYYY-MM-DD'` and `DATE 'YYYY-MM-DD'`.
- Query: comparisons + sorting for date (support `WHERE d >= 'YYYY-MM-DD'` by parsing RHS).
- pgwire: map to PostgreSQL `DATE` (OID 1082) and encode as `YYYY-MM-DD`.
- Introspection: `information_schema.columns` reports date correctly (no timestamp fallback).

### Non-goals (MVP)
- Full date arithmetic (`date + interval`, `age(date, date)`, etc).
- Timestamp/date timezone conversion semantics beyond truncating timestamp→date in UTC when coercing/casting.

## Agent Work Plan

### 0) Repo Work Tracking + Knowledge Base
- [x] Read existing `.codex/knowledge/*` relevant to type mapping/protocol/type inference.
- [x] Create/update `.codex/knowledge/date_type.md` with concrete facts + code locations.
- [x] Update this `WORK.md` continuously (plan + progress) while implementing.

### 1) Core Types: `DataType::Date` + `Value::Date`
- [x] Append `DataType::Date` + update `estimated_size()` + `Display`.
- [x] Append `Value::Date(i32)` + update `Value::data_type()` + `Display`.
- [x] Add shared date helpers (parse/format + timestamp→date truncation in UTC).

### 2) Storage Encoding (Index/PK)
- [x] `src/storage/encoding.rs`: memcomparable encode/decode for `Value::Date` and `DataType::Date`.

### 3) SQL DDL/DML + Casting
- [x] `src/sql/helpers.rs`: `convert_data_type(SqlDataType::Date) -> DataType::Date`.
- [x] `src/sql/helpers.rs`: `coerce_value_for_column()` parses/normalizes date input.
- [x] `src/sql/helpers.rs`: `parse_value_for_copy()` parses date for COPY.
- [x] `src/sql/expr.rs`: `Expr::TypedString { Date, .. }` evaluates to `Value::Date`.
- [x] `src/sql/expr.rs`: `cast_value(..., Date)` supports text→date and timestamp→date.
- [x] `src/sql/helpers.rs`: expression type inference handles `DATE` (casts + `CURRENT_DATE`).

### 4) Comparison/Ordering + Builtins
- [x] `src/sql/expr.rs`: `compare_values()` supports date comparisons (date/date, date/text via parse).
- [x] `src/sql/expr.rs`: `CURRENT_DATE` returns `Value::Date`.

### 5) pgwire
- [x] `src/protocol/handler.rs`: `datatype_to_pgtype(DataType::Date) => Type::DATE`.
- [x] `src/protocol/handler.rs`: `encode_value(Value::Date)` outputs `YYYY-MM-DD`.

### 6) Introspection
- [x] `src/sql/information_schema.rs`: `data_type_to_pg_type(Date) => "date"` + `columns` UDT fields.
- [x] `src/sql/information_schema.rs`: `pg_attribute.atttypid` maps Date to 1082.
- [x] `src/sql/information_schema.rs`: `pg_catalog.pg_type` includes a `date` row (OID 1082).

### 7) Tests + Verification
- [x] Add unit tests for date parse/format + timestamp truncation.
- [x] Add SQL integration coverage: `tests/44_date_type.sql` (+ `.errors` if needed).
- [x] Run `CARGO_NET_OFFLINE=true cargo test -q` (and `cargo clippy -q` if quick).

### Progress Notes
- Implemented DATE end-to-end; `CARGO_NET_OFFLINE=true cargo test -q` passes.

## Follow-up Task: Post-Implementation Review (DATE)

### Feature Request

Validate the DATE implementation against `docs/design/09_date_type.md`, and record any follow-up engineering improvements (code quality, performance, readability, modularity, test coverage).

### Agent Work Plan
- [x] Review `git diff` against the design doc + this work plan.
- [x] Run `CARGO_NET_OFFLINE=true cargo clippy -q` and confirm no DATE-specific warnings/regressions (existing warnings left unchanged).
- [x] Confirm `CARGO_NET_OFFLINE=true cargo test -q` still passes after all edits.

### Progress Notes
- No additional hardening required for MVP scope.

## Follow-up Task: Date Type Improvements (Review)

### Feature Request

Address review comments in `docs/todo/date-type-improvements.md`:
- Date +/- Interval arithmetic support.
- Date - Date subtraction returns integer days.
- Cross-type comparisons between Date and Timestamp.
- `DATE()` function returns Date (not Timestamp).
- `AGE()` accepts Date inputs (e.g., `AGE(CURRENT_DATE, birth_date)`).

### Agent Work Plan
- [x] Implement `Date + Interval` and `Date - Interval` in `src/sql/expr.rs` (`add_values`/`sub_values`).
- [x] Implement `Date - Date` returning `Int32` days in `src/sql/expr.rs` (`sub_values`).
- [x] Implement `Date` vs `Timestamp` comparisons in `src/sql/expr.rs` (`compare_values`).
- [x] Fix `DATE()` function to return `Value::Date` (both JOIN + non-JOIN function evaluators).
- [x] Fix `AGE()` function to accept `Value::Date` (convert to timestamp first).
- [x] Update type inference for date/time arithmetic (`src/sql/helpers.rs::infer_expr_type`) so pgwire OIDs are correct.
- [x] Add SQL coverage (`tests/45_date_type_improvements.sql`) and run `CARGO_NET_OFFLINE=true cargo test -q`.
- [x] Update `.codex/knowledge/date_type.md` with the new behavior + code locations.

### Progress Notes
- Implemented requested review items; `CARGO_NET_OFFLINE=true cargo test -q` passes.

---

## Archived (Previous Work)

# Work: Schemas & `search_path` (CREATE/DROP SCHEMA + schema-qualified names)

## Feature Request

Implement schemas and session-level `search_path` described in `docs/design/05_schemas_search_path.md`.

### MVP (P0)
- DDL:
  - `CREATE SCHEMA [IF NOT EXISTS] <schema>`
  - `DROP SCHEMA [IF EXISTS] <schema> [CASCADE|RESTRICT]` (MVP: enforce RESTRICT semantics)
- Name resolution:
  - Support schema-qualified object names: `schema.table` / `schema.view` / `schema.sequence` / `schema.type`
  - Unqualified names resolve via `search_path`
  - Default schema is `public`
- Session variable:
  - `SET search_path TO ...` updates per-connection search path (support `public` + custom schema list)
  - `CURRENT_SCHEMA()` reflects `search_path[0]` (not hardcoded `public`)
- Introspection:
  - `information_schema.schemata/tables/columns` list custom schemas and assign correct `table_schema`

### Non-goals (MVP)
- Full PostgreSQL `search_path` semantics (`$user`, temp schemas, etc)
- Schema ownership/privileges semantics
- Multi-database semantics (`CREATE DATABASE`)

## Agent Work Plan

### 0) Repo Work Tracking + Knowledge Base
- [x] Read existing `.codex/knowledge/*` notes relevant to name resolution (sequences/UDT already store `schema.name`).
- [x] Update `.codex/knowledge/schemas_search_path.md` with concrete facts + code locations for current implementation.
- [x] Update this `WORK.md` continuously (plan + progress) while implementing.

### 1) Session: `search_path` State + `SET search_path`
- [x] Extend `Session` with `search_path: Vec<String>` default `["public"]`.
- [x] Add accessor returning `(&mut Transaction, &mut HashMap<String,i64>, &[String])` to avoid re-borrowing session fields in executor.
- [x] Implement `Statement::SetVariable` handling for `search_path`:
  - [x] Parse list values into schema identifiers (respect quoting/case rules via `normalize_ident`).
  - [x] Allow `SET search_path TO app, public` (comma-separated list).
  - [x] Keep other `SET` variables as no-op (compat with current behavior).

### 2) Name Resolution: `ResolvedName` + Search Path Lookup
- [x] Add `ResolvedName { schema, name, full }` and normalization helpers.
- [x] DDL create (table/view/sequence/type): if unqualified, write to `search_path[0]`.
- [x] DDL drop/alter + DML/query (tables/views/types/sequences): if unqualified, resolve the first existing object in `search_path`.
- [x] Enforce MVP constraint: identifiers containing `.` are rejected for schema/object names.
- [x] Session-dependent functions:
  - [x] Thread `search_path` through all expression evaluation call-sites that can reach `eval_expr_with_sequences` / `eval_expr_maybe_sequence` (fix build errors first).
  - [x] `CURRENT_SCHEMA()` rewritten to `search_path[0]` in expression evaluation (not only FROM-clause function handling).
  - [x] `nextval/currval/setval` string/regclass arguments resolve unqualified names via `search_path` (not hardcoded `public`).

### 3) Storage: Schema Catalog + Metadata Keys Use `schema.name`
- [x] Table schema keys: store/retrieve by full name `schema.table` (and update all callers).
- [x] View keys: store/retrieve by full name `schema.view`.
- [x] Materialized view/procedure keys: store/retrieve by full name `schema.name`.
- [x] Add schema catalog metadata + store APIs:
  - [x] `create_schema(txn, schema, if_not_exists)`
  - [x] `drop_schema_restrict(txn, schema, if_exists)` (reject if any objects exist under the schema)
  - [x] `list_schemas(txn) -> Vec<String>`
- [x] Ensure built-in schemas (`public`, `pg_catalog`, `information_schema`) are always present for resolution + introspection.

### 4) DDL Wiring: Create/Drop Schema + Schema-aware DDL
- [x] Execute `Statement::CreateSchema` (persist catalog + validate name).
- [x] Execute `DROP SCHEMA` via `Statement::Drop { ObjectType::Schema, .. }`:
  - [x] Enforce RESTRICT by checking tables/views/sequences/types in that schema.
  - [x] Disallow dropping built-in schemas.
- [x] Update existing DDL paths to use resolved full names (CREATE/DROP/ALTER TABLE, CREATE/DROP VIEW, CREATE/DROP SEQUENCE, CREATE/DROP TYPE).
- [x] Foreign keys: store referenced tables as full names (`schema.table`) so FK validation works with schemas.
- [x] Generated constraint/index names stay unqualified (no `schema.` prefix) to match PG naming and keep existing tests stable.
- [x] Fix schema-qualified materialized view DDL:
  - [x] `CREATE MATERIALIZED VIEW` stores catalog + backing table under the same full name.
  - [x] `REFRESH/DROP MATERIALIZED VIEW` resolve unqualified names via `search_path`.
- [x] Fix `SELECT INTO` / `create_table_from_result` to create into `search_path[0]` when unqualified.

### 5) DML/Query Wiring: Schema-qualified + `search_path`
- [x] SELECT/JOIN: resolve `TableFactor::Table` via `search_path` (tables + views), while preserving `information_schema`/`pg_catalog` behavior.
- [x] INSERT/UPDATE/DELETE: resolve target table via `search_path`.
- [x] User-defined types: resolve UDT names via `search_path` (not hardcoded `public`).
- [x] Sequences: resolve sequence names in `nextval/currval/setval` via `search_path`.

### 6) Introspection + Builtins
- [x] Update information schema tables to understand `schema.table` keys:
  - [x] `information_schema.schemata` includes custom schemas (`store.list_schemas`).
  - [x] `information_schema.tables/columns` output correct `table_schema` and unqualified `table_name`.
  - [x] Constraint-related tables (`table_constraints`, `key_column_usage`, `referential_constraints`, `check_constraints`, `constraint_column_usage`) output correct `table_schema`/`table_name` (required by existing `tests/34_information_schema.sql`).
  - [x] `columns.column_default` for SERIAL/IDENTITY uses schema-qualified implicit sequence name (`nextval('schema.t_id_seq'::regclass)`).
- [x] Update `pg_catalog.pg_namespace` (and dependent namespace OIDs like `pg_class.relnamespace`) to include custom schemas.
- [x] FROM-clause `CURRENT_SCHEMA` uses `search_path[0]`.
- [x] Expression `CURRENT_SCHEMA()` uses `search_path[0]` (not hardcoded `public`).

### Progress Notes
- Implemented schema-qualified names + `search_path`; `cargo test` passes; added SQL integration test file for schemas/search_path.

### 7) Tests + Verification
- [x] Add `tests/41_schemas.sql` (+ `tests/41_schemas.errors`) covering:
  - [x] `CREATE SCHEMA app; CREATE TABLE app.users ...;`
  - [x] `SET search_path TO app, public;` + unqualified CREATE/SELECT resolution.
  - [x] `information_schema.schemata/tables/columns` includes `app`.
  - [x] `DROP SCHEMA app` RESTRICT fails when non-empty; succeeds after cleanup.
- [x] Add Rust unit tests for name resolution helpers (`src/sql/names.rs`).
- [x] Run `CARGO_NET_OFFLINE=true cargo test -q` and `CARGO_NET_OFFLINE=true cargo clippy -q`.
- [x] Re-read `git diff` vs this plan; add follow-up hardening tasks if any gaps remain.

### 8) Post-Plan Review (completed)
- [x] Verified features against `docs/design/05_schemas_search_path.md` and `docs/design/04_sequences.md` (MVP coverage).
- [x] Confirmed `cargo test` passes; `cargo clippy` warnings left unchanged (preexisting + not in MVP scope).
- Notes:
  - SQL integration tests (`tests/*.sql`) require running server; added `tests/41_schemas.sql` as schema/search_path regression coverage.

### 9) ORM Compatibility Regression: pgwire Describe + `RETURNING` (post-schema keys)
- [x] Reproduce failing ORM subset (pg-client + knex + drizzle).
- [x] Fix `src/protocol/handler.rs` extended-protocol Describe inference:
  - [x] Resolve table schema using `search_path` and full names (`public.table`) stored in TiKV.
  - [x] Expand `RETURNING *` to concrete columns (names + types) to avoid row/field mismatches in clients.
  - [x] Ensure OIDs match `datatype_to_pgtype()` (INT4/BOOL/JSONB/TIMESTAMPTZ, etc).
- [x] Harden placeholder substitution for `$10`+ (avoid `$1` prefix replacement bug).
- [x] Improve simple-query RowDescription to use `column_types` when result set is empty.
- [x] Run `CARGO_NET_OFFLINE=true cargo test -q`.
- [x] Run `./run_tests.sh`; remaining ORM failures: recursive CTE numeric typing + JSONB `->` typing.

### 10) ORM Advanced Queries: Recursive CTE typing + JSON operator typing
- [x] Fix CTE context schema typing (`src/sql/executor_cte.rs`) to preserve column types (not always `TEXT`).
- [x] Fix expression type inference for JSON access (`Expr::JsonAccess`) so `->` reports `JSONB` OID (and `->>` reports `TEXT`).
- [x] Run `./run_tests.sh` and confirm ORM tests pass.

### 11) Post-Implementation Review + Knowledge Base Updates
- [x] Record protocol Describe facts in `.codex/knowledge/pgwire_describe_returning.md`.
- [x] Record CTE typing + JSON access typing facts in `.codex/knowledge/sql_type_inference_cte_json.md`.
- [x] Re-check `git diff` vs implemented design docs; add follow-up hardening items if any.

## Work: EXPLAIN ANALYZE (SELECT-only)

## Feature Request

Implement `EXPLAIN (ANALYZE)` for SELECT statements as described in `docs/design/15_explain_analyze.md`.

### MVP (P3)
- Support `EXPLAIN (ANALYZE)` for `SELECT/WITH`:
  - Include existing plan tree output.
  - Include total execution time.
  - Include actual row count (at least top-level).
- When `ANALYZE` is requested for non-`SELECT/WITH` statements, return a clear error (MVP avoids DML side effects).
- Keep plain `EXPLAIN` behavior unchanged.

### Non-goals (MVP)
- `EXPLAIN (ANALYZE)` for DML (INSERT/UPDATE/DELETE).
- Accurate per-node timing/rows or statistics system integration.

## Agent Work Plan

### 0) Repo Work Tracking + Knowledge Base
- [x] Read existing `.codex/knowledge/*` for any prior EXPLAIN/plan formatting notes (if any).
- [x] Create/update `.codex/knowledge/explain_analyze.md` with concrete facts + code locations.
- [x] Update this `WORK.md` continuously (plan + progress) while implementing.

### 1) Executor Wiring: `EXPLAIN (ANALYZE)` for SELECT
- [x] Thread `sequence_values` + `search_path` into `Executor::execute_explain(...)` so ANALYZE can execute the inner query consistently.
- [x] Implement `analyze=true` behavior:
  - [x] Only allow `Statement::Query(_)` (SELECT/WITH); error otherwise.
  - [x] Execute the query once, measure total duration, compute `actual_rows`.
- [x] Ensure plain `EXPLAIN` output remains unchanged.

### 2) Output Formatting
- [x] Append ANALYZE summary lines (e.g., `Actual Rows:` and `Execution Time:`) to the existing plan text output.

### 3) Integration Test Coverage
- [x] Add `tests/48_explain_analyze.sql` exercising `EXPLAIN (ANALYZE) SELECT ...`.
- [x] If needed for stable assertions, extend `scripts/integration_test.py` to support pattern-based expected output for non-deterministic fields (timing).

### 4) Verification
- [x] Run `CARGO_NET_OFFLINE=true cargo test -q` (and `cargo clippy -q` if quick).

### Progress Notes
- `CARGO_NET_OFFLINE=true cargo test -q` passes.
- `CARGO_NET_OFFLINE=true cargo clippy -q` reports preexisting warnings (not EXPLAIN-related).

## Follow-up Task: Post-Implementation Review (EXPLAIN ANALYZE)

### Feature Request

Validate the EXPLAIN ANALYZE implementation against `docs/design/15_explain_analyze.md`, and record any follow-up engineering improvements (correctness, performance, readability, test coverage).

### Agent Work Plan
- [x] Review `git diff` vs `docs/design/15_explain_analyze.md` + the work plan above.
- [x] Ensure formatting passes: `cargo fmt -- --check`.
- [x] Ensure unit tests pass: `CARGO_NET_OFFLINE=true cargo test -q`.
- [x] Add stable integration assertions for non-deterministic output via `.assert` support in `scripts/integration_test.py`.

### Progress Notes
- MVP matches the design doc scope; remaining per-node timing/stats are deferred as planned.

## Work: CREATE FUNCTION / TRIGGER (Stage 1: store definitions)

## Feature Request

Implement Stage 1 of `CREATE FUNCTION / TRIGGER` support described in `docs/design/12_functions_and_triggers.md`:
- Accept and persist `CREATE/DROP FUNCTION` and `CREATE/DROP TRIGGER` (migration unblock).
- Add minimal pg_catalog introspection so migrations/ORMs can discover stored objects.
- Triggers do **not** execute in Stage 1 (no side effects).

### MVP (Stage 1 / P1)
- DDL:
  - `CREATE [OR REPLACE] FUNCTION ...` stores `FunctionDef` under `_sys_func_*`.
  - `DROP FUNCTION [IF EXISTS] ...` removes stored function definition(s).
  - `CREATE TRIGGER ...` stores `TriggerDef` under `_sys_trigger_*` (keyed per table + trigger name).
  - `DROP TRIGGER [IF EXISTS] ... ON <table>` removes stored trigger definition.
- Name resolution:
  - Unqualified function names default to `search_path[0]`.
  - Trigger target table resolves via `search_path` (must exist).
  - Trigger referenced function is stored as fully-qualified name (weak existence check).
- Introspection:
  - `pg_catalog.pg_proc` returns stored functions (minimal columns).
  - `pg_catalog.pg_trigger` returns stored triggers (minimal columns, joinable to tables/functions when possible).

### Non-goals (Stage 1)
- Trigger execution semantics (`BEFORE INSERT/UPDATE` row transforms).
- PL/pgSQL execution engine.
- Full signature-based overload resolution for functions.

## Agent Work Plan

### 0) Repo Work Tracking + Knowledge Base
- [x] Read existing `.codex/knowledge/*` relevant to name resolution and system catalogs.
- [x] Create/update `.codex/knowledge/functions_triggers.md` with concrete facts + code locations.
- [x] Update this `WORK.md` continuously (plan + progress) while implementing.

### 1) Core Types + Storage Catalog
- [x] Add `FunctionDef` / `TriggerDef` structs (`serde` + `bincode`) in `src/types/mod.rs`.
- [x] Add `_sys_func_` and `_sys_trigger_` key encoders in `src/storage/encoding.rs`.
- [x] Add `TikvStore` CRUD + listing APIs for functions/triggers in `src/storage/tikv_store.rs`.

### 2) Executor Wiring (DDL)
- [x] Add raw-SQL command handlers (pre-parser intercept) for:
  - [x] `CREATE [OR REPLACE] FUNCTION ...` (supports `$tag$...$tag$` bodies)
  - [x] `DROP FUNCTION [IF EXISTS] ...`
  - [x] `CREATE TRIGGER ... EXECUTE {FUNCTION|PROCEDURE} ...`
  - [x] `DROP TRIGGER [IF EXISTS] ... ON <table>`
- [x] Remove `CREATE TRIGGER not supported` pre-rejection from `src/sql/helpers.rs`.
- [x] Add `ExecuteResult` variants + pgwire tags for function/trigger DDL.

### 3) pg_catalog Introspection
- [x] Extend `src/sql/information_schema.rs`:
  - [x] Fill `pg_proc` rows from stored functions.
  - [x] Add `pg_trigger` table schema + rows from stored triggers.

### 4) Tests + Verification
- [x] Add SQL integration coverage: `tests/46_functions_triggers_ddl.sql` (+ `.assert` if needed).
- [x] Run `cargo fmt -- --check` and `CARGO_NET_OFFLINE=true cargo test -q`.

### Progress Notes
- `cargo fmt -- --check` passes.
- `CARGO_NET_OFFLINE=true cargo test -q` passes.

## Follow-up Task: Post-Implementation Review (FUNCTION/TRIGGER)

### Feature Request

Validate Stage 1 behavior against `docs/design/12_functions_and_triggers.md`, and record follow-up engineering tasks (correctness, parsing robustness, introspection coverage, performance).

### Agent Work Plan
- [x] Review `git diff` vs `docs/design/12_functions_and_triggers.md` + the work plan above.
- [x] Ensure formatting passes: `cargo fmt -- --check`.
- [x] Ensure unit tests pass: `CARGO_NET_OFFLINE=true cargo test -q`.
- [x] Fix review items from `review.md`:
  - [x] `DROP SCHEMA` RESTRICT checks `_sys_func_` and `_sys_trigger_` prefixes.
  - [x] `DROP TABLE` cleans up triggers for the table to avoid orphan metadata.
  - [x] Add regression SQL test for schema drop behavior.
- [ ] Follow-up candidates to consider next:
  - [ ] Implement Stage 2 trigger execution for a limited subset (e.g. `updated_at` assignment).
  - [ ] Implement `docs/design/07_dollar_quoted_strings.md` (allow `SELECT $$...$$` and remove `$$` pre-rejection) to reduce special-casing.
  - [ ] Extend pg_catalog coverage for functions/triggers (additional columns used by ORMs), if needed by real migrations.

### Progress Notes
- Stage 1 MVP complete; trigger execution semantics intentionally deferred.
- Review fixes applied; added regression SQL test `tests/47_schema_drop_functions.sql`.

## Archived (Previous Work)

- Sequences (CREATE SEQUENCE / nextval / SERIAL): implementation facts + code locations in `.codex/knowledge/sequences.md`.
- User-defined types (ENUM / Composite): implementation facts + code locations in `.codex/knowledge/udt_enum_composite.md`.
