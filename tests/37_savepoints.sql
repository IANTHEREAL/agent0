-- Savepoint Tests
-- Purpose: Verify SAVEPOINT, RELEASE SAVEPOINT, and ROLLBACK TO SAVEPOINT

DROP TABLE IF EXISTS t_savepoint;
CREATE TABLE t_savepoint (
    id INT PRIMARY KEY,
    val INT
);

-- Test 1: Basic ROLLBACK TO SAVEPOINT
INSERT INTO t_savepoint VALUES (1, 100);

BEGIN;
SAVEPOINT sp1;
UPDATE t_savepoint SET val = 200 WHERE id = 1;
ROLLBACK TO SAVEPOINT sp1;
COMMIT;

SELECT val FROM t_savepoint WHERE id = 1; -- Should be 100

-- Test 2: ROLLBACK TO re-establishes savepoint (can rollback multiple times)
UPDATE t_savepoint SET val = 100 WHERE id = 1;

BEGIN;
SAVEPOINT sp1;
UPDATE t_savepoint SET val = 200 WHERE id = 1;
ROLLBACK TO SAVEPOINT sp1;
UPDATE t_savepoint SET val = 300 WHERE id = 1;
ROLLBACK TO SAVEPOINT sp1;
COMMIT;

SELECT val FROM t_savepoint WHERE id = 1; -- Should be 100

-- Test 3: RELEASE SAVEPOINT keeps changes
UPDATE t_savepoint SET val = 100 WHERE id = 1;

BEGIN;
SAVEPOINT sp1;
UPDATE t_savepoint SET val = 500 WHERE id = 1;
RELEASE SAVEPOINT sp1;
COMMIT;

SELECT val FROM t_savepoint WHERE id = 1; -- Should be 500

-- Test 4: Nested savepoints - rollback inner
UPDATE t_savepoint SET val = 100 WHERE id = 1;

BEGIN;
SAVEPOINT outer_sp;
UPDATE t_savepoint SET val = 200 WHERE id = 1;
SAVEPOINT inner_sp;
UPDATE t_savepoint SET val = 300 WHERE id = 1;
ROLLBACK TO SAVEPOINT inner_sp;
COMMIT;

SELECT val FROM t_savepoint WHERE id = 1; -- Should be 200

-- Test 5: Nested savepoints - rollback outer destroys inner
UPDATE t_savepoint SET val = 100 WHERE id = 1;

BEGIN;
SAVEPOINT outer_sp;
UPDATE t_savepoint SET val = 200 WHERE id = 1;
SAVEPOINT inner_sp;
UPDATE t_savepoint SET val = 300 WHERE id = 1;
ROLLBACK TO SAVEPOINT outer_sp;
COMMIT;

SELECT val FROM t_savepoint WHERE id = 1; -- Should be 100

-- Test 6: RELEASE inner, then ROLLBACK TO outer
UPDATE t_savepoint SET val = 100 WHERE id = 1;

BEGIN;
SAVEPOINT outer_sp;
SAVEPOINT inner_sp;
UPDATE t_savepoint SET val = 999 WHERE id = 1;
RELEASE SAVEPOINT inner_sp;
ROLLBACK TO SAVEPOINT outer_sp;
COMMIT;

SELECT val FROM t_savepoint WHERE id = 1; -- Should be 100

-- Test 7: Multiple keys with savepoints
INSERT INTO t_savepoint VALUES (2, 200);
INSERT INTO t_savepoint VALUES (3, 300);

BEGIN;
SAVEPOINT sp1;
UPDATE t_savepoint SET val = val + 1000 WHERE id IN (1, 2, 3);
ROLLBACK TO SAVEPOINT sp1;
COMMIT;

SELECT val FROM t_savepoint ORDER BY id; -- Should be 100, 200, 300

-- Test 8: DELETE with savepoint rollback
BEGIN;
SAVEPOINT sp1;
DELETE FROM t_savepoint WHERE id = 2;
ROLLBACK TO SAVEPOINT sp1;
COMMIT;

SELECT COUNT(*) FROM t_savepoint; -- Should be 3

-- Test 9: INSERT with savepoint rollback
BEGIN;
SAVEPOINT sp1;
INSERT INTO t_savepoint VALUES (4, 400);
ROLLBACK TO SAVEPOINT sp1;
COMMIT;

SELECT COUNT(*) FROM t_savepoint; -- Should still be 3

-- Test 10: Mixed operations with savepoint
UPDATE t_savepoint SET val = 100 WHERE id = 1;

BEGIN;
SAVEPOINT sp1;
INSERT INTO t_savepoint VALUES (5, 500);
UPDATE t_savepoint SET val = 999 WHERE id = 1;
DELETE FROM t_savepoint WHERE id = 3;
ROLLBACK TO SAVEPOINT sp1;
COMMIT;

SELECT val FROM t_savepoint WHERE id = 1; -- Should be 100
SELECT COUNT(*) FROM t_savepoint; -- Should be 3 (no id=5, id=3 still exists)

-- Test 11: Full transaction rollback clears savepoints
UPDATE t_savepoint SET val = 100 WHERE id = 1;

BEGIN;
SAVEPOINT sp1;
UPDATE t_savepoint SET val = 888 WHERE id = 1;
ROLLBACK;

SELECT val FROM t_savepoint WHERE id = 1; -- Should be 100

-- Test 12: Commit after savepoint operations
UPDATE t_savepoint SET val = 100 WHERE id = 1;

BEGIN;
SAVEPOINT sp1;
UPDATE t_savepoint SET val = 777 WHERE id = 1;
SAVEPOINT sp2;
UPDATE t_savepoint SET val = 888 WHERE id = 1;
ROLLBACK TO SAVEPOINT sp2;
COMMIT;

SELECT val FROM t_savepoint WHERE id = 1; -- Should be 777

-- Clean up
DROP TABLE t_savepoint;
