# Schemas + search_path facts + code locations

## Design doc
- `docs/design/05_schemas_search_path.md`

## Session `search_path` state
- `src/sql/session.rs`
  - `Session { search_path: Vec<String> }` (default `["public"]`)
  - `Session::set_search_path(Vec<String>)`
  - `Session::get_mut_txn_sequence_values_and_search_path() -> Option<(&mut Transaction, &mut HashMap<String, i64>, &[String])>`

## `SET search_path TO ...`
- `src/sql/executor.rs`
  - `Executor::execute()` handles `Statement::SetVariable { variable, value, .. }`:
    - Only `search_path` is implemented; other `SET` vars remain no-op (`ExecuteResult::Empty`).
    - Values accepted:
      - `Expr::Identifier(ident)` → schema name via `normalize_ident(ident)`
      - `Expr::Value(SingleQuotedString(s))` → schema name via `s.to_lowercase()` (no SQL unescaping beyond what sqlparser provides)
    - Normalization:
      - Drops empty entries and `$user`
      - Single value `DEFAULT`/`default` becomes `["public"]`
      - Rejects any schema name containing `'.'`
      - Empty list becomes `["public"]`

## Name resolution helpers (`ResolvedName` + `search_path` lookup)
- `src/sql/names.rs`
  - `ResolvedName { schema, name, full }` (`full = "{schema}.{name}"`)
  - Validation:
    - `validate_schema_ident(schema)` rejects empty or containing `'.'`
    - `validate_object_ident(name)` rejects empty or containing `'.'`
  - Parsing helpers:
    - `split_object_name(ObjectName) -> Result<(Option<String>, String)>` (uses `normalize_ident`)
    - `parse_full_name("schema.name") -> Result<(String, String)>`
    - `default_schema(search_path) -> &str` (`search_path[0]` or `"public"`)
  - DDL name resolution:
    - `resolve_ddl_object_name(ObjectName, search_path) -> ResolvedName` (unqualified → `search_path[0]`)
  - Existing-object resolution (unqualified probes `search_path` in order; returns first hit):
    - `resolve_existing_table_name(...)` → `store.get_schema(txn, full)`
    - `resolve_existing_view_name(...)` → `store.get_view(txn, full)`
    - `resolve_existing_materialized_view_name(...)` → `store.get_materialized_view(txn, full)`
    - `resolve_existing_sequence_name(...)` → `store.get_sequence(txn, full)`
    - `resolve_existing_type_name(...)` → `store.get_type(txn, full)`
    - `resolve_existing_procedure_name(...)` → `store.get_procedure(txn, full)`

## Storage: schema catalog + full-name keys
- `src/storage/encoding.rs`
  - Schema catalog keys:
    - `_sys_schemadef_{schema}` via `encode_schema_def_key(schema)` / `encode_schema_def_prefix()`
  - Object metadata keys accept full names (`schema.name`):
    - Tables: `_sys_schema_{schema.table}` via `encode_schema_key(full_table_name)`
    - Views: `_sys_view_{schema.view}` via `encode_view_key(full_view_name)`
    - Matviews: `_sys_matview_{schema.matview}` via `encode_matview_key(full_view_name)`
    - Procedures: `_sys_proc_{schema.proc}` via `encode_procedure_key(full_proc_name)`
    - Types: `_sys_type_{schema.type}` via `encode_type_key(full_type_name)`
    - Sequences: `_sys_seqdef_{schema.seq}` via `encode_sequence_key(full_seq_name)`
- `src/storage/tikv_store.rs`
  - Built-in schemas treated as always existing:
    - `TikvStore::is_builtin_schema()` / `TikvStore::schema_exists()`: `public`, `pg_catalog`, `information_schema`
  - Schema catalog APIs:
    - `create_schema(txn, schema, if_not_exists)`
    - `drop_schema_restrict(txn, schema, if_exists)`
      - Rejects dropping built-in schemas
      - RESTRICT check scans for any keys under `schema.*` for:
        - tables (`encode_schema_prefix()`)
        - views (`encode_view_prefix()`)
        - matviews (`encode_matview_prefix()`)
        - procedures (`encode_procedure_prefix()`)
        - types (`encode_type_prefix()`)
        - sequences (`encode_sequence_prefix()`)
    - `list_schemas(txn) -> Vec<String>` (includes built-ins + stored schema defs; sorted/deduped)

