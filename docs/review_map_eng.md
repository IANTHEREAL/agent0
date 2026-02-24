# db9 / db9-server Global Map (v0)

> Goal: produce an “actionable navigation map” (direction first, details later). First provide global layering and how to locate the primary paths; later tasks will deepen and add evidence and details.  
> Scope: **the entire repository**, but **heavily weighted toward the Rust core service**; `cloud-admin-portal/` is treated as a separate block (written separately in both horizontal and vertical dimensions).

---

## 0.0 Methodology (Feynman technique + scientific method)

> Rule: only write facts that can be proven by code. If an explanation contains “should / probably / maybe”, immediately downgrade it to `UNKNOWN`, and record “what to read to falsify/verify it”.

### 0.0.1 Feynman technique: explain the system clearly

- For every module/path, answer: **what is the input**, **what is the output**, **where does state live**, **how do failures converge**, **what are the invariants**
- Any “concept gaps” exposed while explaining become a question list (instead of filling holes with guesses)

### 0.0.2 Scientific method: hypothesis → evidence → conclusion

- **Hypothesis sources**: README / design docs / WORK notes / issues
- **Evidence sources**: key entrypoints read file-by-file / function-by-function, key branch conditions, persisted keys, return formats, error paths (this map targets module/function granularity)
- **Write conclusions**: only when evidence points to a specific code location; otherwise keep `UNKNOWN`

### 0.0.3 Deliverable split: horizontal map + vertical primary flows

- **Horizontal (Architecture Map)**: list entrypoints and boundaries by layer (main/pool/tls/protocol/sql/txn/storage/auth/extensions/obs/portal)
- **Vertical (Primary Flows)**: list end-to-end primary flows by external behavior (entry chain + state ownership + failure boundaries)
- **Priority Lenses**: map high-priority review topics (SQL correctness / multi-tenant isolation & security / performance & observability) onto “nodes” and “paths”

### 0.0.4 Map positioning: high-dimensional navigation map / reading index (not a spec)

This document is positioned as a “high-dimensional navigation map / reading index”, not a “representative implementation spec / compatibility conclusion”.

- Suitable for:
  - Helping reviewers quickly locate entrypoints and primary paths (module/function level)
  - Establishing shared vocabulary (module boundaries, key terms, key branching points)
  - Deciding what to read next (choose files/functions by path & risk)
- Not suitable for:
  - Proving feature coverage, semantic details, or compatibility conclusions
  - Proving security boundaries / isolation are already correct (these must converge via code + tests)

### 0.0.5 Coverage status tags (module/function granularity)

To make the map more representative of the implementation at module/function granularity, each Flow/module is tagged with a coverage status, and we explicitly split “facts” vs “risks/assumptions”:

- `Verified`: key entrypoint functions and core branches have been read; usable as navigation facts (still not a semantic spec)
- `Partial`: entry chain / key branching points have been read, but internal semantics/edge cases are not converged; only usable for “finding the path”
- `Unknown`: implementation has not been read; this section is only a naming/index clue and should not be used for conclusions

Writing conventions:
- Primary Flows: each flow starts with `Status: ...`, and is split into `Facts` (code-proven) / `Risks/Assumptions` (to validate / high-risk / might be overturned)
- Architecture Map: module items are inline-tagged with `(Verified/Partial/Unknown)`; `Facts/Risks` can be added gradually as needed

## 0. How to use this map (recommended reading)

1. Read **Primary Flows** first: walk end-to-end chains by your priority (entry → state → storage → return/failure boundary).
2. For each flow, go back to **Architecture Map** to cross-check: are boundaries clear, is state ownership sane, are invariants upheld?
3. Use **Priority Lenses** (SQL correctness / multi-tenant isolation & security / performance & observability) to tag nodes and run checklists.

### Tag conventions (will be expanded over time)

- `correctness`: SQL semantic correctness (as close to PostgreSQL as possible)
- `tenancy`: multi-tenant isolation (keyspace routing + all persistence must be tenant-isolated)
- `security`: authn/authz/escape surface (RBAC, extensions, portal, input handling)
- `perf`: performance hot spots (scan/sort/join/window, network round-trips, serialization)
- `obs`: observability (sampling, stats, diagnostic table functions, portal dependencies)

---

## 0.4 Path Index (Key Paths, for “quickly finding the functional spine”)

> Only the “path skeleton + key file entrypoints”. Each path should let you locate the corresponding modules; details are deferred to later tasks.

- [Verified] Flow 1 Connection / tenant / auth: `src/main.rs` → `src/protocol/handler.rs` (Startup/auth + tenant routing) → `src/pool.rs` (keyspace client) → `src/storage/tikv_store.rs` + `src/auth/*`
- [Partial] Flow 2 Simple Query: `src/protocol/handler.rs` `SimpleQueryHandler::do_query()` → `src/sql/executor.rs` `Executor::execute()` → `src/sql/session.rs` (txn glue) → `execute_statement_on_txn()` (dispatch to DDL/DML/SELECT)
- [Partial] Flow 3 Extended Query: `src/protocol/handler.rs` `ExtendedQueryHandler::do_query()` → `substitute_parameters()` → `Executor::execute()` (ultimately reuses the Simple execution chain)
- [Partial] Flow 4 Transactions / Savepoints: `src/sql/executor.rs` (statement-level autocommit glue) + `src/sql/session.rs` (BEGIN/COMMIT/ROLLBACK) + `src/txn/*` (savepoints task-local)
- [Partial] Flow 5 DDL: `src/sql/executor.rs` `execute_statement_on_txn()` → `src/sql/ddl.rs`/`src/sql/sequences.rs`/`src/sql/udt.rs`/`src/sql/executor_extensions.rs` → `src/storage/*` (schema/catalog keys)
- [Partial] Flow 6 DML: `src/sql/executor.rs` `execute_{insert,update,delete}` → `src/sql/executor_dml_ops.rs` + `src/sql/dml.rs` → `src/storage/*` (row/index keys) → triggers (see Flow 10)
- [Partial] Flow 7 SELECT: `src/sql/executor.rs` `execute_query()` → `src/sql/executor_select.rs` `execute_query_with_ctes()` → (no FROM → tableless / has JOIN → `src/sql/executor_join.rs` / single table → planner+scan) → `src/sql/expr.rs`/`src/sql/aggregate.rs`/`src/sql/window.rs`
- [Verified] Flow 8 COPY: `src/protocol/handler.rs` (COPY parse + CopyHandler) → `src/sql/executor.rs` `execute_copy_insert()` → `src/storage/*`
- [Partial] Flow 9 Extensions/http: SQL table function → `src/sql/executor_extensions.rs` → `src/extensions/http.rs` (outbound requests + restrictions) → rowset
- [Partial] Flow 10 Async AFTER triggers: `src/main.rs` `spawn_trigger_worker()` → `src/sql/trigger_worker.rs` (consume queue) ← `src/sql/executor_dml_ops.rs`/`src/sql/triggers.rs` (produce events) + diagnostics tables: `src/sql/executor_join.rs` (`_db9_sys_trigger_queue_stats/_db9_sys_trigger_dlq`)
- [Verified] Flow 11 Observability: record `src/sql/executor.rs` (`record_statement`) + `src/sql/session.rs` (`record_commit`) → `src/observability.rs` (rolling window + samples) → sys tables `src/sql/executor_join.rs` (`_db9_sys_observability/_db9_sys_query_samples`) → portal (dashboard polling)
- [Partial] Portal (admin plane): `cloud-admin-portal/backend/app/api/*.py` (tenant CRUD + connect/session + obs/query) ↔ `cloud-admin-portal/backend/app/services/{pd_client,pg_client}.py` ↔ `cloud-admin-portal/frontend/src/*` (`X-Tenant-Session` + polling)

---

## 1. Architecture Map (horizontal: layers & subsystems)

> This section answers: what are module boundaries, where are entrypoints, where does state live, what depends on what, and what invariants must be upheld.

### 1.1 Rust Core Service (`src/`)

**Entry / Bootstrap**
- `src/main.rs` (Verified): server startup, TLS, listen socket, per-connection `DynamicHandlerFactory`, spawns background worker (`sql::trigger_worker`)
  - Automatic keyspace creation (startup keyspace only): if `TikvClientPool::get_client()` error string contains `"does not exist"`, call PD HTTP API `POST /pd/api/v2/keyspaces` to create and retry once
