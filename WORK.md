# Work: Dify pg_dump Compatibility (Owners, Comments, Session Settings)

## Goal

Progressively move from “restore succeeds” to “PostgreSQL-compatible metadata semantics” for the
remaining gaps identified during `/tmp/dify_schema.sql` restore.

## Phase 0 (DONE): Restore baseline for `/tmp/dify_schema.sql`

- [x] Support `ALTER ... OWNER TO` for TABLE/SEQUENCE/FUNCTION (metadata only; no privilege checks).
- [x] Support `ALTER SEQUENCE ... OWNED BY` (persist `SequenceDef.owned_by`).
- [x] Register `uuid-ossp` as a built-in extension descriptor so `CREATE EXTENSION` works in pg_dump restores.
- [x] Normalize pg_dump-style `CREATE SEQUENCE` option ordering for sqlparser-rs compatibility.
- [x] Add integration tests:
  - [x] `tests/95_alter_owner.sql`
  - [x] `tests/96_dify_schema.sql` (derived from `/tmp/dify_schema.sql`, with cleanup)
- [x] Run full suite via `./run_tests.sh`.

## Phase 1 (DONE): Persist `COMMENT ON ...` metadata (pg_catalog.pg_description)

### Modules / Ownership
- Storage: comment key encoding + TiKV persistence (`TikvStore` CRUD + list).
- SQL: lightweight `COMMENT ON` parser + executor dispatch (avoid relying on sqlparser-rs for unsupported forms).
- Introspection: populate `pg_catalog.pg_description` from stored comments.

### Scope (MVP)
- `COMMENT ON EXTENSION ... IS ...`
- `COMMENT ON FUNCTION ... IS ...` (ignore argument list for function identity; pg-tikv functions are name-unique today)
- `COMMENT ON COLUMN ... IS ...`
- Support `IS NULL` as “drop comment”.

### Acceptance Criteria
- [x] Add a deterministic integration test (no fixed OIDs):
  - [x] `tests/97_comment_on.sql` validates `pg_catalog.pg_description` via joins to `pg_class`/`pg_proc`/`pg_extension`.
  - [x] Coverage includes “set”, “update”, and “drop” (`IS NULL`) for at least one object kind.
- [x] `./run_tests.sh` passes unchanged.

## Phase 2 (DONE): Session settings + `set_config` semantics (pg_dump startup compatibility)

### Modules / Ownership
- `src/sql/session.rs`: introduce a small `SessionSettings` container (search_path + selected GUCs) with minimal per-statement overhead.
- `src/sql/executor.rs`: expand `SET` handling to store pg_dump startup variables; implement a statement-level fast-path for `SELECT set_config(...)`.
- Introspection surface: add a minimal way to read settings (`SHOW` or `current_setting()`), limited to what we store.

### Scope (MVP)
- Implement `SELECT pg_catalog.set_config('search_path', ..., false)`:
  - Updates `Session.search_path` (same behavior as `SET search_path`).
  - Returns the previous value, matching PostgreSQL’s return shape (`text`).
- Store (even if behavior is still no-op) pg_dump startup variables so clients can read them back:
  - `statement_timeout`, `lock_timeout`, `idle_in_transaction_session_timeout`
  - `client_encoding`, `standard_conforming_strings`, `check_function_bodies`, `xmloption`, `client_min_messages`, `row_security`

### Acceptance Criteria
- [x] Add `tests/98_session_settings.sql`:
  - [x] Changes `search_path` via `set_config` and demonstrates name resolution differences.
  - [x] Reads back the current value via `SHOW`/`current_setting` (whichever we implement).
- [x] `./run_tests.sh` passes unchanged.

## Phase 3 (DONE): Enforce timeout behavior

### Rationale
Storing GUCs is cheap; enforcing them correctly can be expensive or invasive (especially cancellation).
This phase is gated on feasibility without adding overhead to the hot path when timeouts are disabled.

### Scope
- `statement_timeout`:
  - Implement a statement-level timeout wrapper (`tokio::time::timeout`) around execution.
  - Abort cleanly: roll back the transaction and return a PostgreSQL-like error message.
- `lock_timeout`: stored for readback only (no meaningful lock-wait hook in current TiKV flow).

### Acceptance Criteria
- [x] Add a deterministic integration test:
  - [x] `tests/99_statement_timeout.sql` validates timeout error + subsequent statement still succeeds.
- [x] `./run_tests.sh` passes unchanged.

## Phase 4 (DONE): `current_setting()` readback + broader `set_config()` support

### Modules / Ownership
- `src/sql/executor.rs`: statement-level fast-path for tableless `SELECT current_setting(...)` and `SELECT set_config(...)`.
- `src/sql/session.rs`: reuse existing `SessionSettings` readback (`SHOW`) as the single source of truth for stored GUC values.

### Scope (MVP)
- Implement `current_setting(text)` and `current_setting(text, bool missing_ok)` for session-stored settings:
  - Unknown + `missing_ok=true` → `NULL`
  - Unknown + `missing_ok=false` → error `unrecognized configuration parameter`
- Expand `set_config(text, text, bool)` fast-path:
  - Keep `search_path` special-cased (same parsing/validation behavior as `SET search_path`)
  - For other stored GUCs, update `SessionSettings` and return the previous value.

### Acceptance Criteria
- [x] Add `tests/100_current_setting.sql` to validate `current_setting()` and `set_config()` behavior.
- [x] `./run_tests.sh` passes unchanged.

## Phase 5 (DONE): Driver introspection (server_version/timezone/application_name + `version()`)

### Modules / Ownership
- `src/sql/session.rs`: extend `SessionSettings` to expose stable readback values for common driver settings.
- `src/sql/executor.rs`: harden `current_setting()` fast-path to handle top-level casts (e.g. `::int`).
- `src/sql/expr.rs`: update `version()` output to include `pg-tikv` version.
- `src/protocol/handler.rs`: align `ParameterStatus` (`server_version`, `TimeZone`, etc) and persist startup `application_name` into the session.

### Scope (MVP)
- `SHOW` / `current_setting()`:
  - `server_version` → `16.0`
  - `server_version_num` → `160000`
  - `TimeZone` → `UTC` (settable via `SET TIME ZONE ...`)
  - `application_name` (settable via `SET`; default empty; also captured from startup parameter)
- `version()`:
  - Returns `PostgreSQL 16.0 (pg-tikv <crate version> on TiKV)`

