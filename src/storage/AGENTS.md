# Storage Module

TiKV client wrapper and key encoding. ~5200 lines across 14 files.

## Files

| File | Lines | Purpose |
|------|-------|---------|
| `tikv_store/mod.rs` | 831 | TikvStore struct, cache, core transaction helpers |
| `tikv_store/database.rs` | 262 | Database CRUD operations |
| `tikv_store/schemas.rs` | 451 | Schema operations and cache invalidation |
| `tikv_store/tables.rs` | 716 | Table DDL, data operations, privileges |
| `tikv_store/sequences.rs` | 571 | Sequence operations and nextval/setval |
| `tikv_store/indexes.rs` | 487 | B-tree and GIN indexes, batch operations |
| `tikv_store/triggers.rs` | 418 | Trigger management and rename helpers |
| `tikv_store/functions.rs` | 190 | Function operations with caching |
| `tikv_store/views.rs` | 201 | Views and materialized views |
| `tikv_store/types.rs` | 65 | User-defined types |
| `tikv_store/procedures.rs` | 62 | Stored procedures |
| `tikv_store/extensions.rs` | 74 | Extension management |
| `encoding.rs` | 180 | Key encoding/decoding, serialization |
| `mod.rs` | 20 | Exports |

## Key Layout

```
_sys_next_table_id              → u64
_sys_schema_{table}             → TableSchema (bincode)
_sys_view_{name}                → SQL string
_sys_matview_{name}             → SQL string
_sys_proc_{name}                → SQL string
t_{table_id}_{pk_values}        → Row (bincode)
i_{table_id}_{idx_id}_{vals}    → pk (unique) or empty (non-unique)
```

## Where to Look

| Task | Location |
|------|----------|
| Add metadata type | `encoding.rs` - add prefix + encode/decode |
| Change row format | `encoding.rs` → `serialize_row()` |
| Database operations | `tikv_store/database.rs` - create, drop, rename |
| Schema operations | `tikv_store/schemas.rs` - create, drop, invalidate cache |
| Table DDL | `tikv_store/tables.rs` - create, drop, alter, truncate |
| Row CRUD | `tikv_store/tables.rs` - insert, upsert, delete, scan |
| Sequences | `tikv_store/sequences.rs` - nextval, setval, create |
| Indexes | `tikv_store/indexes.rs` - B-tree and GIN operations |
| Functions | `tikv_store/functions.rs` - create, drop, list |
| Triggers | `tikv_store/triggers.rs` - create, drop, rename helpers |
| Views | `tikv_store/views.rs` - views and materialized views |
| Fix scan issues | `tikv_store/mod.rs` - check SCAN_LIMIT constant |

## SCAN_LIMIT

```rust
const SCAN_LIMIT: u32 = u32::MAX;
```

Uses `u32::MAX` for unbounded scans. Fixed in tikv/client-rust#515 (saturating_add).

## Key Functions

### tikv_store/mod.rs (Core)
```
new_with_keyspace()          # Connect with tenant keyspace
begin()                      # Start pessimistic txn
autocommit_update_key()      # Retry wrapper for single-key ops
check_format_version()       # Version checking and migration
bootstrap_default_database() # Initialize default database
```

### tikv_store/tables.rs (Table Operations)
```
create_table()        # Create table with schema
get_schema()          # Load TableSchema
scan()                # Full table scan
insert() / upsert()   # Write row
delete_by_pk()        # Delete single row
truncate_table()      # Clear all table data
```

### tikv_store/indexes.rs (Index Operations)
```
create_index_entry()           # B-tree index entry
scan_index()                   # Scan index for PKs
create_gin_index_entries()     # GIN inverted index
scan_gin_index_intersection()  # GIN query
batch_get_rows()               # Batch row retrieval
```

### encoding.rs
```
encode_data_key()     # t_{table_id}_{pk}
encode_index_key()    # i_{table_id}_{idx}_{vals}[_{pk}]
encode_pk_values()    # Lexicographic PK encoding
serialize_row()       # Row → bytes
```

## Transaction Model

- Pessimistic transactions only (`begin()`)
- Optimistic available but unused (`begin_optimistic()`)
- All operations require `&mut Transaction`