- `src/tls.rs` (Verified): TLS acceptor construction
- `src/pool.rs` (Verified): `TikvClientPool` (get/cache TiKV client per keyspace)
  - When `keyspace == "default"`, it is converted to TiKV `"DEFAULT"` (case / cache-key consistency risk)

**Protocol (pgwire)**
- `src/protocol/handler.rs` (Verified) (`~3211`): pgwire handler, per-connection session, authentication, tenant routing (`tenant.user` / `tenant:user`), Simple/Extended Query/COPY
  - Tenant parsing: `parse_tenant_username()` only splits on `.` / `:`; no case/validity normalization
  - Auth: `StartupHandler::on_startup()` always uses `CleartextPassword`
  - Executor/session init: `DynamicPgHandler::init_executor()` (keyspace priority: username prefix first, then `PG_KEYSPACE`, then `"default"`)
  - **Security-critical point (needs explicit verification)**: if `authenticate_user()` sees `"gRPC"` / `"transport"` in the auth bootstrap error string, it returns `Ok((true, true))` (allow + superuser); design intent/threat model must be confirmed
  - Extended Query (implementation skeleton read):
    - `impl ExtendedQueryHandler for DynamicPgHandler`:
      - `do_query()`: take `statement` string from `Portal`, substitute parameters via `substitute_parameters()`, then call `executor.execute(session, final_query)` (extended protocol ultimately reuses the simple execution path)
      - `do_describe_statement()/do_describe_portal()`: infer result fields via `infer_result_fields_from_query(...)`; if parameter types are insufficient, fill with `Type::UNKNOWN`
    - Parameter substitution: `substitute_placeholders_outside_strings_and_dollar()` substitutes `$1/$2/...` only in non-string / non-dollar-quoted regions; values are decoded by `param_type` and quoted/escaped as needed (a correctness/security hotspot; this map only records the entrypoint/mechanism)
- `src/protocol/copy_format.rs` (Verified): COPY format handling

**SQL Engine**
- `src/sql/parser.rs` (Unknown): sqlparser-rs Postgres dialect → AST
- `src/sql/executor.rs` (Verified): statement dispatch + autocommit/transaction glue (core dispatch point)
  - Statement-level task-local: `statement_time::with_timestamps(statement_ts, transaction_ts, ...)` + `txn::with_savepoints(session.savepoints(), ...)` (affects `now()`/time functions, `transaction_timestamp()`, and SAVEPOINT rollback log)
    - Fact: `statement_ts = now_timestamp_millis()` is computed once at the beginning of `Executor::execute()` and wraps the entire multi-statement loop; `UNKNOWN`: whether this matches PostgreSQL semantics for `statement_timestamp()`/`now()` (read `src/sql/statement_time.rs` + the time function implementations)
  - Pre-parse interception (bypasses sqlparser AST):
    - `CREATE/DROP EXTENSION`, `CREATE/DROP FUNCTION`, `CREATE/DROP TRIGGER`
    - `REFRESH/DROP MATERIALIZED VIEW`, `CREATE/DROP PROCEDURE`, `CREATE TYPE ... AS ENUM`, `DROP TYPE`
  - Multi-statement & error/skip strategy:
    - `parse_sql(sql)` returns `Vec<Statement>`, executed one-by-one and aggregated into `ExecuteResults(Vec<ExecuteResult>)` (Simple Query Protocol: “one result per statement”)
    - In the pre-parse phase: if `get_skip_reason(sql_upper)` hits, return `ExecuteResult::Skipped`; if parsing fails but `get_unsupported_reason(sql_upper)` hits, also return `Skipped`; otherwise record a failed sample and return an error
  - Each statement execution is wrapped by: `extensions::context::with_context(is_superuser, async { ... })` (extension permission model; verify scope with `src/extensions/context.rs`)
  - Autocommit strategy: outside explicit transactions, each statement is `BEGIN` → execute → on success `COMMIT` / on failure `ROLLBACK`; but observability sys queries use `ROLLBACK` on autocommit success (avoid counting commits/TPS)
  - `SET search_path`: special-cased only in `Statement::SetVariable`; most other `SET` variables are effectively no-op
    - Allowed value forms: `Identifier` / `CompoundIdentifier(len==1)` / `'a,b'` single-quoted string (split by comma); other expressions error out
    - Normalization: drop `$user`; `["default"]` becomes `["public"]`; schema names containing `.` are rejected; empty list defaults to `["public"]`
  - `DROP TABLE IF EXISTS ...`: for each missing table, generate an `ExecuteResult::Notice` before actual execution (`collect_notices_before_statement()`, message like `table "<name>" does not exist, skipping`)
  - Observability account restriction: `_db9_sys_observer` (non-superuser) is limited to:
    - Single-table, no-JOIN queries from `_db9_sys_observability/_db9_sys_query_samples`
    - Tableless queries (`SELECT 1` without FROM; still restricted: no WITH/locks/subqueries)
    - Fact: for this account, transaction control statements (`BEGIN/COMMIT/ROLLBACK/SAVEPOINT/SET ...`) return `Empty` and `continue` directly in `Executor::execute()` (do not enter `session.*` branches); they appear allowed but have no effect. `UNKNOWN`: whether this intentionally prevents session state changes (verify with portal expectations and handler behavior).
  - Tableless query (`execute_tableless_query()`): allows a small set of SRF/functions (`UNNEST/regexp_split_to_table/regexp_matches/jsonb_*`) and `pg_sleep`; for multi-column SRF, rows are “zipped” by max length (short arrays padded with `NULL`)
  - Set operations (`UNION/INTERSECT/EXCEPT`): merged in-memory via `query::apply_union/apply_intersect/apply_except`; returns `column_types: None` (type inference / protocol mapping needs separate verification)
  - `EXPLAIN`:
    - `EXPLAIN (ANALYZE)`: only supports `Statement::Query`, executes for real and appends `Actual Rows/Execution Time`
    - Plan-side schema lookup: `list_tables()` + `get_schema()`; `row_count_lookup` is fixed at `1000` (plan quality and EXPLAIN credibility need separate assessment)
  - `execute_copy_insert()`: if session is not in an explicit transaction, each call does `BEGIN` → insert → on success `COMMIT` / on failure `ROLLBACK`; and explicitly writes all index entries after insert (COPY statement atomicity is covered in Flow 8)
- `src/sql/executor_select.rs` (Partial), `src/sql/executor_join.rs` (Partial), `src/sql/executor_subquery.rs` (Unknown), `src/sql/executor_cte.rs` (Unknown): SELECT execution paths
- `src/sql/ddl.rs` (Unknown), `src/sql/dml.rs` (Unknown), `src/sql/executor_*_ops.rs` (Partial): DDL/DML execution and concrete KV operations
- `src/sql/expr.rs` (Unknown): expressions and built-in functions (largest file; typical correctness hotspot)
- `src/sql/aggregate.rs` (Unknown), `src/sql/window.rs` (Unknown): aggregates and window functions (correctness + perf hotspot)
- `src/sql/planner.rs` (Unknown): planning / index selection (perf + correctness)
- `src/sql/session.rs` (Verified): SQL session / transaction semantics (intertwines with `src/txn/*`)
- `src/sql/sequences.rs` (Unknown), `src/sql/triggers.rs` (Unknown), `src/sql/trigger_queue.rs` (Unknown), `src/sql/trigger_worker.rs` (Unknown): sequences / triggers / async queue worker
- `src/sql/information_schema.rs` (Unknown): system catalog / information schema (compat surface)

**Transaction**
- `src/txn/state.rs` (Partial), `src/txn/savepoints.rs` (Partial): transaction state and savepoint semantics

**Storage**
- `src/storage/tikv_store.rs` (Verified): TiKV wrapper (one of the keyspace isolation anchor points)
  - Keyspace isolation mechanism: `TikvStore::new_with_keyspace()` creates the client via `tikv_client::Config::with_keyspace(ks)`; `TikvStore::key()` is just `to_vec()` (no prefix/namespace)
  - Schema catalog:
    - Built-in schemas: `public` / `pg_catalog` / `information_schema` / `extensions` (`TikvStore::is_builtin_schema`)
    - `list_schema_oids()` uses fixed built-in OIDs: `pg_catalog=11`, `public=2200`, `information_schema=13222`, `extensions=2201`; user schema OIDs live under `_sys_schemadef_*`, are allocated on miss and written back (see `next_schema_oid()`)
  - `get_schema(txn, table_name)`: looks up `_sys_schema_<table_name>` using the **input string as-is** (no `public.` completion / `search_path` logic; callers must resolve/fully-qualify names)
  - PK conflict errors: `insert()` returns a Postgres-style error string (with `DETAIL`) when the key already exists; constraint name is `<short_table>_pkey` where short table is `table_name` without schema
