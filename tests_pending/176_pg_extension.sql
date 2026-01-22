-- PostgreSQL compatible tests from pg_extension
-- 7 tests

SET client_min_messages = warning;

-- PostgreSQL reserves the `pg_` prefix for system schemas, so we can't create
-- a user schema named `pg_extension`. Use a dedicated schema for PostGIS
-- objects instead.
CREATE SCHEMA IF NOT EXISTS crdb_extension;
CREATE EXTENSION IF NOT EXISTS postgis SCHEMA crdb_extension;

SET search_path TO public, crdb_extension;

-- Test 1: statement (line 1)
DROP TABLE IF EXISTS pg_extension_test;
CREATE TABLE pg_extension_test (
  a geography(point, 4326),
  b geometry(polygon, 3857),
  c geometry,
  d geography,
  az geography(pointz, 4326),
  bz geometry(polygonz, 3857),
  am geography(pointm, 4326),
  bm geometry(polygonm, 3857),
  azm geography(pointzm, 4326),
  bzm geometry(polygonzm, 3857)
);

-- Test 2: query (line 15)
SELECT * FROM crdb_extension.geography_columns WHERE f_table_name = 'pg_extension_test';

-- Test 3: query (line 24)
SELECT * FROM crdb_extension.geometry_columns WHERE f_table_name = 'pg_extension_test';

-- Test 4: query (line 33)
SELECT * FROM geography_columns WHERE f_table_name = 'pg_extension_test';

-- Test 5: query (line 42)
SELECT * FROM geometry_columns WHERE f_table_name = 'pg_extension_test';

-- Test 6: query (line 51)
SELECT * FROM crdb_extension.spatial_ref_sys WHERE srid IN (3857, 4326) ORDER BY srid ASC;

-- Test 7: query (line 57)
SELECT * FROM spatial_ref_sys WHERE srid IN (3857, 4326) ORDER BY srid ASC;

RESET search_path;
RESET client_min_messages;
