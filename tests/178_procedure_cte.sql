-- PostgreSQL compatible tests from procedure_cte
-- 17 tests

-- Test 1: statement (line 1)
CREATE TABLE ab (a INT PRIMARY KEY, b INT);
INSERT INTO ab VALUES (1, 10), (2, 20), (3, 30), (4, 40);

-- Test 2: statement (line 5)
CREATE PROCEDURE p(OUT ret INT) LANGUAGE SQL AS $$
  WITH foo AS MATERIALIZED (SELECT 100) SELECT * FROM foo;
$$;

-- Test 3: query (line 10)
CALL p(NULL);

-- Test 4: statement (line 15)
DROP PROCEDURE p;
CREATE PROCEDURE p(OUT ret INT) LANGUAGE SQL AS $$
  WITH foo AS MATERIALIZED (SELECT b FROM ab WHERE a = 3) SELECT * FROM foo;
$$;

-- Test 5: query (line 21)
CALL p(NULL);

-- Test 6: statement (line 27)
DROP PROCEDURE p;
CREATE PROCEDURE p(OUT ret INT) LANGUAGE SQL AS $$
  WITH foo (bar) AS (SELECT 1) SELECT foo.bar + foo2.bar FROM foo, foo foo2;
$$;

-- Test 7: query (line 33)
CALL p(NULL);

-- Test 8: statement (line 39)
DROP PROCEDURE p;
CREATE PROCEDURE p(OUT ret INT) LANGUAGE SQL AS $$
  WITH foo (x) AS MATERIALIZED (SELECT 1),
  bar (x) AS MATERIALIZED (SELECT 2)
  SELECT foo.x + bar.x FROM foo, bar;
$$;

-- Test 9: query (line 47)
CALL p(NULL);

-- Test 10: statement (line 53)
DROP PROCEDURE p;
CREATE PROCEDURE p(OUT ret INT) LANGUAGE SQL AS $$
  WITH foo (x) AS MATERIALIZED (SELECT 100)
  SELECT * FROM (
    WITH bar (x) AS MATERIALIZED (SELECT 200)
    SELECT foo.x + bar.x FROM foo, bar
  ) AS t;
$$;

-- Test 11: query (line 63)
CALL p(NULL);

-- Test 12: statement (line 69)
DROP PROCEDURE p;
CREATE PROCEDURE p(OUT ret INT) LANGUAGE SQL AS $$
  SELECT (
    WITH foo AS MATERIALIZED (SELECT b FROM ab)
    SELECT * FROM foo
  );
$$;

-- Test 13: statement (line 79)
DELETE FROM ab WHERE a > 1;

-- Test 14: query (line 82)
CALL p(NULL);

-- Test 15: statement (line 87)
INSERT INTO ab VALUES (2, 20), (3, 30), (4, 40);

-- Test 16: statement (line 91)
DROP PROCEDURE p;
CREATE PROCEDURE p(OUT res INT[]) LANGUAGE SQL AS $$
  WITH RECURSIVE foo (x, y) AS (
    SELECT a, b FROM ab WHERE a = 1
    UNION ALL
    -- PostgreSQL forbids referencing the recursive CTE within a subquery.
    -- The working table for the recursive term contains the previous iteration,
    -- so max(x)+1 is equivalent to (x+1) here.
    SELECT ab.a, ab.b
    FROM ab
    JOIN foo ON ab.a = foo.x + 1
  )
  SELECT array_agg(y ORDER BY x) FROM foo;
$$;

-- Test 17: query (line 102)
CALL p(NULL);
