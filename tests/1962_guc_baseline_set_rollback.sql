-- GUC baseline: regular SET transaction rollback behavior.
-- P0 tests for #1962 GUC architecture refactoring.
-- Tests that work with CURRENT db9 behavior (pre-refactoring).
-- Uses .assert format to check key invariants that already hold.

\pset tuples_only on

-- ============================================================
-- 1) Regular SET persists through COMMIT
-- ============================================================
SET timezone = 'UTC';
BEGIN;
SET timezone = 'Asia/Shanghai';
SELECT 'commit_in_txn' AS test, current_setting('timezone') AS tz;
COMMIT;
SELECT 'commit_after' AS test, current_setting('timezone') AS tz;

-- ============================================================
-- 2) SET LOCAL reverts at COMMIT (must preserve)
-- ============================================================
SET timezone = 'UTC';
BEGIN;
SET LOCAL timezone = 'Asia/Shanghai';
SELECT 'local_in_txn' AS test, current_setting('timezone') AS tz;
COMMIT;
SELECT 'local_after_commit' AS test, current_setting('timezone') AS tz;

-- ============================================================
-- 3) SET LOCAL reverts at ROLLBACK (must preserve)
-- ============================================================
SET timezone = 'UTC';
BEGIN;
SET LOCAL timezone = 'Asia/Shanghai';
SELECT 'local_in_txn_rb' AS test, current_setting('timezone') AS tz;
ROLLBACK;
SELECT 'local_after_rollback' AS test, current_setting('timezone') AS tz;

-- ============================================================
-- 4) SET LOCAL + SAVEPOINT + ROLLBACK TO (must preserve)
-- ============================================================
SET timezone = 'UTC';
BEGIN;
SET LOCAL timezone = 'Asia/Shanghai';
SAVEPOINT sp1;
SET LOCAL timezone = 'America/New_York';
SELECT 'sp_inner' AS test, current_setting('timezone') AS tz;
ROLLBACK TO sp1;
SELECT 'sp_restored' AS test, current_setting('timezone') AS tz;
COMMIT;
SELECT 'sp_after_commit' AS test, current_setting('timezone') AS tz;

-- ============================================================
-- 5) Autocommit SET is permanent
-- ============================================================
SET timezone = 'UTC';
SET timezone = 'Asia/Shanghai';
SELECT 'autocommit' AS test, current_setting('timezone') AS tz;

-- ============================================================
-- 6) Regular SET removes active local override
-- ============================================================
SET application_name = 'base';
BEGIN;
SET LOCAL application_name = 'local_val';
SELECT 'before_regular_set' AS test, current_setting('application_name') AS val;
SET application_name = 'regular_val';
SELECT 'after_regular_set' AS test, current_setting('application_name') AS val;
COMMIT;

-- Cleanup
RESET timezone;
RESET application_name;
