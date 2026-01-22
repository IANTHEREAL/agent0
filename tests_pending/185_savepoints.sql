-- PostgreSQL compatible tests from savepoints
-- 14 tests

SET client_min_messages = warning;

-- Test 1: statement (line 9)
DROP TABLE IF EXISTS a;
CREATE TABLE a (id INT);

-- Test 2: statement (line 12)
BEGIN;

-- Test 3: statement (line 15)
SELECT pg_sleep(1.5);

-- Test 4: statement (line 18)
SAVEPOINT s;

-- Test 5: statement (line 21)
SELECT pg_sleep(1.5);

-- Test 6: statement (line 24)
INSERT INTO a(id) VALUES (0);

-- Test 7: statement (line 27)
ROLLBACK TO SAVEPOINT s;

-- Test 8: query (line 30)
SELECT * FROM a;

-- Test 9: statement (line 34)
COMMIT;

-- Test 10: statement (line 41)
BEGIN;

-- Test 11: statement (line 44)
SAVEPOINT sp1;

-- Test 12: statement (line 47)
INSERT INTO a(id) VALUES (1);

-- Test 13: statement (line 50)
ROLLBACK TO SAVEPOINT sp1;

-- Test 14: statement (line 53)
ROLLBACK;

RESET client_min_messages;
