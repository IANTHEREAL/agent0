# CREATE FUNCTION / TRIGGER (Stage 1): facts + code locations

## Design doc
- `docs/design/12_functions_and_triggers.md`
  - Stage 1 (MVP): accept `CREATE/DROP FUNCTION` + `CREATE/DROP TRIGGER`, store definitions in TiKV metadata, provide minimal introspection; triggers do not execute.

## Existing baseline (before this task)
- `src/sql/executor.rs`
  - `Statement::CreateFunction { .. }` was a no-op returning `ExecuteResult::Empty`.
- `src/sql/helpers.rs`
  - `get_unsupported_reason()` pre-rejected `CREATE TRIGGER` with `"CREATE TRIGGER not supported"`.

## Types (metadata structs)
- `src/types/mod.rs`
  - `FunctionDef { oid, schema, name, arg_types: Vec<String>, return_type: String, language: String, body: String }`
  - `TriggerDef { oid, schema, name, table, timing, events: Vec<String>, function }`

## Storage keys + encoding
- `src/storage/encoding.rs`
  - `_sys_func_`:
    - `encode_function_key("schema.name") -> b"_sys_func_schema.name"`
    - `encode_function_prefix() -> b"_sys_func_"`
  - `_sys_trigger_`:
    - Trigger names are scoped to a table; keys are `<table_full_name>/<trigger_name>`:
      - `encode_trigger_key("schema.table", "trg") -> b"_sys_trigger_schema.table/trg"`
      - `encode_trigger_table_prefix("schema.table") -> b"_sys_trigger_schema.table/"`
      - `encode_trigger_prefix() -> b"_sys_trigger_"`

## TikvStore APIs (CRUD + scans)
- `src/storage/tikv_store.rs`
  - Functions:
    - `create_function(txn, FunctionDef)` (errors if exists)
    - `replace_function(txn, FunctionDef)` (overwrite)
    - `get_function(txn, "schema.name") -> Option<FunctionDef>`
    - `list_functions(txn) -> Vec<FunctionDef>` (prefix scan `_sys_func_`)
    - `drop_function(txn, "schema.name") -> bool`
  - Triggers:
    - `create_trigger(txn, TriggerDef)` (errors if trigger exists on table)
    - `get_trigger(txn, "schema.table", "trg") -> Option<TriggerDef>`
    - `list_triggers(txn) -> Vec<TriggerDef>` (prefix scan `_sys_trigger_`)
    - `list_triggers_for_table(txn, "schema.table") -> Vec<TriggerDef>` (prefix `_sys_trigger_schema.table/`)
    - `drop_trigger(txn, "schema.table", "trg") -> bool`

## Schema drop + table drop cleanup
- `src/storage/tikv_store.rs`
  - `drop_schema_restrict(...)` now considers:
    - `_sys_func_<schema>.` via `encode_function_prefix()` + `<schema>.`
    - `_sys_trigger_<schema>.` via `encode_trigger_prefix()` + `<schema>.`
  - Prevents `DROP SCHEMA` from leaving orphaned function/trigger metadata.
- `src/sql/ddl.rs`
  - `execute_drop_table(...)` deletes all triggers for the table via:
    - `store.list_triggers_for_table(txn, "<schema.table>")`
    - `store.drop_trigger(txn, "<schema.table>", "<trigger>")`

## Executor wiring (raw SQL commands)
- `src/sql/executor.rs`
  - `Executor::execute()` intercepts (before `parse_sql`) using `strip_leading_sql_comments(...)`:
    - `CREATE [OR REPLACE] FUNCTION ...` → `execute_create_function_cmd(...)`
    - `DROP FUNCTION ...` → `execute_drop_function_cmd(...)`
    - `CREATE [CONSTRAINT] TRIGGER ...` → `execute_create_trigger_cmd(...)`
    - `DROP TRIGGER ...` → `execute_drop_trigger_cmd(...)`
