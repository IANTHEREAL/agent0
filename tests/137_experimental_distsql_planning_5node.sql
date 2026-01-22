-- PostgreSQL compatible tests from experimental_distsql_planning_5node
-- 4 tests

-- Test 1: statement (line 7)
CREATE TABLE kv (k INT PRIMARY KEY, v INT);
INSERT INTO kv SELECT i, i FROM generate_series(1,5) AS g(i);
CREATE TABLE kw (k INT PRIMARY KEY, w INT);
INSERT INTO kw SELECT i, i FROM generate_series(1,5) AS g(i);

-- Test 2: query (line 55)
SELECT kv.k, v FROM kv, kw WHERE v = w ORDER BY kv.k;

-- Test 3: query (line 65)
SELECT kv.k FROM kv, kw WHERE kv.k = kw.k ORDER BY 1;

-- Test 4: query (line 76)
-- CockroachDB exposes mvcc timestamps via a hidden column (crdb_internal_mvcc_timestamp).
-- In PostgreSQL, rows always have MVCC metadata; use the system column xmin as a proxy.
SELECT xmin IS NOT NULL FROM kv LIMIT 1;
