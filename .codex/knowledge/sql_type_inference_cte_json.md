# SQL type inference fixes: recursive CTE + JSON access (`->`) (facts + code locations)

## Recursive CTE typing (CTE context schema)
- `src/sql/executor_cte.rs`
  - `Executor::build_cte_context(...)`
    - For non-recursive CTEs, now preserves column types from `ExecuteResult::Select.column_types` when building the in-memory `TableSchema` for the CTE.
    - If `column_types` is `None`, falls back to inferring from the first row’s `Value::data_type()`; if no rows, defaults to `DataType::Text`.
    - CTE column aliases (`WITH cte(col1, col2) AS (...)`) are normalized with `helpers::normalize_ident(...)`.
  - `Executor::execute_recursive_cte(...)`
    - Base term execution now captures `column_types` and builds the recursive working-table `TableSchema` with those types (same fallback rules as above).
    - This keeps numeric recursive columns (e.g. `level`, `n`) typed as `INT4` (when appropriate), so node clients parse them as numbers.

## JSON access operator typing (`Expr::JsonAccess`)
- `src/sql/helpers.rs`
  - `infer_expr_type(expr, schema)` now handles `Expr::JsonAccess { operator, .. }`:
    - `JsonOperator::Arrow` (`->`) → `DataType::Jsonb` (OID JSONB) so drivers parse JSON values (e.g. `"senior"` → `senior`).
    - `JsonOperator::LongArrow` (`->>`) → `DataType::Text`.
    - `JsonOperator::HashArrow` (`#>`) → `DataType::Jsonb`.
    - `JsonOperator::HashLongArrow` (`#>>`) → `DataType::Text`.

## Integer literal default typing (`Expr::Value(Number)`)
- `src/sql/helpers.rs`
  - `infer_expr_type` for `Expr::Value(SqlValue::Number(n, _))`:
    - Float literal (`.`/`e`/`E`) → `DataType::Float64`
    - Otherwise: `i32` parseable → `DataType::Int32`, else → `DataType::Int64`
  - This matches common PostgreSQL behavior where small integer literals default to `INTEGER` (INT4), not `BIGINT` (INT8).

