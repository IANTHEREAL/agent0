# SQL Engine Architecture

> Contracts live in [docs/sot/sql-engine.md](../sot/sql-engine.md). This page explains how the current SQL engine is structured.

## Canonical Query Pipeline

The current analyzed `SELECT/WITH` pipeline is:

```
query AST
  → expand_views_in_query()
  → build_catalog_snapshot()
  → require_table_privilege() for base tables
  → Analyzer
  → rewrite_query()
  → LogicalPlanner
  → PhysicalPlanner
  → build operators
  → execute operators
  → postprocess results
```

This same semantic entrypoint is used by `EXPLAIN SELECT/WITH`.

## DML Path

INSERT, UPDATE, and DELETE use the analyzed DML path:

```
INSERT / UPDATE / DELETE
  → Analyzer::analyze_{insert,update,delete}()
  → typed expressions and coercion checks
  → executor/dml_analyzed/
  → index maintenance
  → trigger handling
  → optional follow-up enqueue (for example HNSW merge)
```

The DML entrypoint is the `src/sql/executor/dml_analyzed/` directory, not the old flat `executor/dml_analyzed.rs` file.

## Prepared Execution and Reparse Boundaries

Prepared execution reuses prepared/analyzed state where valid, but db9 still has explicit compatibility reparse boundaries:

- schema drift invalidates prepared state and reparses SQL text;
- prepared recursive CTE execution falls back to text execution (tracked in `#1516`);
- parser-boundary raw-SQL utility acceptance exists in the protocol layer for some utility statements that `sqlparser` cannot parse.

These are explicit compatibility shims, not hidden alternative planners for analyzed execution.

## Optimizer

db9 uses an always-on optimizer pipeline:

```
AnalyzedQuery
  → LogicalPlanner
  → logical rewrites / decorrelation / pushdown
  → PhysicalPlanner
  → operator builder
```

The optimizer currently handles:
- single-table and multi-table queries
- set operations
- CTEs
- window functions
- DISTINCT ON
- planner-driven index access paths

`db9.use_optimizer` remains a compatibility/readback GUC, not a runtime path switch.

## Physical Operators

Representative operator families:

| Operator | Purpose |
|---|---|
| `TableScan` / `IndexScan` / `RangeIndexScan` / `InListScan` | Base row and B-tree access paths |
| `GinScan` | Inverted-index posting-list execution with recheck |
| `HnswScan` | Approximate nearest-neighbor scan over base graph plus visible deltas |
| `Filter` | Predicate evaluation |
| `Project` | Expression evaluation and projection |
| `NestedLoopJoin` / `HashJoin` / `HashSemiJoin` | Join execution |
| `HashAggregate` | GROUP BY / aggregates |
| `Sort` / `Limit` / `Distinct` | Ordering, limiting, dedup |
| `Window` | Window functions |
| `SetOperation` | UNION / INTERSECT / EXCEPT |
| `CTE` | CTE iteration/materialization |
| `TableFunction` | `generate_series`, `unnest`, and other table functions |

## Access Paths

### B-tree

Planner access-path selection supports:
- equality lookups
- ranges and bounded ranges
- prefix matching
- partial indexes
- expression indexes
- in-list scans

### GIN

GIN access-path selection is shipped. The planner can emit `ScanType::GinIndexScan`, and runtime builds `GinScanOperator`.

Authoritative behavioral contract: [docs/sot/extensions-gin.md](../sot/extensions-gin.md)

### HNSW

The optimizer can choose an HNSW scan for eligible nearest-neighbor orderings. Runtime loads the base HNSW graph plus visible delta entries so read-your-writes semantics hold.

Current architecture is delta-log plus background merge, not the earlier process-level graph-cache design.

## CTE Materialization

CTE processing remains explicit:

```
WITH processing
  → non-recursive CTE materialization, or
  → recursive CTE iteration with bounded loop and dedup rules
  → materialized CTE map used by downstream execution
```

Recursive prepared CTE execution is one of the explicit documented reparse/fallback boundaries and is tracked separately in `#1516`.

## EXPLAIN

`EXPLAIN SELECT/WITH` goes through the same analyze/rewrite/planning path as execution so access-path reporting stays aligned with runtime semantics.

## Session-Local Plan Cache

db9 ships a session-local prepared plan cache for eligible prepared statements:

- promotion is execution-count based;
- cache keys include normalized SQL, parameter types, database, search path, and resolved table IDs;
- invalidation uses schema-version dependencies.

The broader roadmap item is shared/parameterized reuse beyond this current shipped session-local cache.
