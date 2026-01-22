-- PostgreSQL compatible tests from distsql_automatic_partial_stats
-- 29 tests

SET client_min_messages = warning;

-- CockroachDB cluster settings / automatic stats knobs are not applicable in PostgreSQL.
-- We simulate SHOW STATISTICS via deterministic, exact snapshots stored in a table.

CREATE TABLE data (
  a INT,
  b INT,
  c DOUBLE PRECISION,
  d DECIMAL,
  PRIMARY KEY (a, b, c)
);
CREATE INDEX c_idx ON data (c);
CREATE INDEX d_idx ON data (d);

CREATE SEQUENCE data_stats_created_seq;

CREATE TABLE data_stats_history (
  statistics_name TEXT NOT NULL,
  column_names TEXT NOT NULL,
  row_count BIGINT NOT NULL,
  distinct_count BIGINT NOT NULL,
  null_count BIGINT NOT NULL,
  partial_predicate TEXT,
  created BIGINT NOT NULL
);

CREATE OR REPLACE FUNCTION collect_data_stats(stat_name TEXT) RETURNS INT
LANGUAGE SQL AS $$
  WITH snap AS (SELECT nextval('data_stats_created_seq')::bigint AS created)
  INSERT INTO data_stats_history (
    statistics_name, column_names, row_count, distinct_count, null_count, partial_predicate, created
  )
  SELECT
    stat_name,
    s.column_names,
    s.row_count,
    s.distinct_count,
    s.null_count,
    NULL::text AS partial_predicate,
    snap.created
  FROM snap,
  LATERAL (
    SELECT
      '{a}'::text AS column_names,
      COUNT(*)::bigint AS row_count,
      COUNT(DISTINCT a)::bigint AS distinct_count,
      COUNT(*) FILTER (WHERE a IS NULL)::bigint AS null_count
    FROM data
    UNION ALL
    SELECT '{b}', COUNT(*)::bigint, COUNT(DISTINCT b)::bigint, COUNT(*) FILTER (WHERE b IS NULL)::bigint FROM data
    UNION ALL
    SELECT '{c}', COUNT(*)::bigint, COUNT(DISTINCT c)::bigint, COUNT(*) FILTER (WHERE c IS NULL)::bigint FROM data
    UNION ALL
    SELECT '{d}', COUNT(*)::bigint, COUNT(DISTINCT d)::bigint, COUNT(*) FILTER (WHERE d IS NULL)::bigint FROM data
  ) AS s;
  SELECT 4;
$$;

INSERT INTO data
SELECT a, b, c::double precision, 1
FROM generate_series(1, 10) AS a(a),
     generate_series(1, 10) AS b(b),
     generate_series(1, 10) AS c(c);
SELECT collect_data_stats('__auto__');

SELECT DISTINCT ON (statistics_name, column_names)
  statistics_name, column_names, row_count, distinct_count, null_count, partial_predicate
FROM data_stats_history
ORDER BY statistics_name, column_names, created DESC;

-- CREATE STATISTICS __auto__ FROM data (Cockroach) -> take another snapshot.
SELECT collect_data_stats('__auto__');

SELECT DISTINCT ON (statistics_name, column_names)
  statistics_name, column_names, row_count, distinct_count, null_count, partial_predicate
FROM data_stats_history
ORDER BY statistics_name, column_names, created DESC;

UPDATE data SET d = 2 WHERE a = 1;
SELECT collect_data_stats('__auto__');

SELECT DISTINCT ON (statistics_name, column_names)
  statistics_name, column_names, row_count, distinct_count, null_count, partial_predicate
FROM data_stats_history
ORDER BY statistics_name, column_names, created DESC;

-- Table reloptions for stats collection are CockroachDB-only.
-- ALTER TABLE data SET (sql_stats_automatic_partial_collection_enabled = false);
-- ALTER TABLE data SET (sql_stats_automatic_full_collection_enabled = false);

UPDATE data SET d = 3 WHERE a IN (1, 2);
SELECT collect_data_stats('__auto__');

SELECT DISTINCT ON (statistics_name, column_names)
  statistics_name, column_names, row_count, distinct_count, null_count, partial_predicate
FROM data_stats_history
ORDER BY statistics_name, column_names, created DESC;

-- CockroachDB-only: sql.internal_executor.session_overrides / EnableCreateStatsUsingExtremes.

UPDATE data SET d = 4 WHERE a IN (1, 2);
SELECT collect_data_stats('__auto__');

SELECT DISTINCT ON (statistics_name, column_names)
  statistics_name, column_names, row_count, distinct_count, null_count, partial_predicate
FROM data_stats_history
ORDER BY statistics_name, column_names, created DESC;

INSERT INTO data (a, b, c)
SELECT a, b, c::double precision
FROM generate_series(11, 14) AS a(a),
     generate_series(11, 14) AS b(b),
     generate_series(11, 14) AS c(c);
SELECT collect_data_stats('__auto__');

SELECT DISTINCT ON (statistics_name, column_names)
  statistics_name, column_names, row_count, distinct_count, null_count, partial_predicate
FROM data_stats_history
ORDER BY statistics_name, column_names, created DESC;

INSERT INTO data (a, b, c, d)
SELECT a, b, c::double precision, 5
FROM generate_series(11, 15) AS a(a),
     generate_series(11, 14) AS b(b),
     generate_series(11, 13) AS c(c)
ON CONFLICT (a, b, c) DO UPDATE SET d = EXCLUDED.d;
SELECT collect_data_stats('__auto__');

SELECT DISTINCT ON (statistics_name, column_names)
  statistics_name, column_names, row_count, distinct_count, null_count, partial_predicate
FROM data_stats_history
ORDER BY statistics_name, column_names, created DESC;

DELETE FROM data WHERE a > 11;
SELECT collect_data_stats('__auto__');

SELECT DISTINCT ON (statistics_name, column_names)
  statistics_name, column_names, row_count, distinct_count, null_count, partial_predicate
FROM data_stats_history
ORDER BY statistics_name, column_names, created DESC;

INSERT INTO data (a, b, c)
SELECT a, b, c::double precision
FROM generate_series(15, 25) AS a(a),
     generate_series(15, 25) AS b(b),
     generate_series(15, 25) AS c(c);
SELECT collect_data_stats('__auto__');

SELECT DISTINCT ON (statistics_name, column_names)
  statistics_name, column_names, row_count, distinct_count, null_count, partial_predicate
FROM data_stats_history
ORDER BY statistics_name, column_names, created DESC;

RESET client_min_messages;
