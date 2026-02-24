# Root-Cause Notebook (2026-02-19)

- Branch: `issue-844-architecture-cleanup`
- PR: `https://github.com/c4pt0r/db9/pull/846`
- Scope: architecture-cleanup follow-up, grouped root-cause burn-down

## Latest Iteration (2026-02-19 20:00 UTC)

- Principle recall:
  - Group by root cause, not test-by-test patching.
  - Keep fixes on architecture/contract boundaries, avoid masking.
  - Validate expectation shifts against PostgreSQL 17.7 before accepting.

- CI/root-cause buckets closed:
  1. Unit-test expectation drift vs PostgreSQL semantics
     - Symptoms:
       - `sql::dml::tests::enum_validation_*` expected `public.role` in error text.
       - `sql::expr::typed_eval::tests::test_at_time_zone_offset` expected ISO sign behavior for `'+08:00'`.
     - PostgreSQL 17.7 validation:
       - Enum errors use bare enum name (`role`), not schema-qualified name.
       - `TIMESTAMP ... AT TIME ZONE '+08:00'` follows POSIX sign convention (effectively `UTC-8`), yielding `+8h` shift in this case.
     - Fix:
       - Updated unit assertions in `src/sql/dml.rs`.
       - Updated AT TIME ZONE expectation/comment in `src/sql/expr/typed_eval.rs`.
     - Verification:
       - `cargo test` -> `1706 passed / 0 failed`.
       - `bash scripts/regression_gate.sh` -> pass (`SQL 37/37`, `ORM 122/122`).

  2. Full-suite ORM timeout in `run_tests.sh` due cross-suite DB-state contamination
     - Symptom:
       - `typeorm/schema.test.ts` idempotent synchronize timed out at 30s only in `./run_tests.sh` (after integration corpus), while isolated ORM runs passed.
     - Root cause:
       - `run_tests.sh` ran ORM against the same `postgres` database after 245 integration cases, causing expensive schema introspection in TypeORM synchronize path.
     - Fix:
       - Isolated ORM phase in `run_tests.sh` to an auto-created per-run database (`orm_tests_<timestamp>`), with cleanup on exit.
     - Verification:
       - `./run_tests.sh` -> pass:
         - Integration: `245 passed / 0 failed`
         - ORM: `585 passed / 0 failed / 1 skipped`
       - Built-in integration mode:
         - `python3 scripts/integration_test.py --dsn ...` -> `8 passed / 0 failed`
       - Tier2 e2e:
         - `bash scripts/e2e_tests.sh gorm_smoke` -> pass
         - `bash scripts/e2e_tests.sh sqlalchemy_smoke` -> pass
         - `bash scripts/e2e_tests.sh dify_sqlalchemy_compat` -> pass

## Latest Iteration (2026-02-19 19:22 UTC)

- Principle recall:
  - Root-cause-first, grouped fixes only.
  - New architecture path only (`Analyzer -> Typed IR -> Optimizer/Executor`), no fallback masking.
  - Any test expectation change must be validated on PostgreSQL 17.7 first.

- Baseline before this iteration:
  - Full integration: `242 passed / 3 failed`
  - Open files: `222_structural_fixes_contract.sql`, `39_enum_types.sql`, `75_grouping_sets.sql`

