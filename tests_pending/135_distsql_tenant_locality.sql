-- PostgreSQL compatible tests from distsql_tenant_locality
-- 5 tests

-- Test 1: statement (line 11)
-- CockroachDB FAMILY clauses are not supported in PostgreSQL.
CREATE TABLE t (k BIGINT PRIMARY KEY, v BIGINT);
INSERT INTO t SELECT i, i FROM generate_series(1, 6) AS g(i);

-- Test 2: statement (line 54)
SELECT * FROM t ORDER BY k;

-- Test 3: query (line 58)
SELECT * FROM t WHERE k IN (1, 3, 5) ORDER BY k;

-- Test 4: query (line 66)
SELECT * FROM t WHERE k >= 3 AND k < 5 ORDER BY k;

-- Test 5: query (line 73)
SELECT * FROM t WHERE k >= 1 ORDER BY k LIMIT 10;
