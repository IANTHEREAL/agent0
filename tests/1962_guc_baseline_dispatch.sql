-- GUC baseline: dispatch consistency and dotted-name behavior.
-- P0 tests for #1962 GUC architecture refactoring.

\pset tuples_only on

-- ============================================================
-- 1) SHOW for unknown dotted GUC
-- CURRENT: returns error "unrecognized configuration parameter"
-- TARGET:  returns empty string (PG parity)
-- ============================================================
-- (This will fail with error in current code. After refactoring, should succeed.)
-- SHOW unknown.dotted.guc;

-- ============================================================
-- 2) current_setting for unknown dotted GUC (missing_ok = true)
-- CURRENT: returns NULL (correct, must preserve)
-- ============================================================
SELECT current_setting('unknown.dotted.guc', true) IS NULL AS unknown_dotted_null;

-- ============================================================
-- 3) current_setting for unknown non-dotted GUC (missing_ok = true)
-- CURRENT: returns NULL (correct, must preserve)
-- ============================================================
SELECT current_setting('nonexistent', true) IS NULL AS unknown_nondotted_null;

-- ============================================================
-- 4) default_transaction_isolation SHOW value
-- CURRENT: returns 'read committed' (hardcoded)
-- TARGET:  remains 'read committed' by default, but SET accepted
-- ============================================================
SHOW default_transaction_isolation;

-- ============================================================
-- 5) transaction_isolation normalization
-- CURRENT: all levels normalized to 'repeatable read'
-- TARGET:  SHOW returns user-set value
-- ============================================================
SET default_transaction_read_only = off;
SHOW default_transaction_read_only;

-- ============================================================
-- 6) set_config + current_setting round-trip (must preserve)
-- ============================================================
SELECT set_config('application_name', 'guc_test', false);
SELECT current_setting('application_name') AS app_name;

-- ============================================================
-- 7) set_config with is_local=true in transaction
-- ============================================================
SET application_name = 'base';
BEGIN;
SELECT set_config('application_name', 'local_val', true);
SELECT current_setting('application_name') AS in_txn;
COMMIT;
SELECT current_setting('application_name') AS after_commit;

-- ============================================================
-- 8) IntervalStyle current value
-- ============================================================
SHOW IntervalStyle;

-- ============================================================
-- 9) DateStyle current value
-- ============================================================
SHOW DateStyle;

-- Cleanup
RESET application_name;
