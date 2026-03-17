# Codebase Concerns

**Analysis Date:** 2026-03-17

---

## Tech Debt

**Dual Type Module Hierarchy (#695):**
- Issue: `DataType` and related value types live in both `src/model/mod.rs` and `src/sql/types/`. The SQL engine imports from `crate::model::DataType` throughout, but a shadow hierarchy exists in `src/sql/types/`. Creates confusion about canonical location for type changes.
- Files: `src/model/mod.rs`, `src/sql/types/`, `src/sql/analyzer/expr/coercion.rs`, `src/sql/planner/mod.rs`
- Impact: New type additions require understanding which module is authoritative; coupling between SQL logic and data model representation.
- Fix approach: Designate `src/model/` as the single source of truth for `DataType`; remove or re-export from `src/sql/types/` as a thin alias layer.

**Bincode Serialization Coupling in SQL Layer (#694):**
- Issue: `bincode` is used to serialize SQL-layer objects (role settings, default privileges, statistics, extension metadata, HNSW deltas, UDT definitions) directly into TiKV. Bincode is a binary format with no schema evolution — any variant reorder breaks deserialization.
- Files: `src/sql/default_privileges.rs`, `src/sql/role_settings.rs`, `src/sql/optimizer/statistics.rs` (lines 120–174), `src/sql/hnsw/storage.rs` (line 336), `src/sql/catalog/table_privileges.rs`, `src/storage/tikv_store/worker.rs`, `src/storage/tikv_store/types.rs`
- Impact: `src/model/mod.rs:66` has an explicit comment "Do not reorder variants; append only to preserve bincode compatibility" — this invariant is invisible to new contributors. Any future refactor that reorders enum variants will silently corrupt stored data.
- Fix approach: Replace bincode with a versioned format (e.g. protobuf or bincode2 with schema versioning) for all persistent SQL-layer objects, or add explicit migration gates.

**Statement-Type Detection Duplicated (#696):**
- Issue: SQL statement classification logic appears in both `src/sql/raw_sql.rs` (via `classify()`) and the protocol dispatch layer at `src/protocol/handler/dynamic/query.rs`. The `should_accept_sql_without_sqlparser` bypass path in `src/protocol/handler/query_parser.rs:82` adds a third classification path.
- Files: `src/sql/raw_sql.rs`, `src/protocol/handler/dynamic/query.rs`, `src/protocol/handler/query_parser.rs`
- Impact: A statement type added or changed in one location can silently miss classification in another; the bypass path at `query_parser.rs:82` means some SQL enters execution without ever touching the parser.
- Fix approach: Centralize statement classification in a single module; remove the raw-string bypass or make it a formally documented escape hatch with explicit tests.

**Version Column Rewriting in Wire Encoding (#693):**
- Issue: Internal schema version columns are renamed/hidden at the wire encoding layer rather than at the schema/catalog level.
- Files: `src/protocol/handler/encode/` (inferred from issue description and architecture comment in CLAUDE.md)
- Impact: Column hiding logic leaks into the protocol layer, which should only handle serialization — not schema semantics. Makes column aliasing behavior non-obvious to anyone reading catalog code.
- Fix approach: Handle hidden/internal columns at the catalog/schema layer so the wire encoder only serializes what it receives.

**RLS WHERE Injection Incomplete (Performance Tech Debt):**
- Issue: For UPDATE and DELETE, RLS USING predicates are compiled into a `using_predicate: TypedExpr` field but it is always `None` (placeholder). Correctness is maintained via per-row evaluation in `visibility_policies`, but this scans every row rather than pushing the predicate into the storage scan.
- Files: `src/sql/rls/dml.rs` (lines 229–232, 317–319, 371–372)
- Impact: RLS-enabled UPDATE/DELETE is O(table_size) rather than O(matching_rows). For large tables, this is a significant performance regression vs. PostgreSQL behavior.
- Fix approach: Build the combined USING predicate as `TypedExpr` and inject it into `AnalyzedUpdate/Delete.where_clause`; remove the per-row fallback once injection is proven correct.

**Legacy Binder Still in Use:**
- Issue: `src/sql/binder/` is labeled "legacy name binding" in the architecture, yet it is actively called from view DDL (`src/sql/ddl/view.rs:132,186,354`) and UDT rewriting (`src/sql/udt/enum_rewrite.rs:263`). It predates the Analyzer and uses AST-level heuristics rather than typed scope resolution.
- Files: `src/sql/binder/mod.rs`, `src/sql/ddl/view.rs`, `src/sql/udt/enum_rewrite.rs`
- Impact: View dependency extraction via the binder may miss edge cases handled correctly by the Analyzer. Two name resolution paths exist with different behavior.
- Fix approach: Replace binder usage in view DDL with Analyzer-backed dependency extraction; the binder module can then be removed.

**Parquet Reader Has Many Unimplemented Methods:**
- Issue: The `MockFsWriteStream` and `MockFsBackend` structs in `src/extensions/parquet/reader.rs` have ~20 methods that `bail!("not implemented")`. While these are test stubs, they indicate large surface area of the `FsBackend`/`FsWriteStream` traits that parquet doesn't exercise.
- Files: `src/extensions/parquet/reader.rs` (lines 320–490)
- Impact: Any test path that accidentally routes through these stubs will produce a confusing runtime error rather than a type-level error. Trait implementations that don't fully implement the contract are fragile.
- Fix approach: Use a dedicated partial-mock approach (e.g. `unimplemented!()` at compile time or a focused mock type) rather than runtime `bail!`.

---

## Known Bugs

**Regular SET Not Restored on Savepoint Rollback (#601-followup):**
- Symptoms: `SAVEPOINT sp; SET work_mem = '100MB'; ROLLBACK TO SAVEPOINT sp;` does NOT restore `work_mem` to its pre-savepoint value. PostgreSQL restores it; db9 does not.
- Files: `src/sql/session/transaction.rs` (line 132 — explicit TODO comment)
- Trigger: Any ORM or application that uses savepoints and modifies session settings within a savepoint block.
- Workaround: Use `SET LOCAL` instead of `SET`; `SET LOCAL` is correctly scoped and rolled back.

**SERIALIZABLE Isolation Silently Downgraded:**
- Symptoms: `SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` does not error — it silently downgrades to REPEATABLE READ (TiKV snapshot isolation) with a warning log. Applications that depend on SERIALIZABLE guarantees (write-skew protection, phantom read prevention) will receive incorrect isolation without an error.
- Files: `src/sql/session/settings.rs` (lines 872–877), `src/sql/executor/core/dispatch/utils.rs` (lines 27–33)
- Trigger: Any application that sets `SERIALIZABLE` isolation (Prisma with `SERIALIZABLE` mode in `#840`).
- Workaround: None for write-skew protection. Applications must not rely on SERIALIZABLE semantics.

**Collated String Index Tests Failing (#397/#396):**
- Symptoms: Tests in `tests/repro_pg_tests58_collatedstring_index1.sql` and `tests/repro_pg_tests58_collatedstring_index2.sql` have known mismatches vs. PostgreSQL 17.7.
- Files: `tests/repro_pg_tests58_collatedstring_index1.sql`, `tests/repro_pg_tests58_collatedstring_index2.sql`, `tests/collate_schema_qualified.sql`
- Trigger: Queries over tables with ICU collation columns that use index scans (collated ORDER BY, collated range predicates).
- Workaround: Avoid ICU collation indexes for production workloads until resolved.

**ALTER TYPE Test Mismatch (#395):**
- Symptoms: Known test mismatch between db9 and PostgreSQL 17.7 for `ALTER TYPE` scenarios.
- Files: `src/sql/udt/enum_values.rs`, `src/sql/udt/rename.rs`, `src/sql/alter_type.rs`
- Trigger: Specific ALTER TYPE subcommands (exact repro tracked in issue #395).
- Workaround: None documented.

---

## Security Considerations

**Admin Portal Credential Encryption Fallback (#700):**
- Risk: The admin portal credential encryption falls back to plaintext when the encryption key is absent. Tenant credentials stored without encryption are readable by any process with TiKV access.
- Files: `cloud-admin-portal/backend/` (TypeScript — exact file not identified during analysis)
- Current mitigation: None documented; the issue is tracked and open.
- Recommendations: Block credential writes when the encryption key is unavailable; never silently fall back. Address via #699 (tenant creation refactor) before #700.

**HNSW Index Requires Non-negative INTEGER/BIGINT Primary Keys:**
- Risk: HNSW index creation enforces a constraint that the table primary key must be a non-negative INTEGER or BIGINT. This rejects UUID-keyed tables silently or with an error.
- Files: `src/sql/ddl/create_index.rs` (line 478)
- Current mitigation: Error message is returned at index creation time.
- Recommendations: Document this limitation explicitly in user-facing error messages and consider supporting UUID tables via a mapping layer.

**Insecure HTTP Requests Gated by Environment Variable:**
- Risk: `DB9_HTTP_ALLOW_INSECURE=true` enables plaintext HTTP in the `http` extension. A misconfigured deployment could silently allow HTTP requests to internal services.
- Files: `src/extensions/http.rs` (line 266)
- Current mitigation: Default is to reject HTTP (non-TLS) requests.
- Recommendations: Audit deployment configurations to ensure `DB9_HTTP_ALLOW_INSECURE` is not set in production.

---

## Performance Bottlenecks

**Pre-Materialization Runs Unconditionally on Top-Level SELECT (#857):**
- Problem: `pre_materialize_query_body` is called for every `Cow::Owned` SELECT regardless of whether the query contains async expressions. Only the `Cow::Borrowed` path (prepared statements) checks `query_needs_pre_materialization` first. This means every ad-hoc SELECT incurs the expression-tree traversal overhead unconditionally.
- Files: `src/sql/executor/select/analyzed/mod.rs` (lines 378–403), `src/sql/executor/select/analyzed/pipeline.rs`
- Cause: The check guard was applied only to the `Cow::Borrowed` path; the `Cow::Owned` path was left unconditional.
- Improvement path: Apply `query_needs_pre_materialization` check before calling `pre_materialize_query_body` on the owned path; tracked as #857 → #707 (plan cache).

**No Cross-Session Plan Cache (#707):**
- Problem: The `PreparedPlanCache` (`src/sql/executor/core/plan_cache.rs`) is session-local. Each new connection must re-analyze and re-plan every prepared statement from scratch. High-connection-churn workloads (e.g., Prisma connection pooling) re-plan on every new session.
- Files: `src/sql/executor/core/plan_cache.rs`, `src/sql/session/mod.rs` (line 128)
- Cause: Session-scoped cache was the safe first step; cross-session cache requires invalidation coordination via TiKV schema versions.
- Improvement path: Implement a process-level LRU plan cache keyed on `PlanCacheKey` with schema-version-based invalidation; tracked as #707.

**HNSW Index Loads Graph from TiKV on Every Scan:**
- Problem: By design (comment in `src/sql/hnsw/mod.rs:6`), there is no process-level HNSW graph cache. Every vector ANN query performs a full graph load from TiKV, including all edge data. For large indexes this is multi-hundred-millisecond latency per query.
- Files: `src/sql/hnsw/mod.rs`, `src/sql/hnsw/storage.rs`, `src/sql/operators/hnsw_scan.rs`
- Cause: A prior cache introduced a race condition with uncommitted INSERT; it was removed without replacement.
- Improvement path: Implement an MVCC-aware cache that validates freshness against TiKV timestamps (mentioned in the `mod.rs` design note). Low-priority until MVCC cache design is agreed.

**RLS Per-Row Policy Evaluation on DML:**
- Problem: RLS USING predicates for UPDATE/DELETE are evaluated per row in the `visibility_policies` path rather than being pushed into the storage scan predicate. See Tech Debt section above.
- Files: `src/sql/rls/dml.rs`
- Cause: `using_predicate` TypedExpr field is built as `None` (placeholder); full WHERE injection not yet implemented.
- Improvement path: Build and inject combined USING predicate into `AnalyzedUpdate/Delete.where_clause`.

---

## Fragile Areas

**Bincode Enum Variant Ordering (`src/model/mod.rs`):**
- Files: `src/model/mod.rs` (line 66 comment), `src/sql/hnsw/storage.rs`, `src/sql/optimizer/statistics.rs`
- Why fragile: Any Rust enum serialized with bincode will silently produce wrong deserialization results if variants are reordered. The constraint "append only" is a source comment, not a compiler-enforced contract.
- Safe modification: Only append new variants to the end of any enum serialized with bincode. Never remove, rename, or reorder. Consider adding a compile-time discriminant test.
- Test coverage: `src/sql/optimizer/statistics.rs` has bincode round-trip tests but only for correct encoding, not for backwards compatibility with old stored data.

**`should_accept_sql_without_sqlparser` Bypass Path:**
- Files: `src/sql/raw_sql.rs` (line 550), `src/protocol/handler/query_parser.rs` (line 82), `src/protocol/handler/dynamic/query.rs` (line 827)
- Why fragile: SQL that matches this bypass enters execution without going through `sqlparser`. If a new SQL syntax is added that should be blocked but happens to match the bypass pattern, it will silently succeed.
- Safe modification: Any change to `should_accept_sql_without_sqlparser` must also audit `src/protocol/handler/dynamic/query.rs:827` for behavior consistency. Missing test coverage for this path (tracked as #898).
- Test coverage: No handler-level tests for the fallback path (#898 open).

**HNSW Requires Non-negative Integer Primary Keys:**
- Files: `src/sql/ddl/create_index.rs` (line 478), `src/sql/hnsw/storage.rs`, `src/sql/operators/hnsw_scan.rs`
- Why fragile: The PK constraint is validated at index creation but the storage and scan operators assume this invariant holds at runtime. Tables with UUID PKs or negative integer PKs will bypass the index rather than error if the constraint check is somehow circumvented.
- Safe modification: Do not relax the PK constraint check without also updating the storage and scan operators.
- Test coverage: Integration tests cover the happy path; no tests for PK-constraint bypass.

**GIN Scan Planner/Executor Mismatch:**
- Files: `src/sql/optimizer/build/scan.rs` (line 127, `GinScanOperator::new`), `src/sql/operators/gin_scan.rs`, `src/sql/planner/index_selection.rs`
- Why fragile: The optimizer can produce GIN scan plans (visible in EXPLAIN output), and the `GinScanOperator` exists and is wired. However the CLAUDE.md notes the GIN runtime was previously "not yet implemented" and has since been implemented. The regression gate (`tests/236_gin_index_scan.sql`) guards phase-1 behavior but if the GIN operator fails for untested scan patterns it silently falls back to table scan.
- Safe modification: Any change to `GinScanOperator` must rerun `tests/236_gin_index_scan.sql` and the full FTS suite.
- Test coverage: `tests/236_gin_index_scan.sql` covers basic cases; complex nested boolean GIN queries may not be covered.

**Process-Level Global State and Multi-tenancy:**
- Files: `src/sql/fts_tokenizers.rs` (line 30 — `USER_TSC_CACHE`), `src/sql/advisory_locks.rs` (line 279 — `GLOBAL_LOCK_MANAGER`), `src/sql/collation.rs` (line 161 — process-global collation registry), `src/auth/db9_auth.rs` (line 306 — `JWKS_CACHE`), `src/extensions/fs/embedded/pagefs.rs` (lines 86–88 — `FS9_MAINTENANCE_STARTED`, `FS9_PROCESS_IDENTITIES`)
- Why fragile: These process-level singletons must be carefully keyed by tenant/keyspace to avoid cross-tenant data bleed. Any new addition to these caches that forgets to include the keyspace key will silently serve another tenant's data.
- Safe modification: Any new cache added to these statics must include the tenant keyspace as part of the cache key and have an explicit test for keyspace isolation.
- Test coverage: Multi-tenant isolation is tested at the integration level but not unit-tested for each individual cache.

---

## Scaling Limits

**Per-Tenant Connection Caps (Partial Implementation):**
- Current capacity: Per-user connection counts (`rolconnlimit`) are enforced via `TenantHandle::try_bind_user`. Global max connections enforced via `Semaphore` in `src/main.rs` (line 366).
- Limit: No per-tenant aggregate connection cap (only per-user within a tenant). A single tenant can exhaust the global semaphore.
- Scaling path: Implement per-tenant connection cap as part of #779 resource governance roadmap.

**Sort Memory Limit is Per-Query, Not Per-Tenant:**
- Current capacity: `db9.max_sort_bytes` GUC limits sort memory per query via `src/sql/operators/sort.rs:15`.
- Limit: Multiple concurrent heavy sort queries from one tenant can together exhaust process memory; the limit applies individually to each query.
- Scaling path: Add per-tenant aggregate memory accounting for sort operations as part of #779.

**TiKV Batch Size Fixed at Compile Time:**
- Current capacity: `BATCH_SIZE` constant in `src/storage/tikv_store/mod.rs:49` gates paginated scans to avoid exceeding gRPC message size.
- Limit: Large row values (e.g. JSONB columns with megabyte payloads) can still approach gRPC limits even with small batch counts.
- Scaling path: Use byte-size estimation rather than row-count for batch pagination.

---

## Dependencies at Risk

**`bincode` Serialization Format (Structural):**
- Risk: bincode v1 has no schema evolution, no self-describing format. Stored data in TiKV is forever coupled to the exact Rust struct layout at time of write.
- Impact: Any struct/enum change that touches a bincode-serialized type requires a migration. See model comment at `src/model/mod.rs:66`.
- Migration plan: Introduce versioned wrappers for all bincode-serialized types; migrate to a schema-evolution-capable format (protobuf or serde with explicit versioning) for long-lived storage objects.

**`usearch` FFI Crate (HNSW):**
- Risk: The HNSW implementation uses `usearch` via FFI (`src/sql/hnsw/`). FFI bridges are harder to update, have no Rust safety guarantees across the boundary, and couple the project to the usearch C++ library's release cadence.
- Impact: HNSW feature evolution (new distance metrics, larger index capacity) depends on usearch upstream. Memory safety issues in usearch are not caught by the Rust borrow checker.
- Migration plan: Monitor usearch for breaking API changes; consider an abstraction layer over the FFI so the backend can be swapped.

---

## Missing Critical Features

**No SERIALIZABLE Isolation:**
- Problem: TiKV provides snapshot isolation (REPEATABLE READ), not SERIALIZABLE. Applications requiring SERIALIZABLE (write-skew protection) are silently given weaker guarantees with only a warning log.
- Blocks: Full Prisma ORM compatibility (#840), production workloads that rely on SERIALIZABLE for correctness (e.g. double-spend prevention).

**No Cross-Session Plan Cache:**
- Problem: Every new session re-plans all prepared statements. Tracked as #707.
- Blocks: Connection-pool-heavy ORMs (Prisma, TypeORM) that open/close connections frequently pay re-plan cost on every connection.

**No Parallel/Distributed Query Execution (#708):**
- Problem: All queries execute single-threaded on the db9 process. TiKV is a distributed KV store but queries do not parallelize across TiKV regions.
- Blocks: Analytical queries over large tables; multi-region performance scalability.

**Per-Tenant Resource Governance Incomplete (#779):**
- Problem: Connection caps exist per-user but not per-tenant. QPS rate limiting is present (`TokenBucket` in `src/pool.rs:375`) but memory backpressure and per-tenant timeout enforcement are partial.
- Blocks: True multi-tenant SLA isolation; noisy-neighbor protection.

---

## Test Coverage Gaps

**Handler-Level Fallback Path Tests (#898):**
- What's not tested: The `on_parse` fallback path and `should_accept_sql_without_sqlparser` bypass path in `src/protocol/handler/dynamic/query.rs` (line 827) have no handler-level tests. Behavior is validated only through integration tests.
- Files: `src/protocol/handler/dynamic/query.rs`, `src/sql/raw_sql.rs`, `src/protocol/handler/query_parser.rs`
- Risk: A regression in the bypass SQL classification logic would silently pass or fail without a targeted test.
- Priority: High — these paths handle real SQL sent by ORMs that doesn't parse cleanly.

**Clippy Policy Not Enforced (#411):**
- What's not tested: No CI step enforces `cargo clippy -- -D warnings`. The codebase has 156 `#[allow(dead_code)]` suppressions, multiple `#[allow(clippy::type_complexity)]` and `#[allow(clippy::too_many_arguments)]` suppressions in production code.
- Files: `src/main.rs` (line 8 — `#![allow(clippy::uninlined_format_args)]`), `src/sql/ddl/create_index.rs` (line 109), `src/sql/executor/select/analyzed/mod.rs` (lines 752–753)
- Risk: Without enforcement, dead code and complexity suppressions accumulate; code quality gates are unenforced.
- Priority: Medium — primarily a code quality risk, not a correctness risk.

**Integration Test Output Diffing Without NO_COLOR (#317):**
- What's not tested: `scripts/integration_test.py` does not enforce `NO_COLOR` output and does not produce a diff on golden file mismatches.
- Files: `scripts/integration_test.py`
- Risk: Test failures require manual inspection to find the differing line; colored terminal output in CI logs is noisy.
- Priority: Low — usability issue for developers, not a correctness risk.

**GIN Index Edge Cases:**
- What's not tested: Complex boolean GIN queries (nested AND/OR/NOT on tsquery/jsonb), multi-column GIN predicates, and GIN index behavior after concurrent DML are not explicitly covered.
- Files: `tests/236_gin_index_scan.sql`, `src/sql/operators/gin_scan.rs`, `src/sql/planner/gin_predicate.rs`
- Risk: Silent fallback to table scan for unrecognized GIN patterns means correctness bugs would not be detected by functional tests.
- Priority: High — GIN is used for FTS and JSONB @> queries, which are common in Dify/Activepieces workloads (#375/#885).

**doc-lint Does Not Validate Symbol Existence (#419):**
- What's not tested: `scripts/doc_lint.py` parses `code_entrypoints` YAML but does not verify the referenced Rust symbols actually exist in the codebase.
- Files: `scripts/doc_lint.py` (line 540–548)
- Risk: Documentation references to deleted or renamed symbols are silently accepted; architecture docs drift from code reality.
- Priority: Low — documentation quality issue.

---

*Concerns audit: 2026-03-17*
