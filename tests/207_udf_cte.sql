-- PostgreSQL compatible tests from udf_cte
-- 24 tests

-- Test 1: statement (line 1)
CREATE TABLE ab (a INT PRIMARY KEY, b INT);
INSERT INTO ab VALUES (1, 10), (2, 20), (3, 30), (4, 40);

-- Test 2: statement (line 5)
CREATE FUNCTION f() RETURNS INT LANGUAGE SQL AS $$
  WITH foo AS MATERIALIZED (SELECT 100) SELECT * FROM foo;
$$;

-- Test 3: query (line 10)
SELECT f();

-- Test 4: statement (line 15)
DROP FUNCTION f;
CREATE FUNCTION f() RETURNS INT LANGUAGE SQL AS $$
  WITH foo AS MATERIALIZED (SELECT b FROM ab WHERE a = 3) SELECT * FROM foo;
$$;

-- Test 5: query (line 21)
SELECT f();

-- Test 6: statement (line 27)
DROP FUNCTION f;
CREATE FUNCTION f() RETURNS INT LANGUAGE SQL AS $$
  WITH foo (bar) AS (SELECT 1) SELECT foo.bar + foo2.bar FROM foo, foo foo2;
$$;

-- Test 7: query (line 33)
SELECT f();

-- Test 8: statement (line 39)
DROP FUNCTION f;
CREATE FUNCTION f() RETURNS INT LANGUAGE SQL AS $$
  WITH foo (x) AS MATERIALIZED (SELECT 1),
  bar (x) AS MATERIALIZED (SELECT 2)
  SELECT foo.x + bar.x FROM foo, bar;
$$;

-- Test 9: query (line 47)
SELECT f();

-- Test 10: statement (line 53)
DROP FUNCTION f;
CREATE FUNCTION f() RETURNS INT LANGUAGE SQL AS $$
  WITH foo (x) AS MATERIALIZED (SELECT 100)
  SELECT * FROM (
    WITH bar (x) AS MATERIALIZED (SELECT 200)
    SELECT foo.x + bar.x FROM foo, bar
  ) AS t;
$$;

-- Test 11: query (line 63)
SELECT f();

-- Test 12: statement (line 69)
DROP FUNCTION f;
CREATE FUNCTION f() RETURNS INT LANGUAGE SQL AS $$
  WITH foo AS MATERIALIZED (SELECT 1) SELECT * FROM foo;
$$;

-- Test 13: query (line 75)
WITH bar AS (SELECT 2) SELECT f(), * FROM bar;

-- Test 14: query (line 81)
WITH foo AS (SELECT 2) SELECT f(), * FROM foo;

-- Test 15: statement (line 87)
DROP FUNCTION f;
CREATE FUNCTION f() RETURNS INT LANGUAGE SQL AS $$
  SELECT (
    WITH foo AS MATERIALIZED (SELECT b FROM ab)
    SELECT * FROM foo
  );
$$;

-- Test 16: statement (line 97)
DELETE FROM ab WHERE a > 1;

-- Test 17: query (line 100)
SELECT f();

-- Test 18: statement (line 105)
INSERT INTO ab VALUES (2, 20), (3, 30), (4, 40);

-- Test 19: statement (line 109)
DROP FUNCTION f;
CREATE FUNCTION f() RETURNS INT[] LANGUAGE SQL AS $$
  WITH RECURSIVE foo (x, y) AS (
    SELECT a, b FROM ab WHERE a = 1
    UNION ALL
    -- In PostgreSQL, recursive references must appear in the FROM clause.
    SELECT ab.a, ab.b
    FROM ab
    JOIN foo ON ab.a = foo.x + 1
  )
  SELECT array_agg(y ORDER BY x) FROM foo;
$$;

-- Test 20: query (line 120)
SELECT f();

-- Test 21: statement (line 127)
CREATE SEQUENCE seq_1;

-- Test 22: statement (line 132)
CREATE FUNCTION f138273_1() RETURNS TIMESTAMP LANGUAGE PLpgSQL AS $$
  DECLARE
    decl TIMESTAMP;
  BEGIN
    WHILE false LOOP
      IF true THEN
      ELSIF EXISTS (WITH cte(col) AS (SELECT * FROM (VALUES (currval('seq_1'))) AS foo) SELECT 1 FROM cte) THEN
        RETURN decl;
      END IF;
    END LOOP;
  END;
$$;

-- Test 23: statement (line 147)
CREATE FUNCTION f138273_2() RETURNS INT LANGUAGE PLpgSQL AS $$
  BEGIN
    PERFORM nextval('seq_1');
    WHILE EXISTS (WITH cte(col) AS (SELECT * FROM (VALUES (currval('seq_1'))) AS foo) SELECT 1 FROM cte) LOOP
      RETURN currval('seq_1');
    END LOOP;
  END;
$$;

-- Test 24: query (line 157)
SELECT f138273_2();