- `src/storage/encoding.rs` (Verified): key encoding / layout (the “foundation” of all persisted schema/data/index)

**Auth / RBAC**
- `src/auth/*` (Verified): users/roles/privileges (intertwines with system keys; `tenancy + security` hotspot)
- `src/sql/rbac.rs` (Unknown): SQL-layer RBAC glue (needs verification of division of responsibilities vs `src/auth/*`)

**Extensions**
- `src/extensions/*` (Verified) + `src/sql/executor_extensions.rs` (Verified): built-in extensions (especially `http`: concentrated `security + tenancy + obs` risk)
  - `src/extensions/mod.rs`: extension descriptors (built-in OID/version/default schema) + per-tenant install state `InstalledExtension` (serialized/persisted into `_sys_*` metadata keys; specific key paths need confirmation in store/DDL)
  - `src/extensions/context.rs`: per-statement task-local `ExtensionContext { is_superuser, http_requests }`
    - `with_context(is_superuser, ...)`: wrapped per statement by the executor (avoid plumbing session state everywhere)
    - `try_consume_http_request(max)`: per-statement counter; on overflow errors with `http: max_requests_per_statement exceeded`
  - `src/extensions/http.rs`: `http_*` table functions (outbound HTTP only)
    - Privilege: `execute_table_function()` requires `context::is_superuser()==true`, otherwise `permission denied for extension "http"`
    - SSRF / security restrictions (`validate_url()`): only `http/https`; `http` is disabled by default (requires `DB9_HTTP_ALLOW_INSECURE=true`); no userinfo; only default ports (`https:443` / `http:80`); disallow `localhost/*.localhost/*.local`; disallow direct or DNS-resolved loopback/private/link-local/unspecified IPs
    - Resource limits:
      - max `5` requests per statement (via `ExtensionContext`)
      - max `20` concurrent requests per tenant per node (`Semaphore`, in-process)
      - connect timeout `1s`, overall timeout `5s`
      - request body ≤ `256KiB`; response body ≤ `1MiB`
      - redirects: max `3`; for `301/302/303`, switch to `GET` and clear body/content-type
    - Response constraints: response must be UTF-8; headers are serialized as a JSON array `[{field,value}, ...]`
  - `src/sql/executor_extensions.rs`:
    - `Executor::try_execute_extension_table_function(...)`: identify/execute `extensions.http_*` table functions
      - Name resolution: explicit schema must be `extensions`; if schema is omitted, `search_path` must contain `extensions` (case-insensitive) or it returns `None` (treated as a normal function/table)
      - Parameter rules (http example): `http_get/head/delete(url text)`, `http_post/put(url text, body text, content_type text)`; parameters are evaluated via `expr::eval_expr(expr, None, None)` and must be non-NULL `TEXT`
      - Install/enable: reads `store.get_extension(txn, \"http\")`; not installed → `extension \"http\" is not installed`; disabled → `extension \"http\" is disabled`
      - Execution: `http::execute_table_function(self.tenant_keyspace(), call).await?` (guarded by `ExtensionContext` + limiter)
      - Alias: supports `AS t(col1,...)` to rename output columns; mismatched column count errors out
    - `execute_create_extension_cmd()/execute_drop_extension_cmd()`:
      - Parsing: string scanning (strip comments, trim, case-insensitive), and parse `extensions.http`-style prefix into name=`http`
      - Privilege: only superuser may create/drop
      - `CREATE EXTENSION` validates that the default schema (`extensions`) exists (`store.schema_exists()`)

**Observability**
- `src/observability.rs` (Verified): in-memory observability & sampling (`obs + perf`), plus support for portal/diagnostic queries
  - Config (env):
    - `DB9_OBS_ENABLED` (default true)
    - `DB9_OBS_SAMPLE_EVERY` (default 1000; 1/N sampling)
    - `DB9_OBS_SLOW_MS` (default 200ms; slow queries are always sampled)
    - `DB9_OBS_MAX_SAMPLE_EVENTS` (default 20000; sample event ring cap)
    - `DB9_OBS_MAX_SAMPLE_GROUPS` (default 50; max groups after aggregation)
    - `DB9_OBS_MAX_SQL_LEN` (default 512; normalized SQL is truncated and suffixed with `…`)
  - `ObservabilityRegistry::tenant(keyspace)`: get/cache `Arc<TenantObservability>` by keyspace (empty keyspace maps to `"default"`)
  - `TenantObservability`:
    - `connection_open()`: increments `active_connections`, decremented on Drop (approximate connection count)
    - `record_statement(latency, ok, sql_supplier)`: updates rolling window and samples into `VecDeque<SampleEvent>` by rules
      - Sampling rules: all errors are sampled; all slow queries are sampled; otherwise sample if `fast_rand_u64()%sample_every==0`
      - `normalize_sql()`: collapse whitespace → single spaces, strip trailing `;`, trim, truncate; replace `|` with space (portal pipe-delimited compatibility)
    - `record_commit()`: updates rolling window commit count (TPS source)
    - `snapshot_summary()`: statement/commit/error stats in window, QPS/TPS, avg/p99 latency, active connections
    - `snapshot_query_samples()`: aggregate by `fingerprint(fnv1a_64(normalized_sql))`, compute avg/p99/max and last_seen

**Types**
- `src/types/*` (Unknown): `Value/Row/TableSchema/DataType` etc (cross-cutting correctness + encoding)

### 1.2 Cloud Admin Portal (`cloud-admin-portal/`)

> This section builds a separate horizontal layering: backend / frontend / deploy / scripts, and marks boundaries + trust model when interacting with the core service.

**Backend (`cloud-admin-portal/backend/`)**
- Status: Partial
- Entry: `cloud-admin-portal/backend/app/main.py` (Verified) `create_app()` (FastAPI + CORS; no global auth middleware/dependency observed)
- Tenant sessions: `cloud-admin-portal/backend/app/session.py` (Verified)
  - `SessionManager` generates `session_id` as `ts_<hex>`, default TTL is 1 hour (`Settings.session_ttl_hours`)
  - Session contains `admin_user/admin_password` (in-memory; lost on process restart)
- PG client: `cloud-admin-portal/backend/app/services/pg_client.py` (Verified)
  - Uses `pg8000` (pure Python) to connect to db9-server; username is composed as `tenant.user` (dot-separated)
  - `_run_sql()` formats results like `psql -t -A` (join columns with `|`, join rows with `\n`) for observability/SQL editor parsing
  - Observability reads by querying `_db9_sys_observability()` / `_db9_sys_query_samples()` and splitting by `|` (therefore server-side sampling must avoid `|` in SQL; see replacement logic in `src/observability.rs`)
  - Key APIs (no global auth observed):
  - `cloud-admin-portal/backend/app/api/tenants.py` (Verified)
    - `POST /api/tenants/{tenant_id}/connect`: validate admin credentials, return `session_id` (frontend stores in `sessionStorage`, subsequent requests use `X-Tenant-Session`)
    - `POST /api/tenants/{tenant_id}/query`: requires `X-Tenant-Session`, executes arbitrary SQL using the admin credentials stored in the session
    - `POST /api/tenants/{tenant_id}/observability/bootstrap`: create/rotate `_db9_sys_observer` using admin credentials, store password in portal DB (observability API then uses that account)
    - `GET /api/tenants/{tenant_id}/observability`: read observer username/password from DB and query (endpoint itself does not require `X-Tenant-Session`)
  - `cloud-admin-portal/backend/app/api/users.py` (Verified): user management APIs require `X-Tenant-Session`
  - `cloud-admin-portal/backend/app/api/system.py` (Verified), `cloud-admin-portal/backend/app/api/audit.py` (Verified): health/info/audit logs without auth observed

