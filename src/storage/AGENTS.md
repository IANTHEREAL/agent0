# Storage Module

TiKV client wrapper, key encoding, and metadata management. ~7,000 lines across 24 files.

## Layout

```
src/storage/
├── mod.rs                          # Module exports, unique index error handling
├── kv_stats.rs                     # KV read statistics tracking (tokio task-local)
├── encoding/                       # Key encoding + serialization (~1,900 lines)
│   ├── mod.rs                      # Re-exports + keyspace constants
│   ├── data_keys.rs                # Row/index/GIN key encoding (database-scoped v2)
│   ├── metadata_keys.rs            # Schema/view/function/trigger/cron/worker keys
│   ├── serialization.rs            # Bincode with magic headers (DB9_SCHEMA_V1) + legacy fallback
│   ├── value_encoding.rs           # Memcomparable encoding for all value types
│   └── tests.rs                    # Encoding unit tests
└── tikv_store/                     # TiKV store implementation (~5,000 lines)
    ├── mod.rs                      # TikvStore struct, cache, core transaction helpers
    ├── database.rs                 # Database CRUD (create, drop, list, rename)
    ├── schemas.rs                  # Schema operations and cache invalidation
    ├── tables.rs                   # Table DDL, row CRUD, scan, privileges
    ├── sequences.rs                # Sequence operations (nextval, setval, create, drop)
    ├── indexes.rs                  # B-tree and GIN index operations
    ├── triggers.rs                 # Trigger management, rename helpers
    ├── functions.rs                # Function CRUD with caching
    ├── views.rs                    # Views and materialized views
    ├── types.rs                    # User-defined types (CREATE/DROP TYPE)
    ├── procedures.rs               # Stored procedures (PL/pgSQL)
    ├── extensions.rs               # Extension management
    ├── cron.rs                     # Cron job management (CronJob, CronRun, legacy fallback)
    ├── statistics.rs               # Table statistics persistence (TableStatistics)
    ├── worker.rs                   # Background worker task management (registry, queue, claims, results)
    └── migrations.rs               # Migration record tracking (keyspace-level)
```

## Key Layout

### Metadata Keys (keyspace-level)

```
_sys_next_table_id              → u64 (auto-increment counter)
_sys_next_database_id           → u64 (auto-increment counter)
_sys_format_version             → storage format version marker
_sys_schema_{table}             → TableSchema (bincode with magic header DB9_SCHEMA_V1\0)
_sys_view_{name}                → SQL string
_sys_matview_{name}             → SQL string
_sys_proc_{name}                → SQL string
_sys_dbname_{name}              → database name → ID mapping
_sys_dbid_{id}                  → database ID → name mapping
_sys_migration_*                → global migration tracking
```

### Data Keys (database-scoped v2)

```
d_{db_id}_t_{table_id}_{pk_values}                        → Row (bincode)
d_{db_id}_i_{table_id}_{idx_id}_{vals}[_{pk}]             → pk (unique) or empty (non-unique)
d_{db_id}_i_{table_id}_{idx_id}_gin_{token_hash}{SEP}{pk}  → GIN inverted index entry
```

### Worker Keys (system keyspace `_sys_worker`)

```
_worker_registry_               → task type registries
_worker_queue_                  → task queues (Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql)
_worker_claim_                  → worker claims (pessimistic lock)
_worker_bg_result_              → background job results
```

## Where to Look

| Task | Location |
|------|----------|
| Add metadata type | `encoding/metadata_keys.rs` — add prefix + encode/decode |
| Change row format | `encoding/serialization.rs` → `serialize_row()` |
| Data key encoding | `encoding/data_keys.rs` — row/index/GIN keys |
| Memcomparable encoding | `encoding/value_encoding.rs` — lexicographic sort-preserving |
| Database operations | `tikv_store/database.rs` — create, drop, rename |
| Schema operations | `tikv_store/schemas.rs` — create, drop, invalidate cache |
| Table DDL | `tikv_store/tables.rs` — create, drop, alter, truncate |
| Row CRUD | `tikv_store/tables.rs` — insert, upsert, delete, scan |
| Sequences | `tikv_store/sequences.rs` — nextval, setval, create |
| Indexes | `tikv_store/indexes.rs` — B-tree and GIN operations |
| Functions | `tikv_store/functions.rs` — create, drop, list |
| Triggers | `tikv_store/triggers.rs` — create, drop, rename helpers |
| Views | `tikv_store/views.rs` — views and materialized views |
| Cron jobs | `tikv_store/cron.rs` — CronJob CRUD, legacy deserialization |
| Table statistics | `tikv_store/statistics.rs` — persistence for optimizer |
| Worker tasks | `tikv_store/worker.rs` — registry, queue, claims, results |
| Migrations | `tikv_store/migrations.rs` — migration record tracking |
| KV read stats | `kv_stats.rs` — tokio task-local read tracking |
| Fix scan issues | `tikv_store/mod.rs` — check SCAN_LIMIT constant |

## Constants

```rust
const SCAN_LIMIT: u32 = u32::MAX;          // Unbounded scans
const BATCH_GET_CHUNK_SIZE: usize = 256;    // Batch get chunk size
const TABLE_SCAN_BATCH_SIZE: usize = 1024;  // gRPC message size limit
```

## Serialization

- **Schema format**: Bincode with magic header `DB9_SCHEMA_V1\0`
- **Legacy fallback**: Schemas with old IndexDef format (pre-ce73a8a) fall back gracefully
- **Cron legacy**: CronJobLegacy deserialization path for old format
- **Sunset policy**: 2026-12-31 for legacy formats

## Memcomparable Encoding

- `NULL_TAG = 0x00`, `NOT_NULL_TAG = 0x01`
- Preserves lexicographic sort order for TiKV range scans
- Supports: bool, int32, int64, float64, text, bytes, timestamp, interval, time, date, uuid, array, vector, json

## KV Statistics

Tokio task-local tracking via `KV_READ_STATS`:
- `table_scan_pairs` — rows read in table scans
- `index_scan_pairs` — rows read in index scans
- `batch_get_keys` — keys fetched in batch gets

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

### tikv_store/worker.rs (Worker Operations)
```
register_task_type()     # Register a task type in the registry
enqueue_task()           # Add task to the queue
claim_task()             # Pessimistic lock claim
complete_task()          # Mark task as completed
store_bg_result()        # Store background job result
```

## Transaction Model

- Pessimistic transactions only (`begin()`)
- Optimistic available but unused (`begin_optimistic()`)
- All operations require `&mut Transaction`
- Autocommit: automatic retry (10 attempts, exponential backoff) on TiKV conflict errors
- Explicit transactions: no automatic retry (application must handle conflicts)
