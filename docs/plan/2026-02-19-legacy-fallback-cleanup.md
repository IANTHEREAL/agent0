# Legacy/Fallback Cleanup Plan (2026-02-19)

## Scope and Goal
- Enforce one deterministic architecture path (`Analyzer -> Typed IR -> Optimizer -> Executor/Operators`).
- Remove non-sunset fallback/legacy code that hides semantic or execution inconsistencies.
- Fix root causes only (no wire/test-layer masking).

## Tracked Todo (7 Items)
1. View dependency extraction empty-deps fallback (`src/sql/ddl.rs`) — status: done in branch, validate by tests.
2. `tipg.use_optimizer` dual-path compatibility plumbing/tests — status: in progress; runtime must remain single path.
3. `QueryContext` implicit default/task-local fallback — status: in progress; move to explicit context threading.
4. RETURNING parser fallback path — status: in progress; keep deterministic parse path only.
5. Legacy relation-name conflict scan (pre-`_sys_relname_`) — status: temporary keep with sunset date.
6. Legacy schema deserialization upgrade path — status: temporary keep with sunset date.
7. Catalog introspection scalability + new overflow regression — status: overflow fixed; scalability still in progress.

## Issue Tracker and Progress

| ID | Issue | Root Cause | Current Progress | Next Step |
|---|---|---|---|---|
| 1 | View dependency extraction silently falling back to empty deps | Error path returned empty dependency set on parse failure | Implemented hard-error behavior; no silent empty dependency fallback | Revalidate with DDL dependency/drop-cascade tests |
| 2 | `tipg.use_optimizer` dual-path compatibility plumbing | Historical dual-path compatibility surface still present in code/tests | Runtime behavior already single-path; compatibility handling cleanup is partial | Remove remaining dead dual-path plumbing and align tests/docs to no-op compatibility |
| 3 | `QueryContext` implicit defaults/task-locals | Execution paths still rely on `from_task_locals()` defaulting | Strict mode groundwork exists; call-site cleanup incomplete | Finish explicit context threading and remove production default reliance |
| 4 | Parser RETURNING fallback path | Parse-error fallback masked parser gaps | Deterministic RETURNING parsing path landed; fallback removal validation still pending | Finish parser cleanup validation and remove leftover fallback scaffolding |
| 5 | Legacy relation-name conflict scan (`pre-_sys_relname_`) | Migration compatibility for old clusters | Intentionally retained as temporary shim with warning + sunset | Keep until sunset window closes, then remove |
| 6 | Legacy schema deserialization upgrade path | Migration compatibility for old serialized schema bytes | Intentionally retained as temporary shim with warning + sunset | Keep until sunset window closes, then remove |
| 7 | Catalog introspection scalability + stack overflow P0 | (a) deep recursive SQL AST/Analyzer operations on default worker stack; (b) catalog virtual-table scans still O(N) on introspection joins | Stack-overflow crash is now fixed on default stack with explicit grown-stack lifecycle boundaries; TypeORM schema hook timeout remains | Complete predicate-aware catalog scan/preload path to remove O(N) introspection latency |

## Progress Snapshot (as of 2026-02-19)
- `docs/plan` file created and tracking all 7 items with grouped root causes.
- P0 stack-overflow crash is fixed for the 2000-term OR repro on default runtime stack.
- Root-cause fix applied in architecture path (no fallback masking):
  - explicit deep-stack boundary module: `src/sql/stack_safety.rs`
  - stack-safe drop lifecycle for deep parser/analyzed trees
  - removal of recursive `stmt.to_string()` observability formatting from runtime path
  - analyze/rewrite stack budget restored to deterministic high-water boundary
- Remaining in Group C: TypeORM schema compatibility still times out in synchronize/idempotency path due catalog introspection scalability.
  - Repro evidence: `npm --prefix orm-tests test -- --reporter=verbose -t 'TypeORM Schema & Metadata Compatibility'` fails in idempotency hook timeout.

## Root-Cause Work Groups

### Group A: Parser fallback removal
- Root cause: parse-error recovery fallback in DML RETURNING path masked parser gaps.
- Changes:
  - Remove parse-error fallback entry.
  - Keep one deterministic DML RETURNING parse flow.
- Acceptance:
  - Parser unit tests pass.
  - No `try-legacy-then-new` parse path remains for RETURNING.

### Group B: Explicit query context threading
- Root cause: runtime behavior depended on task-local/default fallbacks (`postgres`, `0`, timezone defaults).
- Changes:
  - Remove implicit runtime defaults from production paths.
  - Thread `QueryContext` explicitly through executor/operator paths.
- Acceptance:
  - Production code paths do not rely on `QueryContext::from_task_locals()` defaults.
  - Missing context fails early and clearly.

### Group C: Catalog introspection architecture
- Root cause:
  - Existing virtual catalog scans were O(number_of_tables) with repeated per-table schema fetches.
  - New preload refactor introduced a stack-overflow regression in ORM-style disjunctive catalog queries.
- Changes:
  - Build one-pass schema preload in scan context.
  - Migrate heavy virtual tables to consume preloaded schemas only.
  - Eliminate recursion/loop causing stack overflow.
  - Keep logic single-path, deterministic.
- Acceptance:
  - No crash for `information_schema.columns` disjunctive filters.
  - TypeORM schema sync no longer times out on catalog scans from this root cause.

