# catalog-introspection — `information_schema` + `pg_catalog` (virtual catalogs)

## Scope
- Coverage and semantics of the virtual `information_schema` and `pg_catalog` tables used for client/ORM introspection.
- OID/type catalog behavior and server introspection contract as implemented on the baseline commit.

## Non-goals
- General SQL planner/executor semantics outside introspection queries (authoritative: `./sql-engine.md`).
- Storage encoding details (authoritative: `./storage-format.md`).
- pgwire protocol framing (authoritative: `./protocol-pgwire.md`).

## External Contracts
- **[Stable] Virtual catalog resolution**
  - The server MUST resolve registered virtual tables when referenced as:
    - `information_schema.<name>` / `pg_catalog.<name>`, or
    - certain unqualified names (where a virtual schema is registered).
  - Evidence: `src/sql/information_schema.rs` (`get_information_schema_schema`, `get_information_schema_data_filtered`), `src/sql/executor/table_utils.rs` (`Executor::get_table_data_filtered` table resolution path), `src/sql/catalog/mod.rs` (`CatalogRegistry::new` registrations).

- **[Stable] Implemented vs stubbed (minimum matrix)**

| Table | Status | Evidence (entrypoint) | Notes / gaps |
|---|---|---|---|
| `pg_catalog.pg_class` | Implemented (partial fields) | `src/sql/catalog/pg_class.rs` | Emits rows for user tables + indexes + sequences + views; many columns are hardcoded placeholders (`relowner`, stats fields). |
| `pg_catalog.pg_type` | Implemented (partial fields) | `src/sql/catalog/pg_type.rs` | Built-in types are a fixed list; user-defined types are added from store metadata; many fields are placeholders. |
| `pg_catalog.pg_attribute` | Implemented (partial fields) | `src/sql/catalog/pg_attribute.rs` | Emits rows for user-table columns; type OID mapping is partial; several fields are constant/defaulted (`atttypmod=-1`, generated/identity empty). |
| `pg_catalog.pg_namespace` | Implemented (partial fields) | `src/sql/catalog/pg_namespace.rs` | Emits rows for schemas from store metadata; owner is currently hardcoded. |
| `pg_catalog.pg_proc` | Implemented (partial fields) | `src/sql/catalog/pg_proc.rs` | Includes a fixed built-in function list + user-defined functions; extension functions appear when enabled. |
| `information_schema.tables` | Implemented (partial fields) | `src/sql/catalog/tables.rs` | Emits base tables from store metadata + views; many columns are NULL per current implementation. |

- **[Stable] Synthetic OIDs for user-defined objects**
  - The server MUST return deterministic synthetic OIDs for user-defined tables/indexes/sequences/views/functions/triggers/attrdefs based on the object IDs in metadata.
  - Evidence: `src/sql/catalog_oids.rs` (OID bases + packing rules), usage in `src/sql/catalog/pg_class.rs`, `src/sql/catalog/pg_proc.rs`, etc.

- **[Experimental] Filter hints for virtual catalogs (plumbing only)**
  - Virtual catalog scan helpers accept an optional `VirtualTableFilter` (`table_name`, `table_schema`) that can be used to reduce the enumerated object set.
  - On the baseline commit, the engine does not extract these hints from query predicates yet; the default filter results in a full scan.
  - Evidence: `src/sql/information_schema.rs` (`VirtualTableFilter`, `get_information_schema_data_filtered`), `src/sql/executor/table_utils.rs` (`Executor::get_table_data_filtered` plumbs `filter`).

## Configuration
This module defines no runtime config keys.

If operational knobs are needed in the future, define keys exactly once in `./ops-config.md` and reference them here.

## Entrypoints
- `src/sql/information_schema.rs`
- `src/sql/catalog/mod.rs` (registry + virtual table trait)
- `src/sql/catalog/pg_attribute.rs`
- `src/sql/catalog/pg_type.rs`
- `src/sql/catalog/pg_tables.rs`
- `src/sql/catalog_oids.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/orm-tests.yml/test`
- Local reproduce (typical):
  - `./run_tests.sh`
  - `python3 scripts/integration_test.py --dsn \"$PG_DSN\" tests/34_information_schema.sql`
  - `python3 scripts/integration_test.py --dsn \"$PG_DSN\" tests/101_server_introspection.sql`

## Change Management
- Any change to virtual catalog coverage (add/remove table/column), OID packing rules, or ORM-facing introspection semantics MUST update this document and the corresponding module entries in `docs/sot/modules.yaml`.
- Breaking changes to catalog surface require DR/ADR per #368.
- Reference: https://github.com/c4pt0r/db9/issues/368
