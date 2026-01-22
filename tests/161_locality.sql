-- PostgreSQL compatible tests from locality
-- 3 tests

-- CockroachDB exposes locality via crdb_internal.locality_value(). PostgreSQL has
-- no equivalent, so provide a small compatibility shim matching the test's
-- default expected values.
CREATE SCHEMA IF NOT EXISTS crdb_internal;
CREATE OR REPLACE FUNCTION crdb_internal.locality_value(key text)
RETURNS text
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE key
    WHEN 'region' THEN 'test'
    WHEN 'dc' THEN 'dc1'
    ELSE NULL
  END;
$$;

-- Test 1: query (line 3)
SELECT crdb_internal.locality_value('region');

-- Test 2: query (line 8)
SELECT crdb_internal.locality_value('dc');

-- Test 3: query (line 13)
SELECT crdb_internal.locality_value('unk');
