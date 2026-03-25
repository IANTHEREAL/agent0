-- Issue #1962 P3: Regular SET and RESET must be rolled back on ROLLBACK.

-- 1) SET inside transaction + ROLLBACK restores prior value.
SET timezone = 'UTC';
BEGIN;
SET timezone = 'Asia/Shanghai';
SELECT current_setting('timezone') AS during_txn;
ROLLBACK;
SELECT current_setting('timezone') AS after_rollback;

-- 2) SET inside transaction + COMMIT persists.
BEGIN;
SET timezone = 'US/Eastern';
COMMIT;
SELECT current_setting('timezone') AS after_commit;

-- 3) SAVEPOINT: SET + ROLLBACK TO restores.
BEGIN;
SET timezone = 'UTC';
SAVEPOINT sp1;
SET timezone = 'Europe/London';
SELECT current_setting('timezone') AS inside_savepoint;
ROLLBACK TO sp1;
SELECT current_setting('timezone') AS after_rollback_to;
COMMIT;

-- 4) SAVEPOINT: SET + RELEASE preserves.
BEGIN;
SET timezone = 'UTC';
SAVEPOINT sp2;
SET timezone = 'Asia/Tokyo';
RELEASE sp2;
SELECT current_setting('timezone') AS after_release;
ROLLBACK;
SELECT current_setting('timezone') AS after_outer_rollback;

-- 5) RESET inside transaction + ROLLBACK restores prior value.
SET timezone = 'Asia/Shanghai';
BEGIN;
RESET timezone;
SELECT current_setting('timezone') AS after_reset;
ROLLBACK;
SELECT current_setting('timezone') AS after_reset_rollback;

-- 6) SET only inside savepoint + ROLLBACK TO + ROLLBACK.
SET timezone = 'US/Eastern';
BEGIN;
SAVEPOINT sp3;
SET timezone = 'Europe/Berlin';
ROLLBACK TO sp3;
SELECT current_setting('timezone') AS after_sp_rollback_to;
ROLLBACK;
SELECT current_setting('timezone') AS after_full_rollback;

-- Reset to default.
RESET timezone;