- `src/sql/executor_functions_triggers.rs`
  - `strip_leading_sql_comments(sql)` removes leading `-- ...` and `/* ... */` blocks for prefix detection/parsing.
  - `execute_create_function_cmd(session, sql)`
    - Parses `CREATE [OR REPLACE] FUNCTION <name>(...) RETURNS <type> ... LANGUAGE <lang> ... AS $tag$...$tag$`
    - Stores `FunctionDef` via `TikvStore::{create_function,replace_function}`
  - `execute_drop_function_cmd(session, sql)`
    - Parses `DROP FUNCTION [IF EXISTS] ...` (drops by name; signature ignored)
  - `execute_create_trigger_cmd(session, sql)`
    - Parses `CREATE TRIGGER <name> BEFORE/AFTER ... ON <table> ... EXECUTE {FUNCTION|PROCEDURE} <func>(...)`
    - Resolves target table via `names::resolve_existing_table_name` (must exist)
    - Resolves function via `names::resolve_existing_function_name` (weak check; falls back to `search_path[0]`)
    - Stores `TriggerDef` via `TikvStore::create_trigger`
  - `execute_drop_trigger_cmd(session, sql)`
    - Parses `DROP TRIGGER [IF EXISTS] <name> ON <table>`
    - Deletes via `TikvStore::drop_trigger`
- `src/sql/names.rs`
  - `resolve_existing_function_name(store, txn, name, search_path) -> Option<ResolvedName>` (like tables/sequences/types)

## Unsupported filter updated
- `src/sql/helpers.rs`
  - `get_unsupported_reason(...)` no longer rejects `CREATE TRIGGER`.

## pg_catalog introspection (minimal)
- `src/sql/information_schema.rs`
  - `pg_trigger_schema()` added and wired into:
    - `is_information_schema_table(...)`
    - `parse_information_schema_table(...)`
    - `get_information_schema_schema(...)`
    - `get_information_schema_data(..., "pg_trigger")`
  - `get_pg_proc_rows(store, txn, schema_oids)` now returns stored functions from `store.list_functions(txn)`
  - `get_pg_trigger_rows(store, txn, user_tables)` returns stored triggers from `store.list_triggers(txn)`
    - `tgrelid` matches `pg_class` table OIDs (derived from stable `table_id` via `src/sql/catalog_oids.rs`).
    - `tgfoid` matches `pg_proc.oid` for stored functions (derived from persisted `FunctionDef.oid` via `src/sql/catalog_oids.rs`).

## Integration tests
- `tests/46_functions_triggers_ddl.sql`
  - Creates a PL/pgSQL trigger function with dollar-quoted body + a trigger, queries `pg_proc`/`pg_trigger`, drops both.
- `tests/46_functions_triggers_ddl.assert`
  - Substring assertions: `FUNC_FOUND=1`, `TRIGGER_FOUND=1`, `TRIGGER_LEFT=0`, `FUNC_LEFT=0`.
- `tests/47_schema_drop_functions.sql`
  - Regression: dropping a schema with a stored function should fail (`RESTRICT`), and dropping a table should clean up triggers automatically.
- `tests/47_schema_drop_functions.errors`
  - Expected error substring for the schema-not-empty failure.
- `tests/47_schema_drop_functions.assert`
  - Asserts `TRIGGER_LEFT=0` and `SCHEMA_LEFT=0`.

## Storage conventions (similar catalogs)
- `src/storage/encoding.rs`
  - Procedure keys: `_sys_proc_{schema}.{name}` via `encode_procedure_key(...)`.
  - Type keys: `_sys_type_{schema}.{name}` via `encode_type_key(...)`.
- `src/storage/tikv_store.rs`
  - Catalog entries for types/sequences use `bincode` serialization and prefix scans (`list_types`, `list_sequences`).

## System catalog baseline (historical)
- Before Stage 1, `pg_proc` returned empty and `pg_trigger` did not exist; this task added minimal, joinable introspection rows.
