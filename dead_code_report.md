# TiPG Dead Code Report (Verified)

**Date:** 2026-02-11 | **Branch:** `master @ a496caf` | **Codebase:** 221 files, ~97,678 lines

---

## Methodology

7-phase automated analysis followed by 4 targeted deep-dive investigations that
verified every `#[allow(dead_code)]` item against actual call sites. Multiple
items from the initial scan were found to be **mislabeled** — the annotation
exists but the code is actively called.

---

## 1. TRULY DEAD CODE — Safe to Remove

These items have **zero callers** in production or test code (or are fully
superseded). Removing them has no functional impact.

### 1.1 Superseded Functions

| File:Line | Item | Why Dead | Est. Lines |
|-----------|------|----------|-----------|
| `src/protocol/handler/dynamic.rs:1835` | `infer_result_fields()` | Superseded by `infer_result_fields_from_query()` which parses the query and infers real field types. This one returns a generic placeholder. | ~25 |
| `src/sql/triggers.rs:220` | `apply_before_triggers()` | Superseded by `apply_before_triggers_with_cache()` (called from dml.rs:262,375,958). This one loads functions on-the-fly from storage; the active version uses a pre-built cache. | ~35 |

### 1.2 Dead Storage/Accessor Methods

| File:Line | Item | Why Dead | Est. Lines |
|-----------|------|----------|-----------|
| `src/storage/tikv_store/tables.rs:377` | `get_by_pk()` | Direct PK lookup bypassing scan. Never called. Could be useful for point queries but no caller exists. | ~30 |
| `src/pool.rs:138` | `TikvClientPool::pd_endpoints()` | Config introspection accessor. No callers in production or tests. | ~5 |

### 1.3 Dead Query Context Field

| File:Line | Item | Why Dead | Est. Lines |
|-----------|------|----------|-----------|
| `src/sql/query_context.rs:17` | `QueryContext.timezone` field | Set in session code but **never read** in any production formatting/eval path. Only read in a test. | ~3 |

**Total truly dead code: ~98 lines**

---

## 2. UNINTEGRATED NEW FEATURES — Waiting to Be Wired In

Complete implementations that exist alongside the main path but whose **call
sites have not been connected yet**. These are planned features, not abandoned
code.

### 2.1 RBAC Enforcement Layer (auth/rbac.rs) — ~460 dead lines

**Integration status: 60% wired.** The data model, DDL (CREATE/ALTER/DROP ROLE),
GRANT/REVOKE metadata, authentication, and `pg_roles` virtual table all work.
What's missing is **runtime privilege checking** before query execution.

| File:Line | Item | What It Does | Wired? |
|-----------|------|-------------|--------|
| `src/auth/rbac.rs:35` | `Privilege::from_str()` | Parse privilege name from SQL | No — parsing done elsewhere |
| `src/auth/rbac.rs:58` | `Privilege::expand_all()` | Expand ALL PRIVILEGES | No — not needed until enforcement |
| `src/auth/rbac.rs:84` | `PrivilegeObject::table()` | Convenience constructor | Test-only |
| `src/auth/rbac.rs:92` | `PrivilegeObject::all_tables()` | Convenience constructor | Test-only |
| `src/auth/rbac.rs:97` | `PrivilegeObject::database()` | Convenience constructor | Test-only |
| `src/auth/rbac.rs:181` | `User::has_privilege()` | Core per-user privilege check | No — THE missing piece |
| `src/auth/rbac.rs:197` | `User::privilege_matches()` | Helper for has_privilege | No |
| `src/auth/rbac.rs:205` | `User::object_matches()` | Helper for has_privilege | No |
| `src/auth/rbac.rs:234` | `Role::new()` | Create Role struct | No — legacy API |
| `src/auth/rbac.rs:332` | `AuthManager::create_role()` | Create role in storage | No |
| `src/auth/rbac.rs:401` | `AuthManager::check_privilege()` | Pre-execution permission gate | No — never called |
| `src/auth/rbac.rs:440` | `AuthManager::list_users()` | List all users | Indirectly via pg_roles |
| `src/auth/rbac.rs:457` | `AuthManager::list_roles()` | List all roles | No |

**Impact:** Any authenticated user can currently execute any SQL command. The
enforcement layer (`check_privilege()`) exists but is not hooked into the
executor. Wiring requires adding calls before DML/DDL execution.

**Recommendation:** **Keep** — this is a planned security feature. Track as
explicit tech debt with a deadline.

### 2.2 Type Inference Convenience APIs (sql/types/) — ~50 lines of dead API surface

**Integration status: 90% wired.** The core module is **actively in production**
(62 call sites for `infer_expr_type()`, direct use of `TypeContext` and
`TypeInferrer` in protocol handler). Only convenience/advanced APIs remain
unintegrated.

