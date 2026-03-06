# db9-server Architecture Review (From Code, Not Docs)

**Status**: Historical

**Reviewed**: All code under `src/` (~170K lines of Rust)
**Date**: 2026-02-20
**Method**: Deep-dive code reading across all 7 major subsystems + targeted verification of competing analysis
**Verdict**: Sound pipeline design undermined by a protocol-layer bypass that violates its own single-path principle

**Revision note**: This is the updated version incorporating verified findings from a competing analysis that focused on the protocol-executor boundary. All claims below are confirmed by reading specific source code.

**Post-PR #847 update**: Reflects state after PR #847 merge (24 files, +1499/-616). Several code smells improved; new observations added.

> **Review record / non-SoT note**
>
> This document is a point-in-time architecture review, not a current architecture contract.
> Validate current behavior against `docs/sot/**`, `docs/ARCHITECTURE.md`, and the implementation under `src/**` before acting on any recommendation here.

---

## 1. Actual Architecture (What the Code Says)

### 1.1 The Advertised Pipeline (Correct for SELECT execution)

```
Client -> TCP -> pgwire (DynamicPgHandler)
                  |
              SQL Parser (sqlparser 0.40)
                  |
          +-- View Expansion (rewriter.rs, async, pre-Analyzer)
          |
      Analyzer (analyzer/) -> AnalyzedQuery / TypedExpr
          |
      Optimizer (optimizer/) -> LogicalPlan -> PhysicalPlan
          |
      Operator Build (optimizer/build.rs) -> BoxedOperator tree
          |
      Executor (executor/) -> pull-based row iteration
          |
      TikvStore (storage/) -> TiKV cluster
```

### 1.2 The Extended Protocol Pipeline (What Actually Happens for Parse/Bind/Execute)

```
Client -> Parse($1, $2...)
            |
        substitute_parameters()   <-- TEXT SUBSTITUTION into SQL string
            |
        Re-parse substituted SQL
            |
        Same pipeline as above (Analyzer -> Optimizer -> Executor)

Client -> Describe
            |
        type_infer.rs             <-- PARALLEL heuristic inference (NOT Analyzer)
            |
        Returns column types to client
```

**This is the single most important architectural flaw: the extended protocol (used by all ORMs, drivers, and prepared statements) bypasses the type-safe parameter binding that PostgreSQL guarantees.**

### Code Distribution

```
Total codebase (src/): ~170K lines
+-- sql/                104,643 lines (61%)
|   +-- executor/        22,216 lines
|   +-- optimizer/       12,149 lines
|   +-- expr/            11,754 lines
|   +-- analyzer/        10,045 lines
|   +-- operators/        7,964 lines
|   +-- catalog/          4,506 lines
|   +-- types/            3,941 lines
|   +-- triggers/         1,901 lines
|   +-- binder/             943 lines (legacy)
|   +-- top-level files  29,224 lines  <-- problem area
+-- protocol/             9,173 lines (5%)
+-- storage/              6,729 lines (4%)
+-- extensions/           4,352 lines (2.5%)
+-- worker/              ~2,000 lines (1%)
+-- types/                1,153 lines
+-- auth/                   979 lines
+-- pool.rs                 625 lines
+-- txn/                    358 lines
+-- main.rs, cli, config  2,498 lines
```

---

## 2. What's Actually Good

### 2.1 Single Execution Path (for SELECT)

Every SELECT goes through `Analyzer -> Optimizer -> Operators`. No "try new, fallback to old." No dual paths. This is rare in database projects at this maturity level and shows discipline.

### 2.2 TypedExpr IR

The analyzer produces a fully-typed expression tree where every node carries a resolved `DataType`. NULL constants carry contextual types. No runtime type inference needed. This is correct database engineering.

**Key invariant**: Every `TypedExpr` node has a concrete, non-Unknown `data_type`. The executor never needs to guess types.

### 2.3 Scope-based Name Resolution

The scope stack (`ScopeStack`) with depth tracking for correlated subqueries is clean. Three resolution modes (unqualified, qualified, stack-based) cover all SQL patterns. JOIN USING merging via hidden columns uses correct COALESCE semantics.

