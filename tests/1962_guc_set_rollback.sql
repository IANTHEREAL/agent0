-- Issue #1962 P3: Regular SET and RESET must be rolled back on ROLLBACK.
-- Uses SHOW (not current_setting) because SHOW reads live session state
-- while current_setting reads from the per-statement snapshot.
\pset tuples_only on

-- 1) SET inside transaction + ROLLBACK restores prior value.
SET timezone = 'UTC';
BEGIN;
SET timezone = 'Asia/Shanghai';
SHOW timezone;
ROLLBACK;
SHOW timezone;

-- 2) SET inside transaction + COMMIT persists.
BEGIN;
SET timezone = 'US/Eastern';
COMMIT;
SHOW timezone;

-- 3) SAVEPOINT: SET + ROLLBACK TO restores.
BEGIN;
SET timezone = 'UTC';
SAVEPOINT sp1;
SET timezone = 'Europe/London';
SHOW timezone;
ROLLBACK TO sp1;
SHOW timezone;
COMMIT;

-- 4) SAVEPOINT: SET + RELEASE preserves.
BEGIN;
SET timezone = 'UTC';
SAVEPOINT sp2;
SET timezone = 'Asia/Tokyo';
RELEASE sp2;
SHOW timezone;
ROLLBACK;
SHOW timezone;

-- 5) RESET inside transaction + ROLLBACK restores prior value.
SET timezone = 'Asia/Shanghai';
BEGIN;
RESET timezone;
SHOW timezone;
ROLLBACK;
SHOW timezone;

-- 6) SET only inside savepoint + ROLLBACK TO + ROLLBACK.
SET timezone = 'US/Eastern';
BEGIN;
SAVEPOINT sp3;
SET timezone = 'Europe/Berlin';
ROLLBACK TO sp3;
SHOW timezone;
ROLLBACK;
SHOW timezone;

-- Reset to default.
RESET timezone;
