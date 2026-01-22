-- PostgreSQL compatible tests from stats
-- 20 tests

-- Test 1: statement (line 6)
-- CockroachDB-only setting.
-- SET CLUSTER SETTING jobs.registry.interval.adopt = '10ms';

-- Test 2: statement (line 14)
CREATE TABLE t (c INT2);

-- Test 3: statement (line 21)
INSERT INTO t SELECT generate_series(1, 10000);
INSERT INTO t SELECT generate_series(-10000, 0);

-- Test 4: statement (line 25)
-- Ensure ANALYZE builds a stable histogram (scan-all due to stats target vs row count).
ALTER TABLE t ALTER COLUMN c SET STATISTICS 100;
ANALYZE t;

-- Test 5: query (line 37)
-- CockroachDB's SHOW HISTOGRAM isn't available in PostgreSQL; use pg_stats instead.
-- We validate that the histogram spans the full data range we inserted.
WITH hb AS (
  SELECT trim(both '{}' from histogram_bounds::text) AS hb
  FROM pg_stats
  WHERE schemaname = 'public' AND tablename = 't' AND attname = 'c'
)
SELECT
  split_part(hb, ',', 1)::INT = -10000
  AND split_part(hb, ',', array_length(string_to_array(hb, ','), 1))::INT = 10000
FROM hb;

-- Test 6: statement (line 49)
CREATE TYPE greeting AS ENUM ('hello', 'hi', 'yo');
CREATE TABLE t122312 (g greeting);

-- Test 7: statement (line 55)
ANALYZE t122312;

-- Test 8: statement (line 60)
INSERT INTO t122312 VALUES ('hi');

-- Test 9: statement (line 63)
ANALYZE t122312;

-- Test 10: statement (line 66)
BEGIN;
ALTER TYPE greeting ADD VALUE 'hey';
SELECT * FROM t122312 WHERE g = 'hi';
COMMIT;

-- Test 11: statement (line 82)
CREATE TABLE t139381 (i INT PRIMARY KEY, j JSONB);
ALTER TABLE t139381 ALTER COLUMN j SET STATISTICS 1;
INSERT INTO t139381
SELECT i, ('{"name": "name_' || i || '", "data": "abcdefghij"}')::JSONB
FROM generate_series(1, 300) v(i);

-- Test 12: statement (line 87)
ANALYZE t139381;

-- Test 13: query (line 90)
-- CockroachDB's SHOW STATISTICS isn't available in PostgreSQL; use pg_stats.
-- We treat a "real" histogram as having > 2 bounds (i.e., more than just endpoints).
SELECT
  ARRAY[attname] AS column_names,
  CASE
    WHEN array_length(histogram_bounds, 1) > 2 THEN 'histogram_collected'
    ELSE 'no_histogram_collected'
  END
FROM pg_stats
WHERE schemaname = 'public' AND tablename = 't139381'
ORDER BY attname;

-- Test 14: statement (line 100)
-- CockroachDB-only setting; approximate by increasing the stats target on the JSONB column.
-- SET CLUSTER SETTING sql.stats.non_indexed_json_histograms.enabled = true;
ALTER TABLE t139381 ALTER COLUMN j SET STATISTICS 100;

-- Test 15: statement (line 103)
ANALYZE t139381;

-- Test 16: query (line 106)
SELECT
  ARRAY[attname] AS column_names,
  CASE
    WHEN array_length(histogram_bounds, 1) > 2 THEN 'histogram_collected'
    ELSE 'no_histogram_collected'
  END
FROM pg_stats
WHERE schemaname = 'public' AND tablename = 't139381'
ORDER BY attname;

-- Test 17: statement (line 116)
CREATE TABLE t141448 (i INT, f FLOAT, b BOOL);

-- Test 18: statement (line 119)
INSERT INTO t141448 VALUES (NULL, NULL, false), (NULL, 1, false), (1, 1, false);

-- Test 19: statement (line 122)
ANALYZE t141448;

-- Test 20: query (line 125)
-- Approximate CRDB's SHOW STATISTICS row_count/distinct_count with pg_class + pg_stats.
WITH row_counts AS (
  SELECT reltuples AS row_count
  FROM pg_class
  WHERE oid = 't141448'::regclass
),
col_stats AS (
  SELECT n_distinct
  FROM pg_stats
  WHERE schemaname = 'public' AND tablename = 't141448' AND attname = 'b'
)
SELECT
  row_count::INT AS row_count,
  CASE
    WHEN n_distinct >= 0 THEN n_distinct::INT
    ELSE round((-n_distinct * row_count)::numeric)::INT
  END AS distinct_count
FROM row_counts, col_stats;