- Root cause buckets closed:
  1. Grouping extensions unsupported on analyzed path (`75_grouping_sets.sql`)
     - Symptom: `expression type not yet supported: Discriminant(52/53/54)`
     - Fix (architecture): `src/sql/analyzer/query.rs`
       - Added analyzer-side rewrite for `GROUPING SETS` / `ROLLUP` / `CUBE` into deterministic `UNION ALL` simple-grouping arms.
       - Added `GROUPING(...)` rewriting to constant bitmask values per expanded grouping arm.
       - Rewrote projected non-grouped grouping-key columns to `NULL` (with stable aliases) per grouping arm.
     - Result: full parity for `tests/75_grouping_sets.sql`.

  2. Vector empty-dimension contract drift (`222_structural_fixes_contract.sql`)
     - Symptom: test expected `[]` accepted for `vector(3)`; PostgreSQL rejects.
     - PostgreSQL 17.7 validation:
       - Environment: installed `postgresql-17-pgvector`
       - `tests/222_structural_fixes_contract.sql` on PG17.7 returns error for `INSERT ... '[]'`
       - Evidence: `/tmp/pg17_222.stdout`, `/tmp/pg17_222.stderr`
     - Fix:
       - Code parity update in `src/sql/types/cast.rs`:
         - Reject empty vectors with PG-style error text: `vector must have at least 1 dimension`
         - Align dimension mismatch wording to PG-style: `expected N dimensions, not M`
       - Test expectation update in `tests/222_structural_fixes_contract.expected` (PG-validated behavior).

  3. Non-idempotent test fixture lifecycle (`39_enum_types.sql`)
     - Symptom: repeated runs left `udt_comp_test`/`comp` artifacts, causing preamble errors.
     - PostgreSQL 17.7 validation:
       - Baseline behavior validated on clean DB (`/tmp/pg17_39.stdout`, `/tmp/pg17_39.stderr`).
     - Fix:
       - Added explicit cleanup in `tests/39_enum_types.sql`:
         - preamble: `DROP TABLE IF EXISTS udt_comp_test;`
         - tail cleanup after expected RESTRICT failure:
           - `DROP TABLE IF EXISTS udt_comp_test;`
           - `DROP TYPE IF EXISTS comp;`
     - Result: idempotent run behavior without changing intended semantic assertions.

- Verification evidence:
  - Build:
    - `cargo build --release` -> pass
  - Focused verification:
    - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests/222_structural_fixes_contract.sql tests/39_enum_types.sql tests/75_grouping_sets.sql`
    - Result: `3 passed / 0 failed`
    - Evidence: `/tmp/issue844_focus_after_restart.log`
  - Full integration:
    - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
    - Result: `245 passed / 0 failed / 0 skipped`
    - Evidence: `/tmp/integration_issue844_after_groupingsets_20260219_192135.log`

## Status

- Current failure set: **none**
- Current test status: **245/245 passed**

## Latest Iteration (2026-02-19 15:56 UTC)

- Objective:
  - Continue grouped root-cause burn-down from baseline `221 passed / 24 failed`.
  - Close worker-system-keyspace startup bucket without adding fallback behavior.
- New code fix:
  - `src/worker/mod.rs`
  - Root cause: PD v2 can return `500 "keyspace already exists"` for idempotent create.
  - Previous behavior treated all `500` as retry-only and skipped existence verification, causing startup delay and transient connection failures.
  - Fix: for `500/503`, still run `GET /pd/api/v2/keyspaces/{name}` existence check before retry/error.
- Verification evidence:
  - `cargo build`: pass
  - Worker-focused tests:
    - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests/186_worker_bg_sql.sql tests/187_worker_cic.sql`
    - Result: `2 passed / 0 failed`
  - Full integration:
    - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
    - Result: `222 passed / 23 failed / 0 skipped`
    - Net change vs previous baseline: `+1 passed / -1 failed`
    - Evidence log: `/tmp/integration_after_worker.log`

## Latest Iteration (2026-02-19 16:08 UTC)

- Objective:
  - Continue root-cause bucket on async expression handling in DML analyzed path.
- New code fix:
  - `src/sql/executor/dml_analyzed.rs`
  - Root cause: DML path evaluated typed expressions directly (`eval_typed_expr`) in WHERE/SET/VALUES/ON CONFLICT without async materialization, so unresolved subqueries leaked to runtime.
  - Fix: route these evaluation points through `materialize_expr_for_row` when `needs_async_materialization` is true.
- Verification evidence:
  - Focused bucket:
    - `tests/19_subqueries.sql tests/32_advanced_procedures.sql tests/33_transaction_consistency.sql tests/71_lateral_join.sql tests/78_update_delete_variants.sql`
    - Result: `3 passed / 2 failed` (`32`, `33`, `78` fixed; `19`, `71` still open)
  - Full integration:
    - Result: `224 passed / 21 failed / 0 skipped`
    - Delta vs previous baseline `222/23`: net `+2` pass
    - Evidence: `/tmp/integration_after_dml_async_fix.log`
  - Flaky/external note:
    - `91_http_extension.sql` failed once in full run but passed on focused rerun:
      `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests/91_http_extension.sql`

## Latest Iteration (2026-02-19 16:14 UTC)

- Objective:
  - Close ANY/ALL NULL-3VL bucket on typed analyzer path.
