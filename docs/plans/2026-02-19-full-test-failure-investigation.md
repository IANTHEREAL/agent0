# Full Test Failure Investigation (2026-02-19)

- Updated: 2026-02-19 09:51 UTC
- Branch: `issue-844-architecture-cleanup`
- Commit: `216dfba`
- PR: `https://github.com/c4pt0r/db9/pull/846`

## Latest Update (2026-02-19 20:00 UTC)

- Goal:
  - Close CI `regression-gate` failures and run all commands listed in `docs/testing.md`.

- Root causes closed:
  1. Unit-test expectation drift (PG contract mismatch)
     - Fixed in:
       - `src/sql/dml.rs` (enum error-name expectation)
       - `src/sql/expr/typed_eval.rs` (AT TIME ZONE numeric-offset expectation)
  2. Full-suite ORM timeout in `run_tests.sh`
     - Root cause: ORM suite reused integration-polluted `postgres` DB.
     - Fix: run ORM in isolated per-run database in `run_tests.sh`.

- Verification runs:
  - `cargo test` -> `1706 passed / 0 failed`
  - `bash scripts/regression_gate.sh` -> pass (`SQL 37/37`, `ORM 122/122`)
  - `./run_tests.sh` -> pass
    - Integration: `245 passed / 0 failed`
    - ORM: `585 passed / 0 failed / 1 skipped`
    - Report: `test-reports/test-report-20260219-195118-9d863e6.md`
  - `python3 scripts/integration_test.py --dsn <local>` (built-in mode) -> `8 passed / 0 failed`
  - `PG_DSN=<local> bash scripts/e2e_tests.sh gorm_smoke` -> pass
  - `PG_DSN=<local> bash scripts/e2e_tests.sh sqlalchemy_smoke` -> pass
  - `PG_DSN=<local> bash scripts/e2e_tests.sh dify_sqlalchemy_compat` -> pass

## Latest Update (2026-02-19 19:22 UTC)

- Commands:
  - `cargo build --release`
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests/222_structural_fixes_contract.sql tests/39_enum_types.sql tests/75_grouping_sets.sql`
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`

- Results:
  - Focused bucket: `3 passed / 0 failed`
  - Full suite: `245 passed / 0 failed / 0 skipped`
  - Final status: **all integration tests green**

- Evidence:
  - `/tmp/issue844_focus_after_restart.log`
  - `/tmp/integration_issue844_after_groupingsets_20260219_192135.log`

### Root Causes Closed In This Iteration

1. `GROUPING SETS/CUBE/ROLLUP` analyzed-path gap
   - File: `src/sql/analyzer/query.rs`
   - Fix: rewrite grouping extensions into deterministic `UNION ALL` grouping-arm queries, including `GROUPING(...)` bitmask lowering.

2. Vector empty-dimension PostgreSQL contract mismatch
   - File: `src/sql/types/cast.rs`
   - Fix: reject empty vectors with PG-style message and align dimension mismatch wording.

3. `39_enum_types.sql` fixture idempotency gap
   - File: `tests/39_enum_types.sql`
   - Fix: explicit composite table/type cleanup at start/end while preserving RESTRICT-drop semantic check.

4. PG17.7-validated expectation update
   - File: `tests/222_structural_fixes_contract.expected`
   - Validation evidence:
     - `/tmp/pg17_222.stdout`
     - `/tmp/pg17_222.stderr`

## Latest Update (2026-02-19 15:56 UTC)