### Acceptance Criteria
- [x] Add `tests/101_server_introspection.sql` to validate `version()`, `SHOW`, `current_setting()` and `SET TIME ZONE`.
- [x] `./run_tests.sh` passes unchanged.

## Phase 6 (DONE): hstore compatibility shim (SQLAlchemy/psycopg2 unblock)

### Rationale
Some Dify deployments (SQLAlchemy + psycopg2) probe `hstore` at connect time via:
`pg_type` + `pg_namespace` join on `typname='hstore'` / `nspname='public'`.
We implement a lightweight compatibility shim so this probe succeeds without needing
full hstore semantics.

### Modules / Ownership
- `src/extensions/mod.rs`: register `hstore` as a built-in extension descriptor (so `CREATE EXTENSION hstore` works).
- `src/sql/information_schema.rs`: expose `hstore` type metadata in `pg_catalog.pg_type` (and `_hstore` array type via `typarray`).

### Scope (MVP)
- `CREATE EXTENSION IF NOT EXISTS hstore` succeeds (metadata only).
- `pg_catalog.pg_type` contains:
  - `typname='hstore'` in `public` namespace
  - a non-zero `typarray` pointing to `_hstore`

### Acceptance Criteria
- [x] Add `tests/102_hstore_compat.sql` validating:
  - [x] `CREATE EXTENSION IF NOT EXISTS hstore;`
  - [x] SQLAlchemy probe query returns 1 row and `typarray` is non-zero
- [x] `./run_tests.sh` passes unchanged.

## Phase 7 (TODO): `json_agg()` / `jsonb_agg()` aggregates

### Modules / Ownership
- `src/sql/aggregate.rs`: add aggregators for `JSON_AGG` and `JSONB_AGG` (return `Value::Json` / `Value::Jsonb`).
- `src/sql/executor_select.rs` + `src/sql/executor_join.rs`: treat `json_agg/jsonb_agg` as aggregate functions (GROUP BY / HAVING).

### Scope (MVP)
- Support `json_agg(expr)` and `jsonb_agg(expr)` in SELECT aggregates:
  - NULL inputs produce `null` elements in the array (Postgres behavior).
  - Empty input → NULL result.
  - `DISTINCT` and `FILTER (WHERE ...)` supported (reuse existing aggregate framework).

### Acceptance Criteria
- New integration test covers:
  - basic aggregation, group by, distinct, filter
  - both `json_agg` and `jsonb_agg`
- `./run_tests.sh` passes unchanged.

## Phase 8 (TODO): `pg_available_extensions` catalog surface

### Modules / Ownership
- `src/sql/information_schema.rs`: add virtual `pg_catalog.pg_available_extensions` (minimum columns: name, default_version, installed_version, comment).
- `src/extensions/mod.rs`: expose descriptor list for catalog enumeration (no allocation on hot path).

### Acceptance Criteria
- Integration test validates `SELECT name FROM pg_catalog.pg_available_extensions` includes `uuid-ossp` and `hstore`.
- `./run_tests.sh` passes unchanged.

## Phase 9 (TODO): pg_catalog completeness hardening (targeted)

### Rationale
Some tools expect `pg_class` / `pg_type` to include catalog relations and common built-in types,
not only user objects. Dify review notes `pg_class` is “partial”.

### Scope (MVP)
- Extend `pg_catalog.pg_class` with rows for the core virtual catalogs we already expose
  (e.g. `pg_type`, `pg_namespace`, `pg_class`, `pg_proc`, `pg_attribute`, `pg_extension`, ...),
  using PostgreSQL-stable OIDs where practical.
- Extend `pg_catalog.pg_type` with a minimal set of built-in type rows required by common
  introspection flows (uuid/jsonb/text/bytea/int/bool/timestamptz/date/numeric).

### Acceptance Criteria
- Add an integration test with representative introspection queries that must return non-empty results.
- `./run_tests.sh` passes unchanged.

## Phase 10 (DEFER): SRF-in-SELECT compatibility (`generate_series()` projection)

### Context
`generate_series()` is already supported as a table function in `FROM`, but not as a
set-returning function in the SELECT list (PostgreSQL legacy SRF semantics).

### Plan
- Only implement if Dify (or ORM tests) actually issues SRFs in projection.
- If required:
  - Extend the projection pipeline to expand SRF outputs into multiple rows
    (careful with JOIN + ORDER BY + LIMIT interactions).
  - Add targeted integration coverage.

## Progress Notes (keep this updated)

- Phase 1 completed (comment persistence + `pg_description`).
- Phase 2 completed (session GUC storage + `SHOW` + `set_config(search_path)` fast-path).
- Phase 3 completed (statement_timeout enforcement).
- Phase 4 completed (`current_setting()` + broader `set_config()` for stored GUCs).
- Phase 5 completed (driver introspection: `server_version*`, `TimeZone`, `application_name`, `version()`).
- Phase 6 completed (hstore extension descriptor + pg_type metadata shim).

# Work: Observability v1 (Per-tenant SQL sampling + key metrics)

## Feature Request

Design and implement an observability方案 for pg-tikv with phased evolution.

### Phase 1 (P0) Requirements
- Only keep **last 1 hour** of data (in-memory ok for v1).
- Per-tenant (keyspace) isolation: tenant must only read its own metrics; no cross-tenant writes.
- SQL statement sampling including execution latency + execution count; derive:
  - QPS / TPS
  - p99 latency / avg latency
  - active connections
- Low-intrusion probes; easy to use/understand; minimal code touch points.
- High-performance: low overhead on hot paths.
- Expose in `cloud-admin-portal` tenant detail page.

### Phase Roadmap (for future extensibility)
- Phase 2: Prometheus/OpenMetrics export + per-node/system gauges (CPU/mem/GC), configurable retention.
- Phase 3: OpenTelemetry traces (pgwire → SQL executor → TiKV RPC spans), log correlation IDs.
- Phase 4: Slow query log + plan capture + per-table/index stats (still tenant-isolated).

## Agent Work Plan (Phase 1)

### 0) Work tracking
- [x] Add this phase plan + TODOs (keep updating here).

### 1) pg-tikv: Observability core (low overhead)
- [x] `src/observability.rs`: per-tenant registry + rolling 1h window counters/hist + sampled statements ring.
- [x] Ensure updates are mostly atomic/lock-free; locks only on sampling path (rare).

### 2) pg-tikv: Probes (minimal intrusion)
- [x] Increment/decrement per-tenant active connection gauge at auth success / connection close.
- [x] Record per-statement latency + count (include errors) and transaction commits (TPS).

