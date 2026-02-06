# pg-tikv Codebase Architecture Review

**Date:** 2026-02-05
**Commit:** `3e54788f66d1b81b618511e830e8e4c39adfea4b` (`chore: add .worktrees/ to .gitignore`)
**Perspective:** Senior database kernel engineer & architect
**Scope:** Full codebase — architecture, code quality, module organization, production readiness

---

## Overall Grade: B+ (83/100)

The architectural skeleton is A+ level (zero layering violations, correct transaction semantics, sound key encoding). Technical debt accumulated during rapid iteration — god files, full result materialization, dual execution paths — is the primary concern.

| Category | Grade | Notes |
|----------|-------|-------|
| Layering discipline | A+ | Zero violations verified via code analysis |
| Type system | A | 20+ PG types, type-safe enums, serde integration |
| Transaction semantics | A+ | Correct SAVEPOINT, Failed state, conflict retry |
| Testing | A- | 803 unit tests, 100+ integration SQL, ORM suite; no property tests |
| Error handling | B+ | Pragmatic anyhow usage, but no structured error codes |
| Code organization | C | Three 4000-7400 line god files |
| Concurrency safety | A | No data races, proper Arc/RwLock usage |
| Memory management | D | Full materialization, no streaming, no backpressure |
| Query optimizer | C+ | Real cost-based foundation, but naive model and no Logical Plan IR |
| Security | B | Good tenant isolation, missing query timeout and SCRAM auth |

---

## 1. Architecture: Clean Three-Tier Layering

```
PostgreSQL Client
    | pgwire
Protocol Handler (handler.rs)
    | SQL String
Parser (sqlparser-rs)
    | AST
Executor (executor/, expr/, operators/)
    | KV Operations
Storage (tikv_store.rs + encoding.rs)
    | gRPC
TiKV
```

**Zero layering violations.** Storage never imports SQL or protocol code. SQL never imports protocol code. No circular dependencies. This is the project's strongest asset and is unusual for a fast-moving codebase.

---

## 2. Strengths

### 2.1 Transaction Model

- Exclusively pessimistic transactions for ACID guarantees
- Correct state machine: `Idle -> Active -> Failed -> Idle`
- Failed state prevents silent data corruption (PostgreSQL-compliant)
- SAVEPOINT with undo log, nested savepoints supported
- Auto-commit retry with exponential backoff (up to 10 attempts) on TiKV write conflicts
- `session.rs` (616 lines) is the best-designed module in the codebase

### 2.2 Key Encoding

- `memcomparable` crate for order-preserving binary encoding
- NULL: explicit `0x00` tag, sorts before all non-NULL values
- NUMERIC: 32-byte canonical encoding prevents `1.0 != 1.00` index lookup failures, negative values bitwise-inverted for correct sort
- v2 format with database-scoped prefixes (`d_{db_id:8bytes}_`) enables efficient tenant isolation
- 32 encoding round-trip tests verify correctness
- GIN inverted index keys with proper separator bytes for prefix scans

### 2.3 Multi-Tenancy

- TiKV native keyspace API for true physical isolation per tenant
- Per-keyspace connection pooling with lazy initialization
- Schema cache per-database with 60s TTL + DDL invalidation
- No cross-tenant data leakage possible by design

### 2.4 Testing

- 803 unit tests across 87 modules
- 100+ integration SQL test files executed against live server
- ORM compatibility suite: TypeORM, Prisma, Sequelize, Knex, Drizzle, Kysely, pg
- Expression evaluation: ~100 tests (excellent)
- Encoding: 32 round-trip tests (excellent)
- Protocol: 78 tests (good)

### 2.5 Observability

- Rolling 1-hour window with 60-second buckets, per-tenant isolation
- Latency histogram with log-scale bins
- Query sampling (0.1% default + all slow queries >200ms)
- FNV1a-64 fingerprinting for query grouping
- `RollingWindow` boxed (130KB) to avoid stack overflow

---

## 3. Critical Issues

### 3.1 God File Crisis