### 2.4 Optimizer Pipeline

`LogicalPlan -> rewrite rules -> PhysicalPlan -> BoxedOperator` is a textbook design:

- Immutable plan trees (safe transformation)
- Shared equi-key extractors across rewrite/planning/build (zero semantic drift)
- Predicate pushdown respects LEFT JOIN nullable-side semantics
- Deterministic cost model (repeatable for debugging)
- LIMIT pushdown to scan operators

### 2.5 Keyspace Isolation

All keys prefixed with `d_{db_id}_`. Per-tenant TikvStore instances. RAII TenantHandle for connection lifecycle. Multi-tenancy is structurally enforced, not convention-based.

### 2.6 Catalog Implementation

35+ virtual tables covering pg_catalog and information_schema. VirtualTable trait with registry pattern. Per-tenant database-scoped scanning.

### 2.7 Cast System

Context-dependent casting (Explicit/Assignment/Implicit) correctly models PostgreSQL behavior. 600+ lines of tests. Varchar(n) assignment semantics match PG SQLSTATE 22001.

---

## 3. Code Smells (Severity-Ordered)

### CRITICAL

#### 3.1 Extended Protocol Uses SQL String Substitution (NEW — Verified)

**Location**: `src/protocol/handler/params/substitute.rs` (440 lines) + `src/protocol/handler/dynamic.rs:1881`

The extended protocol (Parse/Bind/Execute, used by every ORM and prepared statement) does not perform real parameterized execution. Instead:

```rust
// dynamic.rs:1881
let final_query = substitute_parameters(query, portal)?;
// dynamic.rs:1902
executor.execute(session, &final_query).await  // re-parses substituted SQL
```

`substitute_parameters()` converts `$1`, `$2` etc. to quoted/typed literal values injected into the SQL text string, then the entire SQL is re-parsed. Tests confirm:

```rust
// "SELECT $1::text" + param "001" → "SELECT '001'::text"
// "SELECT $1" + param int4(42) → "SELECT 42"
// "SELECT $1" + param "O'Reilly" → "SELECT 'O''Reilly'"
```

**Why this is critical:**
1. **Plan caching is impossible** — every execution re-parses because literal values are baked into SQL
2. **Semantic gap** — PostgreSQL's extended protocol preserves parameter types through planning; here parameters are degraded to text
3. **Edge cases in quoting** — despite quote-escaping, subtle differences between parameter-typed execution and literal-substituted execution exist
4. **Performance** — unnecessary re-parse + re-analyze + re-optimize on every Execute
5. **Security model differs** — though not a SQL injection risk (values are properly escaped), the execution model is fundamentally different from PostgreSQL's

**This directly undermines the "single execution path" strength** — the extended protocol inserts a string-manipulation step before the pipeline that PostgreSQL doesn't have.

#### 3.2 Protocol Layer Duplicates Analyzer Semantics (NEW — Verified)

**Location**: `src/protocol/handler/type_infer.rs` (956 lines) + parameter inference in `dynamic.rs` (~287 lines)

The protocol's `Describe` response (which tells clients the types of output columns and parameters before execution) uses its **own parallel type inference** instead of calling the Analyzer:

- **type_infer.rs** (956 lines): Full query output column type inference — handles SELECT, INSERT, UPDATE, DELETE, CTEs, views, joins, table functions
- **dynamic.rs parameter inference** (~287 lines): AST-level heuristics for INSERT/UPDATE/SELECT parameter types

**Total: ~1,243 lines of heuristic type inference that duplicates and will drift from the Analyzer.**

Evidence of existing divergence risk:
- `generate_series` type matching logic differs between protocol layer (exact type pairs) and Analyzer (`common_type()` function)
- Protocol falls back to hardcoded `Text` type when inference fails (lines 874, 881, 941 in type_infer.rs)
- CTE column renaming logic exists in 3 separate places
- No expression type coercion in protocol layer — `SELECT 1 || 'text'` may get wrong Describe type