- New code fix:
  - `src/sql/analyzer/expr.rs`
  - Root cause: `ANY/ALL` lowering only recognized direct `ArrayLiteral`, missing cast-wrapped literals (`ARRAY[...]::int[]`), causing fallback paths and PG-incompatible NULL semantics.
  - Fix: extract cast-wrapped array-literal elements and route them through the literal lowering path for both `ANY` and `ALL`.
- Verification evidence:
  - Focused:
    - `tests/121_any_all_null_3vl_issue43.sql` -> pass
  - Full integration:
    - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
    - Result: `226 passed / 19 failed / 0 skipped`
    - Delta vs previous baseline `224/21`: net `+2`
    - Evidence: `/tmp/integration_after_anyall_fix.log`

## Working Principles (Recalled)

1. Group failures by shared root cause, never case-by-case patching.
2. Fix only on the new architecture path (`Analyzer -> Typed IR -> Optimizer/Executor`).
3. No legacy fallback revival and no wire/test-layer masking.
4. Any `.expected/.assert/.errors` change must be validated against PostgreSQL 17.7 first.
5. Close one root-cause bucket fully (code + focused tests + evidence) before moving on.

## Current Failure Set (19 files)

- `100_aggregate_alias_fixes.sql`
- `146_case_sensitive_names.sql`
- `149_distinct_order_by_select_list.sql`
- `172_ordinality.sql`
- `180_propagate_input_ordering.sql`
- `187_worker_cic.sql`
- `19_subqueries.sql`
- `21_views.sql`
- `221_drop_cascade_views.sql`
- `222_ddl_export.sql`
- `222_structural_fixes_contract.sql`
- `223_migrations.sql`
- `224_index_namespace.sql`
- `28_advanced_constraints.sql`
- `39_enum_types.sql`
- `71_lateral_join.sql`
- `75_grouping_sets.sql`
- `79_comparison_operators.sql`
- `95_limit_pushdown.sql`

## Next Root-Cause Buckets (Re-grouped from current 23)

1. Correlated/subquery executor handoff gaps:
   - Signals: `subquery expressions must be resolved at executor level`, correlated column depth errors.
   - Files: `19`, `32`, `33`, `71`, `78`.
2. Analyzer/typed expression coverage gaps:
   - Signals: `ALL with non-array operand`, `expression type not yet supported: Discriminant(...)`.
   - Files: `121`, `75`, `79`.
3. Name-resolution/alias propagation drift:
   - Signals: missing/ambiguous columns (`name`, `foo`, `"Y"`, `distinctAlias.User_id`).
   - Files: `146`, `149`, `172`, `180`, `100`.
4. DDL/catalog/TVF compatibility gaps:
   - Signals: unsupported `_db9_sys_*` TVFs and view/drop contract drift.
   - Files: `21`, `221`, `222`, `223`, `224`.
5. Remaining semantic/perf buckets:
   - `28`, `39`, `95`, `222_structural_fixes_contract`.

## Latest Verification Baseline

- `cargo build`: pass
- `cargo test`: pass (`1696 passed`, `0 failed`)
- Focused bucket rerun:
  - `tests/88_http_permission.sql`: pass
  - `tests/72_json_advanced.sql`: pass
  - `tests/73_system_functions.sql`: pass
  - `tests/105_expr_functions.sql`: pass
  - `tests/116_conditional.sql`: pass
- Full integration rerun:
  - Command: `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
  - Result: `194 passed`, `51 failed`, `0 skipped`
  - Evidence: `/tmp/integration_all_after_rc_json_func_conditional.log`

## Recently Closed Root-Cause Bucket

### B4.5.A — Function Surface + JSON/Conditional Semantics (CLOSED)

- Scope files:
  - `tests/88_http_permission.sql`
  - `tests/72_json_advanced.sql`
  - `tests/73_system_functions.sql`
  - `tests/105_expr_functions.sql`
  - `tests/116_conditional.sql`

- Root causes fixed:
  - Incomplete JSON operator integration (`jsonb - text/int`, `#-`) on typed path.
  - Extension permission message contract mismatch (`extension "http"` quoting).
  - `FORMAT` error contract drift (missing PG-style `HINT` line).
  - `CBRT` float behavior drift from PostgreSQL output.
  - CASE-derived output column naming drift (`case`/derived alias vs `?column?`).
  - COALESCE constant-error parity gap (`COALESCE(a, 1/0)` must error like PG).
  - SQL error text contract drift (`division by zero` casing).

