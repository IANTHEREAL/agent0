-- PostgreSQL compatible tests from ordinality
-- 18 tests

-- Test 1: query (line 1)
SELECT name, row_number() OVER () AS i
FROM (VALUES ('a'), ('b')) AS x(name);

-- Test 2: query (line 8)
SELECT row_number() OVER () AS ordinality
FROM (VALUES ('a'), ('b')) AS x(name);

-- Test 3: statement (line 15)
CREATE TABLE foo (x CHAR PRIMARY KEY);
INSERT INTO foo(x) VALUES ('a'), ('b');

-- Test 4: query (line 18)
SELECT x, row_number() OVER (ORDER BY x) AS ordinality
FROM foo
ORDER BY x;

-- Test 5: query (line 24)
SELECT x, row_number() OVER (ORDER BY x) AS ordinality
FROM foo
ORDER BY x
LIMIT 1;

-- Test 6: query (line 29)
SELECT max(ordinality)
FROM (
  SELECT row_number() OVER (ORDER BY x) AS ordinality
  FROM foo
) AS foo_ord;

-- Test 7: query (line 34)
SELECT a.x, a.ordinality, b.x, b.ordinality
FROM (
  SELECT x, row_number() OVER (ORDER BY x) AS ordinality
  FROM foo
) AS a
CROSS JOIN (
  SELECT x, row_number() OVER (ORDER BY x) AS ordinality
  FROM foo
) AS b
ORDER BY a.x, b.x;

-- Test 8: query (line 42)
SELECT xx, row_number() OVER (ORDER BY xx) AS ordinality
FROM (
  SELECT x || x AS xx
  FROM foo
) AS s
ORDER BY xx;

-- Test 9: query (line 48)
SELECT *
FROM (
  SELECT x, row_number() OVER (ORDER BY x) * 2 AS ordinality_x2
  FROM foo
) AS a
JOIN (
  SELECT x, row_number() OVER (ORDER BY x) AS ordinality
  FROM foo
) AS b USING (x)
ORDER BY x;

-- Test 10: query (line 54)
SELECT x, row_number() OVER (ORDER BY x DESC) AS ordinality
FROM foo
ORDER BY x DESC
LIMIT 1;

-- Test 11: query (line 59)
SELECT x, ordinality
FROM (
  SELECT x, row_number() OVER (ORDER BY x) AS ordinality
  FROM foo
) AS s
ORDER BY x DESC
LIMIT 1;

-- Test 12: query (line 64)
SELECT x, ordinality
FROM (
  SELECT x, row_number() OVER (ORDER BY x) AS ordinality
  FROM foo
) AS s
ORDER BY ordinality DESC
LIMIT 1;

-- Test 13: statement (line 69)
INSERT INTO foo(x) VALUES ('c');

-- Test 14: query (line 72)
SELECT x, ordinality
FROM (
  SELECT x, row_number() OVER (ORDER BY x) AS ordinality
  FROM foo
) AS s
WHERE x > 'a'
ORDER BY x;

-- Test 15: query (line 78)
SELECT x, ordinality
FROM (
  SELECT x, row_number() OVER (ORDER BY x) AS ordinality
  FROM foo
) AS s
WHERE ordinality > 1
ORDER BY ordinality DESC;

-- Test 16: query (line 84)
SELECT x, row_number() OVER (ORDER BY x) AS ordinality
FROM foo
WHERE x > 'a'
ORDER BY x;

-- Test 17: query (line 90)
SELECT ordinality = row_number() OVER (ORDER BY x)
FROM (
  SELECT x, row_number() OVER (ORDER BY x) AS ordinality
  FROM foo
) AS s
ORDER BY x;

-- Test 18: query (line 102)
SELECT x, row_number() OVER (ORDER BY x) AS ordinality
FROM foo
ORDER BY x
LIMIT 1;