| File:Line | Item | What It Does | Why Unwired |
|-----------|------|-------------|-------------|
| `src/sql/types/mod.rs:37` | `try_infer_expr_type()` | Error-returning variant (returns `Result` instead of defaulting to `Text`) | Callers prefer the infallible version |
| `src/sql/types/mod.rs:44` | `infer_expr_type_join()` | Multi-table inference shortcut | Callers use `TypeContext::join()` + `TypeInferrer::new()` directly |
| `src/sql/types/registry.rs:90` | `FunctionRegistry::get()` | Get full function signature | `resolve_return_type()` is used instead |
| `src/sql/types/context.rs:56` | `TypeContext::join()` | Multi-table context constructor | Code uses `TypeContext::empty()` + `add_table()` instead |
| `src/sql/types/context.rs:144` | `TypeContext::available_columns()` | List available columns for diagnostics | Not needed yet |
| `src/sql/types/infer.rs:16` | `TypeInferrer.cache` field | Caching layer for repeated inference | Not needed yet |
| `src/sql/types/infer.rs:25` | `TypeInferrer::with_cache()` | Enable caching | Not needed yet |
| `src/sql/types/coercion.rs:38` | `is_temporal()` | Temporal type classification | Not incorporated into core inference |
| `src/sql/types/coercion.rs:50` | `can_coerce()` | Coercion compatibility check | Not incorporated into core inference |
| `src/sql/types/registry.rs:13` | `ReturnType::NumericPromotion` | Enum variant | Structural completeness |

**Recommendation:** **Keep** — actively maintained module (last commit Feb 2026).
These are ready-to-use APIs awaiting gradual adoption. Not dead code.

### 2.3 Operator Framework Gaps — ~200 lines

**Integration status: 70% wired.** Join execution (`NestedLoopJoinOperator`),
scan limit pushdown, and operator execution are all production-active. Only
EXPLAIN introspection methods and CTE scan remain unconnected.

| File:Line | Item | What It Does | Status |
|-----------|------|-------------|--------|
| `src/sql/operators/mod.rs:117` | `PhysicalOperator::children()` | Tree traversal | Test-only — EXPLAIN uses separate impl |
| `src/sql/operators/mod.rs:122` | `PhysicalOperator::children_mut()` | Tree rewriting | Test-only |
| `src/sql/operators/mod.rs:132` | `PhysicalOperator::name()` | Operator name for EXPLAIN | Test-only (20+ test uses) |
| `src/sql/operators/mod.rs:135` | `PhysicalOperator::explain_info()` | EXPLAIN detail | Test-only (20+ test uses) |
| `src/sql/operators/cte.rs:8` | `CTEScanOperator` | CTE scan operator | Test-only, not in executor |
| `src/sql/operators/planner.rs:132` | `PhysicalPlanner.store` field | For future index-aware cost estimation | Accessor dead |
| `src/sql/operators/planner.rs:301` | `PhysicalPlanner::store()` | Accessor | Dead |
| `src/sql/operators/planner.rs:306` | `PhysicalPlanner::search_path()` | Accessor | Dead |
| `src/sql/operators/project.rs:227` | `ProjectOperator.output_names` | Column names for EXPLAIN | Field dead |
| `src/sql/operators/hash_join.rs:300` | `HashTable::row_key_equals_values()` | Probe-by-value optimization | Not implemented |
| `src/sql/operators/hash_join.rs:344` | `HashTable::all_rows_with_indices()` | FULL OUTER JOIN support | Not implemented |
| `src/sql/operators/executor.rs:75` | `execute_operator_tree_with_query_ctx()` | Public API for explicit QueryContext | No callers |

**Note:** EXPLAIN is implemented independently in `src/sql/explain.rs` (670
lines) by parsing the SQL AST directly — it does NOT use the operator trait
methods (`name()`, `explain_info()`). These trait methods are infrastructure
for a potential future refactoring of EXPLAIN to use the executed operator tree.

**Recommendation:** **Keep** operator trait methods (well-tested infrastructure).
Consider removing `CTEScanOperator` if CTE pushdown is not planned.

### 2.4 Session/Pool Accessors — Various

