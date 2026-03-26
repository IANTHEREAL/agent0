# db9-server Knowledge Base (Living Document)

## Overview

PostgreSQL-compatible distributed SQL database on TiKV. Implements pgwire protocol and translates SQL to KV operations.

## Refactor Governance Contract (Architecture Owner)

### Identity

- Act as a senior database systems expert for architecture, execution semantics, and compatibility contracts.

### Role

- Own and enforce one clean execution model for `Analyzer -> Typed IR -> Executor`.
- Own root-cause analysis for CI/ORM regressions at the correct fault layer (`Analyzer` / `Executor` / `Catalog` / `Storage`), not symptom layers.

### Mission

- Build a clean, maintainable, debuggable database core before launch.
- Keep architecture single-path and deterministic: no hidden fallback, no runtime "try-new-then-old".
- Ensure each behavior has a clear module boundary and a single source of truth.
- Deliver structurally correct fixes, not tactical patches.

### Operating Principles

- **PostgreSQL is the specification.** All SQL semantics, type coercion rules, catalog behavior, error codes, and wire protocol responses must align with PostgreSQL. When in doubt, test against real PostgreSQL and match its behavior — do not invent custom semantics.
- Correctness first, then speed; optimize diagnosis path, not by shortcuts.
- Fix only at the root layer where the invariant is broken.
- Never use wire/test-layer masking logic to hide internal inconsistency.
- Test expectations must follow validated semantics (PostgreSQL parity where intended), not vice versa.
- If behavior changes intentionally, update contract and tests in the same change.

## Current Architecture (Latest)

### Execution Model (single path)

```
Client/ORM -> pgwire -> SQL Parser -> Analyzer -> Typed IR -> Optimizer (CBO) -> Executor/Operators -> TiKV Store -> TiKV
```

### Module Boundaries

- `Analyzer` (`src/sql/analyzer/`): name resolution, scope checking, and type inference; outputs `AnalyzedQuery` / `TypedExpr`.
- `Optimizer` (`src/sql/optimizer/`): `AnalyzedQuery → LogicalPlan → (rewrite: decorrelation, predicate pushdown, join reorder) → PhysicalPlan → BoxedOperator`. Handles single-table, multi-table joins, set operations (UNION/INTERSECT/EXCEPT), CTEs, window functions, DISTINCT ON, SemiJoin/AntiJoin. Always-on (the `db9.use_optimizer` GUC is accepted for compatibility but is a no-op — `SET ... = off` logs a notice and is ignored; `SHOW` always returns `on`).
- `Operators` (`src/sql/operators/`): physical operators (scan, filter, project, sort, aggregate, hash_join, hash_semi_join, NLJ, hnsw_scan, window/, CTE, set_operation, table_function).
- `Executor` (`src/sql/executor/`): DDL/DML dispatch, SELECT execution (analyzed path at `executor/select/analyzed/`), background SQL (`bg_sql.rs`).
- `Catalog` (`src/sql/catalog/`): `information_schema` / `pg_catalog` / `cron` compatibility surface (37 virtual table implementations).
- `Storage` (`src/storage/`): all persistent keys must remain keyspace-isolated via `TikvStore`. Database-scoped v2 key format (`d_{db_id}_*`).
- `Worker` (`src/worker/`): unified async task engine (Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql). Global task queue in TiKV, pessimistic locking, no leader election.
- `HNSW` (`src/sql/hnsw/`): HNSW vector index — process-level LRU cache, TiKV persistence (graph + meta), usearch FFI bridge. Operator at `src/sql/operators/hnsw_scan.rs`, pattern detection at `src/sql/planner/hnsw_predicate.rs`.
- `Cron` (`src/cron/`): pg_cron-compatible scheduler integrated with worker engine.

### Multi-tenancy Invariant (Critical)

- All persistent data must be isolated per keyspace (`_sys_*`, table rows, indexes, auth, and future stats).
- Process-level global state is limited to in-memory caches/config/logging.

### TiKV Value Size Invariant (Critical)

