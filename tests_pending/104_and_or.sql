-- PostgreSQL compatible tests from and_or
-- 6 tests

-- Test 1: statement (line 1)
SET client_min_messages = warning;
DROP TABLE IF EXISTS t;
CREATE TABLE t (k INT PRIMARY KEY, a INT, b INT);

-- Test 2: statement (line 4)
INSERT INTO t VALUES (1, NULL, NULL), (2, NULL, 1), (3, 1, NULL), (4, 2, 0), (5, 3, 3);

-- Test 3: query (line 9)
SELECT a <> 2 AND 3 / NULLIF(b, 0) = 1 FROM t ORDER BY k;

-- Test 4: query (line 20)
SELECT a FROM t WHERE a <> 2 AND 3 / NULLIF(b, 0) = 1 ORDER BY k;

-- Test 5: query (line 27)
SELECT a = 2 OR 3 / NULLIF(b, 0) = 1 FROM t ORDER BY k;

-- Test 6: query (line 38)
SELECT a FROM t WHERE a = 2 OR 3 / NULLIF(b, 0) = 1 ORDER BY k;

RESET client_min_messages;
