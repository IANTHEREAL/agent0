# Work Log: Dify Compatibility Testing

## 2026-01-22: Dify Compatibility Testing Complete

### Goal
Test pg-tikv compatibility with Dify (https://github.com/langgenius/dify), a popular LLM application platform that uses PostgreSQL as its metadata database.

### Final Status: SUCCESS

Dify is now running successfully with pg-tikv as the database backend.

### Summary of Work

#### 1. Configuration Changes

**Dify `.env` modifications (`~/lab/dify/docker/.env`):**
```
DB_HOST=10.0.0.164          # Host machine IP (host.docker.internal doesn't work on Linux)
DB_PORT=5433
DB_USERNAME=admin
DB_PASSWORD=admin
DB_DATABASE=postgres
COMPOSE_PROFILES=weaviate   # Removed postgresql profile to skip built-in Postgres
EXPOSE_NGINX_PORT=8088      # Changed from 80 (port conflict)
EXPOSE_NGINX_SSL_PORT=8443  # Changed from 443 (port conflict)
```

**pg-tikv startup:**
```bash
PD_ENDPOINTS=127.0.0.1:45959 PG_PORT=5433 PG_HOST=0.0.0.0 ./target/release/pg-tikv
```

#### 2. Bug Found and Fixed

**Issue:** `COMMENT ON COLUMN` returns `EmptyQueryResponse` instead of `CommandComplete`

**Symptom:** Alembic migrations fail silently after executing `COMMENT ON COLUMN` statements. The psycopg2 driver throws "can't execute an empty query" error.

**Root Cause:** In `src/sql/executor.rs`, the `execute_comment_on_cmd()` function returned `ExecuteResult::Empty`, which mapped to `Response::EmptyQuery` in the wire protocol handler. PostgreSQL clients (psycopg2/libpq) treat `EmptyQueryResponse` as an error for utility statements.

**Fix:** Changed line ~1416 in `src/sql/executor.rs`:
```rust
// Before
Ok(ExecuteResult::Empty)

// After  
Ok(ExecuteResult::CommandComplete { tag: "COMMENT" })
```

**Verification:**
- All 410 unit tests pass
- Dify migrations complete successfully (123 tables created)
- User registration and workspace creation work correctly

#### 3. Test Results

| Test | Status |
|------|--------|
| Database connection from Docker | ✅ Pass |
| Alembic migrations (123 tables) | ✅ Pass |
| Plugin daemon initialization | ✅ Pass |
| Web interface loads | ✅ Pass |
| User registration via API | ✅ Pass |
| Workspace creation | ✅ Pass |
| Database queries (SELECT, INSERT, UPDATE) | ✅ Pass |
| Transactions | ✅ Pass |

#### 4. Remaining Known Issues

**`pg_get_userbyid` function not supported** (Low priority)
- Only affects `psql \dt` command
- Not used by Dify application code
- Workaround: Use `SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'`

### Files Created/Modified

| File | Description |
|------|-------------|
| `src/sql/executor.rs` | Fixed COMMENT ON return value |
| `DIFY_COMPATIBILITY.md` | Compatibility documentation |
| `WORK.md` | This work log |
| `scripts/dify_test.sh` | Test script for Dify deployment |

### How to Reproduce

```bash
# 1. Start TiKV cluster
cd ~/lab/pg-tikv
uv run scripts/tikv_admin.py start --name dify-test --persistent
# Note the PD port from output

# 2. Build and start pg-tikv
cargo build --release
PD_ENDPOINTS=127.0.0.1:<pd_port> PG_PORT=5433 PG_HOST=0.0.0.0 ./target/release/pg-tikv

# 3. Configure Dify
cd ~/lab/dify/docker
# Edit .env with DB_HOST=<your-ip>, DB_PORT=5433, etc.
# Set COMPOSE_PROFILES=weaviate to skip built-in PostgreSQL

# 4. Start Dify
docker compose up -d

# 5. Access Dify
open http://localhost:8088/install
```

### Lessons Learned

1. **Wire protocol matters**: Utility statements (BEGIN, COMMIT, SET, COMMENT, etc.) must return `CommandComplete` or appropriate response, never `EmptyQueryResponse`. Clients like psycopg2 treat empty responses as errors.

2. **Testing with real ORMs is essential**: The unit tests didn't catch this issue because they don't test the wire protocol responses. Testing with actual PostgreSQL drivers (psycopg2, pg, etc.) reveals wire-level compatibility issues.

3. **Docker networking on Linux**: `host.docker.internal` doesn't work by default on Linux Docker. Use the actual host IP address instead.

---

*Completed: 2026-01-22*

---

## 2026-01-22: Volcano Model Refactoring Project

### Goal
Refactor the SQL engine from tightly-coupled optimizer/executor to a clean Volcano (Iterator) model with clear operator abstractions.

### Current Status: ANALYSIS COMPLETE - READY FOR PHASE 1

---

## Current Architecture Analysis

### File Structure (src/sql/)
| File | Lines | Purpose | Pain Points |
|------|-------|---------|-------------|
| `executor.rs` | ~3500 | Main query execution, all statement handling | Monolithic, handles everything |
| `executor_select.rs` | ~1000 | SELECT execution with planning | Planning + execution mixed |
| `executor_join.rs` | ~1500 | JOIN query execution | Nested loop only, coupled |
| `expr.rs` | ~3700 | Expression evaluation | Dual context (single/join) |
| `planner.rs` | ~1100 | Cost-based index selection | Returns ScanType, not operator tree |
| `aggregate.rs` | ~463 | Aggregator state machine | Clean, reusable |
| `window.rs` | ~1025 | Window functions | Full materialization |
| `helpers.rs` | ~1100 | Utility functions | Scattered logic |

### Key Problems Identified

1. **Tight Coupling**: Planner returns `ScanType` enum that executor switches on. No operator abstraction.

2. **No Iterator Model**: Results fully materialized at each step:
   ```rust
   // Current pattern (executor_select.rs)
   let all_rows = self.scan_and_fill(txn, &t, &schema).await?;  // Full materialization
   let filtered_rows = all_rows.into_iter().filter(...).collect();  // Full materialization
   let sorted_rows = filtered_rows.sort_by(...);  // Full materialization
   ```

3. **Dual Expression Contexts**: Two separate evaluation paths:
   - `eval_expr(expr, row, schema)` - single table context
   - `eval_expr_join(expr, ctx)` - join context with JoinContext

4. **Large Functions**: `execute_query_with_ctes()` is 500+ lines handling:
   - Table resolution
   - Subquery resolution
   - Access path selection
   - Row filtering
   - Aggregation
   - Window functions
   - Projection
   - ORDER BY / LIMIT

5. **No Physical Plan**: Directly interprets SQL AST instead of building plan tree.

### Data Flow (Current)
```
SQL String 
  → sqlparser::parse() 
  → Statement AST
  → execute_statement_on_txn()
    → [DDL handlers / DML handlers / SELECT handler]
      → execute_query_with_ctes()
        → Table resolution
        → Filter resolution
        → Access path selection (planner.rs)
        → Full table/index scan → Vec<Row>
        → Filter in memory
        → Aggregate/Window/Sort in memory
        → Project
        → Return ExecuteResult::Select
```

---

## Volcano Model Design

### Target Architecture

```
SQL String
  → Parser (sqlparser-rs) → Statement AST
  → Logical Planner → LogicalPlan tree
  → Physical Planner → PhysicalPlan (operator tree)
  → Executor → calls operator.next() → Row stream
```

### Module Structure (New)
```
src/sql/
├── mod.rs
├── parser.rs                    # Unchanged
├── session.rs                   # Unchanged
├── result.rs                    # Unchanged
│
├── plan/                        # NEW: Plan representations
│   ├── mod.rs
│   ├── logical.rs              # LogicalPlan enum
│   └── physical.rs             # PhysicalPlan trait + impls
│
├── optimizer/                   # NEW: Query optimization
│   ├── mod.rs
│   ├── logical_optimizer.rs    # Logical rewrites (predicate pushdown)
│   └── physical_optimizer.rs   # Physical planning (access path selection)
│
├── operators/                   # NEW: Physical operators
│   ├── mod.rs
│   ├── scan.rs                 # TableScan, IndexScan
│   ├── filter.rs               # Filter operator
│   ├── project.rs              # Projection operator
│   ├── join.rs                 # NestedLoopJoin, HashJoin
│   ├── aggregate.rs            # HashAggregate, StreamingAggregate
│   ├── sort.rs                 # Sort, TopN
│   ├── limit.rs                # Limit, Offset
│   └── window.rs               # WindowAggregate
│
├── executor.rs                  # MODIFIED: Thin wrapper calling operators
├── expr.rs                      # MODIFIED: Unified expression evaluation
├── aggregate.rs                 # REUSE: Aggregator state machine
└── [legacy files]               # Gradually deprecated
```

### Core Trait Definitions

```rust
// src/sql/operators/mod.rs

use crate::types::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tikv_client::Transaction;

/// Execution context passed to operators
pub struct ExecutionContext<'a> {
    pub txn: &'a mut Transaction,
    pub search_path: &'a [String],
}

/// Physical operator trait (Volcano model)
#[async_trait]
pub trait PhysicalOperator: Send + Sync + std::fmt::Debug {
    /// Return the output schema of this operator
    fn schema(&self) -> &TableSchema;

    /// Open the operator (initialize state)
    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()>;

    /// Get the next row, None when exhausted
    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>>;

    /// Close the operator (cleanup resources)
    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()>;

    /// Child operators (for tree traversal)
    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![]
    }

    /// Estimated row count (for explain)
    fn estimated_rows(&self) -> Option<usize> {
        None
    }
}

/// Box type for operators
pub type BoxedOperator = Box<dyn PhysicalOperator>;
```

### Physical Operators (Essential)

| Operator | Priority | Description |
|----------|----------|-------------|
| `TableScan` | P0 | Full table scan from TiKV |
| `IndexScan` | P0 | Index-based scan |
| `Filter` | P0 | Row filtering with predicate |
| `Project` | P0 | Column projection |
| `NestedLoopJoin` | P0 | Basic join implementation |
| `HashAggregate` | P0 | GROUP BY with hash table |
| `Sort` | P0 | ORDER BY implementation |
| `Limit` | P0 | LIMIT/OFFSET |
| `HashJoin` | P1 | Hash join for equi-joins |
| `StreamingAggregate` | P1 | Sorted input aggregation |
| `WindowAggregate` | P1 | Window functions |
| `TopN` | P2 | Combined Sort + Limit |
| `MergeJoin` | P2 | Sorted merge join |

### Expression Evaluation (Unified)

```rust
// src/sql/expr.rs - unified evaluation

/// Unified row context for expression evaluation
pub struct RowContext<'a> {
    /// Column name to (index, value) mapping
    columns: HashMap<String, (usize, &'a Value)>,
    /// Optional table alias mappings for qualified names (t.col)
    table_aliases: HashMap<String, HashMap<String, usize>>,
}

impl<'a> RowContext<'a> {
    /// Create from single table row
    pub fn from_row(row: &'a Row, schema: &'a TableSchema) -> Self { ... }
    
    /// Create from joined rows
    pub fn from_join(rows: &[(&'a Row, &'a TableSchema, &str)]) -> Self { ... }
    
    /// Resolve column reference
    pub fn get(&self, name: &str) -> Option<&Value> { ... }
    
    /// Resolve qualified reference (table.column)
    pub fn get_qualified(&self, table: &str, column: &str) -> Option<&Value> { ... }
}

/// Unified expression evaluation
pub fn eval_expr(expr: &Expr, ctx: &RowContext) -> Result<Value> { ... }
```

---

## Phased Implementation Plan

### Phase 1: Foundation (Week 1-2)
**Goal**: Create operator infrastructure without breaking existing code.

**Tasks**:
1. Create `src/sql/operators/mod.rs` with `PhysicalOperator` trait
2. Create `src/sql/plan/mod.rs` with basic plan structures
3. Implement `TableScan` operator that wraps existing scan logic
4. Implement `Filter` operator
5. Implement `Project` operator
6. Add unit tests for operators

**Verification**: All existing tests pass (operators not integrated yet)

### Phase 2: Basic Query Path (Week 3-4)
**Goal**: Route simple SELECT queries through operator pipeline.

**Tasks**:
1. Create `PhysicalPlanner` that builds operator trees for simple SELECT
2. Modify `executor_select.rs` to optionally use operator path
3. Implement `Limit` operator
4. Implement `Sort` operator
5. Feature flag to switch between old/new path

**Verification**: Simple SELECT queries work through operator path

### Phase 3: Aggregation (Week 5-6)
**Goal**: Support GROUP BY through operators.

**Tasks**:
1. Implement `HashAggregate` operator (reuse existing `Aggregator`)
2. Handle HAVING clause in aggregate operator
3. Add aggregation to physical planner

**Verification**: GROUP BY queries work through operator path

### Phase 4: Joins (Week 7-8)
**Goal**: Support JOIN queries through operators.

**Tasks**:
1. Implement `NestedLoopJoin` operator
2. Unify expression evaluation (eliminate `eval_expr_join`)
3. Update planner for join queries
4. Implement `HashJoin` operator

**Verification**: JOIN queries work through operator path

### Phase 5: Window Functions & Subqueries (Week 9-10)
**Goal**: Complete query support.

**Tasks**:
1. Implement `WindowAggregate` operator
2. Handle subqueries in planner
3. Handle CTEs in planner

**Verification**: All SELECT variants work through operator path

### Phase 6: Optimization & Cleanup (Week 11-12)
**Goal**: Remove old code, optimize performance.

**Tasks**:
1. Remove feature flag, make operator path default
2. Delete deprecated executor code
3. Performance benchmarking
4. Documentation updates

**Verification**: `./run_tests.sh` passes, no performance regression

---

## Risk Mitigation

1. **Feature Flags**: New operator path hidden behind feature flag until stable
2. **Parallel Paths**: Both old and new paths coexist during migration
3. **Incremental Migration**: Each phase is independently testable
4. **Test Coverage**: Run `./run_tests.sh` after each significant change
5. **Rollback Plan**: Git branches for each phase, easy to revert

---

## Reference: DataFusion ExecutionPlan Pattern

DataFusion uses a similar async streaming pattern:

```rust
// DataFusion's approach (simplified)
pub trait ExecutionPlan: Send + Sync {
    fn schema(&self) -> SchemaRef;
    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>>;
    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream>;
}

// Returns a stream of record batches
type SendableRecordBatchStream = Pin<Box<dyn Stream<Item = Result<RecordBatch>> + Send>>;
```

Our approach will be simpler (row-at-a-time instead of batch) but can evolve to batch processing later.

---

## Progress

### Phase 1: Foundation (COMPLETED)

Created `src/sql/operators/` module with Volcano iterator model implementation:

| Operator | File | Description |
|----------|------|-------------|
| `PhysicalOperator` | `mod.rs` | Core trait with `open()`, `next()`, `close()` lifecycle |
| `ExecutionContext` | `context.rs` | Execution context with txn, store, search_path |
| `TableScanOperator` | `scan.rs` | Full table scan from TiKV |
| `IndexScanOperator` | `scan.rs` | Index-based row lookup |
| `FilterOperator` | `filter.rs` | Row filtering with predicates |
| `ProjectOperator` | `project.rs` | Column projection |
| `SortOperator` | `sort.rs` | ORDER BY implementation |
| `LimitOperator` | `limit.rs` | LIMIT/OFFSET handling |

### Phase 2: Physical Planner (COMPLETED)

Created `src/sql/operators/planner.rs`:

- `PhysicalPlanner` - Builds operator trees from SQL AST
  - Uses existing `planner.rs` for cost-based access path selection
  - Supports: FullTableScan, IndexScan, IndexRangeScan, GinIndexScan (fallback)
- `OperatorBuilder` - Fluent API for programmatic operator tree construction

Test coverage: 430 unit tests passing (6 new planner tests added)

### Phase 3: Integration Infrastructure (COMPLETED)

Created integration layer for using operators in the executor:

**New Files:**
- `src/sql/operators/executor.rs` - `execute_operator_tree()` function that runs an operator tree and collects rows
- `src/sql/executor_operators.rs` - Integration helpers for the Executor:
  - `is_simple_operator_query()` - Detects if a query can use the operator path
  - `execute_with_operators()` - Executes simple queries using operator pipeline

**Query Classification:**
Simple queries eligible for operator path:
- Single table (no JOINs)
- No aggregates (COUNT, SUM, etc.)
- No window functions
- No GROUP BY / HAVING
- No DISTINCT
- No CTEs

Test coverage: 440 unit tests passing (9 new tests for query classification)

### Phase 4: Wiring Up the Operator Path (COMPLETED)

Integrated the operator execution path into `executor_select.rs`:

**Changes:**
- Added `PGTIKV_USE_OPERATORS` environment variable flag (default: false)
- Modified `execute_query_with_ctes()` to check for simple queries and route to operator path
- Added `extract_limit()` and `extract_offset()` helper functions

**Activation:**
```bash
PGTIKV_USE_OPERATORS=true PD_ENDPOINTS=127.0.0.1:2379 ./target/release/pg-tikv
```

**Verification:**
- All 440 unit tests pass
- Integration tests pass with operator path enabled
- Simple SELECT queries (with WHERE, ORDER BY, LIMIT) execute correctly through operator pipeline

### Phase 5: Additional Operators (COMPLETED)

Implemented aggregate and join operators to support more complex queries:

**HashAggregateOperator** (`src/sql/operators/aggregate.rs`):
- Reuses existing `Aggregator` state machine from `src/sql/aggregate.rs`
- Supports all aggregate functions: COUNT, SUM, AVG, MIN, MAX, STRING_AGG, ARRAY_AGG
- Hash-based grouping for GROUP BY queries
- 3 unit tests added

**NestedLoopJoinOperator** (`src/sql/operators/join.rs`):
- Supports all join types: INNER, LEFT, RIGHT, FULL OUTER, CROSS
- Condition-based join evaluation
- Proper NULL handling for outer joins
- 5 unit tests added

**ProjectOperator Integration** (`src/sql/operators/planner.rs`):
- Added `project()` method to `OperatorBuilder` for column projection
- Enables building operator trees with explicit projection
- 1 unit test added

Test coverage: 448 unit tests passing

### Phase 6: Executor Integration (COMPLETED)

Wired aggregate and join operators into the executor:

**Aggregate Integration** (`src/sql/executor_operators.rs`):
- Added `is_aggregate_operator_query()` detection function
- Added `execute_aggregate_with_operators()` execution function
- Extracts GROUP BY expressions and aggregate functions from SELECT
- Routes aggregate queries through HashAggregateOperator pipeline
- 6 unit tests added

**Join Integration** (`src/sql/executor_operators.rs`):
- Added `is_simple_join_operator_query()` detection function
- Added `execute_join_with_operators()` execution function
- Added `TableScanOperator::new_with_rows()` for preloaded data
- Supports single JOIN with ON condition
- 5 unit tests added

Test coverage: 465 unit tests passing

## Next Steps

1. ✅ Complete architecture analysis
2. ✅ Implement core operators (TableScan, IndexScan, Filter, Project, Sort, Limit)
3. ✅ Add operator unit tests
4. ✅ Create PhysicalPlanner
5. ✅ Create integration infrastructure (executor.rs, executor_operators.rs)
6. ✅ Wire up operator path in executor_select.rs (behind feature flag)
7. ✅ Implement HashAggregate operator for GROUP BY
8. ✅ Implement NestedLoopJoin operator for JOINs
9. ✅ Wire aggregate operator into executor_select.rs
10. ✅ Add join operator infrastructure (detection + execution functions ready)
11. ✅ Debug and fix operator path regressions (now stable - no additional failures)
12. ⬜ Wire join operator into executor_join.rs (requires careful integration)
13. ⬜ Implement WindowAggregate operator for window functions
14. ⬜ Enable operator path by default after more testing

---

### Integration Testing Results (Final)

**Without `PGTIKV_USE_OPERATORS`**: 102 passed, 2 failed (pre-existing issues)
**With `PGTIKV_USE_OPERATORS=true`**: 102 passed, 2 failed (same as baseline - NO regressions)

The operator path is now stable and does not introduce any additional test failures.

### Bugs Fixed (2026-01-22)

1. **Lazy materialization in scan operator** - `fill_row_defaults` in `scan.rs` was not evaluating default expressions, causing rows added before `ALTER TABLE ADD COLUMN` to show NULL instead of the default value. Fixed by using `fill_row_defaults` from `helpers.rs`.

2. **SELECT INTO not handled** - Operator execution paths returned early without checking `select_into_target`. Fixed by skipping operator path for SELECT INTO queries.

3. **Mixed aggregate projections** - Aggregate operator path only extracted aggregate functions but not non-aggregate columns (e.g., string literals). Fixed by rejecting queries with non-aggregate, non-group-by columns in projection.

4. **Function calls in WHERE clause** - Simple operator path didn't detect custom function calls in WHERE clause (e.g., `WHERE val > add_numbers(10, 15)`). Fixed by adding `expr_has_function_call` helper.

5. **FILTER clause on aggregates** - Aggregate operator path didn't check for `f.filter` on Function expressions. Fixed by checking `f.filter.is_some()` and rejecting such queries.

*Started: 2026-01-22*
*Status: Phase 6 Complete - Operator Path Stable*

**Operator Inventory:**
| Operator | File | Description |
|----------|------|-------------|
| `PhysicalOperator` | `mod.rs` | Core trait with `open()`, `next()`, `close()` |
| `ExecutionContext` | `context.rs` | Execution context with txn, store, search_path |
| `TableScanOperator` | `scan.rs` | Full table scan from TiKV |
| `IndexScanOperator` | `scan.rs` | Index-based row lookup |
| `FilterOperator` | `filter.rs` | Row filtering with predicates |
| `ProjectOperator` | `project.rs` | Column projection and expression evaluation |
| `SortOperator` | `sort.rs` | ORDER BY implementation |
| `LimitOperator` | `limit.rs` | LIMIT/OFFSET handling |
| `HashAggregateOperator` | `aggregate.rs` | GROUP BY with hash table |
| `NestedLoopJoinOperator` | `join.rs` | JOIN implementation (all types) |