| File | Lines | Contents |
|------|-------|----------|
| `protocol/handler.rs` | 7,400 | Auth + Simple Query + Prepared Stmt + COPY + session |
| `sql/executor/join.rs` | 5,810 | Nested Loop + Hash Join + Lateral all in one |
| `sql/expr/mod.rs` | 4,036 | All expression eval + all functions + operators |
| `sql/executor/core.rs` | 3,329 | Main dispatch, `execute()` is 712 lines with 8 levels nesting |
| `sql/executor/select.rs` | 3,210 | Two execution paths intermixed |
| `storage/tikv_store.rs` | 3,831 | Storage abstraction |
| `sql/helpers.rs` | 3,044 | Utility dumping ground |

The `execute()` function in `core.rs` is a textbook god-function: 712 lines, 8 levels of nesting, 21+ string-based pre-parse dispatch branches before even calling the SQL parser.

### 3.2 Full Result Materialization (No Streaming)

The data flow is:

```
TiKV scan -> Vec<Row> fully loaded -> encode to Vec<DataRow> -> stream::iter() fake streaming
```

`handler.rs:5452-5465` uses `stream::iter(data_rows)` — but the data is already fully in memory. Implications:

- `SELECT * FROM big_table` (millions of rows) causes OOM
- `COPY TO STDOUT` also fully materializes (`handler.rs:2924`)
- Portal suspension (prepared statement max_rows) materializes first, then pages
- Window functions materialize entire partitions
- CTEs always materialized, even when referenced only once
- No backpressure mechanism exists

This is the single most impactful limitation for production use.

### 3.3 Dual Execution Model (Incomplete Migration)

A `use_operator_execution()` branch in `select.rs` splits queries into two paths:

- **Legacy path (~90%):** AST direct tree-walk execution, 3000+ lines scattered across select.rs
- **New path (~10%):** Volcano iterator model via `PhysicalOperator` trait, clean but only supports simple single-table SELECT

Every new feature potentially requires implementation in both paths. The new `operators/` module is architecturally sound but its coverage is too narrow to replace the legacy path.

### 3.4 No Logical Plan IR

Current flow: `AST -> Direct Execution`. No normalized intermediate representation.

All mature databases use: `SQL -> AST -> Logical Plan -> Optimize -> Physical Plan -> Execute`

Without a Logical Plan, these optimizations cannot be cleanly implemented:
- Predicate pushdown
- Subquery decorrelation (correlated subqueries currently re-execute per outer row)
- Join reordering (currently naive: sort by row count)
- CTE inlining
- Plan caching

### 3.5 Production Safety Gaps

| Issue | Location | Risk |
|-------|----------|------|
| No graceful shutdown | `main.rs:182-196` bare loop | Active transactions lost on termination |
| No connection limit | `pool.rs` unbounded | Connection storms exhaust resources |
| No query timeout enforcement | Executor layer | Slow queries run indefinitely |
| Cleartext password auth | `handler.rs:3083` | Passwords exposed without TLS |
| Unbounded TiKV scan | `tikv_store.rs:23` `SCAN_LIMIT = u32::MAX` | Single scan can pull entire keyspace |

---

## 4. Query Optimizer Assessment

`planner.rs` (1,317 lines) implements real cost-based optimization, not just heuristics.

### Cost Model

```rust
let index_lookup_cost = 1.0;
let row_fetch_cost = estimated_rows as f64 * 0.5;
let cost = index_lookup_cost + row_fetch_cost;
```

Fixed constants, no I/O or CPU modeling, no cache hit rate. Selectivity: `0.1^matched_columns` or `1/1,000,000` for unique indexes. No statistics collection (no ANALYZE, no histograms, no NDV).

### Capability Matrix

| Capability | Status | Notes |
|-----------|--------|-------|
| Single-table index selection | Yes | Cost-based, B-Tree and GIN |
| Join algorithm selection | Yes | Hash Join vs Nested Loop, coarse |
| Join reordering | No | Sorts by row count, ignores predicates/indexes |
| Predicate pushdown | No | No Logical Plan to push through |
| Subquery decorrelation | No | Correlated subqueries re-execute per row |
| CTE inlining | No | Always materialized |
| Plan cache | No | Re-parses on every execution |
| EXPLAIN ANALYZE | Partial | New operator path only |

---

## 5. Improvement Priority & Roadmap

### P0 — Immediate (Production Stability)

