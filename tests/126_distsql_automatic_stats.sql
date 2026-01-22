-- PostgreSQL compatible tests from distsql_automatic_stats
--
-- CockroachDB has automatic statistics collection and SHOW STATISTICS. PostgreSQL
-- does not expose the same surface area, so this file re-implements a tiny,
-- deterministic "SHOW STATISTICS" analogue for this test only.

SET client_min_messages = warning;

-- Minimal stats store + toggle for "automatic stats collection".
CREATE TABLE pg_tests_auto_stats (
  table_oid oid PRIMARY KEY,
  enabled boolean NOT NULL
);

CREATE TABLE pg_tests_stats_history (
  seq bigserial PRIMARY KEY,
  table_oid oid NOT NULL,
  statistics_name text NOT NULL,
  column_names text[] NOT NULL,
  row_count bigint NOT NULL,
  distinct_count bigint NOT NULL,
  null_count bigint NOT NULL,
  created timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE OR REPLACE PROCEDURE pg_tests_set_auto_stats(p_table regclass, p_enabled boolean)
LANGUAGE plpgsql
AS $$
BEGIN
  INSERT INTO pg_tests_auto_stats(table_oid, enabled)
  VALUES (p_table::oid, p_enabled)
  ON CONFLICT (table_oid) DO UPDATE SET enabled = EXCLUDED.enabled;
END;
$$;

CREATE OR REPLACE PROCEDURE pg_tests_collect_stats(p_table regclass, p_statistics_name text)
LANGUAGE plpgsql
AS $$
DECLARE
  col record;
  row_count bigint;
  distinct_count bigint;
  null_count bigint;
BEGIN
  EXECUTE format('SELECT count(*)::bigint FROM %s', p_table) INTO row_count;

  FOR col IN
    SELECT attname
    FROM pg_attribute
    WHERE attrelid = p_table::oid
      AND attnum > 0
      AND NOT attisdropped
    ORDER BY attnum
  LOOP
    EXECUTE format(
      'SELECT count(DISTINCT %I)::bigint, (count(*) - count(%I))::bigint FROM %s',
      col.attname, col.attname, p_table
    )
    INTO distinct_count, null_count;

    INSERT INTO pg_tests_stats_history(
      table_oid, statistics_name, column_names, row_count, distinct_count, null_count
    )
    VALUES (
      p_table::oid, p_statistics_name, ARRAY[col.attname], row_count, distinct_count, null_count
    );
  END LOOP;
END;
$$;

CREATE OR REPLACE PROCEDURE pg_tests_maybe_collect_stats(
  p_table regclass,
  p_statistics_name text DEFAULT 'auto'
)
LANGUAGE plpgsql
AS $$
BEGIN
  IF EXISTS (
    SELECT 1 FROM pg_tests_auto_stats WHERE table_oid = p_table::oid AND enabled
  ) THEN
    CALL pg_tests_collect_stats(p_table, p_statistics_name);
  END IF;
END;
$$;

CREATE OR REPLACE FUNCTION pg_tests_show_statistics(p_table regclass)
RETURNS TABLE(
  seq bigint,
  statistics_name text,
  column_names text[],
  row_count bigint,
  distinct_count bigint,
  null_count bigint,
  created timestamptz
)
LANGUAGE SQL
STABLE
AS $$
  SELECT seq, statistics_name, column_names, row_count, distinct_count, null_count, created
  FROM pg_tests_stats_history
  WHERE table_oid = p_table::oid
$$;

-- Table 'data' starts with automatic stats enabled.
CREATE TABLE data (
  a int,
  b int,
  c double precision,
  d numeric,
  PRIMARY KEY (a, b, c)
);
CREATE INDEX d_idx ON data (d);
CALL pg_tests_set_auto_stats('data'::regclass, true);

INSERT INTO data
SELECT a, b, c::double precision, NULL::numeric
FROM generate_series(1, 10) AS a(a),
     generate_series(1, 10) AS b(b),
     generate_series(1, 10) AS c(c);
CALL pg_tests_maybe_collect_stats('data'::regclass);

SELECT DISTINCT ON (column_names) statistics_name, column_names, row_count, distinct_count, null_count
FROM pg_tests_show_statistics('data'::regclass)
ORDER BY column_names, created DESC, seq DESC;

-- Disable automatic collection: stats should not update after the UPDATE.
CALL pg_tests_set_auto_stats('data'::regclass, false);

UPDATE data SET d = 10 WHERE (a = 1 OR a = 2 OR a = 3) AND b > 1;
CALL pg_tests_maybe_collect_stats('data'::regclass);

SELECT DISTINCT ON (column_names) statistics_name, column_names, row_count, distinct_count, null_count
FROM pg_tests_show_statistics('data'::regclass)
ORDER BY column_names ASC, created DESC, seq DESC;

-- Re-enable automatic collection.
CALL pg_tests_set_auto_stats('data'::regclass, true);

UPDATE data SET d = 12 WHERE d = 10;
CALL pg_tests_maybe_collect_stats('data'::regclass);

SELECT DISTINCT ON (column_names) statistics_name, column_names, row_count, distinct_count, null_count
FROM pg_tests_show_statistics('data'::regclass)
ORDER BY column_names ASC, created DESC, seq DESC;

-- Cockroach UPSERT -> INSERT ... ON CONFLICT in Postgres.
INSERT INTO data (a, b, c, d)
SELECT a, b, c::double precision, 1::numeric
FROM generate_series(1, 11) AS a(a),
     generate_series(1, 10) AS b(b),
     generate_series(1, 5) AS c(c)
ON CONFLICT (a, b, c) DO UPDATE SET d = EXCLUDED.d;
CALL pg_tests_maybe_collect_stats('data'::regclass);

SELECT DISTINCT ON (column_names) statistics_name, column_names, row_count, distinct_count, null_count
FROM pg_tests_show_statistics('data'::regclass)
ORDER BY column_names ASC, created DESC, seq DESC;

DELETE FROM data WHERE c > 5;
CALL pg_tests_maybe_collect_stats('data'::regclass);

SELECT DISTINCT ON (column_names) statistics_name, column_names, row_count, distinct_count, null_count
FROM pg_tests_show_statistics('data'::regclass)
ORDER BY column_names ASC, created DESC, seq DESC;

-- Tables created with "auto stats enabled".
CREATE TABLE copy AS SELECT * FROM data;
CALL pg_tests_set_auto_stats('copy'::regclass, true);
CALL pg_tests_maybe_collect_stats('copy'::regclass);

SELECT DISTINCT ON (column_names) statistics_name, column_names, row_count, null_count
FROM pg_tests_show_statistics('copy'::regclass)
ORDER BY column_names ASC, created DESC, seq DESC;

CREATE TABLE test_create (x int PRIMARY KEY, y char);
CALL pg_tests_set_auto_stats('test_create'::regclass, true);
CALL pg_tests_maybe_collect_stats('test_create'::regclass);

SELECT DISTINCT ON (column_names) statistics_name, column_names, row_count, distinct_count, null_count
FROM pg_tests_show_statistics('test_create'::regclass)
ORDER BY column_names ASC, created DESC, seq DESC;

DELETE FROM copy WHERE true;
CALL pg_tests_maybe_collect_stats('copy'::regclass);

SELECT DISTINCT ON (column_names) statistics_name, column_names, row_count, null_count
FROM pg_tests_show_statistics('copy'::regclass)
ORDER BY column_names ASC, created DESC, seq DESC;

-- Schema-qualified table.
CREATE SCHEMA my_schema;
CREATE TABLE my_schema.my_table (k int, v int);
CALL pg_tests_set_auto_stats('my_schema.my_table'::regclass, true);

INSERT INTO my_schema.my_table
SELECT k, NULL::int FROM generate_series(1, 10) AS k(k);
CALL pg_tests_maybe_collect_stats('my_schema.my_table'::regclass);

SELECT DISTINCT ON (column_names) statistics_name, column_names, row_count, distinct_count, null_count
FROM pg_tests_show_statistics('my_schema.my_table'::regclass)
ORDER BY column_names ASC, created DESC, seq DESC;

RESET client_min_messages;
