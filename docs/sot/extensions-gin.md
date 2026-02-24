# extensions-gin — HTTP extension + GIN-like indexes + full-text search (FTS)

## Scope
- HTTP extension framework and table functions (`extensions.http_*`) and their security model (SSRF protection, limits, superuser boundary).
- GIN-like inverted index storage/maintenance semantics used for:
  - JSON/JSONB containment (`@>`),
  - ARRAY containment (`@>`),
  - FTS match (`@@`) via `TSVECTOR`/`TSQUERY`.
- Planner access-path note: in the current single-path engine, `GIN` scan selection is
  intentionally disabled until a real runtime GIN operator is implemented.
- Extension-owned configuration knobs and feature flags (defined once; see `./ops-config.md`).

## Non-goals
- Core SQL planner/executor semantics outside extension/index surfaces (authoritative: `./sql-engine.md`).
- Base RBAC privilege model and user/role DDL (authoritative: `./auth-rbac.md`).
- Generic storage format rules (authoritative: `./storage-format.md`, except extension-specific encodings).

## External Contracts
- **[Stable] Extension installation is superuser-gated**
  - `CREATE EXTENSION <name>` MUST require superuser privileges.
  - Evidence: `src/sql/executor/extensions.rs` (`execute_create_extension_cmd`), `tests/86_extensions_framework_http.sql`.

- **[Stable] HTTP table functions require superuser**
  - HTTP table functions under the `extensions` schema (e.g. `extensions.http_get`) MUST be denied for non-superusers by default.
  - Evidence: `src/extensions/http.rs` (`execute_table_function` superuser check), `src/extensions/context.rs` (`is_superuser`), `tests/88_http_permission.sql` + `tests/88_http_permission.errors`.

- **[Stable] SSRF protection defaults (secure-by-default)**
  - HTTP table functions MUST reject:
    - loopback/private/link-local/unspecified IP targets (including IPv6-mapped IPv4),
    - `localhost` and `.localhost`/`.local` suffix hosts,
    - URL userinfo,
    - non-default ports (only `443` for `https`, only `80` for `http`).
  - Evidence: `src/extensions/http.rs` (`validate_url`, `is_ip_forbidden`), `tests/87_http_ssrf_protection.sql` + `tests/87_http_ssrf_protection.errors`.

- **[Stable] Insecure HTTP is disabled by default**
  - Plain `http://` requests MUST be rejected by default and MAY be enabled only via config.
  - Evidence: `src/extensions/http.rs` (`allow_insecure_http`), `./ops-config.md` (`DB9_HTTP_ALLOW_INSECURE`).

- **[Stable] Per-statement and per-tenant request limits**
  - HTTP extension execution MUST enforce per-statement request count limits and per-tenant in-flight concurrency limits.
  - Evidence: `src/extensions/context.rs` (`try_consume_http_request`), `src/extensions/http.rs` (`MAX_REQUESTS_PER_STATEMENT`, `MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE`, `TenantLimiters`).

- **[Stable] GIN-like index lifecycle for JSONB/ARRAY/TSVECTOR**
  - The engine supports a lightweight inverted index (“GIN-like”) storage lifecycle for a restricted subset of `USING gin` indexes:
    - exactly one indexed column,
    - no expressions,
    - no partial predicate,
    - column type is `JSON/JSONB`, `ARRAY`, or `TSVECTOR`.
  - Evidence: `src/sql/ddl.rs` (`supported_gin_index_column`, `create_gin_index_entries` backfill path), `src/sql/dml.rs` (maintenance), `src/sql/gin.rs`, `tests/94_gin_index_query.sql`, `tests/131_gin_fts.sql`.
  - Note: `USING gin` indexes on other column types are accepted syntactically but are outside the supported lifecycle and may not be populated/used.
    - Evidence: `src/sql/index_helpers.rs` (`is_index_materializable`).