**Frontend (`cloud-admin-portal/frontend/`)**
- Status: Partial
- Base fetch: `cloud-admin-portal/frontend/src/api/client.ts` (Verified)
  - Requests to `/tenants/<id>/...` automatically inject `X-Tenant-Session` from `sessionStorage["tenant_session:<id>"]`
- Session hook: `cloud-admin-portal/frontend/src/hooks/useTenantSession.tsx` (Verified)
  - On successful connect, writes `session_id` into `sessionStorage`
- Observability polling: `cloud-admin-portal/frontend/src/api/tenants.ts` (Verified) `useTenantObservability()`
  - Polls every 5s by default; stops polling on HTTP 409 (not bootstrapped)

**Deploy/Scripts**
- `cloud-admin-portal/deploy/`: deployment (secrets, env vars, network topology, default permissions)
- `cloud-admin-portal/scripts/`: ops scripts (dev/build/bootstrap, etc.)

---

## 2. Primary Flows (vertical: main functional paths)

> This section answers: where an external behavior enters, how state flows, where TiKV reads/writes happen, where auth/isolation is applied, and how failures converge.

### 2.1 Core Service Top Flows (first-cut list)

1. **Connection + Tenant Routing + Auth** (`tenancy + security`)
2. **Simple Query** (`Query` message → parse/execute → rowset/tag) (`correctness`)
3. **Extended Query** (Parse/Bind/Describe/Execute/Synchronize) (`correctness + security`)
4. **Autocommit vs Explicit Transaction + Savepoints** (`correctness`)
5. **DDL** (schema/table/index/view/matview/sequence/type/extension) (`correctness + tenancy`)
6. **DML** (INSERT/UPDATE/DELETE + RETURNING + constraints) (`correctness + tenancy`)
7. **SELECT Engine** (scan/index, join, agg, window, subquery, CTE) (`correctness + perf`)
8. **COPY / pg_restore path** (bulk import, format, error recovery) (`correctness + perf`)
9. **Extensions: http** (privilege model, request limits, output spec, sampling/audit) (`security + tenancy + obs`)
10. **Async AFTER triggers worker** (queue consistency, retry/DLQ, cross-tenant fairness) (`correctness + tenancy + obs`)
11. **Observability sys functions** (sampling, grouping, truncation/SQL normalization) (`obs + security`)

### 2.1.1 Flow 1: Connection + Tenant Routing + Auth

- Status: Verified
- Facts (key entrypoints/branches read):
  - Socket accept: in `src/main.rs`, each connection calls `pgwire::tokio::process_socket()` and creates a `DynamicHandlerFactory` for that connection
  - Startup: `src/protocol/handler.rs` `StartupHandler::on_startup()`
    - Read `METADATA_USER`, `parse_tenant_username()` → `(keyspace, actual_user)`
    - Write connection metadata: `METADATA_KEYSPACE` / `METADATA_ACTUAL_USER`
    - Send `Authentication::CleartextPassword` (cleartext password challenge)
  - Password: `StartupHandler::on_startup()` `PasswordMessageFamily` branch
    - `authenticate_user(keyspace, actual_user, password)` → `(is_authenticated, is_superuser)`
    - On success, `init_executor(keyspace, actual_user, is_superuser)` initializes `Executor` + `Session`
  - Keyspace priority: `username prefix` > `PG_KEYSPACE` > `"default"`
  - `parse_tenant_username()` only splits on `.`/`:`; no case/validity normalization
  - **Auth bootstrap fallback**: if `authenticate_user()` sees `"gRPC"` / `"transport"` in the error string, it returns `Ok((true, true))` (“authenticated + superuser”)
- Risks/Assumptions (to validate / needs experiments):
  - Trigger conditions, design intent, and security boundary for the `"gRPC"/"transport"` allow-and-superuser fallback (verify with `src/auth/*` + deployment assumptions)
  - Impact of missing tenant/user normalization on keyspace/cache/authorization boundaries (verify with `src/pool.rs` cache keys + connection experiments)
  - pgwire error responses and logging paths for auth failure / tenant missing / bootstrap branches (continue validating in `src/protocol/handler.rs`)

### 2.1.2 Flow 2: Simple Query (Query → parse/execute → results)

- Status: Partial
- Facts (executor core dispatch read):
  - Entry: `src/protocol/handler.rs` `SimpleQueryHandler::do_query()` (Simple Query Protocol `Query` message entrypoint; calls `Executor::execute()`)
  - `Executor::execute(session, sql)`:
    - `strip_leading_sql_comments(sql)` + `trim_start()` drives “starts_with” checks (affects whether pre-parse interception hits)
    - A small set of commands take the “pre-parse interception” path (e.g. `CREATE EXTENSION`/`CREATE FUNCTION`/`CREATE TRIGGER`), returning `ExecuteResults::single(...)` (bypasses `sqlparser` AST)
    - `parse_sql(sql)` → `Vec<Statement>`; execute each statement and append results into one `ExecuteResults(Vec<ExecuteResult>)`
  - Transaction/failure boundaries:
    - Transaction control statements (`BEGIN/COMMIT/ROLLBACK/SAVEPOINT/...`) directly call `session.*` and return `Empty`
    - Other statements use `is_autocommit = !session.is_in_transaction()` to decide statement-level autocommit wrapping (see Flow 4)
    - `DROP TABLE IF EXISTS ...` may generate `Notice` before execution (one notice per missing table)
  - Observability sampling: for non-observability sys queries, after each statement execution it calls `TenantObservability::record_statement(elapsed, ok, || stmt.to_string())`; parse failures are also recorded as a failed sample (`Duration::from_millis(0)`)
- Risks/Assumptions (to validate / needs experiments):
  - How Simple Query results map into pgwire message sequences (RowDescription/DataRow/CommandComplete/ReadyForQuery…) requires verifying the detailed branches in `src/protocol/handler.rs`
  - Client compatibility impact of non-typical results like `Skipped` / `Notice` (verify how protocol layer encodes these)

### 2.1.3 Flow 3: Extended Query (Parse/Bind/Describe/Execute → final SQL → execute)

- Status: Partial
- Facts (primary execution chain read):
  - Entry: `src/protocol/handler.rs` `impl ExtendedQueryHandler for DynamicPgHandler`
    - `do_query(portal, ...)`: take raw SQL string from portal → `substitute_parameters(query, portal)` → `Executor::execute(session, final_query)`
    - `do_describe_statement` / `do_describe_portal`: infer result fields from SQL string (`infer_result_fields_from_query`); fill missing param types with `Type::UNKNOWN`
  - Execution property: the extended protocol ultimately “executes a substituted SQL string” (no separate prepared-plan executor), so behavior/limits are bound to `Executor::execute()`
  - Return property: `do_query()` maps only `ExecuteResults.last()` to a pgwire `Response`
- Risks/Assumptions (to validate / needs experiments):
  - Accuracy and logic of `infer_result_fields_from_query()` (read the implementation body in `src/protocol/handler.rs`)
  - Correctness/security implications of placeholder substitution (`substitute_parameters`) quoting/type rules (validate with parameter decoding and escaping logic branch-by-branch)

### 2.1.4 Flow 4: Autocommit vs Explicit Transaction + Savepoints

- Status: Partial
- Facts (transaction control entrypoints + autocommit glue read):
  - Applies to:
    - Simple Query: default path within `Executor::execute(session, sql)`
    - COPY FROM: `Executor::execute_copy_insert(session, ...)`
  - Transaction control statements:
    - `Statement::StartTransaction` → `session.begin().await?`
    - `Statement::Commit` → `session.commit().await?`
    - `Statement::Rollback { savepoint: None }` → `session.rollback().await?`
    - `Statement::Savepoint { name }` → `session.create_savepoint(normalize_ident(name))?`
    - `Statement::Rollback { savepoint: Some(name) }` → `session.rollback_to_savepoint(&sp).await?`
    - `Statement::ReleaseSavepoint { name }` → `session.release_savepoint(&sp)?`
  - Autocommit glue: for non-transaction-control statements, use `is_autocommit = !session.is_in_transaction()` to decide whether to wrap in a txn
    - `true`: `session.begin()` → execute → on success `session.commit()` / on failure `session.rollback()`
    - `false`: no automatic commit/rollback; on failure, return error upward
  - Special case: observability sys queries use `session.rollback()` on autocommit success (avoid counting commits/TPS)
  - Savepoints task-local: `Executor::execute()` wraps the whole execution in `crate::txn::with_savepoints(session.savepoints(), ...)`
