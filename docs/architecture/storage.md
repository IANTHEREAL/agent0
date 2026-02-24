# Storage Layer Architecture

> **Contracts**: See [docs/sot/storage-format.md](../sot/storage-format.md) for normative specifications.
> **Navigation**: See [src/storage/AGENTS.md](../../src/storage/AGENTS.md) for detailed code paths and symbols.

## Key Layout

All keys are keyspace-prefixed by TiKV for multi-tenant isolation. Storage format v2 uses database-scoped keys.

### Metadata Keys (keyspace-level)

| Key Pattern | Content |
|-------------|---------|
| `_sys_next_table_id` | Auto-increment counter for table IDs |
| `_sys_next_database_id` | Auto-increment counter for database IDs |
| `_sys_format_version` | Storage format version marker |
| `_sys_schema_{table}` | `TableSchema` (bincode with magic header `DB9_SCHEMA_V1\0`) |
| `_sys_view_{name}` | View SQL definition |
| `_sys_matview_{name}` | Materialized view SQL definition |
| `_sys_proc_{name}` | Stored procedure SQL definition |
| `_sys_dbname_{name}` | Database name → ID mapping |
| `_sys_dbid_{id}` | Database ID → name mapping |
| `_sys_migration_*` | Global migration tracking |

### Data Keys (database-scoped v2)

| Key Pattern | Content |
|-------------|---------|
| `d_{db_id}_t_{table_id}_{pk_values}` | Data row (bincode serialized) |
| `d_{db_id}_i_{table_id}_{idx_id}_{vals}[_{pk}]` | Index entry (unique: pk, non-unique: empty) |
| `d_{db_id}_i_{table_id}_{idx_id}_gin_{token_hash}{SEP}{pk}` | GIN index entry |

### Worker Keys (system keyspace `_sys_worker`)

| Key Pattern | Content |
|-------------|---------|
| `_worker_registry_` | Task type registries |
| `_worker_queue_` | Task queues (Cron, AsyncTrigger, AutoAnalyze, BgDdl, BgSql) |
| `_worker_claim_` | Worker claims (pessimistic lock prevents duplicates) |
| `_worker_bg_result_` | Background job results |

## Transaction Model

- Pessimistic transactions only (`TikvStore::begin()`)
- Snapshot Isolation (prevents dirty reads, non-repeatable reads, write-skew for indexed columns)
- Autocommit: automatic retry (10 attempts, exponential backoff) on TiKV conflict errors
- Explicit transactions: no automatic retry (application must handle conflicts)

> **Contract**: See [docs/sot/storage-format.md](../sot/storage-format.md) for transaction primitives and keyspace isolation invariants.
