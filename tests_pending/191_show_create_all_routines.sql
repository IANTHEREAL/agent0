-- PostgreSQL compatible tests from show_create_all_routines
-- 20 tests

SET client_min_messages = warning;

-- Each file is already executed in an isolated database by the runner, so the
-- CockroachDB `CREATE DATABASE ...; USE ...;` setup is unnecessary here.

DROP VIEW IF EXISTS show_create_all_routines;
CREATE VIEW show_create_all_routines AS
SELECT
  CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END
    || ' ' || n.nspname || '.' || p.proname
    || '(' || pg_get_function_identity_arguments(p.oid) || ')' AS routine,
  pg_get_functiondef(p.oid) AS create_statement
FROM pg_proc AS p
JOIN pg_namespace AS n ON n.oid = p.pronamespace
WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
  AND p.prokind IN ('f', 'p');

-- Test 3: query (line 9)
SELECT * FROM show_create_all_routines ORDER BY 1;

-- Test 4: statement (line 14)
CREATE FUNCTION add_one(x BIGINT) RETURNS BIGINT AS $$SELECT x + 1$$ LANGUAGE SQL;

-- Test 5: query (line 17)
SELECT * FROM show_create_all_routines ORDER BY 1;

-- Test 6: statement (line 32)
CREATE OR REPLACE PROCEDURE double_triple(INOUT double BIGINT, OUT triple BIGINT)
AS $$
BEGIN
  double := double * 2;
  triple := double * 3;
END;
$$ LANGUAGE PLpgSQL;

-- Test 7: query (line 42)
SELECT * FROM show_create_all_routines ORDER BY 1;

-- Test 8: statement (line 66)
CREATE FUNCTION add_one(x DOUBLE PRECISION) RETURNS DOUBLE PRECISION AS $$SELECT x + 1$$ LANGUAGE SQL;

-- Test 9: query (line 69)
SELECT * FROM show_create_all_routines ORDER BY 1;

-- Test 10: statement (line 105)
DROP FUNCTION add_one(x INT8);

-- Test 11: statement (line 108)
DROP FUNCTION add_one(x FLOAT8);

-- Test 12: query (line 111)
SELECT * FROM show_create_all_routines ORDER BY 1;

-- Test 13: statement (line 125)
DROP PROCEDURE double_triple(INT8);

-- Test 14: query (line 128)
SELECT * FROM show_create_all_routines ORDER BY 1;

-- Test 15: statement (line 134)
CREATE SCHEMA s;

-- Test 16: statement (line 137)
CREATE FUNCTION add_one(x BIGINT) RETURNS BIGINT AS $$SELECT x + 1$$ LANGUAGE SQL;

-- Test 17: statement (line 140)
CREATE FUNCTION s.add_one(x BIGINT) RETURNS BIGINT AS $$SELECT x + 1$$ LANGUAGE SQL;

-- Test 18: query (line 143)
SELECT * FROM show_create_all_routines ORDER BY 1;

-- Test 19: statement (line 171)
CREATE FUNCTION select_invalid() RETURNS TRIGGER AS $$
BEGIN
  PERFORM 1 FROM a_b_c;
  RETURN NEW;
END;
$$ LANGUAGE PLpgSQL;

-- Test 20: query (line 178)
SELECT * FROM show_create_all_routines ORDER BY 1;

RESET client_min_messages;
