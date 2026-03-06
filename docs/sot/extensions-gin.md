# extensions-gin — HTTP/embedding extensions, GIN indexes, and FTS

## Scope
- HTTP extension framework and outbound-request security model.
- Embedding extension semantics and visibility/permission rules.
- GIN-like inverted indexes used for JSON/ARRAY/FTS predicates.
- Extension-owned configuration surfaces (defined once in `./ops-config.md`).

## Non-goals
- General SQL engine behavior outside these extension/index surfaces (authoritative: `./sql-engine.md`).
- Base RBAC policy definitions (authoritative: `./auth-rbac.md`).
- Generic storage-format rules outside extension-owned encodings (authoritative: `./storage-format.md`).

## External Contracts
- **[Stable] Extension installation is superuser-gated**
  - `CREATE EXTENSION <name>` and `DROP EXTENSION <name>` require superuser privileges.
  - Evidence: `src/sql/executor/extensions.rs`, `tests/86_extensions_framework_http.sql`.

- **[Stable] HTTP table functions are superuser-only**
  - `extensions.http_*` table functions are denied for non-superusers.
  - Evidence: `src/extensions/http.rs`, `src/extensions/context.rs`, `tests/88_http_permission.sql`.

- **[Stable] SSRF posture is secure-by-default**
  - HTTP extension execution rejects loopback/private/local targets, URL userinfo, and non-default ports.
  - Plain `http://` is rejected unless `DB9_HTTP_ALLOW_INSECURE=1`.
  - Evidence: `src/extensions/http.rs`, `tests/87_http_ssrf_protection.sql`.

- **[Stable] Embedding extension visibility and permissions**
  - `embedding()` and `extensions.embedding_usage()` require the embedding extension to be installed/visible and remain superuser-gated.
  - Visibility precedence is:
    1. in-transaction extension DDL delta,
    2. explicit-transaction snapshot,
    3. latest committed state for autocommit statements.
  - Visibility failure uses function-not-found semantics (`42883`), not capability error `0A000`.
  - Evidence: `src/sql/expr/functions/embedding.rs`, `src/extensions/embedding.rs`, `src/sql/session/mod.rs`, `src/sql/session/transaction.rs`.

- **[Stable] Embedding model is pinned to `text-embedding-v4`**
  - Session/env model values canonicalize to `text-embedding-v4`.
  - Runtime validation is reserved for value-domain checks after successful signature resolution.
  - Evidence: `src/config.rs`, `src/sql/session/settings.rs`, `src/sql/expr/functions/embedding.rs`.

- **[Stable] GIN index lifecycle**
  - db9 supports a restricted `USING gin` lifecycle for JSON/JSONB, ARRAY, and `TSVECTOR` use cases.
  - Backfill and maintenance are implemented during CREATE INDEX and DML writes.
  - Evidence: `src/sql/gin.rs`, `src/sql/ddl/create_index.rs`, `src/sql/dml/insert.rs`, `src/sql/dml/update.rs`, `src/storage/tikv_store/indexes.rs`, `tests/94_gin_index_query.sql`, `tests/131_gin_fts.sql`.

- **[Stable] GIN planner/runtime access path**
  - The planner can emit `ScanType::GinIndexScan` for supported `@@`, `@>`, and `&&` predicates.
  - Runtime evaluation uses posting-list set operations plus recheck filtering for correctness.
  - Pure-negative quals do not use the GIN access path.
  - Evidence: `src/sql/planner/gin_predicate.rs`, `src/sql/planner/index_selection.rs`, `src/sql/optimizer/build/scan.rs`, `src/sql/operators/gin_scan.rs`, `src/storage/tikv_store/indexes.rs`, `tests/236_gin_index_scan.sql`, `tests/218_gin_correctness.sql`.

- **[Experimental] FTS surface is MVP-level**
  - `to_tsvector`, `plainto_tsquery`, `to_tsquery`, `@@`, and `ts_rank` are implemented for basic matching/ranking, not full PostgreSQL FTS parity.
  - Evidence: `src/sql/fts.rs`, `tests/130_fts.sql`.

## Security Considerations
- These surfaces are outbound-network and/or index-maintenance code paths and are security-sensitive.
- Superuser-only execution, HTTPS-by-default, SSRF blocking, and request caps are part of the contract.
- Evidence: `src/extensions/http.rs`, `src/extensions/context.rs`, `tests/87_http_ssrf_protection.sql`, `tests/88_http_permission.sql`.

## Data Model & Invariants
- Installed extensions are persisted per database and gate runtime visibility.
- Embedding usage accounting is persisted independently of plan cache behavior.
- GIN postings are deterministic token-hash-derived index entries; final operator recheck prevents false negatives from tokenization shortcuts.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`.

## Entrypoints
- `src/extensions/http.rs`
- `src/extensions/embedding.rs`
- `src/extensions/context.rs`
- `src/sql/executor/extensions.rs`
- `src/sql/expr/functions/embedding.rs`
- `src/sql/gin.rs`
- `src/sql/fts.rs`
- `src/sql/ddl/create_index.rs`
- `src/sql/dml/insert.rs`
- `src/sql/dml/update.rs`
- `src/sql/planner/gin_predicate.rs`
- `src/sql/planner/index_selection.rs`
- `src/sql/optimizer/build/scan.rs`
- `src/sql/operators/gin_scan.rs`
- `src/storage/encoding/mod.rs`
- `src/storage/tikv_store/indexes.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/ci.yml/integration-tests`
- Local reproduce (typical):
  - `./run_tests.sh`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/86_extensions_framework_http.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/87_http_ssrf_protection.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/94_gin_index_query.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/130_fts.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/131_gin_fts.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/218_gin_correctness.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/236_gin_index_scan.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/272_embedding_extension_concurrent_visibility.sql`

## Change Management
- Any change to HTTP/embedding security posture, extension visibility rules, or GIN planner/runtime behavior MUST update this document and the corresponding `docs/sot/modules.yaml` entry.
- Intentional divergence from PostgreSQL behavior requires explicit SoT rationale and governance linkage.
- Reference: https://github.com/c4pt0r/db9/issues/368
