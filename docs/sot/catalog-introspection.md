# catalog-introspection — `information_schema` + `pg_catalog` virtual catalogs

## Scope
- Coverage and semantics of db9 virtual `information_schema` and `pg_catalog` tables.
- OID/type catalog behavior used by clients and ORMs for introspection.
- Virtual-table registry and executor plumbing for these catalog surfaces.

## Non-goals
- General planner/executor behavior outside introspection queries (authoritative: `./sql-engine.md`).
- Storage encoding details (authoritative: `./storage-format.md`).
- pgwire protocol framing (authoritative: `./protocol-pgwire.md`).

## External Contracts
- **[Stable] Virtual catalog resolution**
  - Registered virtual tables are resolved under `information_schema.<name>` and `pg_catalog.<name>`, plus the limited unqualified cases explicitly registered by the catalog layer.
  - Evidence: `src/sql/information_schema.rs`, `src/sql/executor/table_utils/mod.rs`, `src/sql/catalog/mod.rs`.

- **[Stable] Registry is the authoritative coverage inventory**
  - `CatalogRegistry::new()` is the authoritative implementation inventory for currently registered catalog tables.
  - The matrix below is intentionally minimum and non-exhaustive; it highlights key ORM-facing surfaces, not every registered table.
  - Evidence: `src/sql/catalog/mod.rs`, `src/sql/catalog/virtual_tables.rs`.

- **[Stable] Implemented vs stubbed minimum matrix**

| Table | Status | Evidence | Notes |
|---|---|---|---|
| `pg_catalog.pg_class` | Implemented (partial columns) | `src/sql/catalog/pg_class.rs` | Includes tables, indexes, sequences, and views; many fields are placeholders. |
| `pg_catalog.pg_type` | Implemented (partial columns) | `src/sql/catalog/pg_type.rs` | Built-in types are fixed; user-defined types are added from metadata; several fields are synthetic/defaulted. |
| `pg_catalog.pg_attribute` | Implemented (partial columns) | `src/sql/catalog/pg_attribute.rs` | User-table column coverage exists; several fields remain constant/defaulted. |
| `pg_catalog.pg_namespace` | Implemented (partial columns) | `src/sql/catalog/pg_namespace.rs` | Schema rows exist; ownership and ACL fidelity remain partial. |
| `pg_catalog.pg_proc` | Implemented (partial columns) | `src/sql/catalog/pg_proc.rs` | Built-ins plus user-defined functions; many PG columns remain approximate. |
| `information_schema.tables` | Implemented (partial columns) | `src/sql/catalog/tables.rs` | Base tables and views are exposed; many information-schema columns are NULL/defaulted. |

- **[Stable] Synthetic OIDs for user-defined objects**
  - db9 returns deterministic synthetic OIDs for user-defined tables, indexes, sequences, views, functions, triggers, and attrdefs based on stored object IDs.
  - Evidence: `src/sql/catalog_oids.rs`, `src/sql/catalog/pg_class.rs`, `src/sql/catalog/pg_proc.rs`.

- **[Experimental] Predicate-derived filter hints are still limited**
  - Virtual catalog scan helpers accept filter-hint plumbing, but predicate extraction remains partial and many scans still enumerate broadly.
  - Evidence: `src/sql/information_schema.rs`, `src/sql/executor/table_utils/mod.rs`.

## Configuration
This module defines no runtime config keys.

If operator-facing knobs are ever added here, they MUST be defined exactly once in `./ops-config.md`.

## Entrypoints
- `src/sql/information_schema.rs`
- `src/sql/catalog/mod.rs`
- `src/sql/catalog/virtual_tables.rs`
- `src/sql/catalog/pg_class.rs`
- `src/sql/catalog/pg_attribute.rs`
- `src/sql/catalog/pg_type.rs`
- `src/sql/catalog/pg_tables.rs`
- `src/sql/catalog_oids.rs`
- `src/sql/executor/table_utils/mod.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/ci.yml/integration-tests`
- Local reproduce (typical):
  - `./run_tests.sh`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/34_information_schema.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/101_server_introspection.sql`

## Change Management
- Any change to registered virtual tables, OID synthesis, or ORM-facing introspection behavior MUST update this document and the corresponding `docs/sot/modules.yaml` entry.
- Breaking catalog-surface changes require DR/ADR per #368 rules.
- Reference: https://github.com/c4pt0r/db9/issues/368