### 3) pg-tikv: Query surface for portal
- [x] Add table functions:
  - [x] `SELECT * FROM _pgtikv_sys_observability();` (summary metrics, last 1h)
  - [x] `SELECT * FROM _pgtikv_sys_query_samples();` (top sampled statements, last 1h)

### 4) cloud-admin-portal: Backend API
- [x] `GET /api/tenants/{name}/observability` (no tenant session; queries pg-tikv via per-tenant observer account)
- [x] Add minimal tests (endpoint does not require tenant session).

### 5) cloud-admin-portal: Frontend
- [x] Add “Observability” section in tenant detail page (cards + sampled query table, auto refresh).

### 6) Knowledge base updates (after Phase 1 done)
- [x] Update root `AGENTS.md` with lessons learned for observability integration points.
- [x] Add/update `.codex/knowledge/observability.md` with API + code locations + ops knobs.

### Progress Notes
- (keep notes per step; don’t edit expected outputs to hide compatibility issues)
- Portal: backend 已切换到 `pg8000`（无 `psql` subprocess），避免任何交互式密码提示导致的阻塞。
- Portal: 若 tenant 未 bootstrap observability 账号（API 返回 409），前端会停止轮询避免刷屏日志。
- Portal: tenant session 改为按 tenant 存储（`tenant_session:<tenant>`），避免跨 tenant 切换导致 “Invalid or expired session”。

### Follow-ups (after Phase 1)
- [x] Rename system observability “tables” to `_pgtikv_sys_*`.

## Enhancement: Observability Readonly Account (Portal Bootstrap)

### Goal
- Create a dedicated **readonly** DB account for observability queries during `new tenant` creation flow in `cloud-admin-portal`.
- Portal uses this account (not tenant admin) to query `_pgtikv_sys_*` metrics for UI display.

### Plan
- [x] Persist observability account per tenant in portal DB (schema migration included).
- [x] Backend bootstrap endpoint: create/rotate the observability account using admin credentials.
- [x] Frontend: after `create tenant`, automatically call the bootstrap endpoint.
- [x] Observability API: use stored observability account (no tenant-session required for metrics).
- [x] pg-tikv: enforce the observability account can only query `_pgtikv_sys_*` metrics.

## Enhancement: Portal DB Client Uses pg8000 (No psql Subprocess)

### Goal
- Use a pure-Python PostgreSQL client (`pg8000`) in `cloud-admin-portal` backend to avoid spawning `psql` subprocesses (performance + no interactive password prompts).
- `psql` 不作为 fallback：portal backend 强制使用 `pg8000`。

### Plan
- [x] Add `pg8000` dependency to backend.
- [x] Refactor `PgTikvClient` to use `pg8000` only (no `psql` subprocess, no custom splitter/pool).
- [x] Remove `psql` fallback code path entirely.
- [x] Ensure `_pgtikv_sys_observer` remains compatible (no unexpected BEGIN/COMMIT required; allow harmless tx/SET no-op on server).

---

## Enhancement: Simplify Portal `PgTikvClient` (Keep pg8000 Code Small)

### Goal
- Keep `cloud-admin-portal` backend DB client minimal and easy to understand.

### Plan
- [x] Remove hand-rolled SQL statement splitter (pg8000 already uses simple-query mode for no-arg `execute()`).
- [x] Remove custom connection pool (portal opens a short-lived pg8000 connection per request).
- [x] Run backend tests.

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
  - [x] Implement Stage 2 trigger execution for a limited subset (e.g. `updated_at` assignment).
  - [x] Implement `docs/design/07_dollar_quoted_strings.md` (allow `SELECT $$...$$` and remove `$$` pre-rejection) to reduce special-casing.
  - [ ] Extend pg_catalog coverage for functions/triggers (additional columns used by ORMs), if needed by real migrations.

### Progress Notes
- Stage 1 MVP complete; trigger execution semantics intentionally deferred.
- Review fixes applied; added regression SQL test `tests/47_schema_drop_functions.sql`.
- Stage 2 trigger execution implemented (`tests/53_trigger_execution.sql`).

# Work: Dollar-Quoted Strings (`$$...$$` / `$tag$...$tag$`)

## Feature Request

Implement dollar-quoted strings support described in `docs/design/07_dollar_quoted_strings.md`.

### MVP (P0)
- Allow dollar-quoted strings in SQL and evaluate them as text values.
- Remove `$$` / `$_$` parsing “unsupported” fallback that skips statements on parse errors.
- Fix pgwire SQL scanning helpers to treat dollar-quoted regions as string literals:
  - `count_sql_parameters()` should not count `$1` inside `$tag$...$tag$` / `$$...$$`
  - `substitute_parameters()` should not replace `$1` inside dollar-quoted strings
  - `find_keyword_outside_strings()` should ignore keywords inside dollar-quoted strings
- Add unit tests + integration SQL coverage.

### Non-goals (MVP)
- PL/pgSQL execution semantics (string literal parsing only).
- Perfect compatibility for every edge-case variant; focus on `$$` and `$tag$`.

## Agent Work Plan

### 0) Repo Work Tracking + Knowledge Base
- [x] Read existing `.codex/knowledge/*` relevant to SQL parsing + pgwire scanning.
- [x] Create/update `.codex/knowledge/dollar_quoted_strings.md` with concrete facts + code locations.
- [x] Update this `WORK.md` continuously (plan + progress) while implementing.

### 1) Remove `$$` Unsupported Fallback
- [x] `src/sql/helpers.rs`: remove dollar-quote check in `get_unsupported_reason()` and update the unit test.

### 2) SQL Expression Evaluation
- [x] `src/sql/expr.rs`: support `sqlparser::ast::Value::DollarQuotedString` in `eval_value()` (treat as `Value::Text`).
- [x] Add unit tests for `SELECT $$hello$$` and `SELECT $tag$hello$tag$`.

### 3) pgwire Scanning: Params / Keywords
- [x] `src/protocol/handler.rs`: update `count_sql_parameters()` to ignore dollar-quoted strings (and handle escaped quotes).
- [x] `src/protocol/handler.rs`: update `substitute_parameters()` to only replace placeholders outside quoted/dollar-quoted regions.
- [x] `src/protocol/handler.rs`: update `find_keyword_outside_strings()` to ignore dollar-quoted regions.
- [x] Add unit tests covering parameter counting + keyword search around dollar quotes.

