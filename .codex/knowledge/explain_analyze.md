# EXPLAIN / EXPLAIN ANALYZE: facts + code locations

## Design doc
- `docs/design/15_explain_analyze.md`
  - MVP: `EXPLAIN (ANALYZE)` for SELECT/WITH should include plan tree + total execution time + actual row count (at least top-level).
  - MVP non-goals: DML analyze; per-node timing/rows.

## Current EXPLAIN implementation (before ANALYZE wiring)
- `src/sql/executor.rs`
  - `Executor::execute_statement_on_txn(...)` handles `Statement::Explain { statement, analyze, verbose, .. }`
    - Calls `self.execute_explain(txn, statement, *analyze, *verbose).await`
  - `Executor::execute_explain(txn, statement, _analyze, _verbose) -> ExecuteResult::Select`
    - Loads all table schemas via `store.list_tables(txn)` + `store.get_schema(txn, table_name)`
    - Builds `schema_lookup: Fn(&str) -> Option<TableSchema>`
    - Uses fixed `row_count_lookup` returning `1000` for all tables
    - Generates plan text via:
      - `explain::generate_plan(statement, schema_lookup, row_count_lookup)`
      - `explain::format_plan_text(&plan, 0)`
    - Returns a single text column `QUERY PLAN`, one row per line of the formatted plan.

## Plan generation + formatting
- `src/sql/explain.rs`
  - `PlanNode` variants: `SeqScan`, `IndexScan`, `NestedLoop`, `Sort`, `Limit`, `Aggregate`, `Result`
  - `generate_plan(stmt, schema_lookup, row_count_lookup) -> PlanNode`
    - Only `Statement::Query(_)` produces a real plan; other statements return `PlanNode::Result`.
  - `format_plan_text(plan, indent) -> String`
    - Produces PostgreSQL-like output lines with `(cost=.. rows=.. width=..)`.

## EXPLAIN ANALYZE implementation (SELECT/WITH only)
- `src/sql/executor.rs`
  - `Executor::execute_statement_on_txn(...)`:
    - `Statement::Explain { .. }` now calls:
      - `self.execute_explain(txn, sequence_values, search_path, statement, analyze, verbose).await`
  - `Executor::execute_explain(txn, sequence_values, search_path, statement, analyze, _verbose)`
    - If `analyze=true`:
      - Requires `statement` be `Statement::Query(query)`; otherwise returns error:
        - `EXPLAIN (ANALYZE) is only supported for SELECT/WITH statements`
      - Executes the query once via `self.execute_query(...)` and measures total runtime with `std::time::Instant`.
      - Computes `actual_rows` from `ExecuteResult::Select { rows, .. } => rows.len()`.
    - Appends summary lines to the formatted plan output:
      - `Actual Rows: <n>`
      - `Execution Time: <ms> ms`

## Integration test assertions (substring-based)
- `tests/48_explain_analyze.sql`
  - Runs `EXPLAIN (ANALYZE)` on a simple PK-filtered query and `SELECT 1`.
- `tests/48_explain_analyze.assert`
  - Requires output substrings:
    - `Execution Time:`
    - `Actual Rows: 2`
- `scripts/integration_test.py`
  - Adds optional `<test>.assert` support:
    - Each non-empty, non-`#` line must appear as a substring of the captured `psql` output.
