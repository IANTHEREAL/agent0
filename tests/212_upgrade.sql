-- PostgreSQL compatible tests from upgrade
-- 3 tests

SET client_min_messages = warning;

-- Cockroach cluster version/setting helpers don't exist in Postgres. Provide
-- minimal shims so the test remains meaningful (introspecting and "updating"
-- a version value) while staying self-contained.
CREATE SCHEMA crdb_internal;

CREATE OR REPLACE FUNCTION crdb_internal.node_executable_version()
RETURNS text
LANGUAGE sql
AS $$
  SELECT current_setting('server_version');
$$;

CREATE OR REPLACE FUNCTION crdb_internal.active_version()
RETURNS jsonb
LANGUAGE sql
AS $$
  SELECT jsonb_build_object(
    'internal',
    coalesce(current_setting('crdb.version', true), current_setting('server_version'))
  );
$$;

CREATE OR REPLACE FUNCTION crdb_internal.release_series(v text)
RETURNS text
LANGUAGE sql
AS $$
  -- Keep the leading numeric part (e.g. "16.11" from "16.11 (Ubuntu ...)").
  SELECT regexp_replace(v, '^([0-9]+([.][0-9]+)?).*$', '\1');
$$;

-- Test 1: query (line 7)
SELECT crdb_internal.active_version()->'internal';

-- Test 2: statement (line 14)
SELECT crdb_internal.node_executable_version() AS node_executable_version \gset
SET crdb.version TO :'node_executable_version';

-- Test 3: query (line 18)
SELECT crdb_internal.release_series(current_setting('crdb.version'));

RESET client_min_messages;