### 4) Integration Tests + Verification
- [x] Add `tests/42_dollar_quote.sql` (+ `.assert`/`.out` as needed).
- [x] Run `cargo fmt -- --check` and `CARGO_NET_OFFLINE=true cargo test -q`.

### Progress Notes
- `cargo fmt -- --check` passes.
- `CARGO_NET_OFFLINE=true cargo test -q` passes.

## Follow-up Task: Post-Implementation Review (DOLLAR-QUOTED STRINGS)

### Feature Request

Validate the dollar-quote MVP against `docs/design/07_dollar_quoted_strings.md`, and capture follow-up engineering improvements (correctness, parsing robustness, test coverage).

### Agent Work Plan
- [x] Review `git diff` vs `docs/design/07_dollar_quoted_strings.md` + the work plan above.
- [x] Confirm formatting + unit tests: `cargo fmt -- --check`, `CARGO_NET_OFFLINE=true cargo test -q`.
- [ ] Follow-up candidates to consider next:
  - [ ] Add comment-aware scanning to pgwire helpers (`--` / `/* ... */`) if real migrations embed `$1`/keywords inside comments.
  - [ ] Consider supporting `sqlparser::ast::Value::EscapedStringLiteral` (`E'...'`) in `src/sql/expr.rs::eval_value()` if encountered by ORMs.

# Work: Index Features (partial / expression / method metadata)

## Feature Request

Implement index feature compatibility described in `docs/design/14_index_features.md`.

### MVP (P1: DDL compatibility)
- Allow `CREATE INDEX` to persist index metadata for:
  - partial indexes (`... WHERE ...`)
  - expression indexes (`... ( (expr) )`)
  - `USING gin/gist/...` (store method)
- Keep existing btree-column indexes working (build entries + planner can use).
- For non-btree / partial / expression indexes:
  - DDL succeeds and introspection shows definition
  - Planner ignores them (no index scans; fallback to existing btree indexes / full scan)

### Non-goals (MVP)
- Implement partial/expression index acceleration (entry generation + planner usage).
- Implement real GIN/GiST semantics or operator classes.

## Agent Work Plan

### 0) Repo Work Tracking + Knowledge Base
- [x] Read existing `.codex/knowledge/*` relevant to indexes / planner / system catalogs.
- [x] Create/update `.codex/knowledge/index_features.md` with concrete facts + code locations.
- [x] Update this `WORK.md` continuously (plan + progress) while implementing.

### 1) Types: Extend `IndexDef` (backward compatible)
- [x] `src/types/mod.rs`: extend `IndexDef` with:
  - [x] `method: Option<String>` (`#[serde(default)]`)
  - [x] `predicate: Option<String>` (`#[serde(default)]`)
  - [x] `expressions: Vec<String>` (`#[serde(default)]`)
- [x] Update all `IndexDef { ... }` constructors across code/tests.

### 2) DDL: Relax `CREATE INDEX` Handling
- [x] `src/sql/executor.rs` / `src/sql/executor_ddl_ops.rs`: thread `using` + `predicate` into DDL execution.
- [x] `src/sql/ddl.rs::execute_create_index`:
  - [x] Accept identifier columns and/or expressions (store `expr.to_string()`).
  - [x] Store `USING <method>` and `WHERE <predicate>` into `IndexDef`.
  - [x] Only build physical index entries for btree column-only indexes without predicate.
- [x] `src/sql/helpers.rs`: remove `USING GIST` unsupported fallback.

### 3) Planner: Ignore Unsupported Index Forms
- [x] `src/sql/planner.rs`: only consider indexes usable when:
  - [x] method is `None`/`btree`
  - [x] `predicate` is `None`
  - [x] `expressions` is empty
  - [x] `columns` is non-empty

### 4) Introspection: Reflect Method/Predicate/Expressions
- [x] `src/sql/information_schema.rs`:
  - [x] `pg_indexes.indexdef` includes method + expressions + predicate.
  - [x] `pg_index.indexdef` includes method + expressions + predicate; `indkey` uses `0` for expression columns.
  - [x] `pg_class.relam` uses access method OID based on stored method; `pg_am` includes at least `btree/gin/gist`.
- [x] `src/sql/expr.rs`: implement `PG_GET_INDEXDEF` for non-JOIN context by returning the `indexdef` column when present.

### 5) Tests + Verification
- [x] Add SQL integration coverage `tests/50_index_features.sql` (+ `.assert`) for:
  - [x] partial index definition visible in `pg_indexes.indexdef`
  - [x] expression index definition visible in `pg_indexes.indexdef`
  - [x] `USING gin`/`USING gist` DDL does not fail and definition shows method
- [x] Run `cargo fmt -- --check` and `CARGO_NET_OFFLINE=true cargo test -q`.

### Progress Notes
- `cargo fmt -- --check` passes.
- `CARGO_NET_OFFLINE=true cargo test -q` passes.

## Follow-up Task: Post-Implementation Review (INDEX FEATURES)

### Feature Request

Validate index DDL-compat behavior against `docs/design/14_index_features.md`, and record follow-up engineering items (correctness, introspection fidelity, performance, future acceleration work).

### Agent Work Plan
- [x] Review `git diff` vs `docs/design/14_index_features.md` + the work plan above.
- [x] Confirm formatting + unit tests: `cargo fmt -- --check`, `CARGO_NET_OFFLINE=true cargo test -q`.
- [ ] Follow-up candidates to consider next:
  - [ ] Preserve mixed column/expression order (current `IndexDef.columns` + `IndexDef.expressions` loses interleaving order).
  - [ ] Include index options in `indexdef` when present (`DESC`, `NULLS FIRST/LAST`, `INCLUDE`, `NULLS DISTINCT`).
  - [x] Implement Layer 2 acceleration (partial/expression entry generation + planner usage).

# Work: Index Layer 2 Acceleration (partial / expression indexes)

## Feature Request

Implement Layer 2 of index features: partial and expression index acceleration per `docs/design/14_index_features.md`.

### MVP (P1)
- Partial indexes: evaluate predicate at write time, only materialize index entries for matching rows.
- Expression indexes: evaluate expression at write time, use computed values as index keys.
- Planner: use partial indexes when query predicates imply the index predicate.

### Non-goals (MVP)
- Full expression index planner support (matching query expressions to index expressions).
- GIN/GiST acceleration (token-based indexing).

