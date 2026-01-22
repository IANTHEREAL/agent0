-- PostgreSQL compatible tests from inv_stats
-- 6 tests

SET client_min_messages = warning;

DROP TABLE IF EXISTS t50596;
DROP TABLE IF EXISTS t;

-- Test 1: statement (line 4)
-- CockroachDB cluster setting (no PostgreSQL equivalent).
-- SET CLUSTER SETTING jobs.registry.interval.adopt = '10ms';

-- Test 2: statement (line 10)
CREATE TABLE t (j JSONB, g TEXT);
-- CockroachDB "INVERTED" indexes map most closely to PostgreSQL GIN indexes.
CREATE INDEX t_j_gin ON t USING GIN (j);
-- Use a GIN index over a tsvector expression as an inverted index analogue.
CREATE INDEX t_g_fts_gin ON t USING GIN (to_tsvector('simple', g));
INSERT
INTO
  t
VALUES
  (
    '{"test": "some", "other": {"nested": true, "foo": 3}}',
    '0103000000010000000500000000000000000000000000000000000000000000000000F03F0000000000000000000000000000F03F000000000000F03F0000000000000000000000000000F03F00000000000000000000000000000000'
  );

-- Test 3: statement (line 23)
-- CockroachDB "CREATE STATISTICS ... FROM t" triggers stats collection.
ANALYZE t;

-- Test 4: query (line 45)
-- CockroachDB SHOW HISTOGRAM has no PostgreSQL equivalent; show collected stats instead.
SELECT attname, null_frac, n_distinct, most_common_vals, histogram_bounds
FROM pg_stats
WHERE schemaname = 'public' AND tablename = 't' AND attname = 'j';

-- Test 5: query (line 56)
SELECT attname, null_frac, n_distinct, most_common_vals, histogram_bounds
FROM pg_stats
WHERE schemaname = 'public' AND tablename = 't' AND attname = 'g';

-- Test 6: statement (line 63)
CREATE TABLE t50596 (_jsonb JSONB);
CREATE INDEX t50596_jsonb_gin ON t50596 USING GIN (_jsonb);
INSERT INTO t50596 VALUES ('{}');
ANALYZE t50596;
SELECT NULL FROM t50596 WHERE '1'::jsonb <> _jsonb;

RESET client_min_messages;
