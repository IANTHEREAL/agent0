# db9-server Architecture Review (Codex Independent Report)

**Date**: 2026-02-20  
**Reviewer**: Codex (independent review)  
**Method**: code-first inspection (no trust in existing docs), with direct source references from `/home/zhaiyl/Work/agents/w3/db9`

---

## 0. Executive Verdict

I agree with the peer report on the **main risk direction**: protocol layer semantics are the biggest architectural debt.

I do **not** agree with several detailed claims that are now outdated or overstated.

My final verdict is harsh and simple:
- Core SQL execution architecture is solid.
- Protocol boundary design is still architecturally wrong for PG-compat prepared statements.
- Maintainability debt is real (god files + mixed responsibilities), but some findings in the peer report need correction.

---

## 1. Actual Architecture From Code

### 1.1 SELECT execution path (single semantic path)

Pipeline in code:
1. view expansion + catalog prefetch + privilege check + analyzer + post-analysis rewrite
2. optimizer (`AnalyzedQuery -> LogicalPlan -> PhysicalPlan`)
3. build physical operators + execute

Evidence:
- `src/sql/executor/core/analyze_rewrite.rs:29`
- `src/sql/executor/select/analyzed/mod.rs:45`
- `src/sql/optimizer/mod.rs:107`
- `src/sql/executor/select/mod.rs:74`

### 1.2 Extended protocol path (critical divergence point)

`Parse/Bind/Execute` does SQL-text parameter substitution before execution:
- `src/protocol/handler/dynamic.rs:1881`
- `src/protocol/handler/params/substitute.rs:174`

`Describe` uses separate heuristic type inference stack, not Analyzer output:
- `src/protocol/handler/dynamic.rs:1925`
- `src/protocol/handler/type_infer.rs:893`

This is the largest architectural mismatch with PostgreSQL behavior.

---

## 2. Peer Report: Confirmed Findings

I confirm these findings as materially correct:

1. God files / low modularity are real.
- `src/sql/ddl.rs:1` (4024 LOC)
- `src/sql/planner.rs:1` (3531 LOC)
- `src/sql/expr/typed_eval.rs:1` (3172 LOC)
- `src/sql/executor/select/analyzed/mod.rs:1` (2748 LOC)
- `src/sql/executor/core/view_rewrite.rs:1` (1362 LOC)
- `src/protocol/handler/dynamic.rs:1` (2083 LOC)

2. Optimizer still depends on legacy planner access-path logic.
- `src/sql/optimizer/physical_planner.rs:248`
- `src/sql/planner.rs:316`
- `src/sql/planner.rs:351`

3. GIN plan/runtime mismatch exists.
- optimizer/operator build falls back to table scan: `src/sql/optimizer/build.rs:461`
- operators planner same fallback: `src/sql/operators/planner.rs:252`
- EXPLAIN may still show GIN index scan shape: `src/sql/explain.rs:614`

4. `pre_materialize_async_exprs()` is oversized and fragile.
- large manual recursive reconstruction: `src/sql/executor/select/analyzed/mod.rs:166`
- wildcard fallback weakens change-safety for future enum variants: `src/sql/executor/select/analyzed/mod.rs:953`

5. Error swallowing via `.ok()?` in protocol inference is real.
- `src/protocol/handler/dynamic.rs:144`
- `src/protocol/handler/dynamic.rs:241`
- `src/protocol/handler/dynamic.rs:342`
- `src/protocol/handler/type_infer.rs:244`

6. Binder legacy module is still live in view dependency path.
- `src/sql/ddl.rs:2149`
- `src/sql/ddl.rs:2187`
- `src/sql/ddl.rs:2332`
- `src/sql/binder/mod.rs:167`

7. Regex is compiled per evaluation in operator path.
- `src/sql/expr/operators.rs:235`
- `src/sql/expr/operators.rs:253`
- `src/sql/expr/operators.rs:270`
- `src/sql/expr/operators.rs:288`