| Item | Effort | Impact |
|------|--------|--------|
| **Streaming result sets** — change executor return from `Vec<Row>` to `Stream<Item=Row>` | 1-2 weeks | Eliminates large-query OOM, unlocks production workloads |
| **Graceful shutdown** — signal handling + connection draining | 1 day | Prevents transaction loss on termination |
| **Connection limit + query timeout** | 1-2 days | Prevents DoS and resource exhaustion |

Streaming is the highest-ROI change: it transforms the system from "can run demos" to "can handle production load."

### P1 — Near-Term Refactoring (Developer Productivity)

| Item | Effort | Impact |
|------|--------|--------|
| **Split handler.rs** into startup/simple/extended/copy/session | 2-3 days | Reduce cognitive load, fewer merge conflicts |
| **Split join.rs** into nested_loop/hash/merge | 1-2 days | Independent testing and optimization per join strategy |
| **Unified error type** — `ExecutorError` enum replacing `anyhow` | 2-3 days | Structured error codes, better client experience |
| **Extract validation.rs** from duplicated code | 1 day | Eliminate ~500 lines of repetition |

### P2 — Architecture Upgrade (Feature Ceiling)

| Item | Effort | Impact |
|------|--------|--------|
| **Logical Plan IR** | 2-3 weeks | Unlocks predicate pushdown, subquery decorrelation, join reorder |
| **Complete operator migration** (JOIN, subquery, CTE) | 3-4 weeks | Eliminate dual execution paths, unify code |
| **Statistics collection (ANALYZE)** | 1-2 weeks | Give cost-based optimizer real data |
| **Plan cache** | 1 week | Reduce repeated parsing for prepared statements |

### P3 — Long-Term Quality

| Item | Effort | Impact |
|------|--------|--------|
| Property-based tests (proptest) | 3 days | Catch encoding/expression edge cases |
| Criterion benchmark suite | 2 days | Prevent performance regressions |
| SCRAM-SHA-256 authentication | 2-3 days | Replace cleartext passwords |
| Schema evolution (protobuf replacing bincode) | 1-2 weeks | Support online upgrades |

---

## 6. File Size Report

All `.rs` files over 1000 lines:

| File | Lines | Status |
|------|-------|--------|
| `src/protocol/handler.rs` | 7,400 | Needs split into 5 files |
| `src/sql/executor/join.rs` | 5,810 | Needs split into 3 files |
| `src/sql/expr/mod.rs` | 4,036 | Partially split, needs completion |
| `src/storage/tikv_store.rs` | 3,831 | Large but focused |
| `src/sql/executor/core.rs` | 3,329 | Needs dispatch refactoring |
| `src/sql/executor/select.rs` | 3,210 | Two execution models mixed |
| `src/sql/helpers.rs` | 3,044 | Utility dumping ground |
| `src/sql/executor/ddl.rs` | 2,391 | Large but focused |
| `src/sql/executor/dml.rs` | 1,768 | Acceptable |
| `src/sql/parser.rs` | 1,701 | Acceptable |
| `src/storage/encoding.rs` | 1,658 | Acceptable |
| `src/sql/window.rs` | 1,592 | Focused on one feature |
| `src/sql/planner.rs` | 1,317 | Acceptable |
| `src/sql/expr/evaluator.rs` | 1,161 | Well-organized |

85% of files are under 500 lines. The problem is concentrated in 3-4 god files.

---

## 7. Recommended Reading Order

To understand pg-tikv in ~2.5 hours:

1. `CLAUDE.md` — project overview (5 min)
2. `src/main.rs` — entry point (10 min)
3. `src/types/mod.rs` — type system (20 min)
4. `src/storage/encoding.rs` — key layout (30 min, **critical**)
5. `src/sql/session.rs` — transaction state machine (15 min)
6. `src/sql/parser.rs` — SQL preprocessing (15 min)
7. `src/sql/executor/core.rs` — main dispatch, focus lines 620-1332 and 1703-2100 (20 min)
8. `src/sql/operators/mod.rs` — Volcano model (10 min)
9. `src/sql/executor/ddl.rs` — schema operations (10 min)
10. `src/sql/executor/dml.rs` — data operations (10 min)
11. `src/txn/mod.rs` — savepoint implementation (5 min)

---

## 8. One-Line Summary

The architecture skeleton is A+ grade; the most urgent improvement is streaming result sets — this single change transforms the system from demo-capable to production-capable.
