-- PostgreSQL compatible tests from propagate_input_ordering
-- 16 tests

-- Test 1: statement (line 1)
-- CockroachDB setting (no PostgreSQL equivalent):
-- SET propagate_input_ordering=true;

-- Test 2: query (line 4)
WITH tmp AS (SELECT * FROM generate_series(1, 10) i)
SELECT *
FROM tmp
ORDER BY i % 5 ASC, i ASC;

-- Test 3: query (line 18)
WITH tmp AS (SELECT * FROM generate_series(1, 10) i)
SELECT *
FROM tmp
ORDER BY i % 5 ASC, i ASC;

-- Test 4: query (line 33)
SELECT foo
FROM (SELECT i, i % 2 AS mod2 FROM generate_series(1, 10) i) AS foo
ORDER BY foo.i % 5 ASC, foo.i ASC;

-- Test 5: query (line 48)
SELECT foo.*
FROM (SELECT i, i % 2 AS mod2 FROM generate_series(1, 10) i) AS foo
ORDER BY foo.i % 5 ASC, foo.i ASC;

-- Test 6: query (line 63)
SELECT array_agg(i ORDER BY i % 2 DESC, i) FROM generate_series(1, 5) i;

-- Test 7: query (line 69)
SELECT *
FROM (SELECT * FROM generate_series(1, 2) x) tmp,
     (SELECT * FROM generate_series(8, 12) i) tmp2
ORDER BY tmp.x, tmp2.i % 5 ASC, tmp2.i ASC;

-- Test 8: query (line 87)
WITH tmp AS (SELECT * FROM generate_series(1, 10) i ORDER BY i % 5 ASC, i ASC)
SELECT * FROM tmp ORDER BY i DESC;

-- Test 9: query (line 104)
WITH tmp AS (SELECT * FROM generate_series(1, 10) i ORDER BY i % 5 ASC, i ASC)
SELECT * FROM tmp ORDER BY i DESC;

-- Test 10: statement (line 119)
CREATE TABLE ab (a int, b int);

-- Test 11: statement (line 122)
INSERT INTO ab VALUES (10, 100), (1, 10), (5, 50);

-- Test 12: statement (line 125)
CREATE TABLE xy (x int, y int);

-- Test 13: statement (line 128)
INSERT INTO xy VALUES (2, 20), (8, 80), (4, 41), (4, 40);

-- Test 14: query (line 131)
WITH
  cte1 AS (SELECT b, row_number() OVER (ORDER BY a) AS ord FROM ab),
  cte2 AS (SELECT y, row_number() OVER (ORDER BY x, y) AS ord FROM xy)
SELECT v
FROM (
  SELECT b AS v, 1 AS src, ord FROM cte1
  UNION ALL
  SELECT y AS v, 2 AS src, ord FROM cte2
) AS u
ORDER BY src, ord;

-- Test 15: query (line 145)
WITH
  cte1 AS (SELECT b, row_number() OVER (ORDER BY a + b) AS ord FROM ab),
  cte2 AS (
    SELECT y, row_number() OVER (ORDER BY x, y) AS ord
    FROM (SELECT DISTINCT ON (x) x, y FROM xy ORDER BY x, y) AS s
  )
SELECT v
FROM (
  SELECT b AS v, 1 AS src, ord FROM cte1
  UNION ALL
  SELECT y AS v, 2 AS src, ord FROM cte2
) AS u
ORDER BY src, ord;

-- Test 16: statement (line 158)
-- RESET propagate_input_ordering