8. `FOR SHARE` is upgraded to exclusive lock semantics.
- `src/storage/tikv_store/tables.rs:7`

9. Stub client panic risk exists if stub leaks to production path.
- `src/storage/tikv_store/mod.rs:94`

10. Compatibility gaps confirmed by code surface.
- no real `unknown` type in `DataType`: `src/types/mod.rs:53`
- only partial unknown-literal emulation: `src/sql/analyzer/expr.rs:1090`
- numeric backend max 28 digits: `src/types/mod.rs:72`
- `CREATE DOMAIN` unsupported: `src/sql/raw_sql.rs:278`
- catalog set missing `pg_operator`/`pg_cast`/`pg_stat_user_tables` registrations: `src/sql/catalog/mod.rs:74`

---

## 3. Peer Report: Partially Correct (Needs Correction)

1. “No visitor/transform abstraction on TypedExpr” is outdated.
- traversal exists: `src/sql/expr/typed_visit.rs:1`
- rewrite infra exists: `src/sql/expr/typed_rewrite.rs:1`
- true issue is duplicated/manual usage in specific hotspots, not total absence.

2. “Savepoint rollback non-atomic and can persist partial rollback” is overstated.
- rollback applies undo in current transaction: `src/sql/session.rs:805`
- on undo failure, transaction is aborted: `src/sql/session.rs:833`
- risk is operational complexity/failure handling cost, not obvious persisted partial commit semantics.

3. “SQLSTATE mapped by handler string matching” is partially outdated.
- typed mapping exists: `src/protocol/handler/errors.rs:8`
- but string matching still appears in DML unique conflict branch: `src/sql/dml.rs:333`

4. “Optimizer cost model is only placeholder” is partially true.
- legacy heuristics are still present: `src/sql/optimizer/physical_planner.rs:25`
- but selectivity + column stats are integrated: `src/sql/optimizer/physical_planner.rs:235`, `src/sql/optimizer/selectivity.rs:26`

5. “No GIN EXPLAIN notation” is inaccurate.
- EXPLAIN does print GIN index scan shape: `src/sql/explain.rs:614`
- real issue is EXPLAIN/runtime drift because execution still falls back to table scan.

---

## 4. Peer Report: Not Confirmed / Incorrect

1. “Aggregate collection has two independent implementations that drift.”
- current logical planner delegates to shared collector in build module:
- `src/sql/optimizer/logical_planner.rs:497`
- `src/sql/optimizer/build.rs:1546`

2. “`sql/dml.rs` and `executor/dml_analyzed.rs` mutually import each other.”
- confirmed one-way dependency (`dml_analyzed -> dml`): `src/sql/executor/dml_analyzed.rs:18`
- no reverse import found in `src/sql/dml.rs:1`

3. Some LOC numbers are stale on current code snapshot.
- example: `src/sql/expr/operators.rs` is 954 LOC now, not 1243.

---

## 5. Additional High-Risk Findings I Emphasize

1. Dead optimizer GUC signal (misleading operational contract).
- Status after PR #847: partially fixed.
- `db9.use_optimizer` is now explicit compatibility no-op/readback-only:
- `src/sql/session.rs:254`
- `src/sql/session.rs:412`
- dead field/task-local threading was removed, but acceptance still exists.

2. Context propagation still heavily task-local based and deeply nested in dispatch.
- nested scopes in execution entry: `src/sql/executor/core/dispatch.rs:78`, `src/sql/executor/core/dispatch.rs:664`
- task-local definitions spread across modules:
- `src/sql/query_context.rs:10`
- `src/sql/statement_time.rs:14`
- `src/session_context.rs:6`
- `src/txn/mod.rs:16`
- `src/extensions/context.rs:30`
- `src/storage/kv_stats.rs:29`

3. RBAC bypass when `current_role` is None is explicit and used by user-function path.
- bypass branch: `src/sql/executor/core/statement.rs:17`
- user function passes `None`: `src/sql/executor/user_function.rs:162`

---

## 6. Updated Conclusion (Independent)

