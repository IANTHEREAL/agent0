-- PostgreSQL compatible tests from distsql_tenant
-- 4 tests

-- Test 1: statement (line 3)
CREATE TABLE t (
  k INT PRIMARY KEY,
  v INT,
  w INT,
  x INT
);

-- Test 2: statement (line 10)
INSERT INTO t VALUES (23, 1, 2, 3), (34, 3, 4, 8);

-- Test 3: query (line 13)
SELECT * FROM t WHERE k < 10 OR (k > 20 AND k < 29) OR k > 40;

-- Test 4: query (line 18)
SELECT v, w FROM t WHERE k = 23;
