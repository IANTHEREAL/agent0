# Storage Module

TiKV client wrapper and key encoding. ~800 lines across 3 files.

## Files

| File | Lines | Purpose |
|------|-------|---------|
| `tikv_store.rs` | 580 | TiKV client, CRUD, schema management |
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
| Add TiKV operation | `tikv_store.rs` |
| Fix scan issues | `tikv_store.rs` - check SCAN_LIMIT |

## Critical: SCAN_LIMIT

```rust
const SCAN_LIMIT: u32 = i32::MAX as u32;  // NOT u32::MAX!
```

**Why**: TiKV client adds deleted entry count to limit. `u32::MAX + N` overflows.

## Key Functions

### tikv_store.rs
```
new_with_keyspace()   # Connect with tenant keyspace
begin()               # Start pessimistic txn
scan()                # Full table scan
get_schema()          # Load TableSchema
insert() / upsert()   # Write row
delete_by_pk()        # Delete single row
create_index_entry()  # Secondary index
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
