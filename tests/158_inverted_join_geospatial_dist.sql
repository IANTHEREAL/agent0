-- PostgreSQL compatible tests from inverted_join_geospatial_dist
-- 5 tests

-- CockroachDB's geospatial types/functions are provided by PostGIS in PostgreSQL,
-- but this repo runs on vanilla PostgreSQL. Model the small subset needed by
-- this test using built-in geometric types/operators.
CREATE OR REPLACE FUNCTION wkt_point(wkt text)
RETURNS point
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
  m text[];
BEGIN
  m := regexp_match(
    wkt,
    '(?i)^\s*POINT\s*\(\s*([+-]?[0-9]*\.?[0-9]+)\s+([+-]?[0-9]*\.?[0-9]+)\s*\)\s*$'
  );
  IF m IS NULL THEN
    RAISE EXCEPTION 'invalid POINT WKT: %', wkt;
  END IF;
  RETURN point(m[1]::float8, m[2]::float8);
END;
$$;

CREATE OR REPLACE FUNCTION wkt_polygon(wkt text)
RETURNS polygon
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
  inner_coords text;
  poly_text text;
BEGIN
  inner_coords := regexp_replace(
    wkt,
    '(?i)^\s*POLYGON\s*\(\((.*)\)\)\s*$',
    '\1'
  );
  poly_text := regexp_replace(inner_coords, '\s+', ' ', 'g');
  poly_text := regexp_replace(
    poly_text,
    '([+-]?[0-9]*\.?[0-9]+)\s+([+-]?[0-9]*\.?[0-9]+)',
    '(\1,\2)',
    'g'
  );
  poly_text := regexp_replace(poly_text, '\s*,\s*', ',', 'g');
  poly_text := regexp_replace(poly_text, '\s+', '', 'g');
  RETURN ('(' || poly_text || ')')::polygon;
END;
$$;

CREATE OR REPLACE FUNCTION point_on_segment(p point, a point, b point)
RETURNS boolean
LANGUAGE SQL
IMMUTABLE
AS $$
  SELECT
    abs(((p)[0] - (a)[0]) * ((b)[1] - (a)[1]) - ((p)[1] - (a)[1]) * ((b)[0] - (a)[0])) < 1e-9
    AND (p)[0] BETWEEN LEAST((a)[0], (b)[0]) - 1e-9 AND GREATEST((a)[0], (b)[0]) + 1e-9
    AND (p)[1] BETWEEN LEAST((a)[1], (b)[1]) - 1e-9 AND GREATEST((a)[1], (b)[1]) + 1e-9;
$$;

CREATE OR REPLACE FUNCTION st_intersects(a text, b text)
RETURNS boolean
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
  pa point;
  pb point;
  pts text;
  arr text[];
  i int;
  p1 point;
  p2 point;
  poly polygon;
BEGIN
  pa := wkt_point(a);

  IF b ILIKE 'POINT%' THEN
    pb := wkt_point(b);
    RETURN abs((pa)[0] - (pb)[0]) < 1e-9 AND abs((pa)[1] - (pb)[1]) < 1e-9;
  ELSIF b ILIKE 'LINESTRING%' THEN
    pts := regexp_replace(b, '(?i)^\s*LINESTRING\s*\((.*)\)\s*$', '\1');
    arr := regexp_split_to_array(pts, '\s*,\s*');
    IF array_length(arr, 1) IS NULL OR array_length(arr, 1) < 2 THEN
      RAISE EXCEPTION 'invalid LINESTRING WKT: %', b;
    END IF;
    FOR i IN 1..array_length(arr, 1) - 1 LOOP
      p1 := wkt_point('POINT(' || arr[i] || ')');
      p2 := wkt_point('POINT(' || arr[i + 1] || ')');
      IF point_on_segment(pa, p1, p2) THEN
        RETURN true;
      END IF;
    END LOOP;
    RETURN false;
  ELSIF b ILIKE 'POLYGON%' THEN
    poly := wkt_polygon(b);
    RETURN poly @> pa;
  ELSE
    RAISE EXCEPTION 'unsupported geometry WKT: %', b;
  END IF;
END;
$$;

-- Test 1: statement (line 3)
CREATE TABLE ltable(
  lk int primary key,
  geom1 text,
  geom2 text
);

-- Test 2: statement (line 10)
INSERT INTO ltable VALUES
  (1, 'POINT(3.0 3.0)', 'POINT(3.0 3.0)'),
  (2, 'POINT(4.5 4.5)', 'POINT(3.0 3.0)'),
  (3, 'POINT(1.5 1.5)', 'POINT(3.0 3.0)');

-- Test 3: statement (line 16)
CREATE TABLE rtable(
  rk int primary key,
  geom text
);
CREATE INDEX geom_index ON rtable (geom);

-- Test 4: statement (line 23)
INSERT INTO rtable VALUES
  (11, 'POINT(1.0 1.0)'),
  (12, 'LINESTRING(1.0 1.0, 2.0 2.0)'),
  (13, 'POINT(3.0 3.0)'),
  (14, 'LINESTRING(4.0 4.0, 5.0 5.0)'),
  (15, 'LINESTRING(40.0 40.0, 41.0 41.0)'),
  (16, 'POLYGON((1.0 1.0, 5.0 1.0, 5.0 5.0, 1.0 5.0, 1.0 1.0))');

-- Test 5: query (line 47)
-- CockroachDB uses an index hint (rtable@geom_index); PostgreSQL ignores hints.
SELECT lk, rk
FROM ltable
JOIN rtable ON st_intersects(ltable.geom1, rtable.geom)
ORDER BY lk, rk;