- Risks/Assumptions (to validate / needs experiments):
  - Whether a failure transitions the transaction into an error state and whether subsequent statement behavior matches PostgreSQL (verify `src/sql/session.rs` error paths + rollback strategy)
  - Who consumes the savepoints task-local and how it affects rollback / trigger queue / sequences (read `src/txn/*` + trigger implementation)

### 2.1.5 Flow 5: DDL (CREATE/ALTER/DROP → catalog/store)

- Status: Partial
- Facts (statement dispatch skeleton read):
  - Entry: `Executor::execute()` (Simple/Extended both end up here) → `execute_statement_on_txn(...)`
  - Major DDL branches in `execute_statement_on_txn(...)`:
    - `CREATE TABLE` → `src/sql/ddl.rs` (or `CREATE TABLE AS` via executor internal `execute_create_table_as`)
    - `CREATE INDEX` / `DROP INDEX` / `ALTER TABLE` / `DROP TABLE/VIEW` / `TRUNCATE` → executor methods + `src/sql/ddl.rs`
    - `CREATE/DROP SCHEMA` → `TikvStore::{create_schema,drop_schema_restrict}`
    - `CREATE SEQUENCE` / `DROP SEQUENCE` → `src/sql/sequences.rs`
    - `CREATE TYPE` → `src/sql/udt.rs`
    - `CREATE VIEW` / `CREATE MATERIALIZED VIEW` / `DROP MATERIALIZED VIEW` / `REFRESH MATERIALIZED VIEW` → executor + `src/sql/ddl.rs`
    - RBAC DDL: `CREATE ROLE` / `ALTER ROLE` / `GRANT` / `REVOKE` / `DROP ROLE` → `src/sql/rbac.rs` + `src/auth/*`
    - EXTENSION DDL: `CREATE/DROP EXTENSION` (pre-parse interception) → `src/sql/executor_extensions.rs`
  - Terminal persistence classes: `src/storage/tikv_store.rs` (`_sys_schema_*` / `_sys_schemadef_*` / `_sys_view_*` / `_sys_matview_*` / `_sys_type_*` / `_sys_seq_*` / `_sys_ext_*` etc.)
- Risks/Assumptions (to validate / needs experiments):
  - DDL semantics, name resolution (schema/search_path), OID/constraint consistency, and correctness of on-disk keys still need convergence by reading `src/sql/ddl.rs` / `src/sql/sequences.rs` / `src/sql/udt.rs` / `src/sql/rbac.rs`

### 2.1.6 Flow 6: DML (INSERT/UPDATE/DELETE → row/index → triggers?)

- Status: Partial
- Facts (dispatch entrypoints + implementation file clues read):
  - Entry: `Executor::execute()` → `execute_statement_on_txn(...)` → `Statement::{Insert,Update,Delete}` branches
  - DML implementation entrypoints: `src/sql/executor_dml_ops.rs` `Executor::{execute_insert,execute_update,execute_delete}`
    - Depends on: `src/sql/dml.rs` (row preparation / RETURNING assembly etc.) + `src/storage/tikv_store.rs` (write row + index entries)
    - Trigger clues: `src/sql/executor_dml_ops.rs` imports `triggers/trigger_queue/trigger_worker` (full semantics in Flow 10)
- Risks/Assumptions (to validate / needs experiments):
  - Constraints (PK/UNIQUE/FK/CHECK), RETURNING, concurrency conflicts, and error codes need convergence by reading `src/sql/dml.rs` / `src/sql/executor_dml_ops.rs` + `src/storage/tikv_store.rs`
  - Transaction boundaries / idempotency / failure strategy for triggers enqueue on DML need convergence along Flow 10

### 2.1.7 Flow 7: SELECT Engine (Query → planner/scan/join/agg/window → rowset)

- Status: Partial
- Facts (top-level branching + data source entrypoints read):
  - Entry:
    - `Executor::execute_statement_on_txn(..., Statement::Query(query))` → `Executor::execute_query(...)`
    - `Executor::execute_query(...)` → `build_cte_context(...)` → `src/sql/executor_select.rs` `execute_query_with_ctes(...)`
  - Key top-level branches:
    - set operations: `UNION/INTERSECT/EXCEPT` → `Executor::execute_set_operation(...)` (in-memory merge) → then apply `ORDER BY/LIMIT/OFFSET`
    - tableless: `SELECT ...` with no `FROM` → `Executor::execute_tableless_query(...)` (limited SRF/pg_sleep)
    - join: JOIN present / multiple FROM → `src/sql/executor_join.rs` `execute_join_query_with_ctes(...)`
    - single-table: continue in `src/sql/executor_select.rs` (internally calls planner/scan/expr/agg/window, etc.)
  - “Data source” entrypoint: `src/sql/executor_join.rs` `get_table_data()`
    - sys/virtual tables: `_db9_sys_observability/_query_samples/_trigger_queue_stats/_trigger_dlq`, `current_schema/current_database/...`
    - CTE: read directly from `ctes` map
    - `information_schema.*`: `src/sql/information_schema.rs`
    - view: `TikvStore::get_view()` → recursively execute view query
    - base table: generate candidate fully-qualified names from `search_path` → `TikvStore::get_schema()` / `TikvStore::scan()`
- Risks/Assumptions (to validate / needs experiments):
  - Planner/scan/index selection, and semantic/performance boundaries of join/agg/window/subquery/CTE still require deeper reading: `src/sql/executor_select.rs` / `src/sql/executor_join.rs` / `src/sql/planner.rs` / `src/sql/expr.rs` / `src/sql/aggregate.rs` / `src/sql/window.rs`

### 2.1.8 Flow 8: COPY (COPY IN / COPY TO STDOUT)

- Status: Verified (protocol layer + executor path)
- Facts (key COPY TO/COPY FROM implementation read):
  - COPY TO STDOUT (Simple Query):
    - `src/protocol/handler.rs` `SimpleQueryHandler::do_query()` → `parse_copy_to_command()` (regex; supports `schema.table`, does not support quoted identifiers)
    - `handle_copy_to_stdout()`: rewrite COPY into `SELECT ... FROM <table>`, execute, then use `src/protocol/copy_format.rs` to output COPY text
  - COPY FROM STDIN (Simple Query → CopyHandler):
    - `SimpleQueryHandler::do_query()` recognizes `COPY ... FROM stdin` (regex; only supports bare table name or `public.` prefix)
    - `CopyHandler::on_copy_data()`: append each `CopyData` chunk into `CopyContext.data_buffer` (buffers everything in memory)
    - `CopyHandler::on_copy_done()`:
      - Read schema (a separate begin+rollback transaction)
      - Concatenate all buffer → `lines()` → split by `\t`
      - **Row/column count mismatch is silently skipped** (no error)
      - For each row, call `Executor::execute_copy_insert()`
  - COPY FROM transaction boundary (code-proven):
    - `Executor::execute_copy_insert()` does “one txn per call” when session is not in an explicit transaction (autocommit: `BEGIN`→insert→`COMMIT`), so default COPY FROM is not statement-atomic
    - `Session::begin()` is no-op if already in a transaction (`src/sql/session.rs`), but `CopyHandler::on_copy_done()` unconditionally calls `session.rollback()` after reading schema to end the schema-read txn
- Risks/Assumptions (to validate / needs experiments):
  - If COPY FROM runs inside an explicit transaction, does the unconditional `rollback()` roll back the “outer transaction”? (needs experiments combining `SimpleQueryHandler::do_query()` + COPY + multi-statement/explicit txn behavior)
  - Whether COPY FROM schema name handling matches the `_sys_schema_<table_name>` key rule (regex may drop `public.`; verify actual parameters passed by the handler)
  - Memory/latency impact of buffering everything in `CopyContext.data_buffer` for large imports (perf risk)

### 2.1.9 Flow 9: Extensions/http (table function → outbound request → rowset)

