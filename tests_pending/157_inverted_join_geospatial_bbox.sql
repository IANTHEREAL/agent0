-- PostgreSQL compatible tests from inverted_join_geospatial_bbox
-- 10 tests

SET client_min_messages = warning;

-- PostGIS provides geometry, box2d, and the bounding-box operators used below.
CREATE EXTENSION IF NOT EXISTS postgis;

-- Test 1: statement (line 5)
DROP TABLE IF EXISTS rtable;
DROP TABLE IF EXISTS ltable;

CREATE TABLE ltable(
  lk int primary key,
  geom1 geometry,
  geom2 geometry
);

-- Test 2: statement (line 12)
INSERT INTO ltable VALUES
  (1, 'POINT(3.0 3.0)', 'POINT(3.0 3.0)'),
  (2, 'POINT(4.5 4.5)', 'POINT(3.0 3.0)'),
  (3, 'POINT(1.5 1.5)', 'POINT(3.0 3.0)'),
  (4, NULL, 'POINT(3.0 3.0)'),
  (5, 'POINT(1.5 1.5)', NULL),
  (6, NULL, NULL);

-- Test 3: statement (line 21)
CREATE TABLE rtable(
  rk int primary key,
  geom geometry
);

CREATE INDEX geom_index ON rtable USING GIST (geom);

-- Test 4: statement (line 28)
INSERT INTO rtable VALUES
  (11, 'POINT(1.0 1.0)'),
  (12, 'LINESTRING(1.0 1.0, 2.0 2.0)'),
  (13, 'POINT(3.0 3.0)'),
  (14, 'LINESTRING(4.0 4.0, 5.0 5.0)'),
  (15, 'LINESTRING(40.0 40.0, 41.0 41.0)'),
  (16, 'POLYGON((1.0 1.0, 5.0 1.0, 5.0 5.0, 1.0 5.0, 1.0 1.0))');

-- Test 5: query (line 40)
SELECT lk, rk FROM ltable JOIN rtable ON ltable.geom1 ~ rtable.geom
ORDER BY lk, rk;

-- Test 6: query (line 46)
SELECT lk, rk FROM ltable JOIN rtable ON rtable.geom ~ ltable.geom1
ORDER BY lk, rk;

-- Test 7: query (line 59)
SELECT lk, rk FROM ltable JOIN rtable ON rtable.geom && ltable.geom1
ORDER BY lk, rk;

-- Test 8: query (line 72)
SELECT lk, rk FROM ltable JOIN rtable ON ltable.geom1::box2d ~ rtable.geom
ORDER BY lk, rk;

-- Test 9: query (line 78)
SELECT lk, rk FROM ltable JOIN rtable ON rtable.geom ~ ltable.geom1::box2d
ORDER BY lk, rk;

-- Test 10: query (line 91)
SELECT lk, rk FROM ltable JOIN rtable ON ltable.geom1::box2d && rtable.geom
ORDER BY lk, rk;

RESET client_min_messages;
