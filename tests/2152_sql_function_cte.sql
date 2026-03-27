-- Regression test for issue #2152: SQL-language function with CTE/FROM/subquery.
-- Previously, execute_sql_function() extracted only the first projection
-- expression, discarding WITH (CTEs), FROM, WHERE, and subqueries.

DROP FUNCTION IF EXISTS cte_scalar;
DROP FUNCTION IF EXISTS from_clause_func;
DROP FUNCTION IF EXISTS subquery_func;
DROP TABLE IF EXISTS test_sql_func_data;

CREATE TABLE test_sql_func_data (id int PRIMARY KEY, val text);
INSERT INTO test_sql_func_data VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma');

-- Test 1: SQL function with CTE
CREATE FUNCTION cte_scalar() RETURNS text LANGUAGE sql AS $$
  WITH items AS (SELECT val FROM test_sql_func_data WHERE id = 2)
  SELECT val FROM items
$$;

SELECT cte_scalar();

-- Test 2: SQL function with FROM clause
CREATE FUNCTION from_clause_func() RETURNS text LANGUAGE sql AS $$
  SELECT val FROM test_sql_func_data WHERE id = 1
$$;

SELECT from_clause_func();

-- Test 3: SQL function with subquery
CREATE FUNCTION subquery_func() RETURNS int LANGUAGE sql AS $$
  SELECT (SELECT count(*)::int FROM test_sql_func_data)
$$;

SELECT subquery_func();

-- Cleanup
DROP FUNCTION cte_scalar;
DROP FUNCTION from_clause_func;
DROP FUNCTION subquery_func;
DROP TABLE test_sql_func_data;