| File:Line | Item | Status | Recommendation |
|-----------|------|--------|----------------|
| `src/pool.rs:45` | `TenantEntry::store()` | Test-only | **Keep** — test infrastructure |
| `src/pool.rs:50` | `TenantEntry::active_connections()` | Test-only (8 test call sites) | **Keep** — test infrastructure |
| `src/pool.rs:69` | `TenantHandle::keyspace()` | Dead | **Keep** — will be needed for routing |
| `src/pool.rs:277` | `TikvClientPool::tenant_count()` | Test-only (8 test call sites) | **Keep** — test infrastructure |
| `src/pool.rs:283` | `TikvClientPool::active_tenant_count()` | Test-only | **Keep** — test infrastructure |
| `src/pool.rs:294` | `TikvClientPool::connections_for()` | Test-only | **Keep** — test infrastructure |
| `src/sql/session.rs:366` | `Session::store()` | Dead | **Keep** — background task accessor |
| `src/sql/session.rs:384` | `Session::set_user()` | Dead | **Wire-in** when ALTER USER is implemented |
| `src/sql/session.rs:410` | `Session::current_database()` | Dead | **Wire-in** for `current_database()` SQL function |

### 2.5 Other Unintegrated Features

| File:Line | Item | Status | Recommendation |
|-----------|------|--------|----------------|
| `src/sql/planner.rs:263` | `ScanType::GinIndexScan` variant | Dead | **Keep** — planned GIN scan |
| `src/sql/error.rs:109` | `SqlError::severity()` | Dead | **Wire-in** for pgwire ErrorResponse |
| `src/sql/result.rs:117` | `ExecuteResult::Describe` variant | Dead | **Wire-in** for schema introspection |

---

## 3. ABANDONED CODE — Candidates for Removal

Items that were built speculatively and have been superseded or are unlikely to
be wired in.

| File:Line | Item | Why Abandoned | Est. Lines |
|-----------|------|--------------|-----------|
| `src/sql/operators/planner.rs:317` | `OperatorBuilder` struct + impl | Fluent builder API for operator trees. Only used in 1 test (`test_operator_builder_creates_valid_tree`). The actual planner builds trees directly. | ~75 |
| `src/sql/gin.rs:394` | `extract_tsquery_gin_tokens()` | tsquery GIN token extraction. Only called in 8 unit tests, never in production. The tsquery→GIN path was never connected to the query executor. | ~260 |

**Total abandoned code: ~335 lines**

---

## 4. MISLEADING `#[allow(dead_code)]` ANNOTATIONS — Actually Used

These items are annotated as dead but are **actively called in production or
tests**. The annotations should be **removed**.

| File:Line | Item | Actually Called By |
|-----------|------|------------------|
| `src/sql/gin.rs:30` | `GinTokens::iter_hashes()` | `gin.rs:465-466` in `fn_scan_index_hashes()` — **production** |
| `src/sql/gin.rs:42` | `GinTokens::into_scan_hashes()` | `gin.rs:475` in `fn_scan_index_hashes()` — **production** |
| `src/extensions/fs/backend.rs:40` | `FsBackend::exists()` | Implemented at `backend.rs:188`, tested at `backend.rs:419-432` |
| `src/storage/tikv_store/views.rs:180` | `list_materialized_views()` | `schemas.rs:346` in `drop_schema()` — **production** |
| `src/storage/tikv_store/procedures.rs:49` | `replace_procedure()` | `executor/procedure.rs:1057` — **production** |
| `src/sql/executor/core/mod.rs:108` | `Executor::auth_manager()` | `default_privileges.rs:48,53,62,63,106,107` — **production** (6 sites) |
| `src/sql/catalog/mod.rs:58` | `VirtualTable::schema_name()` | 41 implementations across catalog files, tested |
| `src/protocol/handler/params/scan.rs:252` | `find_keyword_outside_strings()` | Re-exported, called in `parser.rs:1445` — **production** |
| `src/sql/session.rs:375` | `Session::session_user()` | `executor/core/dispatch.rs:540-541` — **production** |
| `src/sql/operators/join.rs:44` | `NestedLoopJoinOperator` impl | `executor/operators.rs:2434`, `operator_tree/mod.rs:1589` — **production** |
| `src/sql/operators/scan.rs:160` | `IndexEqScanOperator::new_with_scan_limit()` | `planner.rs:222-228,245-251` — **production** |

**Action:** Remove these 11 `#[allow(dead_code)]` annotations. They mask the
compiler's ability to detect *real* dead code in the future.

---

## 5. CLIPPY DEAD-CODE-ADJACENT FINDINGS

### 5.1 `unnecessary_wraps` — 73 functions (return `Result` but never `Err`)

Concentrated in SQL function implementations:

| File | Count | Example Functions |
|------|-------|-------------------|
| `src/sql/expr/functions/pg_compat.rs` | 13 | `version()`, `current_schema()`, `pg_backend_pid()` |
| `src/sql/expr/functions/array.rs` | 10 | `array_length()`, `array_dims()`, `array_upper()` |
| `src/sql/expr/functions/string.rs` | 9 | `upper()`, `lower()`, `length()` |
| `src/sql/expr/functions/math.rs` | 5 | `abs()`, `pi()`, `random()` |
| `src/sql/expr/functions/misc.rs` | 4 | `coalesce()`, `nullif()` |
| `src/sql/expr/functions/json.rs` | 3 | `jsonb_array_length()`, `jsonb_typeof()` |
| `src/sql/executor/operators.rs` | 8 | Various operator helpers |
| Other files | 21 | Scattered |

