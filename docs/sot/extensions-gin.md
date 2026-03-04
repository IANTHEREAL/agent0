# extensions-gin — HTTP/embedding extensions + GIN-like indexes + full-text search (FTS)

## Scope
- HTTP extension framework and table functions (`extensions.http_*`) and their security model (SSRF protection, limits, superuser boundary).
- Embedding extension surfaces (`embedding()`, `extensions.embedding_usage()`) and their install/visibility/permission model.
- GIN-like inverted index storage/maintenance semantics used for:
  - JSON/JSONB containment (`@>`),
  - ARRAY containment (`@>`),
  - FTS match (`@@`) via `TSVECTOR`/`TSQUERY`.
- GIN index scan is enabled: the planner selects `GinIndexScan` for eligible `@@`, `@>`, and `&&` predicates.
  A `GinScanOperator` evaluates boolean-tree GIN quals at runtime and a recheck filter ensures correctness.
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

- **[Stable] Embedding extension visibility + permission contract**
  - `embedding()` and `extensions.embedding_usage()` MUST require `CREATE EXTENSION embedding` visibility and MUST enforce superuser-only execution.
  - Visibility evaluation order MUST be:
    1) in-transaction DDL delta override (`CREATE/DROP EXTENSION` in current transaction),
    2) otherwise explicit transaction statements use a transaction-consistent snapshot source,
    3) autocommit statements use latest committed extension catalog state at statement boundary.
  - Parity adjudication for this contract MUST use direct function-call statements (`embedding(...)` / `extensions.embedding_usage()`) on PostgreSQL 17.7 with two independent sessions. Catalog-only probes (for example `pg_extension`) are supportive evidence only.
  - If extension visibility check fails, the user-facing contract MUST be function-not-found semantics (`42883`), not feature-not-supported (`0A000`).
  - Evidence anchors: `src/sql/expr/functions/embedding.rs`, `src/extensions/embedding.rs` (`check_embedding_installed`), `src/session_context.rs`, `src/sql/session/{mod.rs,transaction.rs}`.

- **[Stable] Embedding signature resolution and SQLSTATE layering**
  - Wrong function signature (arity/type mismatch, e.g. `embedding(42)`) MUST fail at function resolution boundary with `42883`.
  - For `embedding(text [, model, dimensions])`, PostgreSQL-style literal coercion MUST apply to the 3rd argument:
    - quoted numeric literals (e.g. `'1024'`) are accepted via implicit cast,
    - non-literal text expressions (e.g. text columns) are rejected at resolution with `42883` unless explicitly cast.
  - Runtime validation (`22023`) MUST be limited to value-domain checks on already-resolved valid signatures (e.g. non-positive dimensions, empty text, unsupported model value).
  - Runtime permission and capability gates MUST remain typed:
    - permission denied => `42501`
    - service unavailable by config => `0A000`
    - internal/infra failures => `XX000`
  - Authoritative rationale: `docs/design/28_embedding_extension_pg_parity_contract.md`.

- **[Stable] Embedding model is pinned to `text-embedding-v4`**
  - Session/env model values MUST canonicalize to `text-embedding-v4`; non-v4 values MUST be rejected (`SET embedding.model`) or forced to v4 (`EMBEDDING_MODEL` env defaulting path).
  - Evidence: `src/config.rs` (`canonical_embedding_model`), `src/sql/session/settings.rs`, `src/sql/expr/functions/embedding.rs`.

- **[Stable] Per-statement and per-tenant request limits**
  - HTTP extension execution MUST enforce per-statement request count limits and per-tenant in-flight concurrency limits.
  - Evidence: `src/extensions/context.rs` (`try_consume_http_request`), `src/extensions/http.rs` (`MAX_REQUESTS_PER_STATEMENT`, `MAX_INFLIGHT_REQUESTS_PER_TENANT_PER_NODE`, `TenantLimiters`).

- **[Stable] GIN-like index lifecycle for JSONB/ARRAY/TSVECTOR**
  - The engine supports a lightweight inverted index ("GIN-like") storage lifecycle for a restricted subset of `USING gin` indexes:
    - exactly one indexed column (or expression-index on `to_tsvector('config', col)`),
    - no partial predicate,
    - column type is `JSON/JSONB`, `ARRAY`, or `TSVECTOR`.
  - Evidence: `src/sql/ddl.rs` (`supported_gin_index_column`, `create_gin_index_entries` backfill path), `src/sql/dml.rs` (maintenance), `src/sql/gin.rs`, `tests/94_gin_index_query.sql`, `tests/131_gin_fts.sql`, `tests/236_gin_index_scan.sql`.
  - Note: `USING gin` indexes on other column types are accepted syntactically but are outside the supported lifecycle and may not be populated/used.
    - Evidence: `src/sql/index_helpers.rs` (`is_index_materializable`).

- **[Stable] GIN planner/executor access path**
  - The planner selects `ScanType::GinIndexScan` when eligible GIN predicates are detected:
    - `@@` (tsvector match): AND/OR/NOT boolean tsquery with at least one positive term.
    - `@>` (JSONB containment): constant RHS tokenized to AND of token hashes.
    - `@>` (ARRAY containment): constant RHS elements as AND of hashes.
    - `&&` (ARRAY overlap): constant RHS elements as OR of hashes.
  - Expression-index matching: `to_tsvector('config', col)` GIN indexes are matched against equivalent LHS expressions.
  - Pure-negative GIN quals (e.g. `!A` with no positive terms) are rejected by the planner (falls through to Seq Scan).
  - `GinScanOperator` evaluates the `GinQual` tree via posting-list set operations (intersect/union/difference) and a recheck filter ensures correctness (no false negatives).
  - Evidence: `src/sql/planner/gin_predicate.rs`, `src/sql/planner/index_selection.rs`, `src/sql/operators/gin_scan.rs`, `src/storage/tikv_store/indexes.rs` (`scan_gin_posting_list`), `tests/236_gin_index_scan.sql`, `tests/218_gin_correctness.sql`.