- Commands:
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests/186_worker_bg_sql.sql tests/187_worker_cic.sql`
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
- Results:
  - Focused worker bucket: `2 passed / 0 failed`
  - Full suite: `222 passed / 23 failed / 0 skipped`
  - Delta from previous baseline (`221/24`): `+1 passed / -1 failed`
- Evidence:
  - `/tmp/integration_worker_186_187_after_fix.log`
  - `/tmp/integration_after_worker.log`

### Root Cause Closed In This Iteration

- Worker keyspace startup idempotency (`src/worker/mod.rs`):
  - PD v2 may return `500 "keyspace already exists"` for idempotent keyspace create.
  - Old logic treated `500/503` as retry-only and skipped immediate existence verification, causing startup delay.
  - New logic performs GET verification for `500/503` before retrying.
  - Outcome: worker-focused tests green and full-suite fail count reduced by 1.

## Latest Update (2026-02-19 16:08 UTC)

- Commands:
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests/19_subqueries.sql tests/32_advanced_procedures.sql tests/33_transaction_consistency.sql tests/71_lateral_join.sql tests/78_update_delete_variants.sql`
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
- Results:
  - Focused DML/subquery bucket: `3 passed / 2 failed`
  - Full suite: `224 passed / 21 failed / 0 skipped`
  - Delta vs previous baseline `222/23`: net `+2`
- Evidence:
  - `/tmp/integration_subquery_bucket_after_fix.log`
  - `/tmp/integration_after_dml_async_fix.log`

### Root Cause Closed In This Iteration

- Async expression materialization gap in analyzed DML path (`src/sql/executor/dml_analyzed.rs`):
  - Previous behavior: DML path ran `eval_typed_expr` directly for WHERE/SET/VALUES/ON CONFLICT expressions and only materialized sequence calls in assignments.
  - Failure mode: unresolved subquery expressions reached typed evaluator and raised
    `subquery expressions must be resolved at executor level`.
  - New behavior: expression evaluation now uses DML-side `eval_typed_expr_maybe_async`, delegating to `materialize_expr_for_row` for async expressions.
  - Confirmed fixed:
    - `tests/32_advanced_procedures.sql`
    - `tests/33_transaction_consistency.sql`
    - `tests/78_update_delete_variants.sql`

### New Observation (Flaky/External)

- `tests/91_http_extension.sql` failed once in full run (`missing expected output: 200|array`) but passed in immediate focused rerun.
- Focused rerun evidence: `/tmp/integration_91_http_extension_recheck.log`

## Latest Update (2026-02-19 16:14 UTC)

