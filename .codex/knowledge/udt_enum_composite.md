# UDT (ENUM / Composite) implementation facts

## Design doc
- `docs/design/03_user_defined_types_enum_composite.md`
  - ENUM MVP: `CREATE TYPE ... AS ENUM`, `DROP TYPE [IF EXISTS] ... [CASCADE|RESTRICT]` (RESTRICT semantics), enum columns in CREATE/ALTER TABLE, INSERT/UPDATE validation, `pg_catalog` + `information_schema` introspection.
  - Composite: `CREATE TYPE ... AS (...)` + `DROP TYPE ...`, exposed via `pg_catalog.pg_type`, but **not usable as a column type**.

## Parser + storage constraints discovered
- `sqlparser` (crate) limitations (v0.40.x in this repo):
  - Cannot parse `CREATE TYPE ... AS ENUM (...)`.
  - Cannot parse `DROP TYPE ...`.
  - Workaround: custom command parsing/execution hook before `parse_sql()` (see `src/sql/executor_udt_cmd.rs` + `src/sql/executor.rs`).
- `bincode` (serde + bincode v1.3) schema evolution limitation:
  - Adding fields to existing serialized structs like `ColumnDef` breaks deserialization of stored `TableSchema` (observed `UnexpectedEof` when trying to add `ColumnDef.udt_name`).
  - Workaround: preserve declared UDT name via a new `DataType::UserDefined(String)` enum variant appended to `DataType` (see `src/model/mod.rs`).

## Schema representation (declared type name retention)
- `src/model/mod.rs`
  - `DataType::UserDefined(String)` (full name `schema.name`, e.g. `public.role`) appended to the `DataType` enum (comment warns not to reorder variants).
  - `ColumnDef` was **not** modified (bincode compatibility).

## Type catalog: metadata storage + API
- `src/storage/encoding.rs`
  - System keys:
    - `_sys_next_type_oid` (`encode_next_type_oid_key()`): persisted u32 counter for user type OIDs.
    - `_sys_type_{schema}.{name}` (`encode_type_key(full_name)`): stored `UserTypeDef` (bincode).
    - `encode_type_prefix()` for scanning all types.
- `src/model/mod.rs`
  - `UserTypeKind`:
    - `Enum { labels: Vec<String> }`
    - `Composite { fields: Vec<(String, DataType)> }`
  - `UserTypeDef { oid: u32, schema: String, name: String, kind: UserTypeKind, owner: String }`
- `src/storage/tikv_store.rs`
  - `TikvStore::next_type_oid(txn) -> u32` (starts at 20000; stores in `_sys_next_type_oid`).
  - `TikvStore::{create_type,get_type,list_types,drop_type}` (CRUD on `_sys_type_...` keys).

## DDL execution + dispatch
- `src/sql/executor.rs`
  - `Executor::execute()` intercepts:
    - `CREATE TYPE ... AS ENUM ...` → `execute_create_type_enum_cmd(...)`
    - `DROP TYPE ...` → `execute_drop_type_cmd(...)`
  - `Statement::CreateType { .. }` executes composite type creation via `udt::execute_create_type(...)`.
- `src/sql/executor_udt_cmd.rs`
  - Custom parsing + execution for sqlparser-unsupported statements:
    - `Executor::execute_create_type_enum_cmd(session, sql)`
      - Parses `CREATE TYPE <name> AS ENUM (<labels>)` (handles quoted identifiers, doubled single-quotes in labels, and `ENUM(` with no whitespace).
    - `Executor::execute_drop_type_cmd(session, sql)`
      - Parses `DROP TYPE [IF EXISTS] <name>[, ...] [CASCADE|RESTRICT]` (accepts CASCADE but behaves as RESTRICT).
- `src/sql/udt.rs`
  - `execute_create_type(...)` handles `UserDefinedTypeRepresentation::Composite`.
  - `create_enum_type(...)` persists enum types (rejects duplicates; rejects empty label set).
  - `drop_types(...)` enforces RESTRICT by scanning all tables and rejecting drops when any `TableSchema.columns[*].data_type == DataType::UserDefined(full_name)`.

## Column type binding (CREATE TABLE / ALTER TABLE ADD COLUMN)
- `src/sql/ddl.rs`
  - `resolve_column_data_type(store, txn, sql_type) -> (DataType, is_serial)`:
    - For `SqlDataType::Custom`:
      - Built-ins: SERIAL/BIGSERIAL/VECTOR/JSON/JSONB handled as before.
      - If custom name matches a stored enum type: `DataType::UserDefined(full_name)`.
      - If custom name matches a stored composite type: error `composite type '<full_name>' cannot be used as a column type`.
      - Otherwise falls back to existing behavior (`convert_data_type(...)`, which defaults unknown custom types to `TEXT`).

## DML enum validation (INSERT / UPDATE)
- `src/sql/dml.rs`
  - `build_enum_label_cache(store, txn, schema) -> HashMap<full_udt_name, HashSet<label>>` (loads each referenced type once per statement).
  - `validate_enum_values(schema, row, cache)`:
    - NULL allowed.
    - Non-NULL must be `Value::Text` and be a member of enum labels.
    - Error string prefix: `invalid input value for enum <full_name>: ...`.
- `src/sql/executor_dml_ops.rs`
  - Builds enum cache once per target table per statement and passes it into `dml::execute_insert_row` / `dml::execute_update_row`.

## Introspection (pg_catalog / information_schema)
- `src/sql/information_schema.rs`
  - `pg_catalog.pg_type` rows:
    - `get_pg_type_rows(...)` appends rows for all stored user types (`TikvStore::list_types()`), plus the existing `vector` row.
  - `pg_catalog.pg_enum`:
    - `pg_enum_schema()` + `get_pg_enum_rows(...)` produce rows for enum labels.
    - `pg_enum.oid` is synthetic: `(enum_type_oid * 1_000_000) + (label_index + 1)`.
  - `information_schema.columns`:
    - For `DataType::UserDefined("schema.name")`: `data_type='USER-DEFINED'`, `udt_schema=schema`, `udt_name=name`.
  - `pg_catalog.pg_attribute`:
    - For `DataType::UserDefined(full_name)`: `atttypid` uses stored type OID (fallbacks to TEXT OID 25 if missing).

## Wire/protocol + value encoding compatibility
- `src/protocol/handler.rs`
  - `datatype_to_pgtype(...)`: `DataType::UserDefined(_)` maps to `TEXT` for pgwire (values are stored/transmitted as strings).
- `src/storage/encoding.rs`
  - `decode_value_memcomparable(..., data_type)`: `DataType::UserDefined(_)` decodes as `Value::Text`.
- `src/sql/helpers.rs`
  - `parse_value_for_copy(...)`: `DataType::UserDefined(_)` parses COPY input as text.

## Tests added
- `tests/39_enum_types.sql` + `tests/39_enum_types.errors`:
  - Covers enum create/use/invalid label, `information_schema.columns` UDT fields, `pg_type`/`pg_enum` join, `pg_attribute` atttypid join, DROP TYPE RESTRICT behavior, composite create + disallow as column type.
- `src/sql/dml.rs` unit tests:
  - Validate enum label membership, NULL acceptance, and error message contents for invalid values / non-text values.
