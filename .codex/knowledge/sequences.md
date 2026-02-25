# Sequences implementation facts + code locations

## Design doc
- `docs/design/04_sequences.md`
  - MVP: `CREATE SEQUENCE` / `DROP SEQUENCE`, `nextval/currval/setval`, SERIAL/IDENTITY compatibility (implicit `{table}_{column}_seq` must exist; `currval` is session-scoped).

## Implementation (current)

### Types
- `src/model/mod.rs`
  - `SequenceState { last_value: i64, is_called: bool }`
  - `SequenceBacking::{ TableId(u64), Standalone(SequenceState) }`
  - `SequenceDef { oid, schema, name, start_value, increment, min_value, max_value, cache_size, is_cycled, owned_by, owner, backing }`
  - `SequenceDef::full_name() -> String` → `"{schema}.{name}"`

### Storage keyspace
- `src/storage/encoding.rs`
  - Sequence def key prefix: `b"_sys_seqdef_"` via `encode_sequence_key(full_name)` / `encode_sequence_prefix()`.
- `src/storage/tikv_store.rs`
  - Existing per-table autoincrement key prefix remains: `b"_sys_seq_" + table_id.to_be_bytes()` in `TikvStore::{next_sequence_value,set_sequence_value}`.

### Storage APIs + semantics
- `src/storage/tikv_store.rs`
  - CRUD: `create_sequence/get_sequence/list_sequences/drop_sequence`
  - `nextval_sequence(txn, full_name) -> i64`
    - `SequenceBacking::TableId(table_id)` → `next_sequence_value(txn, table_id)` cast to i64.
    - `SequenceBacking::Standalone(state)` → `nextval_standalone(full_name, increment, min, max, is_cycled, state)`.
  - `setval_sequence(txn, full_name, value, is_called) -> i64`
    - `SequenceBacking::TableId(table_id)`
      - writes `_sys_seq_` to `value` when `is_called=true`
      - writes `_sys_seq_` to `value-1` when `is_called=false` (so next insert/nextval returns `value`).
    - `SequenceBacking::Standalone(state)` → `setval_standalone(full_name, min, max, state, value, is_called)`.
  - Pure helpers (used by the methods above):
    - `nextval_standalone(...)` (handles `is_called`, `increment`, `min/max`, `cycle`, overflow).
    - `setval_standalone(...)` (bounds check + sets `last_value/is_called`).
  - Unit tests: `#[cfg(test)] mod sequence_tests` covers `is_called` + cycle + bound error.

### SQL sequences module
- `src/sql/sequences.rs`
  - Name normalization: `normalize_sequence_name(ObjectName, search_path)` (unqualified schema = `names::default_schema(search_path)`).
  - String/regclass parsing: `parse_sequence_name_token(token)` supports quoted identifiers + schema-qualified names.
  - Function-arg name resolution: `resolve_sequence_full_name_from_value(store, txn, search_path, Value)` probes `search_path` for unqualified sequence names (first match wins).
  - Implicit SERIAL/IDENTITY sequence def: `build_implicit_sequence_def(table, column, table_id)` → `SequenceBacking::TableId(table_id)` + `owned_by = Some((table, column))`.
  - DDL execution:
    - `execute_create_sequence(...)` parses `SequenceOptions::{StartWith,IncrementBy,MinValue,MaxValue,Cycle,Cache}`.
    - `execute_drop_sequence(...)`.
  - Expression evaluation wiring:
    - `expr_uses_sequence_functions(&Expr) -> bool` (via `sqlparser::ast::visit_expressions`).
    - `expr_uses_current_schema(&Expr) -> bool` (CURRENT_SCHEMA rewrite trigger).
    - Non-join: `eval_expr_with_sequences(...)` + `replace_sequence_functions(...)` → rewrites `NEXTVAL/CURRVAL/SETVAL` to literals by calling `TikvStore::{nextval_sequence,setval_sequence}` and updating `last_sequence_values`.
    - Join: `eval_expr_join_with_sequences(...)` + `replace_sequence_functions_join(...)` (uses `eval_expr_join` for arg evaluation).

### Session currval cache
- `src/sql/session.rs`
  - Field: `Session.last_sequence_values: HashMap<String, i64>`
  - Accessor: `Session::get_mut_txn_sequence_values_and_search_path() -> Option<(&mut Transaction, &mut HashMap<String,i64>, &[String])>`

