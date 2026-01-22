-- PostgreSQL compatible tests from explain
-- 5 tests

SET client_min_messages = warning;

DROP TABLE IF EXISTS t;
DROP TABLE IF EXISTS t88037;

-- Test 1: statement (line 3)
CREATE TABLE t (a INT PRIMARY KEY);

INSERT INTO t (a) VALUES (1), (2), (3);

-- Test 2: query (line 7)
EXPLAIN (COSTS OFF)
SELECT *
FROM (
  SELECT avg(a) OVER () FROM t
) AS sub;

-- Test 3: statement (line 23)
EXPLAIN (COSTS OFF)
SELECT avg(a) OVER (ROWS (SELECT count(*) FROM t) PRECEDING) FROM t;

-- Test 4: statement (line 28)
CREATE TABLE t88037 AS
SELECT g, g % 2 = 1 AS _bool, '0.0.0.0'::INET + g AS _inet
FROM generate_series(1, 5) AS g;

-- Test 5: query (line 33)
EXPLAIN (COSTS OFF)
SELECT NULL AS col_1372
FROM t88037 AS tab_489
WHERE tab_489._bool
ORDER BY tab_489._inet, tab_489._bool ASC
LIMIT 92;

RESET client_min_messages;