- Status: Partial (extension itself Verified; integration with SELECT engine pending)
- Facts (extension recognition/privilege/limits read):
  - Entry: `src/sql/executor_extensions.rs` routes `extensions.http_get(...)`-style table functions to `Executor::try_execute_extension_table_function(...)`
    - If schema is omitted (`http_get(...)`), it is recognized as an extension table function only if `search_path` contains `extensions`
  - Privilege + install state:
    - `CREATE/DROP EXTENSION`: only superuser allowed (`execute_create_extension_cmd()/execute_drop_extension_cmd()`)
    - `http_*` table functions: extension must be installed and `enabled=true`, otherwise error
    - `http::execute_table_function()` additionally requires statement context `is_superuser==true` (otherwise `permission denied for extension "http"`)
  - Outbound request constraints (`src/extensions/http.rs`):
    - Only `https://` by default (`http://` requires `DB9_HTTP_ALLOW_INSECURE=true`)
    - Only default ports (`https:443` / `http:80`), no userinfo, disallow localhost/private/link-local/unspecified IPs (including DNS results)
    - Max 5 requests per statement; max 20 concurrent requests per tenant per node
    - connect timeout 1s, overall timeout 5s; request body ≤ 256KiB; response ≤ 1MiB; redirects ≤ 3
  - Output: single-row rowset: `status int4`, `content_type text nullable`, `headers jsonb`, `content text` (content must be UTF-8)
- Risks/Assumptions (to validate / needs experiments):
  - The exact integration point where `Executor::try_execute_extension_table_function(...)` is invoked within the SELECT execution chain is still unknown (read TableFactor/FunctionScan handling in `src/sql/executor_select.rs` / `src/sql/executor_join.rs`)

### 2.1.10 Flow 10: Async AFTER triggers (enqueue → worker → DLQ/stats)

- Status: Partial (worker spawn + sys diagnostics tables Verified; enqueue/worker semantics pending)
- Facts (worker spawn + sys diagnostics entrypoints read):
  - Worker spawn: `src/main.rs` `sql::trigger_worker::spawn_trigger_worker(client_pool.clone())`
  - Sys diagnostics tables (`src/sql/executor_join.rs` `get_table_data()`):
    - `_db9_sys_trigger_queue_stats` (scan queue prefix and aggregate pending/processing/failed, DLQ count, events in last 1 minute, etc.)
    - `_db9_sys_trigger_dlq` (scan DLQ prefix and list failed events)
  - DML path has trigger-related module clues: `src/sql/executor_dml_ops.rs` (imports `triggers/trigger_queue/trigger_worker`)
  - Event persistence/consumption entrypoints: `src/sql/trigger_queue.rs` / `src/sql/trigger_worker.rs`
- Risks/Assumptions (to validate / needs experiments):
  - Enqueue transaction boundaries, idempotency, and retry/DLQ strategy need convergence by reading `src/sql/triggers.rs` / `src/sql/trigger_queue.rs` / `src/sql/trigger_worker.rs`

### 2.1.11 Flow 11: Observability (record → snapshot → sys functions/portal)

- Status: Verified
- Facts (collection, aggregation, and sys-table mapping read):
  - Collection entrypoints:
    - `Executor::execute()`: for non-observability sys queries, after each statement execution call `TenantObservability::record_statement(...)` (parse failures are also recorded as a failed sample)
    - `Session::commit()`: after successful TiKV txn commit, call `TenantObservability::record_commit()` to increment commit count (TPS source; see `src/sql/session.rs`)
    - `TenantObservability::connection_open()`: each connection holds a `ConnectionGuard`, decremented on Drop
  - Sampling + normalization (`src/observability.rs`):
    - Errors are always sampled; slow queries (≥`DB9_OBS_SLOW_MS`) are always sampled; others sampled at `1/DB9_OBS_SAMPLE_EVERY`
    - SQL `normalize_sql()` (collapse whitespace, strip trailing `;`, truncate to `DB9_OBS_MAX_SQL_LEN` and suffix with `…`), and replace `|` with space
  - External snapshots:
    - `snapshot_summary()`: statement/commit/error, QPS/TPS, avg/p99 latency, active_connections (window: last 1h / or min since process start)
    - `snapshot_query_samples()`: aggregate by `fnv1a_64(normalized_sql)`, return top-N by sample_count and include last_seen_ms_ago
  - Sys functions (SQL layer mapping):
    - SQL execution treats `_db9_sys_observability()` / `_db9_sys_query_samples()` as “pseudo tables” under `FROM <table>`: `src/sql/executor_join.rs` `Executor::get_table_data()`
    - Name matching supports `_DB9_SYS_OBSERVABILITY` / `_DB9_SYS_QUERY_SAMPLES`, and also schema-prefixed forms (`...ends_with("._DB9_SYS_*")`)
    - Data source: `self.observability().snapshot_summary()` / `snapshot_query_samples()`
  - The same entrypoint also implements trigger queue diagnostics tables: `_db9_sys_trigger_queue_stats` / `_db9_sys_trigger_dlq` (scan queue/DLQ prefixes via TiKV `txn.scan(...)` and aggregate/list)
- Risks/Assumptions (to validate / needs experiments):
  - Lock contention / memory overhead of sampling under high concurrency and coupling with portal polling frequency (perf/obs risk; needs load tests and config validation)

### 2.2 Portal Top Flows (first-cut list)

- Status: Partial
- Flows (index):
  1. **Portal login/tenant selection → connect to db9-server** (credentials/privilege model) (`security + tenancy`)
  2. **Dashboard pulls observability data** (polling frequency, SQL normalization, error handling) (`obs + perf`)
  3. **Bootstrap/ops actions** (create observer account, privilege tightening, secret management) (`security + tenancy`)
- Facts (key implementation entrypoints read):
  - “Login/connect” is not a global account system: `cloud-admin-portal/frontend/src/hooks/useTenantSession.tsx` uses `POST /api/tenants/{tenant_id}/connect` to obtain `session_id`, stored into `sessionStorage["tenant_session:<tenant_id>"]`
    - `cloud-admin-portal/frontend/src/api/client.ts` automatically injects `X-Tenant-Session` for `/tenants/<id>/...`
    - Backend verification: `cloud-admin-portal/backend/app/api/tenants.py` `get_tenant_session()` / `cloud-admin-portal/backend/app/api/users.py` `get_tenant_session()`
    - Session storage: `cloud-admin-portal/backend/app/session.py` (in-memory; includes admin password; default 1h TTL)
  - Observability dashboard:
    - Frontend: `cloud-admin-portal/frontend/src/api/tenants.ts` `useTenantObservability()` polls every 5s by default; stops polling on HTTP 409 (not bootstrapped)
    - Backend: `cloud-admin-portal/backend/app/api/tenants.py` `GET /api/tenants/{tenant_id}/observability` queries `_db9_sys_observability()` / `_db9_sys_query_samples()` using observer credentials stored in DB
    - PG client: `cloud-admin-portal/backend/app/services/pg_client.py` (`pg8000` + pipe-delimited output parsing)
  - Bootstrap observer:
    - Backend: `cloud-admin-portal/backend/app/api/tenants.py` `POST /api/tenants/{tenant_id}/observability/bootstrap` creates/rotates `_db9_sys_observer` using admin credentials and stores the password in portal DB
  - Security boundary (implementation observation): multiple management APIs (e.g. `GET/POST /api/tenants`, `GET /api/audit-logs`, `GET /api/system/health`, `GET /api/tenants/{tenant_id}/observability`) have no global auth dependency observed; deployment may rely on network isolation / reverse proxy protection (verify with `cloud-admin-portal/deploy/` and real deployment)
- Risks/Assumptions (to validate / needs experiments):
  - Whether `GET /api/tenants/{tenant_id}/observability` not requiring `X-Tenant-Session` is intentional design (confirm with threat model and deployment boundary)

---

## 3. Scenario × Layer Matrix (for “following the breadcrumbs”)

> Rule: each cell only contains entry functions / key structs / key files. Fill in gradually later.

