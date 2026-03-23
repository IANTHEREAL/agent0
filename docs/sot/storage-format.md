# storage-format — Persistent layout, serialization, and isolation invariants

## Scope
- Storage-format versioning and keyspace/database isolation invariants.
- High-level persistent key families owned by db9.
- Serialization contracts for schemas, rows, and other core payloads.
- TiKV transaction primitives used by the storage layer.

## Non-goals
- SQL planning/execution semantics (authoritative: `./sql-engine.md`).
- pgwire protocol behavior (authoritative: `./protocol-pgwire.md`).
- RBAC policy semantics (authoritative: `./auth-rbac.md`).
- Exhaustive prose duplication of every encoder/helper in `src/storage/encoding/**`.

## External Contracts
- **[Stable] Storage format v2 marker**
  - Each db9 TiKV keyspace MUST store `_sys_format_version = u32(2)` in big-endian form.
  - Keyspaces containing v1-era data MUST NOT be auto-upgraded silently.
  - Evidence: `src/storage/tikv_store/mod.rs`, `src/storage/encoding/mod.rs`.

- **[Stable] Tenant isolation uses TiKV keyspace selection**
  - Persistent data isolation between tenants is implemented by constructing `TikvStore`/`TransactionClient` with the selected TiKV keyspace.
  - Evidence: `src/storage/tikv_store/mod.rs` (`new_with_keyspace`), `src/pool.rs`.

- **[Stable] Database-scoped prefixing inside a keyspace**
  - Within a keyspace, database-local data is partitioned by the binary prefix `d_{db_id:8bytes}_`.
  - Database scans/deletes MUST use ranges derived from that prefix.
  - Evidence: `src/storage/encoding/mod.rs`, `src/storage/tikv_store/mod.rs`.

- **[Stable] High-level key families (non-exhaustive inventory)**
  - Keyspace-level metadata:
    - `_sys_next_database_id`,
    - `_sys_dbname_<name>`,
    - `_sys_dbid_<u64be>`,
    - `_sys_format_version`,
    - system-worker metadata in the configured worker system keyspace,
    - `_gc_instance_<gc_instance_id>` records in the worker system keyspace, encoded as `has_min:u8 + min_start_ts:u64be + updated_at_version:u64be`; graceful shutdown deletes the local row and crash leftovers are reaped once they age past the GC liveness window.
  - Database-local metadata and data:
    - schema/catalog metadata (`sys_schema_*`, `sys_view_*`, `sys_seqdef_*`, `sys_ext_*`, comments, routines, types, cron metadata, stats),
    - table rows `t_{table_id}_...`,
    - secondary indexes `i_{table_id}_{index_id}_...`,
    - GIN postings `i_{table_id}_{index_id}_gin_...`,
    - HNSW base graph/meta/delta families under `d_{db_id}_hnsw_{table_id}_{index_id}_...`.
  - The implementation modules, not this prose page, are the exhaustive encoder inventory.
  - Evidence: `src/storage/encoding/mod.rs`, `src/storage/encoding/metadata_keys.rs`, `src/storage/encoding/data_keys.rs`, `src/sql/hnsw/storage.rs`.

- **[Stable] Serialization contracts**
  - `TableSchema` is serialized as `DB9_SCHEMA_V2` + MessagePack named-map payload.
  - `Row` is serialized with bincode.
  - Function definitions use a magic-header + bincode payload.
  - Schema deserialization rejects legacy V1 payloads and missing schema headers.
  - Evidence: `src/storage/encoding/serialization.rs`.

- **[Stable] Transaction primitives**
  - Primary storage transactions are pessimistic by default.
  - Some operations that intentionally survive caller rollback (for example sequences) use dedicated autocommitted helper logic with bounded retry.
  - Evidence: `src/storage/tikv_store/mod.rs`, `src/sql/sequences.rs`.

## Data Model & Invariants
- **Tenant isolation invariant**: persistent data MUST NOT cross TiKV keyspaces.
- **Database partitioning invariant**: user tables, indexes, and database-local metadata MUST remain under the selected database prefix.
- **Ordering invariant**: scan keys and memcomparable index encodings MUST preserve ordered range-scan behavior.
- **Serialization invariant**: storage readers MUST reject unsupported schema-format versions instead of silently mis-decoding them.
- **Value size invariant**: no single KV value written via `txn_put` should exceed TiKV's `raft-entry-max-size` (default 8 MB). Features that persist growable data (indexes, statistics, large rows) MUST use per-row entries or fixed-size pages, not monolithic blobs. See `docs/design/30_tikv_value_size_design_lessons.md` for rationale and audit results.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`.

## Entrypoints
- `src/storage/encoding/mod.rs`
- `src/storage/encoding/serialization.rs`
- `src/storage/tikv_store/mod.rs`
- `src/sql/hnsw/storage.rs`
- `src/pool.rs`
- `src/txn/mod.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/ci.yml/regression-gate`, `ci:.github/workflows/ci.yml/integration-tests`
- Local reproduce (typical):
  - `cargo test`
  - `./scripts/regression_gate.sh`
  - `./run_tests.sh`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/100_create_database.sql`

## Change Management
- Any change to format-version handling, key families, serialization formats, or isolation invariants MUST update this document and the corresponding `docs/sot/modules.yaml` entry.
- Breaking persistent-format changes require DR/ADR per #368 rules.
- Reference: https://github.com/c4pt0r/db9/issues/368
