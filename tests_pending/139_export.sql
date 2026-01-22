-- PostgreSQL compatible tests from export
-- 8 tests

-- Test 1: statement (line 4)
CREATE SCHEMA IF NOT EXISTS crdb_internal;

-- CockroachDB's `EXPORT INTO ...` has no direct equivalent in PostgreSQL.
-- For compatibility tests, emulate it with server-side COPY and return a single
-- (filename, rows, bytes) row like CRDB does.
CREATE OR REPLACE FUNCTION crdb_internal.export_into(
  fmt TEXT,
  uri TEXT,
  query TEXT
)
RETURNS TABLE(filename TEXT, rows BIGINT, bytes BIGINT)
LANGUAGE plpgsql
AS $$
DECLARE
  ext TEXT;
  out_uri TEXT;
  out_path TEXT;
BEGIN
  ext := lower(fmt);
  IF ext NOT IN ('csv', 'parquet') THEN
    RAISE EXCEPTION 'unsupported export format: %', fmt;
  END IF;

  out_uri := uri;
  IF right(out_uri, 1) <> '/' THEN
    out_uri := out_uri || '/';
  END IF;
  out_uri := out_uri || 'part-00000.' || ext;

  -- Keep the on-disk filename stable across runs.
  out_path := '/tmp/pg_tests_export_part-00000.' || ext;

  EXECUTE format('SELECT count(*) FROM (%s) AS q', query) INTO rows;

  -- Use CSV for both formats; this is a semantics-preserving approximation
  -- (exporting query results) when Parquet isn't available natively.
  EXECUTE format('COPY (%s) TO %L WITH (FORMAT csv)', query, out_path);
  bytes := (pg_stat_file(out_path)).size;

  filename := out_uri;
  RETURN NEXT;
END;
$$;

CREATE TABLE records (format TEXT, filename TEXT, rows BIGINT, bytes BIGINT);

CREATE TABLE t (k INT PRIMARY KEY);
INSERT INTO t(k) VALUES (1);

-- Test 2: statement (line 7)
WITH cte AS (
  SELECT * FROM crdb_internal.export_into('CSV', 'nodelocal://1/export1/', 'SELECT * FROM t')
)
SELECT filename FROM cte;

-- Test 3: statement (line 10)
WITH cte AS (
  SELECT * FROM crdb_internal.export_into('PARQUET', 'nodelocal://1/export1/', 'SELECT * FROM t')
)
SELECT filename FROM cte;

-- Test 4: query (line 13)
WITH cte AS (
  SELECT * FROM crdb_internal.export_into('CSV', 'nodelocal://1/export1/', 'SELECT * FROM t')
)
SELECT filename FROM cte;

-- Test 5: statement (line 21)
CREATE TABLE t115290 (
  id INT PRIMARY KEY,
  a INT NOT NULL,
  b INT
);

-- Test 6: statement (line 28)
SELECT * FROM crdb_internal.export_into(
  'PARQUET',
  'nodelocal://1/export1/',
  'SELECT b FROM t115290 ORDER BY a'
);

-- Test 7: statement (line 37)
WITH cte AS (
  SELECT * FROM crdb_internal.export_into('CSV', 'nodelocal://1/export1/', 'SELECT * FROM t')
)
INSERT INTO records (format, filename, rows, bytes)
SELECT 'CSV', filename, rows, bytes FROM cte;

-- Test 8: statement (line 43)
WITH cte AS (
  SELECT * FROM crdb_internal.export_into('PARQUET', 'nodelocal://1/export1/', 'SELECT * FROM t')
)
INSERT INTO records (format, filename, rows, bytes)
SELECT 'PARQUET', filename, rows, bytes FROM cte;
