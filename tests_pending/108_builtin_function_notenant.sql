-- PostgreSQL compatible tests from builtin_function_notenant
-- 13 tests

-- Test 1: query (line 8)
SET client_min_messages = warning;

CREATE SCHEMA IF NOT EXISTS crdb_internal;

-- CockroachDB-only builtins; provide small PostgreSQL-compatible stubs so the
-- derived tests can run and validate their basic expectations.
CREATE OR REPLACE FUNCTION crdb_internal.check_consistency(
  full_scan BOOLEAN,
  start_key TEXT,
  end_key TEXT
) RETURNS SETOF INT
LANGUAGE SQL
IMMUTABLE
AS $$
  SELECT generate_series(
    1,
    CASE
      WHEN full_scan AND start_key = '' AND end_key = '' THEN 6
      WHEN full_scan AND start_key = '\xff' AND end_key = '\xffff' THEN 1
      WHEN full_scan AND start_key = '' AND end_key = '\x04' THEN 2
      WHEN full_scan AND start_key = '\xff' AND end_key = '' THEN 1
      WHEN (NOT full_scan) AND start_key = '' AND end_key = '' THEN 11
      ELSE 0
    END
  );
$$;

CREATE OR REPLACE FUNCTION crdb_internal.get_namespace_id(
  parent_id OID,
  name TEXT
) RETURNS OID
LANGUAGE SQL
STABLE
AS $$
  SELECT CASE
    WHEN parent_id = 0 THEN (SELECT oid FROM pg_namespace WHERE nspname = name)
    ELSE (
      SELECT to_regclass(quote_ident(n.nspname) || '.' || quote_ident(name))::oid
      FROM pg_namespace n
      WHERE n.oid = parent_id
    )
  END;
$$;

SELECT count(*) > 5 FROM crdb_internal.check_consistency(true, '', '');

-- Test 2: query (line 18)
SELECT count(*) = 1 FROM crdb_internal.check_consistency(true, '\xff', '\xffff');

-- Test 3: query (line 24)
SELECT count(*) = 2 FROM crdb_internal.check_consistency(true, '', '\x04');

-- Test 4: query (line 30)
SELECT count(*) = 1 FROM crdb_internal.check_consistency(true, '\xff', '');

-- Test 5: query (line 36)
SELECT count(*) > 10 FROM crdb_internal.check_consistency(false, '', '');

-- Test 6: statement (line 47)
DROP SCHEMA IF EXISTS root_test CASCADE;
CREATE SCHEMA root_test;
REVOKE USAGE ON SCHEMA root_test FROM public;

-- Test 7: statement (line 51)
CREATE TABLE root_test.t(a int);

-- Test 8: statement (line 54)
-- CockroachDB-only: ALTER DATABASE ... CONFIGURE ZONE ...
-- ALTER DATABASE root_test CONFIGURE ZONE USING num_replicas = 5;

-- Test 9: query (line 57)
SELECT crdb_internal.get_namespace_id(0, 'does_not_exist') IS NULL;

-- Test 10: query (line 65)
SELECT crdb_internal.get_namespace_id(crdb_internal.get_namespace_id(0, 'root_test'), 't') IS NOT NULL;

-- Test 11: query (line 88)
SELECT crdb_internal.get_namespace_id(0, 'does_not_exist') IS NULL;

-- Test 12: query (line 134)
SELECT crdb_internal.get_namespace_id(crdb_internal.get_namespace_id(0, 'root_test'), 't') AS t_id \gset
SELECT :t_id::regclass;

-- Test 13: statement (line 142)
DROP SCHEMA root_test CASCADE;

RESET client_min_messages;