## DDL + DML wiring: use `schema.name`
- `src/sql/ddl.rs`
  - CREATE/DROP/ALTER TABLE + VIEW + SEQUENCE + TYPE resolve names via `src/sql/names.rs` and pass `ResolvedName.full` to store APIs.
  - Foreign keys:
    - `ForeignKeyConstraint.ref_table` stored as full name (`schema.table`) so FK validation works with schemas.
  - Materialized views:
    - `execute_create_materialized_view(store, txn, search_path, name, ...)`
      - Resolves `name` to full name
      - Stores matview def under full name, creates backing table under the same full name
      - Calls `create_implicit_sequences_for_schema(...)` and sets per-table `_sys_seq_{table_id}` via `set_sequence_value(...)`
    - `execute_drop_materialized_view(store, txn, search_path, names, if_exists)`
      - Resolves existing matview via `search_path`, drops matview def + owned sequences + backing table
    - `execute_refresh_materialized_view(store, txn, full_name, rows)`
      - Verifies matview exists, truncates backing table, inserts rows, resets per-table sequence value
- `src/sql/executor_ddl_ops.rs`
  - `create_table_from_result(txn, search_path, target_name, result)` (SELECT INTO)
    - Resolves `target_name` to `search_path[0]` via `resolve_ddl_object_name`
    - Creates table under full name via `ddl::create_table_from_select_into(...)`
- `src/sql/executor_select.rs`
  - All SELECT INTO call sites pass `search_path` to `create_table_from_result(...)`.
- `src/sql/executor_dml_ops.rs`
  - INSERT/UPDATE/DELETE resolve target tables via `resolve_existing_table_name(...)` and use full names.
- `src/sql/executor_join.rs`
  - JOIN execution resolves tables/views using full names and `search_path`.

## Expression `CURRENT_SCHEMA()` + sequence name resolution uses `search_path`
- `src/sql/sequences.rs`
  - Detection:
    - `expr_uses_sequence_functions(expr)` detects `NEXTVAL|CURRVAL|SETVAL`
    - `expr_uses_current_schema(expr)` detects `CURRENT_SCHEMA`
  - Rewrite:
    - `replace_sequence_functions(...)` and `replace_sequence_functions_join(...)`:
      - `CURRENT_SCHEMA()` → literal `Value::Text(default_schema(search_path))`
      - `NEXTVAL/CURRVAL/SETVAL` resolve unqualified sequence names via `search_path` using `resolve_sequence_full_name_from_value(...)`
- `src/sql/executor.rs`
  - `Executor::eval_expr_maybe_sequence(...)` and `Executor::eval_expr_join_maybe_sequence(...)`:
    - Use sequence/current_schema rewrite when expression uses `NEXTVAL|CURRVAL|SETVAL` **or** `CURRENT_SCHEMA`.
- `src/sql/dml.rs`
  - Expression evaluation for RETURNING/DEFAULT/INSERT/UPDATE calls `sequences::eval_expr_with_sequences(..., search_path, ...)` when expression uses `NEXTVAL|CURRVAL|SETVAL` or `CURRENT_SCHEMA`.

## Introspection understands `schema.table` keys
- `src/sql/information_schema.rs`
  - `split_schema_and_name(full)` uses `names::parse_full_name(full)` to derive `(table_schema, table_name)`
  - Schema enumeration uses `store.list_schemas(txn)`
  - `columns.column_default` for SERIAL uses schema-qualified implicit sequence:
    - `nextval('schema.table_col_seq'::regclass)`
  - Custom schema OIDs:
    - Built-ins have fixed OIDs; custom schemas assigned deterministically starting at 20000 (via schema list ordering).
  - `pg_catalog.pg_namespace`, `pg_catalog.pg_class.relnamespace`, `pg_catalog.pg_constraint.connamespace`, `pg_catalog.pg_type.typnamespace` use that schema OID mapping.

## Materialized view / procedure command parsing (string-based)
- `src/sql/executor_procedure.rs`
  - `object_name_from_token(token) -> ObjectName` parses `schema.name` (split on `.`) for cmd-only statements.
  - `REFRESH MATERIALIZED VIEW`:
    - Resolves existing name via `names::resolve_existing_materialized_view_name(...)` using session `search_path`.
  - `DROP MATERIALIZED VIEW`:
    - Calls `ddl::execute_drop_materialized_view(..., search_path, ...)` (resolution happens inside DDL).
  - `CALL` / `DROP PROCEDURE`:
    - Resolves existing name via `names::resolve_existing_procedure_name(...)` using session `search_path`.
  - `CREATE PROCEDURE` (cmd parser):
    - Resolves DDL name via `names::resolve_ddl_object_name(...)` and checks `store.schema_exists(...)`.
