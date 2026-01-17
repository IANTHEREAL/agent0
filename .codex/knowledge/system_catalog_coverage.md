# System catalog coverage (pg_catalog / information_schema): facts + code locations

## Design doc
- `docs/design/11_system_catalog_coverage.md`
  - Stable OID generation is required for join-heavy catalogs (pg_namespace/pg_class/pg_index/pg_attribute/pg_type/etc).
  - Missing catalogs called out: `pg_proc` (was empty), `pg_enum`, `pg_sequence`, `pg_attrdef`, `pg_tables`, `pg_views`, optional `pg_depend`.

## Virtual system catalogs entry points
- `src/sql/information_schema.rs`
  - Table routing helpers:
    - `is_information_schema_table(table_name: &str) -> bool`
    - `parse_information_schema_table(table_name: &str) -> Option<&str>`
    - `get_information_schema_schema(table_name: &str) -> Option<TableSchema>`
    - `get_information_schema_data(store, txn, table_name) -> Result<(TableSchema, Vec<Row>)>`
  - Current pg_catalog tables implemented (pre-change):
    - `pg_type`, `pg_enum`, `pg_namespace`, `pg_class`, `pg_index`, `pg_attribute`, `pg_proc`, `pg_trigger`,
      `pg_constraint`, `pg_am`, `pg_indexes`, `pg_description`.
  - Additional pg_catalog tables implemented (current):
    - `pg_attrdef`, `pg_sequence`, `pg_tables`, `pg_views`, `pg_depend`.
  - OID mapping (pre-change):
    - `build_schema_oid_map(schemas)`: built-ins fixed; user schemas assigned by sorted name order starting at 20000 (unstable when new schema is inserted in sort order).
    - `get_pg_class_rows/get_pg_index_rows/get_pg_attribute_rows/...`: `oid_counter` based on per-query sequential assignment starting at 16384 (unstable across schema changes; dependent tables mirror the same counter for join consistency).

## Stable OID strategy (current)
- `src/sql/catalog_oids.rs`
  - `pg_class` object OIDs (stable, derived from IDs):
    - `pg_class_table_oid(table_id)` → `TABLE_OID_BASE + table_id`
    - `pg_class_index_oid(table_id, index_id)` → `INDEX_OID_BASE + (table_id << INDEX_ID_BITS) + index_id` (`INDEX_ID_BITS=20`, `index_id` must fit)
    - `pg_class_pk_index_oid(table_id)` → `pg_class_index_oid(table_id, 0)`
    - `pg_class_sequence_oid(sequence_oid)` → `SEQUENCE_OID_BASE + sequence_oid`
    - `pg_class_view_oid(view_oid)` → `VIEW_OID_BASE + view_oid` (view_oid is derived from a stable hash of the full view name in `src/sql/information_schema.rs`).
  - Stored object OIDs:
    - `pg_proc_function_oid(function_oid)` → `FUNCTION_OID_BASE + function_oid`
    - `pg_trigger_oid(trigger_oid)` → `TRIGGER_OID_BASE + trigger_oid`
- `src/storage/encoding.rs`
  - New system keys:
    - `_sys_next_schema_oid` via `encode_next_schema_oid_key()`
    - `_sys_next_sequence_oid` via `encode_next_sequence_oid_key()`
    - `_sys_next_function_oid` via `encode_next_function_oid_key()`
    - `_sys_next_trigger_oid` via `encode_next_trigger_oid_key()`
  - Schema def values:
    - `_sys_schemadef_{schema}` now stores `u32` OID bytes (BE), not presence-only empty values.
- `src/storage/tikv_store.rs`
  - OID allocators:
    - `next_schema_oid()`, `next_sequence_oid()`, `next_function_oid()`, `next_trigger_oid()`
  - Schema OIDs:
    - `create_schema(...)` writes schema OID into `_sys_schemadef_{schema}` value.
    - `list_schema_oids(...)` scans `_sys_schemadef_` keys and reads OID from value; legacy empty values are assigned an OID and updated.
  - Legacy migration (OID=0) for stored defs:
    - `get_sequence/list_sequences` assign `SequenceDef.oid` when missing and persist.
    - `get_function/list_functions` assign `FunctionDef.oid` when missing and persist.
    - `list_triggers` assigns `TriggerDef.oid` when missing and persists (table+name key).
    - `create_sequence/create_function/replace_function/create_trigger` ensure OID is set before persisting.
- `src/types/mod.rs`
  - New persisted OID fields (serde default 0 for backward compatibility):
    - `SequenceDef { oid: u32, ... }`
    - `FunctionDef { oid: u32, ... }`
    - `TriggerDef { oid: u32, ... }`