- Commands:
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests/121_any_all_null_3vl_issue43.sql`
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
- Results:
  - Focused ANY/ALL 3VL: `1 passed / 0 failed`
  - Full suite: `226 passed / 19 failed / 0 skipped`
  - Delta vs previous baseline `224/21`: net `+2`
- Evidence:
  - `/tmp/integration_121_after_anyall_fix.log`
  - `/tmp/integration_after_anyall_fix.log`

### Root Cause Closed In This Iteration

- ANY/ALL casted-array literal lowering gap (`src/sql/analyzer/expr.rs`):
  - Previous behavior only handled direct `ArrayLiteral`; `ARRAY[...]::int[]` skipped literal path.
  - This broke SQL 3VL expectations for NULL-containing ANY/ALL arrays in Issue #43 test.
  - New behavior extracts cast-wrapped array literals and applies the same literal lowering for `ANY` and `ALL`.
  - Confirmed fixed:
    - `tests/121_any_all_null_3vl_issue43.sql`

## Latest Update (2026-02-19 10:53 UTC)

- Command:
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
- Result:
  - `194 passed`, `51 failed`, `0 skipped`
- Evidence:
  - `/tmp/integration_all_after_rc_json_func_conditional.log`

### New Closures in This Cycle

- Previously failing focused bucket is now fully green:
  - `tests/88_http_permission.sql`
  - `tests/72_json_advanced.sql`
  - `tests/73_system_functions.sql`
  - `tests/105_expr_functions.sql`
  - `tests/116_conditional.sql`

### Current Failed Cases Count

- Total failed files: **51**
- Fast extract:
  - `rg -n "\\[FAILED\\]" /tmp/integration_all_after_rc_json_func_conditional.log`

## Task

Run full tests, summarize all failures, and investigate root causes with concrete evidence.

## Commands Executed

1. `cargo test`
2. `python3 scripts/integration_test.py tests`
3. `cd orm-tests && npm test`
4. Focused ORM repro: `cd orm-tests && DEBUG=true npx vitest run typeorm/schema.test.ts -t "idempotently" --testTimeout=180000`

## Environment Verification

### Initial failing environment (stale target)

- Listener on `5433` was from deleted workspace binary:
  - `/home/zhaiyl/Work/agents/w2/db9/target/debug/db9-server (deleted)`
- Version observed then: `db9 0.1.0-92287e20`

### Corrected environment (this branch)

- Stopped stale `w2` process.
- Started clean TiKV dev cluster: `PD_ENDPOINTS=127.0.0.1:12379`.
- Started server from current workspace.
- Verified runtime:
  - `version() = PostgreSQL 16.0 (db9 0.1.0-216dfba4 2026-02-19 on TiKV)`

## Test Results (Corrected Environment)

- `cargo test`: **PASS** (`1686 passed`, `0 failed`)
- SQL integration: **FAIL** (`146 passed`, `99 failed`, `0 skipped`)
- ORM: **FAIL** (`720 passed`, `1 failed`, `7 skipped`)

## Delta vs Stale-Target Run

- Stale-target run had `101` SQL failures.
- Corrected run has `99` SQL failures.
- Files that stopped failing after environment fix:
  - `64_subquery_patterns.sql`
  - `86_extensions_framework_http.sql`

Interpretation: stale runtime mismatch was real and affected outcomes, but most failures are genuine in current branch/runtime as well.

## SQL Failure Taxonomy (99)

- `output differs from expected`: **72**
- `SQL errors detected`: **14**
- `unexpected SQL errors`: **5**
- `missing expected output` variants: **8**

Top surfaced SQL error signatures from integration log:

- `column "missing_col" does not exist` (4)
- `Invalid JSON: expected value at line 1 column 1` (3)
- `unknown function: TO_CHAR`
- `unknown function: BASIC_ADD` / `BASIC_MULTIPLY` / `REPLACE_TEST` / `TEST_ELSIF` / `TEST_INT4` / `TEST_RAISE_EXCEPTION`
- `unsupported table-valued function: match_documents`
- `unsupported table-valued function: _db9_sys_export_ddl`
- `pg_background_launch: worker engine not available`
- `expected BOOLEAN, found TEXT/INT in WHERE clause`
- `cannot cast type DATE to TIMESTAMPTZ`

## Root Cause Analysis

### RC1 — Runtime Target Drift (confirmed, partially resolved)

Earlier failures were run against stale `w2` binary (`db9 0.1.0-92287e20`). After correcting to `216dfba`, two failures disappeared. This explains part of the mismatch but not the remaining 99 failures.

### RC2 — Worker subsystem unavailable in current local bring-up

Server logs include:

- `Failed to initialize isolated system keyspace '_sys_worker'; refusing fallback. Worker engine not started.`

This directly explains worker/background-related failures (e.g., `186_worker_bg_sql.sql`, parts of `187_worker_cic.sql`, async/background behavior expectations).

### RC3 — Broad compatibility/expectation drift (not isolated to #844 touch points)

Most failures are output drift across many unrelated domains (catalog/introspection, functions, DDL, JSON, EXPLAIN plans, permissions). This shape does not indicate a single regression introduced by Issue #844 refactor files.

### RC4 — Function/TVF coverage gaps visible in failing files

`unknown function` and `unsupported table-valued function` errors indicate missing/disabled compatibility surface for specific tests (UDF/TVF paths and helper functions), causing SQL-errors class failures.

### RC5 — Semantics tightened in predicate typing paths

Errors like `expected BOOLEAN, found TEXT/INT in WHERE clause` indicate strict boolean predicate typing behavior. Some expected outputs appear to assume different behavior, producing output/error expectation mismatches.

### RC6 — EXPLAIN/optimizer expected text drift

Several failures are exact-text missing expectations in plan output (`26_explain.sql`, `48_explain_analyze.sql`, `95_limit_pushdown.sql`, `226_optimizer_index_scan_explain.sql`, `227_optimizer_result_equivalence.sql`), consistent with textual/format/cost drift rather than single crash failure.

### RC7 — ORM failure is a timeout/performance issue

Failing case:

- `typeorm/schema.test.ts` -> `synchronize idempotency` (second `initialize`)

Evidence:

- Full ORM run: timeout at 30s.
- Focused run with `--testTimeout=180000`: **passes** in `140781ms`.
- Queries emitted during long path include repeated schema introspection and `ALTER TABLE ... ALTER COLUMN ... TYPE TIMESTAMP WITH TIME ZONE`.

Interpretation: this is a slow DDL/introspection path, not a deterministic semantic assertion failure.

## In-Scope Conclusion for Issue #844

No evidence from this run that Issue #844 refactor introduced a discrete new functional regression. The failure set is broad/systemic and mostly pre-existing compatibility/performance debt; environment mismatch originally amplified confusion.

## Full SQL Failed Cases (99)

| File | Reason |
|---|---|
| `100_aggregate_alias_fixes.sql` | output differs from expected |
| `100_current_setting.sql` | output differs from expected |
| `101_server_introspection.sql` | output differs from expected |
| `105_expr_functions.sql` | output differs from expected |
| `107_alter_default_privileges_for_table.sql` | output differs from expected |
| `107_join_on_truthiness.sql` | output differs from expected |
| `107_join_using_natural_full_right_issue86.sql` | output differs from expected |
| `108_boolean_text_where.sql` | unexpected SQL errors |
| `108_chained_natural_using_join_wildcard.sql` | output differs from expected |
| `110_copy_from_stdin_column_mismatch.sql` | output differs from expected |
| `113_copy_from_stdin_blank_lines.sql` | output differs from expected |
| `114_copy_from_stdin_validation.sql` | output differs from expected |
| `115_join_using_outer_select_star_issue55.sql` | output differs from expected |
| `116_conditional.sql` | output differs from expected |
| `116_join_using_outer_unqualified_refs_issue86.sql` | output differs from expected |
| `120_savepoint_trigger_enqueue_undo_issue34.sql` | output differs from expected |
| `121_any_all_null_3vl_issue43.sql` | output differs from expected |
| `122_trigger_substitution_prefix_collisions_issue23.sql` | output differs from expected |
| `124_copy_from_stdin_autocommit_atomic_issue32.sql` | output differs from expected |
| `126_pg_type_is_visible.sql` | output differs from expected |
| `130_fts.sql` | output differs from expected |
| `131_gin_fts.sql` | missing expected output: 0.06079271 |
| `132_udf_setof_vector.sql` | SQL errors detected |
| `133_alter_default_privileges_global_tables.sql` | output differs from expected |
| `133_alter_default_privileges_multi_owner.sql` | output differs from expected |
| `134_alter_default_privileges_revoke_future_only.sql` | output differs from expected |
| `134_alter_default_privileges_with_grant_option.sql` | output differs from expected |
| `135_alter_default_privileges_tables_apply_to_views.sql` | output differs from expected |
| `135_grant_on_all_tables_in_schema.sql` | output differs from expected |
| `140_join_using_qualified_wildcard_issue424.sql` | output differs from expected |
| `146_case_sensitive_names.sql` | output differs from expected |
| `149_distinct_order_by_select_list.sql` | output differs from expected |
| `150_lateral_error_propagation.sql` | unexpected SQL errors |
| `159_varchar_metadata.sql` | output differs from expected |
| `172_ordinality.sql` | output differs from expected |
| `180_propagate_input_ordering.sql` | output differs from expected |
| `186_worker_bg_sql.sql` | SQL errors detected |
| `187_worker_cic.sql` | missing expected output: 1:b_count_after_conflict |
| `19_subqueries.sql` | SQL errors detected |
| `219_gin_chinese_fts.sql` | output differs from expected |
| `219_window_error_propagation.sql` | unexpected SQL errors |
| `21_views.sql` | output differs from expected |
| `221_alter_index_rename.sql` | output differs from expected |
| `221_drop_cascade_views.sql` | output differs from expected |
| `222_ddl_export.sql` | SQL errors detected |
| `222_structural_fixes_contract.sql` | output differs from expected |
| `223_migrations.sql` | output differs from expected |
| `224_index_namespace.sql` | output differs from expected |
| `226_optimizer_index_scan_explain.sql` | missing expected output: -- Test 1: Point lookup on indexed column should show Index Scan |
| `227_optimizer_result_equivalence.sql` | missing expected output: -- Result equivalence: optimizer ON must produce same results as optimizer OFF. |
| `22_new_features.sql` | output differs from expected |
| `24_json_comprehensive.sql` | output differs from expected |
| `25_phase1_features.sql` | SQL errors detected |
| `26_explain.sql` | missing expected output: Seq Scan on explain_test |
| `27_constraints.sql` | output differs from expected |
| `28_advanced_constraints.sql` | output differs from expected |
| `32_advanced_procedures.sql` | output differs from expected |
| `33_transaction_consistency.sql` | SQL errors detected |
| `39_enum_types.sql` | output differs from expected |
| `41_schemas.sql` | output differs from expected |
| `42_dollar_quote.sql` | output differs from expected |
| `44_date_type.sql` | output differs from expected |
| `45_date_type_improvements.sql` | SQL errors detected |
| `46_functions_triggers_ddl.sql` | output differs from expected |
| `47_schema_drop_functions.sql` | output differs from expected |
| `48_explain_analyze.sql` | missing expected output: Sort  (cost=9976.78..9976.78 rows=1000 width=36) |
| `49_plpgsql_functions.sql` | output differs from expected |
| `50_index_features.sql` | output differs from expected |
| `51_numeric_dollar_quote.sql` | output differs from expected |
| `52_index_features_extended.sql` | output differs from expected |
| `54_index_layer2.sql` | output differs from expected |
| `58_string_functions.sql` | output differs from expected |
| `59_math_functions.sql` | output differs from expected |
| `60_datetime_functions.sql` | SQL errors detected |
| `71_lateral_join.sql` | output differs from expected |
| `72_json_advanced.sql` | output differs from expected |
| `73_system_functions.sql` | output differs from expected |
| `75_grouping_sets.sql` | output differs from expected |
| `78_update_delete_variants.sql` | output differs from expected |
| `79_comparison_operators.sql` | output differs from expected |
| `80_string_operations.sql` | output differs from expected |
| `81_udf_comprehensive.sql` | unexpected SQL errors |
| `83_udf_elsif_issue.sql` | SQL errors detected |
| `84_udf_advanced.sql` | SQL errors detected |
| `85_udf_types_and_params.sql` | SQL errors detected |
| `86_async_triggers.sql` | output differs from expected |
| `88_http_permission.sql` | unexpected SQL errors |
| `90_functions_comprehensive.sql` | SQL errors detected |
| `90_timezone_conversion.sql` | output differs from expected |
| `91_http_extension.sql` | SQL errors detected |
| `91_timestamp_precision.sql` | output differs from expected |
| `92_bytea_functions.sql` | output differs from expected |
| `93_encode_decode.sql` | output differs from expected |
| `94_gin_index_query.sql` | missing expected output: metadata @> '{"type": "pdf"}'::jsonb |
| `95_limit_pushdown.sql` | missing expected output: Index Scan using idx_limit_pushdown_a on limit_pushdown_t |
| `98_session_settings.sql` | output differs from expected |
| `99_statement_timeout.sql` | output differs from expected |
| `basic_compat.sql` | SQL errors detected |
| `optimizer_pushdown_results.sql` | output differs from expected |

## Evidence Files

- `/tmp/cargo_test_w3.log`
- `/tmp/integration_all_w3.log`
- `/tmp/integration_failed_cases_w3.txt`
- `/tmp/orm_test_w3.log`
- `/tmp/typeorm_schema_idempotency_w3.log`
- `orm-tests/test-results.json`

## Recommended Next Triage Slice

1. Stabilize worker keyspace init in local/dev setup (or gate worker-dependent tests when worker engine unavailable).
2. Prioritize SQL-errors class (`14 + 5`) before output-diff class (`72`) because they indicate stronger semantic gaps.
3. For ORM timeout, profile second synchronize path around `ALTER TABLE ... TYPE TIMESTAMPTZ` and catalog introspection query latency.

## Addendum: CI-Like Clean Rerun (2026-02-19 07:50 UTC)

To eliminate local-environment drift, tests were rerun with a CI-equivalent stack:

- fresh `tiup playground` on `PD 127.0.0.1:2379`
- `cargo build --release`
- `DB9_BOOTSTRAP_ADMIN_USER=admin`
- `DB9_BOOTSTRAP_ADMIN_PASSWORD=admin`
- explicit `_sys_worker` keyspace creation via PD API v2 before db9-server startup

### Updated Results (CI-like stack)

- SQL integration: `148 passed`, `97 failed`, `0 skipped`
- ORM: `720 passed`, `1 failed`, `7 skipped`

### Delta vs previous corrected run (99 failures)

- Newly passing after `_sys_worker` availability:
  - `186_worker_bg_sql.sql`
  - `187_worker_cic.sql`

### Root-Cause Refinement

- Worker failures were environmental (missing `_sys_worker` keyspace), not core SQL semantics.
- Remaining `97` failures persist even under CI-like runtime, confirming real product/test debt.
- Dominant buckets remain:
  - `72` output diffs
  - `13` SQL errors
  - `5` unexpected SQL errors
  - `7` missing-expected-text variants

### Representative Structural Gaps in New Architecture Path

- UDF call resolution in analyzed typed execution path:
  - `CREATE FUNCTION` succeeds but call sites resolve as `unknown function` in several suites.
- Function surface parity gaps in typed evaluator path (e.g., `TO_CHAR`, `AGE`).
- Aggregate-in-expression handling regressions (`ARRAY_AGG` nested in predicates/CASE/COALESCE).
- Session/GUC behavioral drift from PostgreSQL expectations (search_path/statement_timeout formatting).

### Evidence Artifacts (addendum)

- `/tmp/integration_all_ci_like.log`
- `/tmp/integration_failed_cases_ci_like.txt`
- `/tmp/orm_test_ci_like.log`

## Addendum: Post-UDF Root-Cause Fix Rerun (2026-02-19, latest)

### Code changes validated in this cycle

- UDF/function catalog prefetch hardening in analyzed execution snapshot path.
- Async expression classification + materialization for UDF calls in optimizer runtime (`Filter`/`Project`).
- `RETURNS SETOF <table>` user table-function schema prefetch.
- `LIMIT/OFFSET` constant-expression folding extension for `MinMax` typed nodes.
- Unit test compatibility fix for async `ProjectOperator::project_row` signature change.

### Current verified test results

- `cargo test`: **PASS** (`1688 passed`, `0 failed`)
- Focused UDF bucket:
  - `tests/81_udf_comprehensive.sql` **PASS**
  - `tests/83_udf_elsif_issue.sql` **PASS**
  - `tests/84_udf_advanced.sql` **PASS**
  - `tests/85_udf_types_and_params.sql` **PASS**
  - `tests/132_udf_setof_vector.sql` **PASS**
- Full SQL integration rerun:
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
  - **Result:** `153 passed`, `92 failed`, `0 skipped`

### Delta vs previous `99`-failure baseline

Resolved in this cycle (no new failures introduced):

- `81_udf_comprehensive.sql`
- `83_udf_elsif_issue.sql`
- `84_udf_advanced.sql`
- `85_udf_types_and_params.sql`
- `132_udf_setof_vector.sql`
- `186_worker_bg_sql.sql`
- `187_worker_cic.sql`

### Updated interpretation

- UDF resolution/materialization root-cause bucket is now closed.
- Worker-keyspace bootstrap instability is no longer reproducing in current environment.
- Remaining `92` failures are still broad cross-domain compatibility debt, now isolated from the UDF/worker buckets fixed above.

### Latest evidence artifacts

- `/tmp/integration-full-20260219-after-udf-fixes.log`
- `/tmp/integration-full-20260219.log`

## Addendum: Session + Predicate Root-Cause Fixes (2026-02-19, latest)

### Code/Test changes validated in this cycle

- Session/settings parity:
  - `set_config()` returns newly set value (PG semantics).
  - timeout readback uses PG-like `Nms` formatting.
  - default/reset `search_path` aligned to `"$user", public`.
  - internal default-schema resolution skips `$user` placeholder.
  - wire `ParameterStatus(search_path)` aligned with SQL readback.
- Catalog compat:
  - `pg_type_is_visible(NULL)` and `pg_table_is_visible(NULL)` now return `NULL`.
- Analyzer predicate coercion:
  - boolean contexts now allow UNKNOWN-like constants:
    - `'true'/'false'` -> implicit bool cast
    - `NULL` -> boolean-typed NULL
  - applied in query + DML analyzed paths.
- Error-contract refresh (PG-validated substrings):
  - `tests/108_boolean_text_where.errors`
  - `tests/150_lateral_error_propagation.errors`
  - `tests/150_lateral_error_propagation.assert`
  - `tests/219_window_error_propagation.errors`

### PostgreSQL 17.7 verification run

- `SHOW search_path` => `"$user", public`
- `SELECT current_setting('search_path')` => `"$user", public`
- `SELECT pg_type_is_visible(NULL)` => `NULL`
- `SELECT pg_table_is_visible(NULL)` => `NULL`
- `tests/108_boolean_text_where.sql` emits WHERE-boolean-type error in PG
- `tests/150_lateral_error_propagation.sql` emits ambiguous-id + WHERE-boolean + missing-col errors in PG
- `tests/219_window_error_propagation.sql` emits missing-col errors in PG

### Current verified test results

- `cargo test`: **PASS** (`1691 passed`, `0 failed`)
- Focused session/catalog/predicate bucket:
  - `tests/98_session_settings.sql` **PASS**
  - `tests/99_statement_timeout.sql` **PASS**
  - `tests/100_current_setting.sql` **PASS**
  - `tests/101_server_introspection.sql` **PASS**
  - `tests/126_pg_type_is_visible.sql` **PASS**
  - `tests/107_join_on_truthiness.sql` **PASS**
  - `tests/108_boolean_text_where.sql` **PASS**
  - `tests/150_lateral_error_propagation.sql` **PASS**
  - `tests/219_window_error_propagation.sql` **PASS**
- Full SQL integration rerun:
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
  - **Result:** `165 passed`, `80 failed`, `0 skipped`

### Delta vs previous `92`-failure baseline

Resolved in this cycle (no new failures introduced):

- `98_session_settings.sql`
- `99_statement_timeout.sql`
- `100_current_setting.sql`
- `101_server_introspection.sql`
- `126_pg_type_is_visible.sql`
- `107_join_on_truthiness.sql`
- `108_boolean_text_where.sql`
- `150_lateral_error_propagation.sql`
- `219_window_error_propagation.sql`

### Updated failure taxonomy (80)

- `63` output diffs
- `9` SQL errors
- `1` unexpected SQL errors
- `7` missing-expected-output

### Latest evidence artifacts

- `/tmp/integration_all_w3_after_rc_session_visibility.log`
- `/tmp/integration_all_w3_after_rc_boolean.log`

## Addendum: ACL + JOIN USING Root-Cause Closures (2026-02-19, latest)

### Code/Test changes validated in this cycle

- Default privileges / ACL ownership semantics:
  - `CREATE SCHEMA ... AUTHORIZATION` now seeds schema owner privileges (`USAGE`, `CREATE`) with grant option.
  - `GRANT/REVOKE` authority now follows PG-like semantics:
    - superuser OR grant-option OR object owner (table objects).
  - Removed hard superuser-only gate from runtime GRANT/REVOKE path.
- JOIN USING/NATURAL merged-column semantics:
  - merged-column metadata is tracked in analyzer scope.
  - unqualified merged refs are analyzed as `COALESCE(left, right)` with unified type coercion.
  - wildcard projection order for chained `USING/NATURAL` joins now follows join-plan flattening.
  - qualified wildcard (`table.*`) keeps hidden USING right-side duplicates (PG-compatible shape).

### Current verified test results

- Focused ACL bucket:
  - `tests/107_alter_default_privileges_for_table.sql` **PASS**
  - `tests/133_alter_default_privileges_global_tables.sql` **PASS**
  - `tests/133_alter_default_privileges_multi_owner.sql` **PASS**
  - `tests/134_alter_default_privileges_revoke_future_only.sql` **PASS**
  - `tests/134_alter_default_privileges_with_grant_option.sql` **PASS**
  - `tests/135_alter_default_privileges_tables_apply_to_views.sql` **PASS**
  - `tests/135_grant_on_all_tables_in_schema.sql` **PASS**
- Focused JOIN USING bucket:
  - `tests/107_join_using_natural_full_right_issue86.sql` **PASS**
  - `tests/108_chained_natural_using_join_wildcard.sql` **PASS**
  - `tests/115_join_using_outer_select_star_issue55.sql` **PASS**
  - `tests/116_join_using_outer_unqualified_refs_issue86.sql` **PASS**
  - `tests/140_join_using_qualified_wildcard_issue424.sql` **PASS**
- Full SQL integration rerun:
  - `python3 scripts/integration_test.py --dsn postgres://admin:admin@127.0.0.1:5433/postgres tests`
  - **Result:** `177 passed`, `68 failed`, `0 skipped`