- No single KV value written via `txn_put` should exceed TiKV's `raft-entry-max-size` (default 8 MB). Features that persist growable data MUST use per-row entries or fixed-size pages, not monolithic blobs.
- **Known violation:** HNSW graph blob (#1969) — remediation in progress.
- Design doc: `docs/design/30_tikv_value_size_design_lessons.md`.

## Current Sprint & Issue Tracking

- **Epic:** #1959 — Open Issue Triage: Refactoring Groups and Execution Plan (80 issues)
- **Sprint W17:** #1961 — Concentrated Refactoring (Type Resolution, GUC Architecture, Code Quality)

## Repository Layout (stable)

```
db9-server/
├── src/
│   ├── sql/                           # SQL engine (~118K lines)
│   │   ├── analyzer/                  # Semantic analysis → AnalyzedQuery/TypedExpr
│   │   │   ├── expr/                  # Expression analysis (coercion, functions, literals, operators)
│   │   │   ├── query/                 # Query analysis (from_clause, group_by, projection, set_expr)
│   │   │   └── types/                 # Typed IR definitions (TypedExpr, AnalyzedQuery)
│   │   ├── optimizer/                 # CBO: LogicalPlan → PhysicalPlan → operators
│   │   │   ├── logical_planner/       # AnalyzedQuery → LogicalPlan
│   │   │   ├── physical_planner/      # LogicalPlan → PhysicalPlan
│   │   │   ├── build/                 # PhysicalPlan → BoxedOperator (scan, join, aggregate)
│   │   │   ├── rewrite/               # Plan rewrites (decorrelation, predicate pushdown)
│   │   │   ├── join_reorder/          # Cost-based join reordering (DPccp algorithm)
│   │   │   └── selectivity/           # Selectivity estimation from column statistics
│   │   ├── operators/                 # Physical operators (Volcano iterator model)
│   │   │   ├── hash_join/             # Equi-join with hash table
│   │   │   ├── hash_semi_join.rs      # Semi/anti-join for EXISTS decorrelation
│   │   │   ├── hnsw_scan.rs           # HNSW approximate nearest neighbor scan
│   │   │   └── window/                # Window functions (access, aggregates, ranking)
│   │   ├── executor/                  # DDL/DML dispatch + SELECT execution
│   │   │   ├── core/                  # Statement dispatch + infrastructure
│   │   │   │   ├── dispatch/          # Statement routing (mod.rs, prepared.rs, utils.rs)
│   │   │   │   ├── view_rewrite/      # View expansion (expr.rs, query.rs, table.rs)
│   │   │   │   └── catalog_prefetch/  # Batch catalog lookups
│   │   │   ├── select/analyzed/       # Single-path SELECT executor
│   │   │   ├── dml_analyzed/          # Analyzed INSERT/UPDATE/DELETE
│   │   │   └── procedure/             # Stored procedures + materialized views
│   │   ├── expr/                      # Expression system
│   │   │   ├── typed_eval/            # Runtime evaluator (arithmetic, helpers)
│   │   │   ├── traverse/              # Expression tree traversal
│   │   │   └── functions/             # 13 categories (array…vector)
│   │   ├── catalog/                   # information_schema + pg_catalog + cron (37 views)
│   │   ├── types/                     # Type inference, coercion, mapping
│   │   │   ├── registry/              # FunctionRegistry (aggregate_window, json, math, misc, string, system, temporal)
│   │   │   └── cast/                  # CAST between types
│   │   ├── ddl/                       # DDL: CREATE/ALTER/DROP (alter_table, create_index, create_table, drop, view)
│   │   ├── dml/                       # DML helpers: defaults, foreign_keys, insert, update, delete
│   │   ├── session/                   # Per-session state (settings, transaction)
│   │   ├── hnsw/                      # HNSW vector index (cache, storage, usearch FFI)
│   │   ├── planner/                   # Index selection, scan strategy, expression-index, HNSW predicate
│   │   ├── explain/                   # EXPLAIN output (format, transform)
│   │   ├── triggers/                  # Trigger subsystem (cache, before, rewrite, enqueue, execute)
│   │   ├── sequences/                 # SEQUENCE management (DDL, eval, replace)
│   │   ├── rewriter/                  # SQL rewriter (flatten, remap)
│   │   ├── plpgsql/                   # PL/pgSQL parser + executor
│   │   ├── parser/                    # SQL parser (operator_rewrite, preprocess, tokenizer)
│   │   ├── binder/                    # Legacy name binding
│   │   └── stats.rs                   # TableStatsCache (per-tenant)
│   ├── protocol/
│   │   ├── copy_format.rs             # COPY format parsing (CSV, TEXT, BINARY)
│   │   └── handler/                   # pgwire handler
│   │       ├── dynamic/               # DynamicPgHandler (mod.rs, query.rs, copy.rs, startup.rs)
│   │       ├── encode/                # Value encoding + type mapping
│   │       ├── params/                # Parameter counting + decoding
│   │       ├── copy/                  # COPY context management
│   │       ├── portal.rs              # Portal state + suspended queries
│   │       ├── query_parser.rs        # Db9QueryParser (pgwire QueryParser trait)
│   │       ├── server_params.rs       # ParameterStatus provider
│   │       ├── tenant.rs              # Multi-tenancy username parsing
│   │       └── errors.rs              # SQLSTATE mapping + error helpers
│   ├── storage/
│   │   ├── encoding/                  # Key encoding (data_keys, metadata_keys, value_encoding, serialization)
│   │   ├── tikv_store/               # TiKV operations (tables, indexes, schemas, sequences, cron, worker, statistics, migrations)
│   │   └── kv_stats.rs               # KV read statistics tracking (task-local)
│   ├── worker/                        # Unified async task engine (Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql)
│   ├── cron/                          # pg_cron-compatible scheduler (parser, types, config, worker, process_list)
│   ├── extensions/                    # HTTP extensions + fs9 file system (backend, decoders, streaming, glob)
│   ├── auth/                          # Authentication + RBAC
│   ├── model/                         # Data model types (DataType, Value, Row, TableSchema)
│   ├── txn/                           # Transaction state + savepoints
│   ├── main.rs                        # Server entry point (TLS, worker/cron startup)
│   ├── cli.rs                         # CLI argument parser
│   ├── config.rs                      # Server configuration
│   ├── session_context.rs             # Tokio task-local session context
│   ├── observability.rs               # Logging, tracing, metrics
│   ├── pool.rs                        # TiKV connection pool
│   └── tls.rs                         # TLS setup
├── docs/                              # Architecture + feature docs
├── tests/                             # SQL integration tests (563 test files)
├── orm-tests/                         # TypeORM, Prisma, Sequelize compatibility
└── scripts/
```

## Where to Look

| Task | Location |
|------|----------|
| Add SQL function | `src/sql/expr/functions/` (13 categories: array, datetime, encoding, fs9, fts, json, math, misc, pg_compat, regex, string, uuid, vector) |
| Add SQL statement | `src/sql/executor/` (DDL in `ddl.rs`/`ddl/`, DML in `dml_analyzed/`, SELECT in `select/analyzed/`) |
| Fix type inference | `src/sql/types/infer.rs` |
| Fix analyzer / name resolution | `src/sql/analyzer/` (query/, expr/, scope.rs) |
| Optimizer / query planning | `src/sql/optimizer/` (logical_planner/ → rewrite/ → physical_planner/ → build/) |
| Join reordering | `src/sql/optimizer/join_reorder/` (algorithms.rs, cost.rs, predicates.rs) |
| Subquery decorrelation | `src/sql/optimizer/rewrite/decorrelate.rs` |
| Physical operators | `src/sql/operators/` (scan, join, hash_join/, hash_semi_join, sort, aggregate, window/, etc.) |
| Index planning / scan strategy | `src/sql/planner/` (mod.rs, index_selection.rs, predicate.rs, scan_type.rs, hnsw_predicate.rs) |
| HNSW vector index | `src/sql/hnsw/` (mod.rs, storage.rs) + `src/sql/operators/hnsw_scan.rs` + `src/sql/planner/hnsw_predicate.rs` |
| EXPLAIN output | `src/sql/explain/` (mod.rs, format.rs, transform.rs) |
| Add PostgreSQL type mapping | `src/protocol/handler/encode/types.rs` |
| Change key encoding | `src/storage/encoding/` (data_keys.rs, metadata_keys.rs, value_encoding.rs, serialization.rs) |
| Catalog / pg_catalog views | `src/sql/catalog/` (37 virtual table implementations) |
| Multi-tenancy | `src/pool.rs` + `src/protocol/handler/tenant.rs` |
| Triggers | `src/sql/triggers/` (cache, before, rewrite, enqueue, execute) + `src/sql/executor/triggers.rs` (DDL) |
| Transaction state | `src/txn/` (state.rs, savepoints.rs) |
| Session state / GUCs | `src/sql/session/` (mod.rs, settings.rs, transaction.rs) + `src/session_context.rs` |
| Worker / background tasks | `src/worker/` (engine.rs, types.rs, config.rs, gc.rs, metrics.rs) |
| Cron scheduling | `src/cron/` (parser.rs, types.rs, config.rs, worker.rs, process_list.rs) |
| DDL (CREATE/ALTER/DROP) | `src/sql/ddl/` (alter_table.rs, create_index.rs, create_table.rs, drop.rs, view.rs) |
| DML helpers / FK validation | `src/sql/dml/` (foreign_keys.rs, defaults.rs, insert.rs, update.rs, delete.rs) |
| PL/pgSQL | `src/sql/plpgsql/` (parser.rs, executor.rs, utils.rs) |
| fs9 file operations | `src/extensions/fs/` (mod.rs, backend.rs, decoders.rs, streaming.rs, glob.rs) |

## Build & Test Commands

```bash
cargo build
cargo test
PD_ENDPOINTS=127.0.0.1:2379 cargo run
python3 scripts/integration_test.py
cd orm-tests && npm test
```

## Dev Environment (EKS, us-west-2)

AWS SSO login: `aws sso login --profile sso` (PingCAP SSO, account `385595570414`)

### TiKV (namespace: `tidb-serverless`)

| Item | Value |
|------|-------|
| PD Endpoint | `serverless-cluster-pd.tidb-serverless.svc.cluster.local:2379` |
| TiKV nodes | 9 (3 pools × 3 replicas) |
| TLS secret | `serverless-cluster-cluster-client-secret` (ca.crt, tls.crt, tls.key) |

### S3

| Item | Value |
|------|-------|
| FS9 dev bucket | `dev-us-west-2-f02-db9-fs` |
| FS9 staging bucket | `staging-us-west-2-f02-db9-fs` |
| IRSA ServiceAccount | `db9-fs-access` (namespace `db9`) → `arn:aws:iam::385595570414:role/dev-us-west-2-f02-db9-fs-irsa` |
| Region | `us-west-2` |

### db9-server (namespace: `db9`)

| Item | Value |
|------|-------|
| External LB | `a0082d61f575740c0a6c8043480822b8-5c40c37ca03be403.elb.us-west-2.amazonaws.com:5433` |
| Credentials | `admin` / `admin` |
| ECR image | `385595570414.dkr.ecr.us-west-2.amazonaws.com/pg-tikv:latest` |

## SQL Test Contract (must follow)

- `.expected` > `.errors` > `.assert` priority; use only one validation mode per test.
- Always enforce deterministic output (`ORDER BY`, fixed values, no random-dependent assertions).
- Do not update expected outputs blindly; validate against real PostgreSQL first.
- **Before changing any `.expected`, `.errors`, or `.assert` file, you MUST run the corresponding `.sql` against real PostgreSQL 17.7 and verify the new expected output matches PG's actual output.** No exceptions — guessing what PG returns is not acceptable.

## Agent Team Methodology (must follow)

When using multi-agent teams for issue triage, planning, refactoring analysis, or any code investigation:

### 1. Evidence over opinion
- Every agent **must read actual source code** (file paths, line numbers, function signatures) before making claims.
- Use `grep`, `git log`, `git blame`, and file reads — not just `gh issue view`.
- Conclusions without code evidence are rejected.

### 2. Verify, don't assume
- Agents must **check current master** to confirm whether an issue is actually fixed or still present.
- Run targeted searches (e.g., grep for the function name, read the specific file) rather than trusting issue descriptions at face value.
- If an agent claims "this is fixed," it must cite the merged PR **and** verify the fix exists on master.

### 3. Agents must challenge each other
- When multiple agents analyze overlapping areas, they must **find and surface disagreements**.
- Consensus without debate is a red flag — push agents to argue from different perspectives.
- The final output should reflect resolved disagreements, not rubber-stamped agreement.

### 4. Realistic time estimates
- Estimates must be grounded in **actual codebase metrics**: file sizes (lines of code), module coupling (how many callers/callees), test coverage gaps, CI turnaround time.
- Factor in network latency, build times, and dependency chains.
- No hand-waving. If an agent says "2 days," it must explain why (e.g., "mapping.rs is 400 lines, 6 call sites to update, 3 integration tests to add, CI takes ~15 min").

### 5. Structured output with evidence
- Final output must include **evidence tables**: issue number, affected file:line, current state on master, recommended action, estimated effort with justification.
- Group issues by **code hotspot** (which module/file), not by issue type — the goal is to find concentrated refactoring targets that batch-resolve multiple issues.
- Prioritize by **issues resolved per refactoring effort** (bang for buck).

### 6. Test hypotheses
- When feasible, agents should **run commands** to validate claims (e.g., `cargo test`, `grep` for patterns, count occurrences of duplicated code).
- "I believe this is duplicated 15 times" must become "I confirmed 15 occurrences in these files: [list]."

## Documentation

- `docs/ARCHITECTURE.md` - Architecture entry point (module map, execution pipeline, documentation guide)
- `src/sql/AGENTS.md` - SQL execution layer navigation (code paths and symbols)
- `docs/sot/README.md` - Source of Truth module registry (normative contracts)

For the full module-by-module documentation index, see [docs/ARCHITECTURE.md §4 Module Map](docs/ARCHITECTURE.md).