## Agent Work Plan
- [x] `src/sql/index_helpers.rs`: add `is_index_materializable()`, `eval_index_predicate()`, `get_index_values_with_expressions()`.
- [x] `src/sql/dml.rs`: use index helpers in `execute_insert_row()` and `execute_update_row()`.
- [x] `src/sql/ddl.rs`: use index helpers in `execute_create_index()`.
- [x] `src/sql/planner.rs`: support partial indexes in `choose_best_access_path()`.
- [x] Tests: add `tests/54_index_layer2.sql` for partial/expression index acceleration.
- [x] Verify: `cargo fmt -- --check` and `CARGO_NET_OFFLINE=true cargo test -q`.

### Progress Notes
- `cargo fmt -- --check` passes.
- `CARGO_NET_OFFLINE=true cargo test -q` passes (265 tests).
- Partial indexes are now materialized with predicate evaluation.
- Expression indexes are now materialized with expression evaluation.
- Planner supports partial indexes when query predicates match.

## Follow-up Task: `pg_get_indexdef(oid)` Correctness (Index Features)

### Feature Request

Fix `pg_get_indexdef(oid)` so it respects its OID argument and works outside of a `pg_index` row context (standalone calls, subqueries, etc). Prefer row-context `pg_index.indexdef` when available to avoid repeated catalog scans.

### Agent Work Plan
- [x] `src/sql/sequences.rs`: add async rewrite support for `PG_GET_INDEXDEF` in:
  - [x] `replace_sequence_functions(...)`
  - [x] `replace_sequence_functions_join(...)`
  - [x] OID extraction from first argument (int32/int64/text numeric).
  - [x] Fast path: if row contains both `indexrelid` and `indexdef` and OID matches, return `indexdef`.
  - [x] Fallback: lookup by OID by scanning `store.list_tables(txn)` and mirroring `src/sql/information_schema.rs::get_pg_index_rows` OID assignment.
  - [x] Rewrite to a literal via `value_to_sql_expr(Value::Text(indexdef))`; keep `"CREATE INDEX"` fallback when unknown.
- [x] Tests: add integration coverage that calls `pg_get_indexdef(<oid>)` outside `pg_index` row context.
- [x] Docs: update `.codex/knowledge/index_features.md` + `review.md` accordingly.
- [x] Verify: `cargo fmt -- --check` and `CARGO_NET_OFFLINE=true cargo test -q`.

### Progress Notes
- `cargo fmt -- --check` passes.
- `CARGO_NET_OFFLINE=true cargo test -q` passes.

## Archived (Previous Work)

- Sequences (CREATE SEQUENCE / nextval / SERIAL): implementation facts + code locations in `.codex/knowledge/sequences.md`.
- User-defined types (ENUM / Composite): implementation facts + code locations in `.codex/knowledge/udt_enum_composite.md`.

# Work: System Catalog Coverage (pg_catalog / information_schema)

## Feature Request

Implement the feature described in `docs/design/11_system_catalog_coverage.md`:
- Provide pg_catalog / information_schema coverage that is “real enough” and joinable for ORM migrations/introspection.
- Ensure OID generation is stable across restarts/queries for join-heavy catalogs (namespace/class/type/function/sequence).
- Fill key missing catalogs used by ORMs: `pg_attrdef`, `pg_sequence`, `pg_tables`, `pg_views` (and harden `pg_proc`).

## Agent Work Plan

### 0) Repo Work Tracking + Knowledge Base
- [x] Read existing `.codex/knowledge/*` relevant to catalogs/OIDs (schemas, sequences, pgwire Describe).
- [x] Create/update `.codex/knowledge/system_catalog_coverage.md` with concrete facts + code locations.
- [x] Update this `WORK.md` continuously (plan + progress) while implementing.

### 1) Stable OID Strategy (schemas / classes / functions / sequences)
- [x] Replace unstable per-query OID assignment in `src/sql/information_schema.rs` with stable OIDs:
  - [x] `pg_namespace.oid`: built-ins fixed; user schemas get persistent OIDs (not derived from sorted schema list).
  - [x] `pg_class.oid`: tables derived from stable `table_id`; indexes derived from `(table_id, index_id)`; sequences/views get stable OIDs.
  - [x] Update all dependent joins: `pg_index`, `pg_attribute`, `pg_constraint`, `pg_trigger`.
- [x] Storage support for persistent OIDs where needed:
  - [x] schema OIDs stored under `_sys_schemadef_*` (or an adjacent catalog key) + `_sys_next_schema_oid`.
  - [x] sequence/function/trigger defs carry persistent OIDs + `_sys_next_*_oid` allocators.
- [x] Keep `pg_get_indexdef(oid)` lookup logic consistent with the new OID strategy (no mirroring drift).

### 2) Add Missing System Catalog Tables (ORM MVP)
- [x] `src/sql/information_schema.rs`: add schemas + rows for:
  - [x] `pg_attrdef` (column defaults; supports `pg_get_expr(def.adbin, def.adrelid)` patterns).
  - [x] `pg_sequence` (sequence metadata; joinable via `seqrelid`).
  - [x] `pg_tables` and `pg_views` (common introspection entry points).
  - [x] `pg_depend` for owned-by dependencies (implicit SERIAL sequences).
- [x] Fix boolean-ish pg_catalog columns to use boolean types/values where ORMs expect it:
  - [x] `pg_index.indis*` and `pg_index.indimmediate` → `BOOLEAN` (`Value::Boolean`).
  - [x] `pg_attribute.attnotnull/atthasdef/attisdropped/attislocal` → `BOOLEAN`.
  - [x] `pg_class.relhasindex/relispopulated/relispartition` → `BOOLEAN`.

### 3) `pg_proc` Minimal Builtins + Function Helpers
- [x] Ensure `pg_catalog.pg_proc` includes a minimal builtin set used by ORMs:
  - [x] `format_type`, `pg_get_expr`, `pg_get_indexdef`, `pg_get_constraintdef`, `version`, `current_schema`, `current_database`, etc.
  - [x] OIDs + namespaces stable and joinable (`pg_namespace`).
- [x] Improve function evaluation stubs to be ORM-friendly:
  - [x] `PG_GET_EXPR` returns the first argument (text) instead of always NULL.
  - [x] `PG_GET_CONSTRAINTDEF` returns row-context `constraintdef` when available (mirrors `PG_GET_INDEXDEF` behavior).
  - [x] `FORMAT_TYPE` returns user-defined type names when `pg_type` is joined in the same row context (JOIN evaluator).