### Architecture score (my view)

- Execution core (Analyzer->Optimizer->Executor): **A-**
- Protocol boundary correctness: **D+**
- Maintainability/modularity: **D+**
- Error architecture consistency: **C**
- PG compatibility (advanced features + protocol semantics): **C**

### Hard conclusion

The system is **not architecturally broken at query-core level**; it is **architecturally inconsistent at protocol boundary level**.

If this project wants strong PG/ORM compatibility, protocol-layer semantic duplication and SQL-text parameter substitution must be treated as **P0 architecture debt**, not cleanup debt.

---

## 7. Priority Actions (Recommended)

1. P0: replace SQL text substitution with real typed parameter binding path (`Parse/Bind/Execute` parity with PG).
2. P0: make `Describe` metadata derive from Analyzer/typed pipeline output, not a parallel inference engine.
3. P1: remove `.ok()?` swallowing in protocol inference paths and return structured errors.
4. P1: make GIN runtime fallback explicit in EXPLAIN/NOTICE, or implement true GIN operator execution.
5. P1: refactor `pre_materialize_async_exprs()` onto shared typed rewrite traversal.
6. P1: remove/replace dead `db9.use_optimizer` toggle semantics.
7. P1: propagate security context into internal SQL execution paths (especially user functions).
8. P2: split god files (`ddl.rs`, `planner.rs`, `dynamic.rs`, `typed_eval.rs`) into subsystem modules.

---

## 8. Status Update After PR #847 (Merged 2026-02-20)

### 8.1 What improved

1. Legacy fallback cleanup landed (tracking issue #844 closed).
2. Parser RETURNING fallback was removed (shim count reduced).
3. `db9.use_optimizer` dead runtime threading was removed; setting is kept as compatibility no-op.
4. Query-context scoping became cleaner (`with_scoped_query_context`) and less manually threaded.
5. Legacy fallback paths now include explicit sunset policy annotations.

### 8.2 What is still unresolved (high priority)

1. P0 protocol mismatch is unchanged:
- Execute still performs SQL text substitution:
  - `src/protocol/handler/dynamic.rs:1881`
  - `src/protocol/handler/params/substitute.rs:174`
- Describe still uses protocol-side heuristic inference:
  - `src/protocol/handler/dynamic.rs:1925`
  - `src/protocol/handler/type_infer.rs:893`

2. `pre_materialize_async_exprs()` became larger, not smaller:
- `src/sql/executor/select/analyzed/mod.rs:266`
- now spans roughly 1.1k lines (manual per-variant reconstruction).

3. Tree-walk logic remains fragmented:
- recursive predicate walk: `src/sql/expr/typed_visit.rs:1`
- iterative stack walk: `src/sql/expr/classify.rs:44`
- async recursive rewrite: `src/sql/expr/typed_rewrite.rs:39`
- large manual traversal in pre-materialization path: `src/sql/executor/select/analyzed/mod.rs:266`

### 8.3 New or amplified structural smell after #847

1. Oversized executor files are now explicit split targets:
- `src/sql/executor/select/analyzed/mod.rs:1` (2748 LOC)
- `src/sql/executor/core/view_rewrite.rs:1` (1362 LOC)

2. `QueryContext::from_task_locals()` is strict fail-fast in production:
- `src/sql/query_context.rs:96`
- this is preferable to silent defaults, but missing context now panics.

### 8.4 Updated issue alignment

1. Keep P0 unchanged:
- #867 remove SQL text substitution
- #868 Analyzer-backed Describe metadata

2. Update P1 structural tracks:
- #870 should explicitly unify the 3+ tree-walk patterns and target `pre_materialize_async_exprs` first.
- #869 should use updated LOC baselines and remain focused on its current module list.
- add dedicated issue for oversized executor file split (`select/analyzed/mod.rs`, `view_rewrite.rs`).

3. Keep independent quick wins unchanged:
- #871 error unification
- #872 regex cache
- #873 NULL guard dedup