| Flow \\ Layer | main/pool/tls | protocol | sql | txn | storage | auth | extensions | obs | portal |
|---|---|---|---|---|---|---|---|---|---|
| Conn + tenant + auth | `src/main.rs` + `src/pool.rs` | `src/protocol/handler.rs` `parse_tenant_username()` + `StartupHandler::on_startup()` + `authenticate_user()` + `init_executor()` | `src/sql/session.rs` + `Executor::new()` | TODO | `src/storage/tikv_store.rs` + `TikvClientPool::get_client()` | `src/auth/*` `AuthManager::{bootstrap,authenticate}` | - | `src/observability.rs` connection guard | `cloud-admin-portal/*` (per-tenant connect) |
| Simple query | - | `src/protocol/handler.rs` `SimpleQueryHandler::do_query()` | `src/sql/executor.rs` `Executor::execute()` | `src/sql/session.rs` `Session::{begin,commit,rollback}` | `src/storage/tikv_store.rs` (via `execute_statement_on_txn()` → DDL/DML/Query) | - | `src/extensions/context.rs` `with_context()` | `src/observability.rs` `TenantObservability::record_statement()` | - |
| Extended query | - | `src/protocol/handler.rs` `ExtendedQueryHandler for DynamicPgHandler` (`do_query/do_describe_*`) | `src/sql/executor.rs` `Executor::execute()` (executes substituted SQL) | `src/sql/session.rs` (same as Simple Query) | `src/storage/tikv_store.rs` (same as Simple Query) | - | `src/extensions/context.rs` (still wrapped per statement) | `src/observability.rs` (same sampling path) | - |
| DDL | - | - | `src/sql/executor.rs` `execute_statement_on_txn()` → `src/sql/ddl.rs` | `src/sql/session.rs` (autocommit glue) | `src/storage/tikv_store.rs` (schema/catalog keys) | `src/auth/*` (role/rbac DDL) | `src/sql/executor_extensions.rs` | - | - |
| DML | - | - | `src/sql/executor.rs` `execute_{insert,update,delete}()` | `src/sql/session.rs` (autocommit glue) | `src/storage/tikv_store.rs` (row/index keys) | `src/sql/rbac.rs`/`src/auth/*` (permission checks) | - | - | - |
| SELECT engine | - | - | `src/sql/executor.rs` `execute_query()` → `execute_query_with_ctes()` | `src/sql/session.rs` (txn/sequence values) | `src/storage/tikv_store.rs` (scan/get/txn) | - | - | - | - |
| COPY | - | `src/protocol/handler.rs` `parse_copy_*` + `handle_copy_to_stdout()` + `CopyHandler::*` | `Executor::{parse_value_for_copy,execute_copy_insert}` | TODO | `TikvStore::get_schema()` | - | - | - | - |
| Extensions/http | - | - | `src/sql/executor_extensions.rs` `try_execute_extension_table_function()` + `execute_{create,drop}_extension_cmd()` | `src/sql/session.rs` (autocommit glue) | `src/storage/tikv_store.rs` (`get_extension/put_extension/drop_extension`) | `src/protocol/handler.rs`/`src/sql/session.rs` (superuser) | `src/extensions/{context,http}.rs` | `src/observability.rs` (sampling is recorded at statement layer) | `cloud-admin-portal/*` (if portal depends on http ext, boundary must be explicit) |
| Async triggers | `src/main.rs` `spawn_trigger_worker()` | - | `src/sql/executor_dml_ops.rs` + `src/sql/triggers.rs` + `src/sql/trigger_queue.rs` + `src/sql/trigger_worker.rs` + `src/sql/executor_join.rs` (sys diagnostics tables) | `src/sql/session.rs` (txn semantics for enqueue) | `src/storage/encoding.rs` (queue key encoding clues) + `txn.scan(...)` (sys diagnostics) | - | - | - | - |
| Observability | - | - | `src/sql/executor.rs` `is_observability_*` + `execute_tableless_query()` | `src/sql/session.rs` `commit()->record_commit()` | - | `src/auth/*` (observer user creation/privileges) | - | `src/observability.rs` (registry/rolling window/samples) | `cloud-admin-portal/*` (dashboard queries) |

---

## 4. Priority Overlay (focus mapping for deeper review later)

### 4.1 SQL correctness (`correctness`)

Primary focus points (first cut):
- Statement dispatch + transaction semantics glue: `src/sql/executor.rs`, `src/sql/session.rs`, `src/txn/*`
- SELECT main engine: `src/sql/executor_select.rs`, `src/sql/executor_join.rs`, `src/sql/executor_subquery.rs`, `src/sql/executor_cte.rs`
- Expressions / built-in functions: `src/sql/expr.rs`
- Aggregates / windows: `src/sql/aggregate.rs`, `src/sql/window.rs`
- Planner/index usage: `src/sql/planner.rs`, `src/sql/index_helpers.rs`

Next deep-dive questions (placeholders; later tasks will add evidence one by one):
- Are there semantic inconsistencies across execution paths (JOIN vs non-JOIN, tableless vs table scan, subquery reuse, etc.)?
- NULL/3-valued logic, type inference/coercion, ordering/collation, time/timezone, numeric precision
- Transaction boundaries: autocommit, rollback-on-error behavior, savepoint behavior
- Consistency between DDL/DML and system catalog / information schema

### 4.2 Multi-tenant isolation & security (`tenancy + security`)

Invariants (must hold globally):
- **Any persistent data must be tenant-isolated**: all keys must be written through a keyspace-aware client/store; no “global keys” shared across tenants.
- Tenant routing at connection level must be reliable: username parsing, default keyspace, and error branches must not be bypassable.
- Cross-tenant access via extensions/portal must be explicitly forbidden or strongly isolated.

Primary focus points (first cut):
- `src/protocol/handler.rs` (tenant routing + auth entrypoint)
- `src/pool.rs` / `src/storage/tikv_store.rs` (keyspace client creation/reuse strategy)
- `src/auth/*` (user/role/privilege storage & checks)
- `src/extensions/*` (external I/O: HTTP)
- `cloud-admin-portal/*` (credentials, permission boundaries, risk of “dashboard using admin password”, etc.)

### 4.3 Performance & observability (`perf + obs`)

Primary focus points (first cut):
- Planner and scan strategy: `src/sql/planner.rs`
- Join/window memory + sorting: `src/sql/executor_join.rs`, `src/sql/window.rs`
- TiKV round-trips and batching: `src/storage/tikv_store.rs`, `src/storage/encoding.rs`
- Sampling and sys table functions: `src/observability.rs` + `src/sql/executor_join.rs` (sys functions already exist)
- Portal polling/caching: `cloud-admin-portal/frontend/*` + backend query aggregation strategy

Next deep-dive questions (placeholders; later tasks will add evidence one by one):
- O(N^2) join/window, full sorts, unnecessary clone/serialize, repeated scans
- Whether observability backfires on performance (sampling path, SQL normalization/truncation, lock contention)
- Portal polling frequency and “slow-query positive feedback” (dashboards causing load)

---

## 5. Next Reads (v1 direction: from map to details)

- Suggested next fills (ordered by “how much it improves map precision”):
  1) SELECT engine branching + data sources: `src/sql/executor_select.rs` / `src/sql/executor_join.rs` / `src/sql/executor_subquery.rs` / `src/sql/executor_cte.rs`
  2) DDL/DML KV write paths + constraint anchor points: `src/sql/ddl.rs` / `src/sql/dml.rs` / `src/sql/executor_*_ops.rs` + `src/storage/tikv_store.rs`
  3) Privilege model anchor points: `src/auth/*` + `src/sql/rbac.rs` + `src/protocol/handler.rs` (superuser/observer/tenant routing)
  4) Trigger queue + worker: `src/sql/triggers.rs` / `src/sql/trigger_queue.rs` / `src/sql/trigger_worker.rs` (plus sys tables: `src/sql/executor_join.rs`)
  5) Expression/type-system hotspots (`correctness`): `src/sql/expr.rs` / `src/sql/helpers.rs` / `src/types/*`

---

## 6. Evidence Appendix (doc evidence & hypotheses to verify)

> Note: this chapter only records “what docs say” and “conflicts across docs”. They are not implementation facts; implementation is determined by code.

### 6.1 Docs read list (Sources / Claims)

