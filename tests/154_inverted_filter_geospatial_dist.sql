-- PostgreSQL compatible tests from inverted_filter_geospatial_dist
-- 13 tests

SET client_min_messages = warning;

-- PostGIS provides geometry + ST_* functions.
CREATE EXTENSION IF NOT EXISTS postgis;
-- Needed for a multicolumn GiST index that includes a TEXT column.
CREATE EXTENSION IF NOT EXISTS btree_gist;

DROP TABLE IF EXISTS geo_table;
CREATE TABLE geo_table (
  k INT PRIMARY KEY,
  s TEXT NOT NULL,
  geom geometry NOT NULL
);

CREATE INDEX geom_index ON geo_table USING GIST (geom);

-- Test 1: statement (line 11)
INSERT INTO geo_table VALUES
  (1, 'foo', 'POINT(1 1)'),
  (2, 'foo', 'LINESTRING(1 1, 2 2)'),
  (3, 'foo', 'POINT(3 3)'),
  (4, 'bar', 'LINESTRING(4 4, 5 5)'),
  (5, 'bar', 'LINESTRING(40 40, 41 41)'),
  (6, 'bar', 'POLYGON((1 1, 5 1, 5 5, 1 5, 1 1))'),
  (7, 'foo', 'LINESTRING(1 1, 3 3)');

-- Test 2: query (line 22)
SELECT k FROM geo_table WHERE ST_Intersects('MULTIPOINT((2.2 2.2), (3.0 3.0))'::geometry, geom) ORDER BY k;

-- Test 3: query (line 29)
SELECT k FROM geo_table WHERE ST_CoveredBy('MULTIPOINT((2.2 2.2), (3.0 3.0))'::geometry, geom) ORDER BY k;

-- Test 4: query (line 71)
SELECT k FROM geo_table WHERE ST_Intersects('MULTIPOINT((2.2 2.2), (3.0 3.0))'::geometry, geom) ORDER BY k;

-- Test 5: query (line 79)
SELECT k FROM geo_table WHERE ST_CoveredBy('MULTIPOINT((2.2 2.2), (3.0 3.0))'::geometry, geom) ORDER BY k;

-- Test 6: query (line 100)
SELECT k FROM geo_table WHERE ST_Intersects('MULTIPOINT((2.2 2.2), (3.0 3.0))'::geometry, geom) ORDER BY k;

-- Test 7: query (line 108)
SELECT k FROM geo_table WHERE ST_CoveredBy('MULTIPOINT((2.2 2.2), (3.0 3.0))'::geometry, geom) ORDER BY k;

-- Test 8: statement (line 114)
DROP INDEX geom_index;

-- Test 9: statement (line 118)
CREATE INDEX geom_index2 ON geo_table USING GIST (s gist_text_ops, geom);

-- Test 10: query (line 128)
SELECT k FROM geo_table WHERE s = 'foo' AND ST_Intersects('MULTIPOINT((2.2 2.2), (3.0 3.0))'::geometry, geom) ORDER BY k;

-- Test 11: query (line 134)
SELECT k FROM geo_table WHERE s = 'foo' AND ST_CoveredBy('MULTIPOINT((2.2 2.2), (3.0 3.0))'::geometry, geom) ORDER BY k;

-- Test 12: query (line 161)
SELECT k FROM geo_table WHERE s = 'foo' AND ST_Intersects('MULTIPOINT((2.2 2.2), (3.0 3.0))'::geometry, geom) ORDER BY k;

-- Test 13: query (line 168)
SELECT k FROM geo_table WHERE s = 'foo' AND ST_CoveredBy('MULTIPOINT((2.2 2.2), (3.0 3.0))'::geometry, geom) ORDER BY k;

RESET client_min_messages;