**Recommendation:** Low priority. These wrap `Ok(...)` unnecessarily but follow a
consistent pattern (all function implementations return `Result` for uniformity).
Fixing would improve type honesty but is a large, low-risk refactor.

### 5.2 `unused_self` — 19 methods that don't use `self`

| File | Count | Notes |
|------|-------|-------|
| `src/sql/operators/window.rs` | 10 | Window operator designed with `&self` for future state |
| `src/auth/rbac.rs` | 2 | AuthManager methods |
| `src/sql/types/infer.rs` | 1 | TypeInferrer method |
| Other files | 6 | Scattered |

**Recommendation:** Low priority. The window.rs cluster is intentional design
(methods will use self when window state is added).

### 5.3 `redundant_clone` — 11 unnecessary `.clone()` calls

| File:Line | Notes |
|-----------|-------|
| `src/sql/operators/planner.rs:203,223,246,263,278,283` | 6 clones in planner |
| `src/sql/dml.rs:1463` | 1 clone in DML |
| `src/sql/executor/table_utils.rs:680,820` | 2 clones in table utils |
| `src/sql/expr/evaluator.rs:453` | 1 clone in evaluator |
| `src/sql/pg_numeric.rs:26` | 1 clone in numeric |

**Recommendation:** Quick fix. Each `.clone()` is on a value dropped immediately
after.

---

## 6. `pub` VISIBILITY OVER-EXPOSURE

| Metric | Count |
|--------|-------|
| `pub fn/struct/enum` (over-exposed) | **618** |
| `pub(crate)` (correct) | 227 |
| Over-exposure rate | **73%** |

Top 5 files: `storage/encoding.rs` (57), `types/mod.rs` (38),
`expr/functions/string.rs` (29), `expr/functions/math.rs` (21),
`auth/rbac.rs` (20).

**Recommendation:** Tightening `pub` → `pub(crate)` is the **highest-leverage
action** for ongoing dead code detection. The compiler cannot flag dead `pub`
items in non-root modules of a binary crate.

---

## 7. UNREACHABLE CODE PATTERNS

### Concerning (2 items)

| File:Line | Issue |
|-----------|-------|
| `src/sql/executor/operators.rs:2923` | `unreachable!()` relies on implicit invariant between `is_distinct_on` bool and `distinct` Option. If they go out of sync → runtime panic. Replace with explicit error. |
| `src/extensions/http.rs:377` | `_ => {}` in redirect status match. Any status code with a `Location` header will be followed, potentially causing infinite loop on non-redirect responses. |

### Legitimate (5 items)

`ddl.rs:367`, `table_utils.rs:671,1190,1195`, `dispatch.rs:812` — all properly
guarded.

---

## Summary of Recommended Actions

### Immediate (no feature impact)

| Action | Lines | Effort |
|--------|-------|--------|
| Remove `infer_result_fields()` | 25 | 5 min |
| Remove `apply_before_triggers()` | 35 | 5 min |
| Remove `get_by_pk()` | 30 | 5 min |
| Remove `pd_endpoints()` | 5 | 2 min |
| Remove `OperatorBuilder` struct + impl | 75 | 10 min |
| Remove `extract_tsquery_gin_tokens()` + its 8 tests | 260 | 15 min |
| Remove `QueryContext.timezone` field | 3 | 2 min |
| Remove 11 misleading `#[allow(dead_code)]` annotations | 0 | 15 min |
| Remove 11 redundant `.clone()` calls | 0 | 15 min |
| **Total** | **~433** | **~1 hour** |

### Track as Tech Debt (needs roadmap decision)

| Subsystem | Dead Lines | Decision Needed |
|-----------|-----------|-----------------|
| RBAC enforcement | ~460 | When to wire `check_privilege()` into executor? |
| Operator EXPLAIN infra | ~200 | Refactor EXPLAIN to use operator tree, or keep AST-based? |
| CTEScanOperator | 126 | CTE pushdown planned? |
| Type inference convenience APIs | ~50 | Adopt gradually or remove? |

### High-Leverage Structural Improvement

| Action | Impact |
|--------|--------|
| Tighten 618 `pub` → `pub(crate)` items | Enables compiler to find significantly more dead code |
| Enable `#![warn(dead_code)]` permanently in `main.rs` | CI catches future dead code automatically |