### Group D: Migration shims governance
- Root cause: old-cluster compatibility shims still required for migration window.
- Changes:
  - Keep only explicitly sunsetted migration shims (#5, #6).
  - Keep warnings + concrete sunset date in code/comments/docs.
- Acceptance:
  - No non-sunset fallback paths remain.

## Execution Order
1. Fix Group C P0 overflow regression first.
2. Complete Group A parser cleanup validation.
3. Complete Group B explicit context threading.
4. Finish Group D governance cleanup/annotations.
5. Run full `docs/testing.md` matrix and capture exact pass/fail evidence.
6. Update PR with root-cause summary, architecture rationale, and validation artifacts.

## Validation Matrix (must run)
- `cargo test`
- `python3 scripts/integration_test.py --dsn "$PG_DSN"`
- `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/`
- `bash scripts/regression_gate.sh`
- `cd orm-tests && PG_DSN="$PG_DSN" npm test`
- Tier2 e2e smokes from `docs/testing.md`
- `./run_tests.sh`

## PostgreSQL Oracle Rule
- If any `.expected` / `.assert` / `.errors` files are changed, validate the corresponding `.sql` against PostgreSQL 17.7 first and align outputs with PG semantics before commit.

## 2026-02-20 Overflow Investigation Log (P0)

### Repro (stable)
- Minimal crashing SQL:
  - `SELECT (SELECT oid FROM pg_catalog.pg_class WHERE relname = 'pg_class' AND relnamespace = (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'pg_catalog')) = 1;`
- Symptom:
  - server process aborts with `thread 'tokio-runtime-worker' has overflowed its stack`.

### What was ruled out
- Not an OID comparison primitive bug:
  - `SELECT oid = 1 FROM pg_catalog.pg_class LIMIT 1;` passes.
- Not catalog query itself in standalone execution:
  - `SELECT oid FROM pg_catalog.pg_class ... relnamespace=(SELECT oid FROM pg_namespace ...)` passes.
- Not the new filter/join predicate split path:
  - temporary disabling of filter split did not change crash behavior.
- Not pre-materialization recursion depth explosion in `pre_materialize_async_exprs` / `materialize_expr_for_row` counters:
  - debug depth guards did not trip before overflow.

### Fault-layer localization (with instrumentation)
- Outer query (`select:no-from`) enters `execute_via_optimizer` Step1 pre-materialization.
- Projection pre-materialization starts, enters `ScalarSubquery(pg_class)` execution.
- Nested `pg_class` query enters Step1 pre-materialization and reaches `WHERE` pre-materialization start.
- Overflow occurs exactly inside nested `WHERE` pre-materialization path before entering inner `ScalarSubquery(pg_namespace)` branch.
- Standalone `pg_class` query with same `WHERE` pre-materialization completes successfully.

### Current root-cause hypothesis (architecture-level)
- The crash is triggered by recursive eager pre-materialization of projection subqueries (outer query pre-materializing projection, which recursively executes subqueries that also pre-materialize).
- Same inner query path is safe standalone but overflows when invoked under outer projection pre-materialization, indicating stack-budget composition issue across nested async execution frames.
- This is an execution-architecture issue (eager recursive pre-materialization in projection path), not a parser/AST fallback issue.

### Proposed clean direction (no fallback/patch masking)
- Remove eager Step1 pre-materialization for projection expressions in `execute_via_optimizer`.
- Keep Step1 pre-materialization for clauses where it is planning-critical (WHERE/HAVING/JOIN ON/GROUP/ORDER paths as needed).
- Rely on existing runtime async projection materialization (`project_rows` -> `materialize_expr_for_row`) for deterministic evaluation.
- Re-validate sequence-function semantics explicitly after this refactor (avoid changing per-row semantics accidentally).

### Status
- Investigation complete to fault layer; fix implementation in progress.
- Instrumentation currently present in working tree for localization and will be removed before final commit.

## 2026-02-20 Test Matrix + New Root Cause (Harness)

### Full docs/testing.md Matrix Results (current branch)
- `cargo test` — passed (`1711 passed, 0 failed`).
- `python3 scripts/integration_test.py --dsn "$PG_DSN"` — passed (`8 passed, 0 failed`).
- `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/` — passed (`245 passed, 0 failed`).
- `bash scripts/regression_gate.sh --dsn "$PG_DSN"` — passed:
  - unit tests passed,
  - extended protocol smoke passed (`13/13`),
  - regression SQL pack passed (`37/37`),
  - ORM regression pack passed (`122/122`).
- `cd orm-tests && PG_DSN="$PG_DSN" npm test` — passed (`721 passed, 7 skipped, 0 failed`).
- `PG_DSN="$PG_DSN" bash scripts/e2e_tests.sh gorm_smoke` — passed.
- `PG_DSN="$PG_DSN" bash scripts/e2e_tests.sh sqlalchemy_smoke` — passed.
- `PG_DSN="$PG_DSN" bash scripts/e2e_tests.sh dify_sqlalchemy_compat` — passed (`19 passed`).
- `./run_tests.sh` — passed after harness fix (`integration 245/245, orm 585 passed + 1 skipped`).

### New Root Cause Found (Non-engine, but critical for signal quality)
- Symptom:
  - First `./run_tests.sh` run reported `239 passed / 6 failed` in integration, with failures in fs9 and CIC tests.
- Root cause:
  - `run_tests.sh` defaulted to fixed port `15433`.
  - A stale existing server already occupied `15433`.
  - New pg-tikv startup failed with `Address already in use`, but script readiness only checked `pg_isready`; it accidentally connected to stale server and continued.
  - This produced false failures unrelated to current branch code.
- Fix applied (clean harness fix, not test masking):
  - `run_tests.sh` now selects a free ephemeral port when `PG_PORT` is not explicitly set.
  - If `PG_PORT` is explicitly set and occupied, script fails fast with clear error.
  - Readiness now requires spawned pg-tikv PID to remain alive; cannot pass readiness against an unrelated stale instance.
  - DSN/psql host-port wiring uses explicit `PG_HOST`/`PG_PORT`.
- Verification:
  - Re-ran `./run_tests.sh`; integration and ORM both passed end-to-end on the spawned instance.