### Delta vs previous `80`-failure baseline

Resolved in this cycle (no new failures introduced):

- `107_alter_default_privileges_for_table.sql`
- `133_alter_default_privileges_global_tables.sql`
- `133_alter_default_privileges_multi_owner.sql`
- `134_alter_default_privileges_revoke_future_only.sql`
- `134_alter_default_privileges_with_grant_option.sql`
- `135_alter_default_privileges_tables_apply_to_views.sql`
- `135_grant_on_all_tables_in_schema.sql`
- `107_join_using_natural_full_right_issue86.sql`
- `108_chained_natural_using_join_wildcard.sql`
- `115_join_using_outer_select_star_issue55.sql`
- `116_join_using_outer_unqualified_refs_issue86.sql`
- `140_join_using_qualified_wildcard_issue424.sql`

### Updated failure taxonomy (68)

- `51` output diffs
- `9` SQL errors
- `1` unexpected SQL errors
- `7` missing-expected-output

### Remaining high-signal buckets

- FTS/GIN/ranking parity (`130`, `131`, `219`, `94`)
- EXPLAIN/assert contract drift (`26`, `48`, `95`, `226`, `227`, `optimizer_pushdown_results`)
- HTTP/JSON error-surface cluster (`88`, `90`, `91`, `basic_compat`)

### Evidence artifacts (latest)

- `/tmp/integration_all_after_rc_acl.log`
- `/tmp/integration_all_after_rc_join_using.log`
- `/tmp/failed_after_rc_join_using.txt`
- `/tmp/failed_after_rc_join_using_cases.txt`