**Critical drift scenario**: Fix a type inference bug in the Analyzer → fixes execution but NOT the Describe response → clients get wrong metadata → ORM failures.

#### 3.3 God Files Everywhere

The codebase has a severe problem with files that are too large and do too much:

| File | Lines | What It Does |
|------|-------|-------------|
| `sql/ddl.rs` | 4,019 | CREATE/ALTER/DROP for TABLE, INDEX, VIEW, TRIGGER, SEQUENCE, FUNCTION, PROCEDURE, ALTER SYSTEM, and more. Not a file -- a whole subsystem. |
| `sql/planner.rs` | 3,531 | Index selection + scan planning + predicate analysis + typed filter analysis + all index type support. Should be 5+ files. |
| `expr/typed_eval.rs` | 3,052 | Expression evaluation -- massive match on every TypedExprKind |
| `executor/select/analyzed/mod.rs` | ~2,748 | Analyzed SELECT executor — **grew by ~428 lines in #847** (window frame handling) |
| `protocol/handler/dynamic.rs` | 2,083 | Connection lifecycle + auth + query execution + transaction management + COPY + parameter inference + session init |
| `storage/encoding.rs` | 1,798 | Key encoding with 200+ lines for Decimal alone |
| `sql/parser.rs` | ~1,599 | SQL parsing mixed with semantic concerns (reduced by #847: -157 lines) |
| `sql/rewriter.rs` | 1,555 | View expansion + query rewriting |
| `sql/session.rs` | 1,504 | Session state + settings + GUC handling |
| `sql/dml.rs` | 1,455 | DML helpers |
| `sql/explain.rs` | 1,357 | EXPLAIN formatting |
| `expr/operators.rs` | 1,243 | Binary/unary operator evaluation |

Files above 500 lines should be rare in a well-factored Rust codebase. Here, **12+ files exceed 1,300 lines**.

#### 3.4 Duplicated Logic Across Layers

**Aggregate Collection** -- Two separate implementations:
- `optimizer/logical_planner.rs` has `collect_aggregate_exprs()`
- `optimizer/build.rs` has `collect_agg_exprs_from()`
- Similar work, not shared. If one changes, the other silently diverges.

**Index Selection** -- The optimizer calls into the legacy `planner.rs`:
```rust
// optimizer/physical_planner.rs
crate::sql::planner::choose_best_access_path_for_typed_filter(...)
```
This means planner.rs (3,531 lines) exists as a dependency with its own duplicate predicate analysis. There are **two** predicate analysis functions:
- `analyze_predicates()` (AST-level, legacy path)
- `choose_best_access_path_for_typed_filter()` (TypedExpr-level, optimizer path)

Both do the same thing with different input types.

**Window Function Type Inference** -- Two sources of truth:
- `sql/types/registry.rs` hardcodes SUM/AVG window returns as Numeric
- `sql/types/infer.rs` independently overrides window function return types

**DML Execution** -- Unclear boundary:
- `sql/dml.rs` (1,455 lines) -- helpers and data structures
- `sql/executor/dml_analyzed.rs` -- the actual analyzed DML executor
- Both import from each other

#### 3.5 `pre_materialize_async_exprs()` is a Maintenance Nightmare

In `executor/select/analyzed/mod.rs`, this function manually pattern-matches on **every TypedExprKind variant** and reconstructs the tree to replace subquery nodes with materialized constants.

**Post-PR #847**: This function grew significantly (~2,748 lines in analyzed/mod.rs total, up from ~2,320) due to added window frame handling. The growth makes splitting more urgent, not less.

1. Must be updated every time a new TypedExprKind variant is added
2. No compile-time guarantee of completeness (new variants fall through silently)
3. Duplicates a visitor pattern that should exist on TypedExpr

A proper `TypedExpr::transform()` or visitor method would reduce this to ~50 lines.

### SERIOUS

#### 3.6 Context Propagation via Task-Locals (NEW — Verified, Improved by #847)

**Locations**: 6 modules scattered across the codebase

**Post-PR #847 improvements:**
- `USE_OPTIMIZER` task-local removed (dead GUC fully cleaned up)
- `CURRENT_TIMEZONE` task-local added (replaces separate timezone propagation)
- `with_scoped_query_context()` introduced — consolidates `QueryContext` setup into a single wrapper
- `from_task_locals()` now panics on missing context instead of silent `None` fallback (correct fail-fast)
- Nesting reduced from 5 levels to ~3 levels in `dispatch.rs`

| Module | Task-Locals | Variables |
|--------|-------------|-----------|
| `sql/query_context.rs` | 4 | CONNECTION_ID, CURRENT_DATABASE_NAME, CURRENT_USER_NAME, CURRENT_TIMEZONE |
| `sql/statement_time.rs` | 2 | STATEMENT_TIMESTAMP_MILLIS, TRANSACTION_TIMESTAMP_MILLIS |
| `session_context.rs` | 2 | MAX_SORT_BYTES, CURRENT_SEARCH_PATH |
| `txn/mod.rs` | 1 | SAVEPOINTS |
| `extensions/context.rs` | 1 | CTX (composite: is_superuser, allow_local_fs, tenant_keyspace, http_requests) |
| `storage/kv_stats.rs` | 1 | KV_READ_STATS |

**Remaining problems:**
- Functions deep in the executor tree still implicitly depend on these variables with zero syntactic indication
- Compiler cannot help — no compile-time indication which context is needed
- Task-locals are still scattered across 6 modules instead of a single `ExecutionContext` struct

#### 3.7 RBAC Bypass for Internal Paths (NEW — Verified)

**Location**: `src/sql/executor/core/statement.rs:17-22`

```rust
let Some(username) = current_role else {
    // Internal execution path (trigger worker, internal plumbing).
    return Ok(());  // BYPASS: all privilege checks skipped
};
```

**AND** in `analyze_rewrite.rs:52`:
```rust
if current_role.is_some() {
    // Only check SELECT privilege if there's a user
    for table_name in catalog.base_table_full_names() {
        self.require_table_privilege(txn, current_role, Privilege::Select, table_name).await?;
    }
}
```

**Mitigation**: External connections always have `current_user` set during auth handshake. Only internal paths (trigger worker, UDF execution) pass None.

**But**: `user_function.rs` explicitly passes `None` to `execute_statement_on_txn()`, meaning **all SQL executed inside PL/pgSQL functions runs with no RBAC checks**. This is a design gap — PostgreSQL uses SECURITY DEFINER/INVOKER semantics; here it's always effectively SECURITY DEFINER with superuser privileges.

#### 3.8 Parser Boundary is Shim-Heavy (NEW — Verified, Reduced by #847)

**Location**: `src/sql/parser.rs` (~1,599 lines after #847, was 1,756)

**Post-PR #847**: RETURNING fallback (`try_parse_statement_with_returning_fallback()`) deleted (-157 lines). Down from 9 to 8 shims.

8 post-parse shims working around sqlparser-rs limitations:

| Shim | What It Does | Why |
|------|-------------|-----|
| `preprocess_reset_role` | `RESET ROLE` → `SET ROLE NONE` | sqlparser-rs lacks RESET ROLE |
| `preprocess_explain` | `EXPLAIN (ANALYZE, VERBOSE)` → keyword form | sqlparser-rs requires keyword, not parens |
| `preprocess_create_sequence` | Normalizes option ordering | sqlparser-rs expects fixed order; pg_dump doesn't |
| `preprocess_cte_materialized` | Strips `AS [NOT] MATERIALIZED` | sqlparser-rs can't parse CTE hints |
| `rewrite_all_any_subquery_parse_compat` | `ANY(SELECT)` → `ANY(ARRAY(SELECT))` | sqlparser-rs can't parse subquery form |
| `rewrite_jsonb_exists_ops` | `?`, `?|`, `?&` → function calls | sqlparser-rs operator gap |
| `rewrite_vector_distance_ops` | `<->`, `<#>`, `<=>` → function calls | sqlparser-rs no custom operators |
| `rewrite_at_time_zone_placeholders` | `AT TIME ZONE $n` → `AT TIME ZONE 'UTC'` | sqlparser-rs expects literal |

These aren't bugs — they're necessary workarounds. But they add ~470 lines of custom tokenizer and SQL-aware rewriting that must be maintained alongside sqlparser-rs upgrades.

#### 3.9 Three Competing Tree-Walk Implementations (Updated by #847)

The codebase now has **three** separate TypedExpr tree-walk implementations that are not unified:

1. **`typed_visit.rs`** — Recursive read-only visitor (`expr_any()`) covering all TypedExprKind variants
2. **`classify.rs`** — After PR #847, converted to iterative stack-based traversal (`push_expr_children()` + `expr_any_iter()`) for stack safety
3. **`typed_rewrite.rs`** — Async recursive rewriter (`SequenceMaterializeCtx::rewrite_expr()`) for sequence function materialization

Additionally, `typed_eval.rs` (3,052 lines) is one giant recursive match. `operators.rs` (1,243 lines) is another giant match. There are 55 call sites for `eval_typed_expr` across operators. The codebase has `typed_fold.rs` (620 lines), but it only folds constants.

The infrastructure for a canonical visitor/transform API partially exists but key paths (especially `pre_materialize_async_exprs()`) still use manual recursion instead of a shared abstraction.

#### 3.10 Error Handling: Structured Primary Path with Residual String Matching

Three error patterns coexist:
1. **`anyhow::Result`** -- everywhere. No structured error codes.
2. **`SqlError`** -- custom enum with SQLSTATE codes. Used in some places.
3. **`AnalyzerError`** -- the analyzer's own error type.

The protocol layer's **primary path is already structured**: `errors.rs` has `sqlstate_for_executor_error()` which performs `SqlError` downcast as the first check. Only residual cases fall through to string matching:
```rust
// dml.rs:332-334 — residual string matching
e.to_string().contains("duplicate key value violates unique constraint")
```
The remaining work is to eliminate these residual string-match paths by ensuring all error origins use `SqlError`.

#### 3.11 Silent GIN Index Degradation

When the optimizer encounters a GIN index scan:
```rust
// optimizer/build.rs
ScanType::Gin { .. } => {
    // GIN not implemented in operator path yet -- fall back to full scan
}
```
No warning. No EXPLAIN notation. A query that should use a GIN index for full-text search silently does a full table scan.

#### 3.12 Protocol Handler Swallows Errors

```rust
// handler/dynamic.rs
let mut txn = store.begin().await.ok()?;  // Error -> None, loses context
```
Multiple `.ok()` calls convert structured errors into `None`. Parameter type inference defaults to TEXT when this happens, causing downstream ORM binding failures with no useful error message.

#### 3.13 Dead GUC and Legacy Fallbacks (NEW — Verified, Partially Resolved by #847)

**`db9.use_optimizer`** — **RESOLVED by PR #847** (scope of issue #844, now closed). The `use_optimizer` session field, task-local, and GUC handling have all been removed. Clean deletion.

**Legacy schema deserialization** (`encoding.rs:1130-1143`): Double-deserialization fallback — tries current format, falls back to legacy without `IndexDef.state` field. Necessary for clusters upgraded before ce73a8a.

**Legacy DDL name conflict** (`ddl.rs`): Dual-path check for pre-#775 clusters. **Post-PR #847**: Now has explicit sunset date (2026-12-31) and `warn_legacy_relname_conflict_scan_once()` emitting a one-time deprecation warning. This is good progress — the debt is now time-bounded.

#### 3.14 Savepoint Rollback is Non-Atomic

`prepare_rollback_to()` returns undo records that the caller must apply to TiKV asynchronously. If application fails midway (network error), some keys are rolled back and others are not. No recovery mechanism.

### MODERATE

#### 3.15 The Optimizer Cost Model is a Placeholder

Fixed heuristics:
- Scan: `rows * 0.01 + 1`
- Filter selectivity: `rows / 3` (no stats)
- Default estimated rows: 1000
- Join: Cartesian product, no selectivity for non-equi joins

Statistics infrastructure exists (ANALYZE, histograms, MCVs) but the cost model barely uses it. No cost-based join reordering.

#### 3.16 Test Stub Can Panic in Production

```rust
fn client(&self) -> &TransactionClient {
    self.client.as_ref().expect("TikvStore: no client...")
}
```
Called from every storage operation. If `TikvStore::new_stub()` leaks to production, the process panics.

#### 3.17 Binder — Risk Reduced but Debt Not Cleared

`src/sql/binder/` (943 lines) still has scope walking, CTE binding, and dependency extraction that duplicates the Analyzer. Used for view circular-dependency checks.

**Post-PR #847**: New AST-based `extract_dependencies_from_query()` added to `binder/mod.rs` — this replaces the old text-based SQL-round-trip approach for view dependency extraction. The old version is marked `allow(dead_code)`. Risk is reduced (no more SQL re-parsing for dep extraction), but the binder module itself still exists as structural debt.

#### 3.18 NULL Check Duplication in Operators

Every comparison and arithmetic operator repeats the same NULL guard:
```rust
BinaryOperator::Eq => {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        Ok(Value::Null)
    } else { ... }
}
// ... repeated 11 more times
```
A single guard at the top for strict operators would eliminate 40+ lines.

#### 3.19 Regex Compiled Per Row

```rust
BinaryOperator::PGRegexMatch => {
    match regex::Regex::new(&pattern) {  // compiled every row evaluation
```
For `WHERE col ~ '^foo'`, this compiles the regex for every row.

#### 3.20 Stack Safety Hardening (Added by #847 — Not a Smell)

**Location**: `src/sql/stack_safety.rs` (25 lines)

PR #847 added `with_grown_stack()` (32MB red zone, 64MB grown stack) and `drop_on_grown_stack()` for safely dropping deep SQL expression trees. This is **engineering hardening** — a reasonable safeguard against stack overflow on deeply nested queries.

However, it also exposes the underlying structural issue: recursive tree processing (in `typed_eval.rs`, `pre_materialize_async_exprs()`, etc.) has unbounded stack depth proportional to expression tree depth. The iterative conversion of `classify.rs` (also in #847) is the correct long-term direction — convert remaining recursive walkers to iterative or use a visitor abstraction with bounded stack.

---

## 4. Structural Issues (Architecture Level)

### 4.1 The `sql/` Directory is a Monolith

`sql/` contains 104,643 lines across 100+ files with ~15 distinct subsystems: analyzer, optimizer, executor, expression system, DDL, DML, session management, EXPLAIN, scan planning, FTS, check constraints, RBAC, sequences, triggers, statistics, PL/pgSQL, query rewriting. There is no layering enforced by the module system -- everything can import everything else.

### 4.2 No Clear Boundary Between Planning and Execution

- Optimizer produces `PhysicalPlan` -> `BoxedOperator`
- But executor also does planning work: `pre_materialize_async_exprs()` evaluates subqueries; DML executor builds its own scan plans; EXPLAIN re-runs the optimizer independently
- No single entry point for "a query starts executing"

### 4.3 Async/Sync Boundary is Unclear

```
Async (view expansion) -> Sync (analysis) -> Sync (optimization) -> Async (execution)
```

The async-sync-async sandwich forces awkward patterns. The Catalog trait is synchronous, so all catalog data must be prefetched before analysis. If data is missing, you get a late error with no ability to fetch more.

### 4.4 Session is a God Object

`Session` (1,504 lines) holds: transaction state, savepoint state, sequence cache, session settings, user identity, database identity, connection ID, timestamps, idle tracking, server config, TikvStore reference, observability reference. Classic "context object" anti-pattern.

### 4.5 Protocol-Executor Boundary Violation (NEW)

The protocol layer (`handler/`) does semantic work that belongs in the executor:
- **Type inference**: 1,243 lines of parallel heuristic inference in `type_infer.rs` + `dynamic.rs`
- **Parameter handling**: Text substitution in `substitute.rs` instead of passing typed parameters through the pipeline
- **Schema resolution**: Protocol layer independently resolves table schemas and view definitions

This creates a **dual semantic path**: Describe uses one inference engine, Execute uses another. PostgreSQL guarantees these are identical; here they can diverge.

---

## 5. PostgreSQL Compatibility Gaps

| Gap | Impact |
|-----|--------|
| Extended protocol uses text substitution, not real parameterized execution | Plan caching impossible; semantic differences from PG |
| No `unknown` literal type (PG's untyped string) | Type inference differs from PG for untyped parameters |
| JSON comparison `jsonb = text` rejected | ORMs comparing JSON to strings fail |
| Numeric precision max = 28 (PG = 131072) | Large decimal values silently truncated |
| No domain types | `CREATE DOMAIN` unsupported |
| No range types | `int4range`, `tsrange` unsupported |
| No collation support | All text uses default UTF-8 |
| No custom operators | `CREATE OPERATOR` unsupported |
| FOR SHARE -> FOR UPDATE upgrade | Silent behavior difference from PG |
| PL/pgSQL functions always bypass RBAC | Different from PG's SECURITY INVOKER default |
| Missing pg_stat_user_tables | ORMs can't read table statistics |
| Missing pg_operator | Operator resolution incomplete |
| Missing pg_cast | Cast introspection unavailable |
| No Interval coercion in UNION/CASE | Interval + other types in CASE/UNION fails |
| UserDefined type coercion missing | UNION with UDT and known type fails |

---

## 6. Summary Scorecard

| Aspect | Grade | Notes |
|--------|-------|-------|
| **Execution pipeline** | A- | Single-path for SELECT, but extended protocol inserts text substitution |
| **Type system** | B | Good IR, but coercion gaps and conservative fallbacks |
| **Optimizer** | B- | Structure excellent, cost model is placeholder |
| **Storage layer** | B+ | Clean keyspace isolation, correct encoding |
| **Protocol layer** | D+ | Text substitution, parallel type inference, error swallowing, god file |
| **Code organization** | D+ | God files, no module boundaries, 15 subsystems in one dir |
| **Error handling** | C- | Three error systems, SQLSTATE assigned at wrong layer |
| **Context propagation** | C+ | 11→~11 task-locals, 5→~3-level nesting after #847, fail-fast on missing context |
| **Testability** | B | Good unit tests in analyzer/optimizer, weak integration boundaries |
| **PG compatibility** | C | Core SQL works, extended protocol semantics differ, advanced features missing |
| **Maintainability** | C- | Hard to onboard, hard to modify safely, high coupling, hidden dual paths |

**Previous protocol grade was C+. Downgraded to D+ after discovering text substitution and parallel type inference.**

---

## 7. Top Recommendations (Priority Order — Revised)

### 1. Implement Real Parameterized Execution (NEW — Highest Priority)

Replace `substitute_parameters()` with a parameter-passing mechanism that threads typed parameters through the pipeline without text substitution. This is the prerequisite for:
- Plan caching (#707)
- Correct extended protocol semantics
- ORM compatibility (many ORMs assume parameterized execution)
- Performance (eliminate re-parse/re-analyze/re-optimize per Execute)

**Approach**: Pass `Vec<TypedParam>` alongside the parsed AST to the Analyzer, which resolves `$N` references to typed parameter nodes in the TypedExpr tree.

### 2. Replace Protocol Type Inference with Analyzer (NEW)

Delete `type_infer.rs` (956 lines) and the parameter inference in `dynamic.rs` (~287 lines). Instead, make `Describe` call the Analyzer to produce typed output column metadata. This ensures Describe and Execute always agree on types.

### 3. Extract God Files

`ddl.rs`, `planner.rs`, `dynamic.rs`, `typed_eval.rs` should each be broken into 3-5 focused modules. This is the single highest-leverage structural refactor.

**Suggested splits:**
- `ddl.rs` -> `ddl/table.rs`, `ddl/index.rs`, `ddl/view.rs`, `ddl/trigger.rs`, `ddl/sequence.rs`, `ddl/function.rs`, `ddl/system.rs`
- `planner.rs` -> `planner/predicate.rs`, `planner/index_selection.rs`, `planner/scan_type.rs`, `planner/typed_filter.rs`
- `dynamic.rs` -> `handler/startup.rs`, `handler/query.rs`, `handler/transaction.rs`, `handler/copy.rs`

### 4. Unify TypedExpr Visitor/Transform API

Infrastructure partially exists (`typed_visit.rs` for read-only visitor, `typed_rewrite.rs` for async rewrite, `classify.rs` for iterative traversal after #847). But these are three separate implementations, and key paths (especially `pre_materialize_async_exprs()`) still use manual recursion.

Unify into a canonical API:
```rust
impl TypedExpr {
    fn transform<F: FnMut(&TypedExpr) -> Option<TypedExpr>>(&self, f: &mut F) -> TypedExpr
}
```
Then migrate `pre_materialize_async_exprs()` and other manual walks to use it. This would eliminate thousands of lines of boilerplate.

### 5. Unify Error Handling

Pick `SqlError` with SQLSTATE codes, assigned at the source. Keep `anyhow` only for internal/infra errors. Kill string-matching for error classification in the handler.

### 6. Consolidate Context Propagation (NEW — Partially Addressed by #847)

PR #847 introduced `with_scoped_query_context()` and reduced nesting from 5 to ~3 levels. The remaining step: replace the remaining scattered task-locals with a single `ExecutionContext` struct passed via one task-local, making all dependencies explicit.

### 7. Add RBAC to PL/pgSQL Execution (NEW)

Propagate the calling user's identity through `execute_statement_on_txn()` instead of passing `None`. Implement SECURITY INVOKER/DEFINER semantics matching PostgreSQL.

### 8. Move Index Selection into the Optimizer

Move typed-filter index selection from `planner.rs` into `optimizer/physical_planner.rs`. Delete legacy AST-based predicate analysis functions.

### 9. Make GIN Fallback Explicit

Either implement GIN operator support or emit a WARNING/NOTICE when a GIN index is available but unused. Add EXPLAIN notation showing "GIN index available but not used."

### 10. Clean Dead Signals (NEW — Partially Resolved by #847)

~~Remove `db9.use_optimizer` GUC, task-local, and session field.~~ **Done by #847.** Remaining: remove or sunset legacy schema deserialization fallback. Legacy DDL name conflict scan already has sunset date (2026-12-31) added by #847.

---

## 8. Comparison: What My Initial Review Missed

| Finding | Initial Review | Competing Analysis | Verdict |
|---------|---------------|-------------------|---------|
| SQL string substitution | **MISSED** | Found as P0 | **Competing analysis was right — this is the #1 issue** |
| Protocol semantic duplication | Mentioned as god file | Found as P0 with specific line counts | Competing analysis was more precise |
| Task-local fragmentation | Not identified | Found as P1 | Verified: 11 vars, 5 levels |
| RBAC bypass | Not identified | Found as P1 | Verified: mitigated for external, real for UDF |
| Parser shims | Not identified specifically | Found as P1 (8 shims) | Verified: 8 remain after #847 removed RETURNING fallback |
| Dead GUC signals | Not identified | Found as P1 | Verified; **resolved by #847** (use_optimizer fully removed, #844 closed) |
| God files | Found as #1 priority | Found | We agreed |
| TypedExpr visitor | Found | Not mentioned | My finding |
| Error handling unification | Found | Not mentioned | My finding |
| GIN silent degradation | Found | Not mentioned | My finding |
| Savepoint non-atomicity | Found | Not mentioned | My finding |
| Regex compiled per row | Found | Not mentioned | My finding |
| PG compatibility gaps | 13 items identified | Not focused on | My finding |
| Optimizer cost model | Analyzed in detail | Not mentioned | My finding |

**Self-assessment**: My initial review was stronger on internals (type system, optimizer, storage, PG compatibility) but weaker on the protocol-executor boundary. The competing analysis correctly identified the most critical architectural issue (text substitution) that I completely missed because I under-examined the protocol handler's extended query flow.

The protocol layer is where the system meets the outside world, and that's where the most consequential architectural debt lives.