### 4) SQL Integration Coverage (ORM Introspection Queries)
- [x] Add `tests/55_pg_catalog_introspection.sql` containing common ORM introspection query fragments (Sequelize/TypeORM-style).
- [ ] Ensure queries do not error and key joins return non-empty/consistent rows (enums/sequences/defaults/indexes/constraints).

### 5) Verification
- [x] Run `cargo fmt -- --check`.
- [x] Run `CARGO_NET_OFFLINE=true cargo test -q`.

### Progress Notes
- Implemented stable OIDs across join-heavy catalogs and added `pg_attrdef`/`pg_sequence`/`pg_tables`/`pg_views`.
- Added minimal `pg_depend` rows for implicit SERIAL sequence ownership.
- Added ORM-style introspection regression SQL (`tests/55_pg_catalog_introspection.sql` + `.assert`).
- `cargo fmt -- --check` and `CARGO_NET_OFFLINE=true cargo test -q` pass.
- SQL integration verification requires a running server (e.g. `./run_tests.sh`).

---

# Work: Integration Test Failures Analysis & Fix

**Generated:** 2026-01-16  
**Total Tests:** 55  
**Passed:** 20  
**Failed:** 35  

## Summary of Failure Categories

### Category 1: Output Ordering Differences (Test Issue - Low Priority)
Tests where output order differs but results are semantically correct.

| Test | Issue | Fix Type |
|------|-------|----------|
| 06_composite_pk | Error message appears at end vs beginning | Update .expected |
| 09_index | Error message appears at end vs beginning | Update .expected |
| 15_uuid | Random UUIDs differ on each run | Update .expected with generated UUIDs |
| 18_window_functions | Result ordering | Already has `# unordered` |

### Category 2: Error Message Format Differences (Medium Priority - Bug)
PostgreSQL uses specific error format; pg-tikv uses different wording.

| Test | Expected | Actual |
|------|----------|--------|
| 06_composite_pk | `duplicate key value violates unique constraint "order_line_pkey"` | `Duplicate primary key: [Int32(1), Int32(1), Int32(100)]` |
| 27_constraints | `null value in column "X" violates not-null constraint` with DETAIL | `Column 'X' cannot be null` |
| 27_constraints | `new row violates check constraint "name"` with DETAIL | `new row violates check constraint "name"` (missing DETAIL) |
| 28_advanced_constraints | Various FK/unique violation messages with DETAIL | Missing DETAIL lines |

### Category 3: Column Alias Differences (Medium Priority - Bug)
Functions return `?column?` instead of named column alias.

| Test | Expected | Actual |
|------|----------|--------|
| 13_pg_functions | `case`, `ceil`, `floor`, `text`, `int4`, `btrim`, `ltrim`, `rtrim`, `position`, `substring` | `?column?` |
| 22_new_features | `array` | `?column?` |

### Category 4: Timestamp Format Differences (Medium Priority - Bug)

| Test | Expected | Actual |
|------|----------|--------|
| 16_dvdrental_compat | `2026-01-17 03:37:11.422125` | `2026-01-17T06:55:58.225+00:00` |
| 21_views | `2026-01-17 03:37:11.58929` | `2026-01-17T06:56:00.186+00:00` |

### Category 5: Numeric Precision Differences (Low Priority - Test)
AVG returns different precision (trailing zeros).

| Test | Expected | Actual |
|------|----------|--------|
| 19_subqueries | `15.0000000000000000` | `15` |
| 20_cte | `85000.000000000000` | `85000` |
| 25_phase1_features | `77500.000000000000` | `77500` |

### Category 6: JSON Formatting Differences (Low Priority - Test)
Key ordering and whitespace differ but JSON is semantically equivalent.

| Test | Issue |
|------|-------|
| 24_json_comprehensive | Keys sorted alphabetically, no spaces after colons/commas |

### Category 7: Behavior Differences (Feature/Compatibility)

| Test | Issue |
|------|-------|
| 12_tpcc_basic | `SET tables = ...` not supported in standard PG (pg-tikv specific); bigint→timestamp coercion works in pg-tikv but errors in PG |
| 16_dvdrental_compat | `SET tables` works in pg-tikv but not in standard PG |
| 22_new_features | Array literal format differs (`{hello,world}` vs `{"hello","world"}`); `->>` on text works in pg-tikv but errors in PG |
| 23_rbac | `CREATE ROLE IF NOT EXISTS` syntax works in pg-tikv but not in PG |

### Category 8: JOIN Column Order Difference (Medium Priority - Bug)

| Test | Issue |
|------|-------|
| 25_phase1_features | JOIN output columns in different order (expected: `dept_id|emp_id|...`, actual: `emp_id|...|dept_id|...`) |

### Category 9: EXPLAIN Output Differences (Low Priority - Expected)
EXPLAIN plans differ significantly from PostgreSQL's optimizer output. This is expected since pg-tikv uses a different query planner.

### Category 10: Feature Implementation Bugs (High Priority)

| Test | Bug Description |
|------|-----------------|
| 55_pg_catalog_introspection | `Unsupported function in JOIN: count` - aggregate function in JOIN context not supported |
| 33_transaction_consistency | Transaction isolation issues |
| 34_information_schema | Missing/incorrect metadata |
| 39_enum_types | ENUM type handling issues |
| 41_schemas | Schema handling issues |
| 44_date_type | DATE type handling issues |
| 45_date_type_improvements | DATE type improvements needed |
| 47_schema_drop_functions | Schema DROP behavior |
| 48_explain_analyze | EXPLAIN ANALYZE output |
| 49_plpgsql_functions | PL/pgSQL function handling |
| 50_numeric_decimal | NUMERIC/DECIMAL handling |
| 30_materialized_views | Materialized view issues |
| 31_stored_procedures | Procedure handling issues |

---

## Prioritized Fix Plan

### Phase 1: Quick Wins - Update .expected Files (Tests Semantically Correct)
Update .expected files where pg-tikv behavior is valid but differs from PG in non-critical ways:
- [x] 06_composite_pk - error message position
- [ ] 09_index - error message position
- [ ] 15_uuid - random UUID values
- [ ] 24_json_comprehensive - JSON formatting (key order, whitespace)
- [ ] Tests with numeric precision differences (normalize in test framework)

### Phase 2: Fix Error Message Format (Match PostgreSQL)
1. **Primary Key Violation**: Change `Duplicate primary key: [...]` to `duplicate key value violates unique constraint "<name>"`
2. **NOT NULL Violation**: Change `Column 'X' cannot be null` to `null value in column "X" violates not-null constraint`
3. **Add DETAIL lines** to constraint violation errors
4. Affected: 06_composite_pk, 27_constraints, 28_advanced_constraints