- **[Experimental] Tokenization guarantees and limits**
  - For JSONB containment (`@>`), tokenization MUST avoid false negatives and MAY allow false positives (final containment recheck is required for correctness).
  - Token extraction uses a bounded recursion depth guardrail.
  - Evidence: `src/sql/gin.rs` module doc + tests (`tokens_objects_and_arrays_avoid_false_negatives_for_contains_examples`, `MAX_GIN_DEPTH`).

- **[Experimental] FTS MVP semantics**
  - `to_tsvector`, `plainto_tsquery`, `to_tsquery`, `@@`, and `ts_rank` implement a minimal FTS-compatible surface sufficient for basic matching and ordering, not a full PostgreSQL-equivalent FTS engine.
  - Evidence: `src/sql/fts.rs`, `tests/130_fts.sql`.

## Security Considerations
- **Threat model (primary): SSRF / exfiltration**
  - The HTTP and embedding extensions perform outbound network requests and are therefore security-sensitive surfaces.
- **Default posture (MUST): secure by default**
  - Superuser-only execution is the default boundary.
  - `http://` is disabled by default; enabling it via `DB9_HTTP_ALLOW_INSECURE` changes security posture and requires DR/ADR per #368.
  - Embedding requests require explicit provider configuration (`EMBEDDING_API_KEY`) and extension installation.
  - URL validation blocks private/loopback/local targets and non-default ports.
  - Evidence anchors: `src/extensions/http.rs` (`execute_table_function`, `validate_url`), `src/sql/expr/functions/embedding.rs`, tests `tests/87_http_ssrf_protection.sql`, `tests/88_http_permission.sql`.
- **Operational guardrails**
  - Per-statement request cap, per-tenant in-flight cap, and timeouts reduce blast radius but do not replace access control.
  - Evidence: `src/extensions/http.rs` (timeouts/limits), `src/extensions/context.rs` (per-statement counter).

## Data Model & Invariants
- **Installed extensions state** is persisted per database; extension enablement gates runtime execution.
  - Evidence: `src/sql/executor/extensions.rs` (installed/enabled checks), storage persistence in `src/storage/tikv_store.rs` (`get_extension`/`put_extension` paths).
- **Embedding usage accounting** is persisted per database/day using `sys_embedding_usage_YYYYMMDD` keys and is independent of planner cache behavior.
  - Evidence: `src/extensions/embedding.rs` (`record_embedding_tokens`, `read_embedding_usage`), `src/storage/encoding/metadata_keys.rs` (`encode_embedding_usage_key_v2`).
- **GIN-like inverted index entries** are derived from token hashes and stored as index keys; token hashing is deterministic.
  - Evidence: `src/sql/gin.rs` (FNV-1a hashing), storage encoding: `src/storage/encoding.rs` (`encode_gin_index_key_v2`, `encode_gin_index_token_range_v2`).
  - Cross-link: key layout details are authoritative in `./storage-format.md`.

## Configuration
This module MUST NOT redefine config keys. Relevant keys are defined exactly once in `./ops-config.md`:
- `DB9_HTTP_ALLOW_INSECURE`
- `EMBEDDING_API_KEY`
- `EMBEDDING_ENDPOINT` / `EMBEDDING_BASE_URL`
- `EMBEDDING_MODEL`
- `EMBEDDING_DIMENSIONS`

## Entrypoints
- `src/extensions/http.rs`
- `src/extensions/embedding.rs`
- `src/extensions/context.rs`
- `src/sql/executor/extensions.rs`
- `src/sql/expr/functions/embedding.rs`
- `src/sql/gin.rs`
- `src/sql/fts.rs`
- `src/session_context.rs`
- `src/sql/session/mod.rs`
- `src/sql/session/transaction.rs`
- `src/storage/encoding.rs`
- `src/sql/planner/gin_predicate.rs`
- `src/sql/planner/index_selection.rs`
- `src/sql/operators/gin_scan.rs`
- `src/storage/tikv_store/indexes.rs`

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
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/218_gin_correctness.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/219_gin_chinese_fts.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/236_gin_index_scan.sql`
  - `python3 scripts/integration_test.py --dsn "$PG_DSN" tests/272_embedding_extension_concurrent_visibility.sql`
  - `cargo test embedding_null_still_checks_extension_gate -- --nocapture`
  - `cargo test txn_snapshot_ts_is_scoped -- --nocapture`
  - `cargo test test_session_settings_embedding_model_accepts_only_v4 -- --nocapture`

## Change Management
- Any change to HTTP/embedding extension security posture (permission boundary, URL validation rules, model constraints, limits/timeouts), GIN-like access-path eligibility, or tokenization semantics MUST update this document and the corresponding module entries in `docs/sot/modules.yaml`.
- Any change to embedding function visibility/signature SQLSTATE behavior MUST also update `docs/design/28_embedding_extension_pg_parity_contract.md` and keep the two documents consistent (`docs/sot/**` remains authoritative).
- Breaking changes to security defaults or contracts require DR/ADR per #368 rules (impact surface + migration + rollback + verification updates).
- Reference: https://github.com/c4pt0r/db9/issues/368
