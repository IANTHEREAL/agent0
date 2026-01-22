-- PostgreSQL compatible tests from mixed_version_timeseries_range_already_exists
-- 2 tests

SET client_min_messages = warning;
CREATE SCHEMA IF NOT EXISTS crdb_internal;

-- Test 1: statement (line 7)
-- CockroachDB zone configs / ranges have no PostgreSQL equivalent. Model the
-- intent of "configure zone for timeseries" as a local table update.
DROP TABLE IF EXISTS crdb_internal.zone_configs;
CREATE TABLE crdb_internal.zone_configs (
  target TEXT PRIMARY KEY,
  gc_ttlseconds INT,
  num_replicas INT
);
INSERT INTO crdb_internal.zone_configs(target, gc_ttlseconds, num_replicas)
VALUES ('timeseries', 12345, 5);

-- Test 2: statement (line 19)
-- CockroachDB cluster settings are not available in PostgreSQL; record an
-- analogous "version" setting locally for the test.
DROP TABLE IF EXISTS crdb_internal.cluster_settings;
CREATE TABLE crdb_internal.cluster_settings (
  variable TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE OR REPLACE FUNCTION crdb_internal.node_executable_version()
RETURNS TEXT
LANGUAGE sql
AS $$SELECT current_setting('server_version')$$;
INSERT INTO crdb_internal.cluster_settings(variable, value)
VALUES ('version', crdb_internal.node_executable_version());

RESET client_min_messages;