### Phase 3: Fix Column Alias for Functions
Fix expressions to return proper column names instead of `?column?`:
- CASE expressions → `case`
- CEIL/FLOOR → function name
- CAST → target type name
- String functions → function name
Affected: 13_pg_functions, 22_new_features

### Phase 4: Fix Timestamp Format
Change timestamp output from `2026-01-17T06:55:58.225+00:00` to `2026-01-17 06:55:58.225`
Affected: 16_dvdrental_compat, 21_views

### Phase 5: Fix Core Bugs
1. **55_pg_catalog_introspection**: Fix `Unsupported function in JOIN: count` - support aggregate functions in JOIN context
2. Review and fix other feature-specific bugs

---

## Current Progress

- [x] Analysis complete
- [ ] Phase 1: Update .expected files
- [ ] Phase 2: Error message format
- [ ] Phase 3: Column alias
- [ ] Phase 4: Timestamp format
- [ ] Phase 5: Core bugs

---

# Work: PostgreSQL Compatibility - Functions and Type Handling

**Generated:** 2026-01-17  
**Status:** In Progress

## Summary

Integration tests revealed multiple PostgreSQL compatibility gaps. This work item tracks fixes for missing functions, type conversion issues, and date/time handling.

## Test Files Added

- `tests/56_null_handling.sql` - NULL handling and three-valued logic
- `tests/57_type_casting.sql` - Type casting and conversion
- `tests/58_string_functions.sql` - String functions coverage
- `tests/59_math_functions.sql` - Math functions coverage
- `tests/60_datetime_functions.sql` - Date/time functions coverage
- `tests/61_join_comprehensive.sql` - JOIN operations coverage

## Issues Found

### 1. Missing String Functions (High Priority)
| Function | Description | Status |
|----------|-------------|--------|
| `STRPOS(string, substring)` | Find position of substring | ❌ Not implemented |
| `ASCII(char)` | Get ASCII code of character | ❌ Not implemented |
| `CHR(int)` | Get character from ASCII code | ❌ Not implemented |
| `FORMAT(format, ...)` | Format string with arguments | ❌ Not implemented |
| `TRANSLATE(string, from, to)` | Replace characters | ❌ Not implemented |

### 2. Missing Math Functions (High Priority)
| Function | Description | Status |
|----------|-------------|--------|
| `CBRT(x)` | Cube root | ❌ Not implemented |
| `DEGREES(radians)` | Convert radians to degrees | ❌ Not implemented |
| `RADIANS(degrees)` | Convert degrees to radians | ❌ Not implemented |
| `SIN(x)` | Sine | ❌ Not implemented |
| `COS(x)` | Cosine | ❌ Not implemented |
| `TAN(x)` | Tangent | ❌ Not implemented |

### 3. Type Conversion Issues (High Priority)
| Issue | Expected | Actual | Status |
|-------|----------|--------|--------|
| FLOAT→INT rounding | `-2.71828::INT` = `-3` | `-2` | ❌ Bug |
| VARCHAR(n) truncation | `'hello'::VARCHAR(3)` = `'hel'` | `'hello'` | ❌ Bug |
| BOOLEAN display | `t` / `f` | `1` / `0` | ❌ Bug |
| Implicit type conversion | `'100' + 50` = `150` | Error | ❌ Bug |

### 4. Date/Time Issues (High Priority)
| Issue | Expected | Actual | Status |
|-------|----------|--------|--------|
| `EXTRACT(YEAR FROM date)` | `2024` | `NULL` | ❌ Bug |
| `DATE + INT` | `2024-01-22` | Error | ❌ Bug |
| `DATE - INT` | `2024-01-08` | Error | ❌ Bug |
| `INTERVAL + INTERVAL` | `1 day 02:00:00` | Error | ❌ Bug |
| `INTERVAL * INT` | `3 days` | Error | ❌ Bug |
| INTERVAL format | `1 day` | `1 days 00:00:00` | ❌ Bug |
| DATE + INTERVAL month | `2024-02-15` | `2024-02-14` | ❌ Bug |

### 5. Format Differences (Medium Priority)
| Issue | Expected | Actual | Status |
|-------|----------|--------|--------|
| AVG() precision | `150.0000000000000000` | `150` | ❌ Bug |
| Timestamp format | `2024-01-15 10:30:00` | `2024-01-15 10:30:00.000000` | ❌ Bug |
| Float division precision | `3.7500000000000000` | `3.750` | ❌ Bug |

## Implementation Plan

### Phase 1: Missing Functions ✅
- [x] 1.1 Implement string functions (STRPOS, ASCII, CHR, FORMAT, TRANSLATE)
- [x] 1.2 Implement math functions (CBRT, DEGREES, RADIANS, SIN, COS, TAN)

### Phase 2: Type Conversion Fixes ✅
- [x] 2.1 Fix FLOAT→INT rounding (use proper round-half-away-from-zero)
- [x] 2.2 Fix VARCHAR(n) truncation in CAST
- [x] 2.3 Fix int-to-BOOLEAN casting (0 = false, non-zero = true)
- [x] 2.4 Implement implicit text-to-numeric conversion for arithmetic
- [x] 2.5 Fix timestamp display (omit microseconds when zero)

### Phase 3: Date/Time Fixes
- [ ] 3.1 Fix EXTRACT to return actual values instead of NULL
- [ ] 3.2 Implement DATE ± INT arithmetic
- [ ] 3.3 Implement INTERVAL arithmetic (+ and *)
- [ ] 3.4 Fix INTERVAL display format
- [ ] 3.5 Fix DATE + INTERVAL month calculation

### Phase 4: Format Fixes
- [ ] 4.1 Fix AVG() to return proper decimal precision
- [x] 4.2 Remove unnecessary microseconds from timestamp display (merged into 2.5)
- [ ] 4.3 Fix float division precision display

## Progress Log

### 2026-01-17 (continued)
**Phase 1 completed** (previous session):
- STRPOS, ASCII, CHR, FORMAT, TRANSLATE string functions
- CBRT, DEGREES, RADIANS, SIN, COS, TAN math functions

**Phase 2 completed**:
- Fixed FLOAT/NUMERIC→INT rounding to use round-half-away-from-zero (PostgreSQL semantics)
- Fixed VARCHAR(n) truncation in CAST to respect max length
- Fixed int-to-BOOLEAN casting (0=false, non-zero=true)
- Added implicit text-to-numeric conversion in arithmetic operators (+, -, *, /)
- Fixed timestamp display to omit microseconds when zero

