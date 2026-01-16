# pgwire Describe + RETURNING: facts + code locations

## Problem triggered by schemas (`schema.table` keys)
- Table/view/etc metadata keys now use full names like `public.users` (see `.codex/knowledge/schemas_search_path.md`).
- pgwire extended protocol (Parse/Bind/Describe/Execute) relies on `Describe*` to provide RowDescription (names + OIDs) for clients (node-postgres, ORMs).
- Old Describe inference in `src/protocol/handler.rs` tried `store.get_schema(txn, table)` using **unqualified** table name, which no longer matches stored full names; this caused:
  - wrong/unknown OIDs (TEXT everywhere) → JS clients see numbers/booleans as strings
  - `RETURNING *` Describe returning `*` as a single column → RowDescription/DataRow column count mismatch in clients

## Current implementation

### Extended protocol Describe entry points
- `src/protocol/handler.rs`
  - `impl ExtendedQueryHandler for DynamicPgHandler`:
    - `do_describe_statement()` → `infer_result_fields_from_query(&stmt.statement)`
    - `do_describe_portal()` → `substitute_parameters(...)` then `infer_result_fields_from_query(&final_query)`
  - `impl ExtendedQueryHandler for PgHandler` uses the same Describe flow.

### DML RETURNING field inference (AST-based)
- `src/protocol/handler.rs`
  - `DynamicPgHandler::infer_result_fields_from_query()`:
    - Parses SQL via `crate::sql::parse_sql(...)`
    - For `Statement::{Insert,Update,Delete} { returning: Some(..) }` delegates to `infer_returning_fields_from_statement(...)`
  - Helper pipeline:
    - `infer_returning_fields_from_statement(store, session, stmt)`:
      - Extracts target `ObjectName` + `returning: Vec<SelectItem>`
      - Uses `session.search_path()` and either `session.get_mut_txn()` or a temp `store.begin()` txn
      - Calls `infer_returning_fields_with_txn(...)`
    - `infer_returning_fields_with_txn(store, txn, search_path, table_name, returning)`:
      - Loads `TableSchema` using `resolve_table_schema_for_object_name(...)` (searches `search_path` and schema-qualified names)
      - Expands `SelectItem::Wildcard` / `SelectItem::QualifiedWildcard` to concrete columns
      - Emits `FieldInfo` with OIDs via `datatype_to_pgtype(...)`
    - Name/type helpers:
      - `normalize_sql_ident(...)` (quoted idents preserve case; unquoted lowercased)
      - `split_object_name_for_catalog(...)` (supports 1 or 2 name parts only)
      - `expr_column_name(...)` / `expr_referenced_column_type(...)`

### Placeholder substitution for Execute + DescribePortal
- `src/protocol/handler.rs`
  - `substitute_parameters(query, portal)`:
    - Replaces `$n` with literal values **in reverse order** (`$10` before `$1`) to avoid `$1` prefix replacement bugs.

## Simple-query RowDescription type correctness for empty results
- `src/protocol/handler.rs`
  - `result_to_response(ExecuteResult::Select { column_types, .. })` now prefers `column_types` (when present) to choose OIDs even if `rows` is empty.