- `src/sql/information_schema.rs`
  - Uses stable schema OIDs via `store.list_schema_oids(txn)` (replaces `build_schema_oid_map`).
  - Uses stable pg_class-related OIDs via `catalog_oids::*` in:
    - `get_pg_class_rows` (tables/indexes/pk/sequences/views; views use `stable_hash_u32(full_view_name)` + `pg_class_view_oid(...)`)
    - `get_pg_index_rows` (indexrelid/indrelid)
    - `get_pg_attribute_rows` (attrelid)
    - `get_pg_constraint_rows` (conrelid/confrelid)
    - `get_pg_proc_rows` (pg_proc.oid)
    - `get_pg_trigger_rows` (pg_trigger.oid + tgrelid + tgfoid)
  - Boolean-typed catalog columns (ORM-friendly):
    - `pg_class.relhasindex`, `pg_class.relispopulated`, `pg_class.relispartition`
    - `pg_index.indisunique`, `pg_index.indisprimary`, `pg_index.indisexclusion`, `pg_index.indimmediate`, `pg_index.indisclustered`, `pg_index.indisvalid`
    - `pg_attribute.attnotnull`, `pg_attribute.atthasdef`, `pg_attribute.attisdropped`, `pg_attribute.attislocal`
  - Builtin `pg_proc` rows:
    - `get_pg_proc_rows(...)` prepends a minimal pg_catalog builtin set (e.g. `format_type`, `pg_get_expr`, `pg_get_indexdef`, `pg_get_constraintdef`, `version`, `current_schema`, `current_database`, `current_user`).
  - `pg_depend` (minimal ownership dependencies):
    - `get_pg_depend_rows(...)` emits dependency rows for `SequenceDef.owned_by` linking sequence (`objid`) to owning table (`refobjid`) + column attnum (`refobjsubid`), with `classid/refclassid = 1259` (pg_class) and `deptype = 'a'`.

## Stored catalog objects (non-table metadata)
- Schemas:
  - `src/storage/encoding.rs`: `_sys_schemadef_{schema}` via `encode_schema_def_key()`; value stores `u32` schema OID bytes (BE).
  - `src/storage/tikv_store.rs`:
    - `create_schema(...)` allocates OID (`next_schema_oid`) and writes it as the value.
    - `list_schema_oids(...)` reads OIDs; upgrades legacy empty values by allocating + persisting.
    - `list_schemas(...)` still scans keys for names (built-ins + custom).
- Sequences:
  - `src/types/mod.rs`: `SequenceDef { oid, schema, name, start_value, increment, min_value, max_value, cache_size, is_cycled, owned_by, owner, backing }`.
  - `src/storage/tikv_store.rs`: `create_sequence/get_sequence/list_sequences` serialize/deserialize `SequenceDef` via `bincode`.
  - `src/sql/information_schema.rs`: sequences appear in:
    - `pg_class` (`relkind='S'`, OID = `catalog_oids::pg_class_sequence_oid(SequenceDef.oid)`)
    - `pg_sequence` (`seqrelid` matches the `pg_class` OID)
- Functions / triggers:
  - `src/types/mod.rs`: `FunctionDef { oid, ... }` and `TriggerDef { oid, ... }` stored via `bincode`.
  - `src/storage/tikv_store.rs`: CRUD + list for functions/triggers.
  - `src/sql/information_schema.rs`:
    - `pg_proc.oid` is derived from persisted function OID (`catalog_oids::pg_proc_function_oid(FunctionDef.oid)`).
    - `pg_trigger.oid` is derived from persisted trigger OID (`catalog_oids::pg_trigger_oid(TriggerDef.oid)`).

## ORM helper functions (SQL execution)
- `src/sql/expr.rs`
  - `eval_function(...)` and `eval_function_join(...)` implement several pg_catalog helpers.
  - Behaviors relevant to ORM introspection:
    - `PG_GET_CONSTRAINTDEF(...)` returns `constraintdef` when available from the current row context (`pg_constraint.constraintdef`).
    - `PG_GET_EXPR(adbin, adrelid)` returns the first argument as text (so `pg_attrdef.adbin` can be a textual SQL expression).
    - `FORMAT_TYPE(oid, typmod)` maps common built-in OIDs; for unknown OIDs in JOIN context it returns the joined `typname` when present.

## `pg_get_indexdef(oid)` rewrite dependency
- `src/sql/sequences.rs`
  - Handles `PG_GET_INDEXDEF` rewrite during execution to support calls outside `pg_index` row context:
    - `lookup_indexdef_by_oid(store, txn, oid)` matches `pg_catalog.pg_index` by using `src/sql/catalog_oids.rs` (`pg_class_index_oid` / `pg_class_pk_index_oid`).