### Executor plumbing
- `src/sql/executor.rs`
  - `execute_statement_on_txn(txn, sequence_values, search_path, stmt)` threads sequence cache + `search_path` through statement execution.
  - Helpers:
    - `Executor::eval_expr_maybe_sequence(..., search_path, ...)` (trigger: `NEXTVAL|CURRVAL|SETVAL` or `CURRENT_SCHEMA`)
    - `Executor::eval_expr_join_maybe_sequence(..., search_path, ...)`
- `src/sql/executor_select.rs`
  - ORDER BY: precomputes sort keys when any ORDER BY expression uses `NEXTVAL/CURRVAL/SETVAL` (avoids calling eval in sort comparator).
- `src/sql/executor_join.rs`, `src/sql/executor_dml_ops.rs`, `src/sql/dml.rs`
  - SELECT/JOIN/DML expression evaluation routes through the sequence-aware helpers (including DEFAULT + RETURNING).

### SERIAL/IDENTITY bridge + cleanup
- `src/sql/ddl.rs`
  - `create_implicit_sequences_for_schema(store, txn, schema)` called after `store.create_table` in:
    - `execute_create_table(...)`
    - `create_table_from_query_result(...)`
    - `create_table_from_select_into(...)`
  - `execute_alter_table(...)` / `AlterTableOperation::AddColumn`
    - Detects SERIAL/BIGSERIAL via `resolve_column_data_type(...)`
    - Detects IDENTITY via `ColumnOption::Generated { generated_as: Always|ByDefault, generation_expr: None }`
    - Creates implicit sequence object when `is_serial=true`.
  - `execute_drop_table(...)` calls `drop_owned_sequences_for_table(store, txn, table_name)` before dropping the table.

### System catalog + type inference
- `src/sql/information_schema.rs`
  - `get_pg_class_rows(...)` appends sequences from `store.list_sequences(txn)` as `relkind='S'` (OID derived from persisted `SequenceDef.oid`).
  - `get_pg_sequence_rows(...)` exposes `pg_catalog.pg_sequence` and joins to `pg_class` via `seqrelid`.
- `src/sql/helpers.rs`
  - `infer_expr_type()` returns `DataType::Int64` for `NEXTVAL/CURRVAL/SETVAL`.

### Integration tests
- `tests/40_sequences.sql` smoke coverage for DDL + functions + SERIAL.
- `tests/40_sequences_load.py`
  - Asserts standalone `nextval/currval/setval` + `is_called` semantics.
  - Asserts `currval` cross-connection error contains `not yet defined in this session`.
  - Asserts implicit SERIAL sequence exists in `pg_class` (`relkind='S'`), shares backing with INSERT-generated ids, and `DROP TABLE` removes the implicit sequence object.

## Current repo behavior (before implementation)

### Internal autoincrement (table_id-based)
- `src/storage/tikv_store.rs`
  - `TikvStore::next_sequence_value(txn, table_id) -> i32`
    - key bytes: `b"_sys_seq_" + table_id.to_be_bytes()`
    - value bytes: `u64::to_be_bytes`, updated via `increment_sys_key` (missing => 1, else +1).
  - `TikvStore::set_sequence_value(txn, table_id, value: u64)`
    - writes `value.to_be_bytes()` to the same key.
- `src/sql/dml.rs`
  - `fill_missing_columns(...)`:
    - if `ColumnDef.is_serial` and value missing, calls `store.next_sequence_value(txn, schema.table_id)` and fills `Value::Int32` (or `Value::Int64` when column type is `Int64`).
- `src/sql/executor.rs`
  - `execute_copy_insert(...)` also uses `store.next_sequence_value(txn, schema.table_id)` for serial columns.

### Information schema advertises `nextval('..._seq'::regclass)` for serial
- `src/sql/information_schema.rs`
  - `get_columns_rows(...)`:
    - if `col.is_serial`, sets `column_default` to schema-qualified `nextval('schema.table_col_seq'::regclass)` (implicit sequence object must exist).
    - sequence name uses `sequences::implicit_sequence_name(table_name, column_name)`; schema/table are derived from the stored full table name (`schema.table`).

### Archived: pre-sequences stubs (no longer true)
- Removed (sequences DDL + `nextval/currval/setval` are implemented; see sections above).
