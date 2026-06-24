# Design Document Index

This directory contains design notes, implementation plans, review records, and gap analyses for db9-server.

`docs/design/**` is **not** the source of truth for current behavior. The authoritative current contracts live under `docs/sot/**`.

Each design document should carry a status banner near the top:
- `Active`: current design guidance that still matches the intended architecture direction
- `Draft`: proposal or incomplete design; not a shipped-behavior contract
- `Historical`: useful record of a past design or implementation plan; not current architecture
- `Superseded`: explicitly replaced by a newer design or shipped architecture

Tracked design docs are linted for an explicit status classification so that readers and tooling can distinguish live guidance from archived context.
Only `Active` design docs are required to keep `src/**` references live under `doc_lint.py`; `Draft`, `Historical`, and `Superseded` docs may contain forward-looking or archived paths.

When a design doc conflicts with current code or SoT:
- SoT wins
- code and tests decide current implementation truth
- the design doc should be downgraded to `Historical` or `Superseded` instead of being mistaken for SSOT

The migration-priority list below remains useful as an index, but readers must check each document's status banner before treating it as current guidance.

Each document may include:
- **Design**: MVP scope, semantics, storage/execution/protocol change points, performance considerations
- **Test plan**: Unit tests, SQL integration tests (`./run_tests.sh`), ORM test recommendations

## P0 (Critical for migration to work)

- `docs/design/01_savepoints.md`: SAVEPOINT / ROLLBACK TO / RELEASE
- `docs/design/02_alter_table_migration.md`: ALTER TABLE common subset (rename/drop constraint/alter column)
- `docs/design/03_user_defined_types_enum_composite.md`: CREATE TYPE AS ENUM (high frequency in migrations)
- `docs/design/04_sequences.md`: SEQUENCE + nextval/currval/setval + SERIAL compatibility
- `docs/design/05_schemas_search_path.md`: schema + search_path
- `docs/design/07_dollar_quoted_strings.md`: `$$...$$` / `$tag$...$tag$`
- `docs/design/08_numeric_decimal.md`: NUMERIC/DECIMAL exact decimals
- `docs/design/09_date_type.md`: DATE type
- `docs/design/11_system_catalog_coverage.md`: pg_catalog / information_schema coverage

## P1 (Migration ecosystem / toolchain essentials)

- `docs/design/06_copy_to_and_options.md`: COPY TO / options (pg_dump/restore)
- `docs/design/10_pgwire_array_vector_oids.md`: Array/Vector pgwire OIDs
- `docs/design/12_functions_and_triggers.md`: Allow CREATE FUNCTION/TRIGGER to land (store definitions first)
- `docs/design/14_index_features.md`: partial/expression/GIN/GiST (DDL compatibility first)

## P2/P3 (Not hard migration dependencies, enhance as needed)

- `docs/design/17_extensions_framework_http.md`: Extension framework + HTTP extension (Supabase style)
- `docs/design/28_embedding_extension_pg_parity_contract.md`: Embedding extension compatibility contract (function visibility, SQLSTATE boundaries, intentional-divergence governance)
- `docs/design/16_set_returning_functions.md`: generate_series / unnest (FROM clause)
- `docs/design/15_explain_analyze.md`: EXPLAIN ANALYZE
- `docs/design/13_listen_notify.md`: LISTEN/NOTIFY

## Performance Optimization

- `docs/design/db9_optimization.md`: next-step execution plan and agent-team backlog for performance optimization milestones `M0` to `M7`
- `docs/design/performance-optimization-design-template.md`: template for `M1` to `M7` optimization workstreams
- `docs/design/18_hash_join.md`: Hash Join implementation (equi-join optimization)
  - Design overview: [18_hash_join.md](./18_hash_join.md)
  - Detailed implementation plan: [hash_join_implementation_plan.md](./hash_join_implementation_plan.md)

## Vector Search

- `docs/design/27_hnsw_vector_index.md`: HNSW vector index (approximate nearest neighbor search)
  - historical original implementation design; current architecture no longer uses the process-level cache described there
  - PR: [#1241](https://github.com/c4pt0r/db9-server/pull/1241), Issue: [#1220](https://github.com/c4pt0r/db9-server/issues/1220)

## Worker Architecture

- `docs/design/34_worker_kernel_and_protocols.md`: target worker kernel and
  outer protocol design (v2) — single load-bearing inventory row, leased
  CAS claims, shared observation walk, database liveness fence, trigger
  outbox, and recovery contracts.

## Correctness and Lifecycle

- `docs/design/36_transaction_lifecycle_correctness.md`: transaction contract,
  database lifecycle fencing, stale-write prevention, and delete/drop semantics
  based on TiDB and CockroachDB design patterns.
