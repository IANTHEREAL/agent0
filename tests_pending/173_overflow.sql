-- PostgreSQL compatible tests from overflow
-- 12 tests

SET client_min_messages = warning;

-- Test 1: statement (line 3)
DROP TABLE IF EXISTS large_numbers;
CREATE TABLE large_numbers (a BIGINT);

-- Test 2: statement (line 6)
INSERT INTO large_numbers VALUES (9223372036854775807),(1);

-- Test 3: statement (line 9)
SELECT sum(a) FROM large_numbers;

-- Test 4: statement (line 12)
DELETE FROM large_numbers;

-- Test 5: statement (line 15)
INSERT INTO large_numbers VALUES (-9223372036854775808),(-1);

-- Test 6: statement (line 18)
SELECT sum(a) FROM large_numbers;

-- Test 7: statement (line 23)
DROP TABLE IF EXISTS t88128;
CREATE TABLE t88128 (i INT);

-- Test 8: statement (line 26)
INSERT INTO t88128 VALUES (100000);

-- Test 9: query (line 32)
SELECT i - 1000 < 9223372036854775800 FROM t88128;

-- Test 10: query (line 40)
SELECT i - (-1000) > -9223372036854775800 FROM t88128;

-- Test 11: query (line 48)
SELECT i + 1000 > -9223372036854775800 FROM t88128;

-- Test 12: query (line 56)
SELECT i + (-1000) < 9223372036854775800 FROM t88128;

RESET client_min_messages;