Core service (Rust):
- `README.md`: externally stated feature list and constraints / ORM compatibility positioning.
- `AGENT.md` / `CLAUDE.md`: dev conventions and architecture/capability positioning (navigation only; must be verified in code).
- `docs/architecture.md`: component layering and some limitations (**has clear inconsistencies with README**, see the next conflict list).
- `docs/multi-tenancy.md`: keyspace isolation, username routing (`tenant.user`/`tenant:user`), default keyspace, bootstrap admin positioning.
- `docs/authentication.md`: cleartext password, RBAC, and bootstrap positioning.
- `docs/extensions.md`: `http` extension security restrictions and limits (port/SSRF/concurrency/max calls per statement, etc.).
- `docs/configuration.md`: environment variables and deployment examples (includes some possibly outdated log examples).
- `docs/README.md`: docs index and “supported features” positioning (includes JOIN support scope entries).
- `docs/quickstart.md`: quick start and connection examples.
- `docs/admin-cli.md`: management scripts/CLI external usage positioning.
- `docs/sql-reference.md`: SQL/function docs (more conservative positioning).
- `docs/constraint-implementation-report.md`: constraint implementation test report (FK/CHECK/UNIQUE, etc.; includes perf note: FK validation may scan full table).
- `docs/design/README.md`: design doc index (P0/P1/P2).
- `docs/design/*.md`: P0/P1/P2 design details (implementation must follow code).
- `docs/backlogs/*`: designs still marked “not implemented” (may conflict with current state; must be decided by code).
- `docs/NEON_TUTORIAL_ASSESSMENT.md`: feature checklist aligned to the Neon tutorial (many conflicts with other docs; needs code comparison).
- `DB9_SQL_SPEC.md`: SQL spec summary based on “then-current code and tests” (may differ from current state).
- `bug.md`: reproduction of Numeric result OID bug, root-cause hypothesis, portal workaround (needs code validation).
- `review.md`: Async AFTER trigger queue design/implementation summary (provides entrypoint clues).
- `PROGRESS.md` / `TODO.md`: historical progress and TODOs (clear timeline conflicts with current code/README; use cautiously).
- `WORK.md`: multi-round implementation/fix notes (contains the then-current validation method and regression points; still must be validated against code).

Portal (`cloud-admin-portal`):
- `cloud-admin-portal/README.md`: functionality, APIs, auth model (per-tenant session).
- `cloud-admin-portal/AGENTS.md`: code structure and key patterns (sessionStorage + `X-Tenant-Session`).
- `cloud-admin-portal/CLAUDE.md`: architecture and notes (conflicts with AGENTS/bug.md about pg client; decide by code).
- `cloud-admin-portal/IMPLEMENTATION_SUMMARY.md`: SQLite metadata/audit logs/soft delete implementation summary.
- `docs/todo/admin-portal-redesign.md`: portal redesign doc (legacy scripts → `cloud-admin-portal/`).
- `WEB_WORK.md`: portal progress and stage conclusions (timeline conflicts with old positioning; decide by code).

PRDs (feature positioning driven by needs/compat):
- `prds/*`: “quick compatibility” requirements such as system functions.
- `prds/dify-database-compatibility-spec.md` + `prds/dify-compatibility/*`: Dify workload requirements for JSONB/timezone/bytea functions/GIN indexes, etc.

In-repo guidance (affects later reading order and conventions):
- `src/sql/AGENTS.md` / `src/protocol/AGENTS.md` / `src/storage/AGENTS.md`: submodule tour and common pitfalls (navigation only; not implementation facts).

### 6.2 Doc conflicts / timeline conflicts (Hypotheses to verify)

> These are not conclusions, but “hypotheses/conflicts to verify”. When the relevant code is read later, each item will be closed out one by one.

- **Feature positioning conflicts**:
  - `docs/architecture.md` “Not Supported” list (RIGHT/FULL JOIN, triggers, TLS, materialized views, etc.) clearly conflicts with `README.md` and `review.md`.
  - `DB9_SQL_SPEC.md` claims “triggers are only stored not executed / $$ strings not supported”, which may conflict with the direction in `review.md` / `docs/design/07_*`.
  - `PROGRESS.md` still says “FK/CHECK not enforced”, conflicting with `docs/constraint-implementation-report.md` / `README.md`.
- **Multi-tenant keyspace creation**:
  - `docs/multi-tenancy.md` says keyspaces must be created manually; `src/main.rs` appears to attempt PD HTTP API creation on connection failure (needs verification of the error branch and behavior).
- **Auth/log positioning**:
  - `docs/configuration.md` log examples mention “Password authentication: disabled”, conflicting with `README.md` / `src/main.rs` positioning of “enabled”.
- **Portal pg client implementation conflicts**:
  - `cloud-admin-portal/AGENTS.md` says `pg8000`; `cloud-admin-portal/CLAUDE.md` says `psql subprocess`; `bug.md` also hints `pg8000`. Use `cloud-admin-portal/backend/app/services/pg_client.py` as the source of truth.
  - `docs/todo/admin-portal-redesign.md` old diagrams/examples still mention `psql subprocess`; `WORK.md` / `WEB_WORK.md` position that backend has switched to `pg8000` and removed `psql` fallback (decide by code).
- **Portal global auth positioning conflicts**:
  - `cloud-admin-portal/README.md` explicitly says “No global portal auth” (tenant list/create/delete does not require login).
  - `docs/todo/admin-portal-redesign.md` / `WEB_WORK.md` describe JWT bearer global auth and protected tenant CRUD (decide by the real implementation; clarify threat model/deployment assumptions).
- **Missing docs / outdated references**:
  - `AGENT.md` references `HOW_TO_TEST.md`, which does not exist in the repo (confirm whether it was renamed/moved into `docs/`, and decide whether to add or remove the reference).
- **Backlogs vs current state**:
  - `docs/backlogs/06_copy_to_and_options.md` still says “missing COPY TO STDOUT”; but the protocol layer already has a `COPY ... TO STDOUT` path (verify support scope and semantics).
  - `docs/backlogs/11_system_catalog_coverage.md` still says `pg_proc` returns empty; `WORK.md` notes pg_catalog + stable OIDs were added (verify current state in `src/sql/information_schema.rs`).
- **JOIN support positioning conflicts**:
  - `docs/README.md` “Supported SQL Features” table lists only INNER/LEFT JOIN; `docs/NEON_TUTORIAL_ASSESSMENT.md` marks RIGHT/FULL OUTER JOIN as supported (verify in JOIN execution path).

### 6.3 Doc-stated invariants / security boundaries (Doc-stated boundaries)

Multi-tenancy (keyspace):
- **Connection routing**: username `tenant.user` / `tenant:user` chooses keyspace; no prefix uses default keyspace (`docs/multi-tenancy.md`, `README.md`).
- **Isolation scope**: each keyspace has its own users/roles/tables/metadata; cross-keyspace joins are disallowed (`docs/multi-tenancy.md`).
- **Keyspace creation**: docs lean toward “external creation” (pd-ctl/tiup), but core service may attempt auto-create (see conflicts; decide by code).

Authentication / RBAC:
- **Auth method**: Cleartext password (`docs/authentication.md`); production requires TLS (same doc).
- **Bootstrap admin**: explicit initial superuser bootstrap via `DB9_BOOTSTRAP_ADMIN_PASSWORD` (optionally `DB9_BOOTSTRAP_ADMIN_USER`), per keyspace; `DB9_DEV=1` enables legacy dev bootstrap (insecure).
- **Fallback password**: no global fallback password mechanism is implemented; authentication depends on per-user passwords.

HTTP extension:
- **Default privilege**: only SUPERUSER may execute (`docs/extensions.md`).
- **Network restrictions**: by default only `https://` + 443; `DB9_HTTP_ALLOW_INSECURE=true` enables `http://` + 80.
- **SSRF protection**: disallow `localhost/.localhost/.local`, disallow resolving to loopback/private/link-local/unspecified ranges.
- **Resource limits**: connect timeout 1s, total timeout 5s, max body 256KiB, max response 1MiB, max redirects 3, max 5 HTTP calls per statement, max 20 concurrent per tenant per node (docs claim “hard-coded in code”).

Portal (`cloud-admin-portal`):
- **No global portal login**: tenant list/create/disable are anonymous; user management requires per-tenant connect (`cloud-admin-portal/README.md`; conflicts with `docs/todo/admin-portal-redesign.md`/`WEB_WORK.md`, must be verified in code).
- **Tenant session**: `POST /api/tenants/{name}/connect` returns `session_id`; frontend stores in `sessionStorage`; backend stores in-memory with default TTL 1h (`cloud-admin-portal/AGENTS.md`, `cloud-admin-portal/CLAUDE.md`).
- **Tenant delete semantics**: TiKV keyspaces can only be DISABLED (not truly deleted); portal also implements soft-delete (SQLite) to hide in UI (`cloud-admin-portal/IMPLEMENTATION_SUMMARY.md`, `docs/admin-cli.md`).
