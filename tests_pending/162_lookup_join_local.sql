-- PostgreSQL compatible tests from lookup_join_local
-- 6 tests

SET client_min_messages = warning;
DROP TABLE IF EXISTS small, large;

-- CockroachDB-specific; no equivalent in PostgreSQL.
-- SET distsql_workmem = '8MiB';

-- Test 1: statement (line 24)
CREATE TABLE small (n INT PRIMARY KEY);
INSERT INTO small SELECT generate_series(0, 6);

-- Test 2: statement
CREATE TABLE large (n INT, v TEXT);
CREATE INDEX large_n_idx ON large(n) INCLUDE (v);

-- Test 3: statement
INSERT INTO large
SELECT g % 7, repeat('a', 52)
  FROM generate_series(0, 69999) AS g(g);

-- Test 4: query (line 30)
SELECT small.n, sum(length(large.v)) FROM small
INNER JOIN large ON small.n = large.n
GROUP BY small.n
ORDER BY small.n;