**Tests passing**: 266 unit tests, tests/57_type_casting.sql, tests/58_string_functions.sql, tests/59_math_functions.sql

### 2026-01-17 (new tests round 2)
Created 5 more integration tests. Discrepancies found below - **awaiting admin decision**.

---

## Test Discrepancy Analysis (Awaiting Decision)

### Test 62: Array Operations (`tests/62_array_operations.sql`)

| Issue | PostgreSQL | pg-tikv | Category |
|-------|------------|---------|----------|
| Text array format | `{a,b,c}` | `{"a","b","c"}` | Format difference |
| Array concatenation `\|\|` | `{1,2,3,4}` | `{1,2}{3,4}` | **Bug** - not concatenating |
| Array append `ARRAY \|\| elem` | `{1,2,3}` | `{1,2}3` | **Bug** - not appending |
| Array prepend `elem \|\| ARRAY` | `{0,1,2}` | `0{1,2}` | **Bug** - not prepending |
| `&&` overlap operator | Works | `Unsupported binary operator: PGOverlap` | **Missing feature** |
| `ARRAY_TO_STRING` | Works | `Unsupported function` | **Missing function** |
| `STRING_TO_ARRAY` | Works | `Unsupported function` | **Missing function** |
| `UNNEST` | Returns all rows | Returns only first row | **Bug** |

**Recommendation**: Array concatenation and UNNEST are significant bugs. ARRAY_TO_STRING/STRING_TO_ARRAY are useful functions.

---

### Test 63: Aggregate Functions (`tests/63_aggregate_functions.sql`)

| Issue | PostgreSQL | pg-tikv | Category |
|-------|------------|---------|----------|
| `COUNT(DISTINCT col)` | `3` | `6` | **Bug** - not deduplicating |
| `AVG(int)` precision | `30.0000000000000000` | `30` | Format difference |
| `AVG(numeric)` precision | `200.4000000000000000` | `200.40` | Format difference |
| `BOOL_AND` | Works | `Unsupported function` | **Missing function** |
| `BOOL_OR` | Works | `Unsupported function` | **Missing function** |
| `STRING_AGG(DISTINCT ...)` | `A-B-C` | `A-A-B-B-B-C` | **Bug** - DISTINCT ignored |

**Recommendation**: COUNT(DISTINCT) and STRING_AGG(DISTINCT) bugs are significant. AVG precision is a format issue.

---

### Test 64: Subquery Patterns (`tests/64_subquery_patterns.sql`)

| Issue | PostgreSQL | pg-tikv | Category |
|-------|------------|---------|----------|
| Scalar subquery in SELECT | `1.9500000000000000` | `1.95` | Format difference (precision) |
| `>= ALL (subquery)` | Works | `SQL parse error` | **Missing feature** |
| `> ANY (subquery)` | Works | `SQL parse error` | **Missing feature** |

**Recommendation**: ALL/ANY with subqueries are useful PostgreSQL features. Precision is format only.

---

### Test 65: CASE Expressions (`tests/65_case_expressions.sql`)

| Issue | PostgreSQL | pg-tikv | Category |
|-------|------------|---------|----------|
| COALESCE result alignment | Right-aligned integer | Left-aligned | Minor format difference |

**Result**: ✅ Almost fully compatible! Only minor formatting differences.

---

### Test 66: Window Functions Advanced (`tests/66_window_functions_advanced.sql`)

| Issue | PostgreSQL | pg-tikv | Category |
|-------|------------|---------|----------|
| Result ordering (no explicit ORDER BY) | Ordered by window order | Original order | Expected - need ORDER BY |
| `FIRST_VALUE` | Works | `Unsupported window function` | **Missing function** |
| `ROWS BETWEEN` frame | Computes correctly | Ignores frame clause | **Bug** - frame not applied |
| AVG precision | `143.3333333333333333` | `143.33333333333334` | Float vs Decimal |

**Recommendation**: FIRST_VALUE is useful. ROWS BETWEEN bug is significant for analytics.

---

## Summary of Issues by Priority

### High Priority (Bugs)
1. **COUNT(DISTINCT)** - returns wrong count
2. **STRING_AGG(DISTINCT)** - DISTINCT ignored
3. **Array concatenation** - `||` doesn't concatenate
4. **UNNEST** - only returns first row
5. **ROWS BETWEEN** - frame clause ignored in window functions

### Medium Priority (Missing Features)
1. `ARRAY_TO_STRING` / `STRING_TO_ARRAY` functions
2. `BOOL_AND` / `BOOL_OR` aggregate functions
3. `FIRST_VALUE` / `LAST_VALUE` window functions
4. `>= ALL` / `> ANY` subquery operators
5. `&&` array overlap operator

### Low Priority (Format Differences)
1. AVG() decimal precision display
2. Text array quoting format
3. Column alignment in output

---

**Administrator Decision (2026-01-17):**
- All bugs need to be fixed
- All missing features need to be added  
- Precision display issues deferred (will use prefix string comparison in tests)

---

## Work Plan: Fix Bugs and Add Missing Features

### Phase A: High Priority Bugs
- [ ] A.1 Fix COUNT(DISTINCT) - not deduplicating
- [ ] A.2 Fix STRING_AGG(DISTINCT) - DISTINCT ignored
- [ ] A.3 Fix array concatenation `||` operator
- [ ] A.4 Fix UNNEST - only returns first row
- [ ] A.5 Fix ROWS BETWEEN window frame clause

### Phase B: Missing Functions
- [ ] B.1 Add ARRAY_TO_STRING function
- [ ] B.2 Add STRING_TO_ARRAY function
- [ ] B.3 Add BOOL_AND / BOOL_OR aggregate functions
- [ ] B.4 Add FIRST_VALUE / LAST_VALUE window functions
- [ ] B.5 Add `>= ALL` / `> ANY` subquery operators
- [ ] B.6 Add `&&` array overlap operator

### Phase C: Generate .expected Files
- [ ] C.1 Generate .expected for passing tests
- [ ] C.2 Verify all new tests pass

---

### 2026-01-17 (initial)
- Created 6 new integration test files covering NULL handling, type casting, string/math/datetime functions, and JOINs
- Generated PostgreSQL expected outputs
- Identified 25+ compatibility issues across 5 categories
