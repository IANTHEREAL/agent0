-- PostgreSQL compatible tests from distsql_subquery
-- 9 tests

-- Test 1: statement (line 6)
CREATE TABLE ab (a INT, b INT);

-- Test 2: statement (line 9)
INSERT INTO ab VALUES (1, 1), (1, 3), (2, 2);

-- Test 3: statement (line 12)
-- CockroachDB-only cluster setting.
-- SET CLUSTER SETTING kv.range_merge.queue.enabled = false

-- Test 4: query (line 30)
SELECT ARRAY(SELECT a FROM ab ORDER BY b);

-- Test 5: statement (line 37)
CREATE TABLE t86075 (k INT PRIMARY KEY, c REGPROCEDURE, a REGPROCEDURE[]);
INSERT INTO t86075 VALUES (1, 1, ARRAY[1]);

-- CockroachDB logic-test directive.
-- retry

-- Test 6: statement (line 45)
SELECT (SELECT c FROM t86075) FROM t86075;

-- Test 7: statement (line 48)
SELECT (SELECT a FROM t86075) FROM t86075;

-- Test 8: statement (line 51)
SELECT (SELECT (c, c) FROM t86075) FROM t86075;

-- Test 9: statement (line 54)
SELECT k FROM t86075 WHERE c = (SELECT c FROM t86075 LIMIT 1);