- Key implementation points (new architecture only):
  - Analyzer JSON operator coverage completed in `src/sql/analyzer/expr.rs` and `src/sql/analyzer/types.rs`.
  - Typed evaluator JSON mapping completed in `src/sql/expr/typed_eval.rs`.
  - Minus operator result typing for JSONB added in `src/sql/types/coercion.rs`.
  - HTTP extension permission object name adjusted in `src/extensions/http.rs`.
  - `FORMAT` function contract aligned in `src/sql/expr/functions/misc.rs`.
  - `CBRT` behavior aligned in `src/sql/expr/functions/math.rs`.
  - CASE alias inference improved in `src/sql/analyzer/query.rs`.
  - COALESCE constant-fold candidate pre-evaluation parity in `src/sql/expr/typed_eval.rs`.
  - Division-by-zero text aligned in `src/sql/error.rs`.

- PostgreSQL 17.7 validation performed:
  - `FORMAT('%.3s', 'hello')` emits `ERROR` + `HINT`.
  - `CBRT(27)` -> `3.0000000000000004`.
  - `CASE` output names and `COALESCE(a, 1/0)` error behavior in `tests/116_conditional.sql`.
  - `SELECT VERSION() LIKE 'PostgreSQL%'` used to keep `tests/105_expr_functions.sql` PG-valid.

## Test Contract Update (PG-validated)

- Updated test SQL/expected:
  - `tests/105_expr_functions.sql`:
    - `VERSION() LIKE '%db9-server%'` -> `VERSION() LIKE 'PostgreSQL%'` (PG 17.7 validated).
  - `tests/105_expr_functions.expected` adjusted accordingly (`version_test = t`).

## Current Open Failure Buckets (51)

### O1 — COPY/ingest/output contract cluster

- `110_copy_from_stdin_column_mismatch.sql`
- `113_copy_from_stdin_blank_lines.sql`
- `114_copy_from_stdin_validation.sql`
- `124_copy_from_stdin_autocommit_atomic_issue32.sql`

### O2 — EXPLAIN/optimizer output contract cluster

- `26_explain.sql`
- `48_explain_analyze.sql`
- `95_limit_pushdown.sql`
- `226_optimizer_index_scan_explain.sql`
- `227_optimizer_result_equivalence.sql`
- `optimizer_pushdown_results.sql`

### O3 — DDL/catalog/schema/index compatibility cluster

- `21_views.sql`
- `221_alter_index_rename.sql`
- `221_drop_cascade_views.sql`
- `222_ddl_export.sql`
- `223_migrations.sql`
- `224_index_namespace.sql`
- `27_constraints.sql`
- `28_advanced_constraints.sql`
- `41_schemas.sql`
- `46_functions_triggers_ddl.sql`
- `47_schema_drop_functions.sql`
- `50_index_features.sql`
- `52_index_features_extended.sql`
- `54_index_layer2.sql`

### O4 — Type/function/date/encoding semantics cluster

- `59_math_functions.sql`
- `90_timezone_conversion.sql`
- `91_timestamp_precision.sql`
- `92_bytea_functions.sql`
- `93_encode_decode.sql`
- `44_date_type.sql`

### O5 — Query semantics/output cluster

- `100_aggregate_alias_fixes.sql`
- `121_any_all_null_3vl_issue43.sql`
- `146_case_sensitive_names.sql`
- `149_distinct_order_by_select_list.sql`
- `159_varchar_metadata.sql`
- `172_ordinality.sql`
- `180_propagate_input_ordering.sql`
- `19_subqueries.sql`
- `22_new_features.sql`
- `25_phase1_features.sql`
- `32_advanced_procedures.sql`
- `33_transaction_consistency.sql`
- `39_enum_types.sql`
- `42_dollar_quote.sql`
- `49_plpgsql_functions.sql`
- `51_numeric_dollar_quote.sql`
- `71_lateral_join.sql`
- `75_grouping_sets.sql`
- `78_update_delete_variants.sql`
- `79_comparison_operators.sql`
- `222_structural_fixes_contract.sql`

## Next Root-Cause Plan

1. Close O1 (COPY cluster) first as one execution-path batch.
2. Close O2 (EXPLAIN/optimizer contract) second; keep results-parity and explain-parity synchronized.
3. Then handle O3 (DDL/catalog/schema/index) as one catalog/DDL invariant batch.
4. For any expected-output updates in O4/O5, run the exact SQL on PostgreSQL 17.7 before changing expectations.
