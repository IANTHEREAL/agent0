# storage-format — Persistent key layout, format versioning, and isolation invariants

## Scope
- Key layout / encoding rules (storage format v2 prefixes, `_sys_*`, `t_*/i_*`).
- TiKV store abstraction (`TikvStore`) and on-disk format version enforcement.
- Transaction primitives (pessimistic by default; limited optimistic/autocommit helpers).
- Keyspace routing and **tenant/database isolation invariants**.

## Non-goals
- SQL query semantics and planner/executor behavior (authoritative: `./sql-engine.md`).
- pgwire protocol and startup messaging (authoritative: `./protocol-pgwire.md`).
- Auth/RBAC semantics and policy defaults (authoritative: `./auth-rbac.md`).
- Extension semantics (authoritative: `./extensions-gin.md` for extension-owned surfaces).

## External Contracts
- **[Stable] Storage format v2 marker and compatibility boundary**
  - Each TiKV keyspace used by pg-tikv MUST contain a format marker `_sys_format_version` set to big-endian `u32(2)`.
  - If a keyspace contains `_sys_format_version != 2`, startup MUST fail with an incompatibility error.
  - If `_sys_format_version` is missing but v1-era table keys exist, startup MUST refuse to auto-upgrade and MUST require re-initialization/migration.
  - Evidence: `src/storage/tikv_store.rs` (`TikvStore::check_format_version`), `src/storage/encoding.rs` (`encode_format_version_key`, `encode_next_table_id_key`, `encode_schema_prefix`).

- **[Stable] Tenant isolation via TiKV keyspace**
  - Persistent data for different tenants MUST be isolated by TiKV keyspace.
  - The storage client MUST be created with `Config::with_keyspace(<tenant>)` when a tenant keyspace is selected.
  - Evidence: `src/storage/tikv_store.rs` (`new_with_keyspace`), `src/pool.rs` (`TikvClientPool` keyspace selection), `src/main.rs` (`create_keyspace`).
  - Cross-link: tenant keyspace selection inputs originate from username parsing in `./protocol-pgwire.md`.

- **[Stable] Database-scoped prefixing (format v2)**
  - Within a keyspace, all per-database metadata and user data MUST be partitioned by a fixed binary prefix: `d_{db_id:8bytes}_` (big-endian `u64`).
  - Database range deletes/scans MUST use lexicographic ranges derived from this prefix.
  - Evidence: `src/storage/encoding.rs` (`encode_database_data_prefix`, `encode_database_data_range`, `encode_*_v2` helpers), usages in `src/storage/tikv_store.rs`.

- **[Stable] Key layout (high-level)**
  - Keyspace-level metadata keys (format v2) include:
    - `_sys_next_database_id` (allocator),
    - `_sys_dbname_<name>` (database name → ID),
    - `_sys_dbid_<u64be>` (database ID → definition).
  - Database-scoped keys (format v2) use the `d_{db_id}_` prefix and include:
    - `sys_schema_*` / `sys_schemadef_*` / `sys_view_*` / `sys_seqdef_*` / `sys_ext_*` / `sys_comment_*` (metadata),
    - `t_{table_id}_...` (table rows),
    - `i_{table_id}_{index_id}_...` (secondary indexes, including `..._gin_<token_hash>...` postings).
  - Evidence: `src/storage/encoding.rs` (module-level key layout comment + `encode_*_v2` functions).

- **[Stable] Value/row/schema serialization**
  - Table schemas and rows MUST be persisted as serialized blobs and MUST remain backward-compatible on read.
  - Current behavior:
    - `TableSchema` and `Row` are stored using `bincode` serialization.
    - Some stored payloads are versioned with a magic header; deserializers MUST accept legacy unversioned payloads.
  - Evidence: `src/storage/encoding.rs` (`serialize_schema`/`deserialize_schema`, `serialize_row`/`deserialize_row`, versioned payload helpers).

- **[Stable] Transaction primitives and non-transactional emulation (sequences)**
  - The primary transaction mode MUST be pessimistic transactions.
  - Certain operations that must survive caller rollbacks (e.g., sequences) MAY use an internal auto-committed update helper with bounded retry.
  - Evidence: `src/storage/tikv_store.rs` (`begin` pessimistic, `autocommit_update_key` comment + retry loop), `src/sql/sequences.rs` (sequence call sites).

## Data Model & Invariants
- **Isolation invariant (MUST)**: tenant data MUST NOT be accessible across TiKV keyspaces.
  - Evidence: `src/storage/tikv_store.rs` (`Config::with_keyspace`), `src/pool.rs` (`get_client` by keyspace).
- **Database partitioning invariant (MUST)**: user tables, indexes, and per-database metadata MUST remain under the database prefix for the selected database ID.
  - Evidence: `src/storage/encoding.rs` (`encode_*_v2`), database operations in `src/storage/tikv_store.rs`.
- **Ordering invariant (SHOULD)**: keys used for scans MUST preserve lexicographic order for range scans; index value encodings use memcomparable encoding.
  - Evidence: `src/storage/encoding.rs` (`encode_value_memcomparable`, key range helpers).
- **Extension-specific layouts**: storage encodings that exist primarily for extension semantics (e.g., GIN postings) are defined here, while query semantics live in `./extensions-gin.md`.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`:
- `PD_ENDPOINTS`
- `PG_KEYSPACE`

## Entrypoints
- `src/storage/encoding.rs`
- `src/storage/tikv_store.rs` (`TikvStore`)
- `src/pool.rs` (`TikvClientPool`)
- `src/txn/mod.rs`
- `src/main.rs` (`create_keyspace`)

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/regression-gate.yml/regression-gate`, `ci:.github/workflows/orm-tests.yml/test`
- Local reproduce (typical):
  - `cargo test`
  - `./scripts/regression_gate.sh`
  - `./run_tests.sh`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/100_create_database.sql`

## Change Management
- Any change to key layouts, serialization formats, format version handling, keyspace/database isolation invariants, or transaction primitives MUST update this document and the corresponding module entries in `docs/sot/modules.yaml`.
- Breaking changes to persistent format or invariants require DR/ADR per #368 rules (impact surface + migration + rollback + verification updates).
- Reference: https://github.com/c4pt0r/tipg/issues/368