- **[Experimental] GIN planner/executor access path is currently disabled**
  - The planner currently does not select `ScanType::GinIndexScan`; GIN-eligible predicates execute through non-GIN scan paths with predicate filtering.
  - Runtime/operator builders reject accidental `GinIndexScan` routing with explicit errors (no silent fallback).
  - Evidence: `src/sql/planner/index_selection.rs`, `src/sql/optimizer/build/scan.rs`, `src/sql/operators/planner.rs`, `src/sql/planner/tests.rs`, `tests/131_gin_fts.assert`.

- **[Experimental] Tokenization guarantees and limits**
  - For JSONB containment (`@>`), tokenization MUST avoid false negatives and MAY allow false positives (final containment recheck is required for correctness).
  - Token extraction uses a bounded recursion depth guardrail.
  - Evidence: `src/sql/gin.rs` module doc + tests (`tokens_objects_and_arrays_avoid_false_negatives_for_contains_examples`, `MAX_GIN_DEPTH`).

- **[Experimental] FTS MVP semantics**
  - `to_tsvector`, `plainto_tsquery`, `to_tsquery`, `@@`, and `ts_rank` implement a minimal FTS-compatible surface sufficient for basic matching and ordering, not a full PostgreSQL-equivalent FTS engine.
  - Evidence: `src/sql/fts.rs`, `tests/130_fts.sql`.

## Security Considerations
- **Threat model (primary): SSRF / exfiltration**
  - The HTTP extension can reach arbitrary network targets unless constrained; it is therefore a security-sensitive surface.
- **Default posture (MUST): secure by default**
  - Superuser-only execution is the default boundary.
  - `http://` is disabled by default; enabling it via `DB9_HTTP_ALLOW_INSECURE` changes security posture and requires DR/ADR per #368.
  - URL validation blocks private/loopback/local targets and non-default ports.
  - Evidence anchors: `src/extensions/http.rs` (`execute_table_function`, `validate_url`), tests `tests/87_http_ssrf_protection.sql`, `tests/88_http_permission.sql`.
- **Operational guardrails**
  - Per-statement request cap, per-tenant in-flight cap, and timeouts reduce blast radius but do not replace access control.
  - Evidence: `src/extensions/http.rs` (timeouts/limits), `src/extensions/context.rs` (per-statement counter).

## Data Model & Invariants
- **Installed extensions state** is persisted per database; extension enablement gates runtime execution.
  - Evidence: `src/sql/executor/extensions.rs` (installed/enabled checks), storage persistence in `src/storage/tikv_store.rs` (`get_extension`/`put_extension` paths).
- **GIN-like inverted index entries** are derived from token hashes and stored as index keys; token hashing is deterministic.
  - Evidence: `src/sql/gin.rs` (FNV-1a hashing), storage encoding: `src/storage/encoding.rs` (`encode_gin_index_key_v2`, `encode_gin_index_token_range_v2`).
  - Cross-link: key layout details are authoritative in `./storage-format.md`.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`:
- `DB9_HTTP_ALLOW_INSECURE`

## Entrypoints
- `src/extensions/http.rs`
- `src/extensions/context.rs`
- `src/sql/executor/extensions.rs`
- `src/sql/gin.rs`
- `src/sql/fts.rs`
- `src/storage/encoding.rs`

## Verification (Gates)
Gate IDs are defined in `./testing-gates.md` (do not restate semantics here).
- Gate IDs: `ci:.github/workflows/orm-tests.yml/test`
- Local reproduce (typical):
  - `./run_tests.sh`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/86_extensions_framework_http.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/87_http_ssrf_protection.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/94_gin_index_query.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/130_fts.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/131_gin_fts.sql`

## Change Management
- Any change to HTTP extension security posture (superuser boundary, URL validation rules, limits/timeouts), GIN-like access-path eligibility, or tokenization semantics MUST update this document and the corresponding module entries in `docs/sot/modules.yaml`.
- Breaking changes to security defaults or contracts require DR/ADR per #368 rules (impact surface + migration + rollback + verification updates).
- Reference: https://github.com/c4pt0r/db9/issues/368
