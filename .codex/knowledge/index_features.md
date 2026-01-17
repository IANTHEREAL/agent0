# Index Features (partial / expression / method): facts + code locations

## Design doc
- `docs/design/14_index_features.md`
  - MVP goal is DDL compatibility for:
    - partial indexes (`CREATE INDEX ... WHERE <predicate>`)
    - expression indexes (`CREATE INDEX ... ( (expr) )`)
    - index methods (`USING gin/gist/...`)
  - MVP does **not** require planner acceleration for these forms.

## sqlparser AST (v0.40.0)
- `sqlparser::ast::Statement::CreateIndex` fields:
  - `using: Option<Ident>`
  - `columns: Vec<OrderByExpr>` (each has `expr: Expr`)
  - `predicate: Option<Expr>` (partial index WHERE clause)

## Core types and storage model
- `src/types/mod.rs`
  - `IndexDef` is persisted inside `TableSchema.indexes` and now includes:
    - `name: String`, `id: u64`, `columns: Vec<String>`, `unique: bool`
    - `method: Option<String>` (`None` => btree)
    - `predicate: Option<String>` (serialized `WHERE ...` expression)
    - `expressions: Vec<String>` (serialized index expressions)
  - `TableSchema.get_index_values(index, row)` derives index key values from `index.columns` only.
- `src/storage/encoding.rs`
  - index key format:
    - unique: `i_{table_id}_{index_id}_{index_values}` -> stores PK
    - non-unique: `i_{table_id}_{index_id}_{index_values}_{pk}` -> empty value

## DDL execution path
- `src/sql/executor.rs`
  - Matches `Statement::CreateIndex { .. }` and calls `Executor::execute_create_index(...)`.
- `src/sql/executor_ddl_ops.rs`
  - `Executor::execute_create_index(...)` resolves `table_name` via `names::resolve_existing_table_name(...)`,
    scans rows, then calls `src/sql/ddl.rs::execute_create_index(...)`.
  - For non-materialized indexes (non-btree / predicate present / expression columns), it skips the table scan and passes an empty `rows` vector.
- `src/sql/ddl.rs`
  - `execute_create_index(...)`:
    - accepts identifier columns and/or expression entries:
      - identifier/compound identifier -> stored in `IndexDef.columns`
      - other expressions -> stored as `expr.to_string()` in `IndexDef.expressions`
    - stores `Statement::CreateIndex.using` into `IndexDef.method` and `predicate` into `IndexDef.predicate`
    - only materializes physical index entries when:
      - method is `None`/`btree`
      - `predicate` is `None`
      - `expressions` is empty
      - `columns` is non-empty

## Existing “unsupported” gate
- `src/sql/helpers.rs`
  - `get_unsupported_reason(sql_upper)` no longer treats `USING GIST` as unsupported.
  - Note: this function is consulted only when `parse_sql(...)` fails.

## Planner usage of indexes
- `src/sql/planner.rs`
  - `choose_best_access_path(schema, predicates, estimated_rows)` iterates `schema.indexes` and calls `evaluate_index(index, ...)`.
  - `evaluate_index` assumes `index.columns` represent matchable predicate columns.
  - `is_planner_usable_index(index)` skips indexes with:
    - non-btree method
    - predicate present
    - expression columns present
    - zero `columns`

## Introspection surfaces for index definitions
- `src/sql/information_schema.rs`
  - `pg_indexes` virtual table:
    - rows built by `get_pg_indexes_rows(...)` using `schema.indexes`
    - `indexdef` string is currently always `USING btree (...)` and uses `idx.columns`
  - `pg_index` virtual table:
    - rows built by `get_pg_index_rows(...)`
    - stores `indkey` array derived from `idx.columns` only
    - stores `indexdef` string used by `pg_get_indexdef`
  - `pg_class` rows for indexes:
    - built by `get_pg_class_rows(...)`
    - `relam` (access method OID) is currently hard-coded to `403` (btree)
  - `pg_am` rows:
    - `get_pg_am_rows()` returns at least: `btree/hash/gist/gin/spgist/brin` with fixed OIDs.
- `src/sql/expr.rs`
  - `eval_function` / `eval_function_join` include a `PG_GET_INDEXDEF` fallback that returns the `indexdef` column when present (else `"CREATE INDEX"`).
- `src/sql/sequences.rs`
  - `replace_sequence_functions(...)` and `replace_sequence_functions_join(...)` special-case `PG_GET_INDEXDEF` during async expression rewriting:
    - Extract OID from arg0 (`Value::{Int32,Int64,Float64,Text(numeric)}`).
    - Fast path: if the current row contains both `indexrelid` and `indexdef` and `indexrelid == oid`, rewrite to the row’s `indexdef`.
    - Fallback: `lookup_indexdef_by_oid(store, txn, oid)` scans `store.list_tables(txn)` and matches catalog OIDs via `src/sql/catalog_oids.rs` (`pg_class_index_oid` / `pg_class_pk_index_oid`) rather than mirroring a per-query counter.
    - Rewrites to a literal via `value_to_sql_expr(Value::Text(indexdef))` so later evaluation is pure/sync.
  - Integration coverage: `tests/50_index_features.sql` asserts `PG_GET_INDEXDEF_STANDALONE=...` works without `pg_index` row context.
