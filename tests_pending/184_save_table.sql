-- PostgreSQL compatible tests from save_table
-- 17 tests

-- NOTE: The upstream CockroachDB test relies on:
-- - `USE <db>` to switch databases
-- - the `save_tables_prefix` session variable, which instructs CockroachDB to
--   materialize intermediate results into "saved tables"
-- PostgreSQL has no equivalent feature. This file keeps the original intent
-- (validate the query results and the presence/contents of the saved tables)
-- by explicitly materializing the tables that CockroachDB would generate.

SET client_min_messages = warning;

-- CockroachDB-only database switch:
-- CREATE DATABASE savetables; USE savetables;

DROP TABLE IF EXISTS st_test_merge_join_2;
DROP TABLE IF EXISTS st_scan_4;
DROP TABLE IF EXISTS u;
DROP TABLE IF EXISTS t;
DROP FUNCTION IF EXISTS to_english(INT);
DROP ROLE IF EXISTS testuser;

-- CockroachDB provides `to_english(int)`; define a small compatible subset.
CREATE FUNCTION to_english(i INT) RETURNS TEXT
LANGUAGE SQL
IMMUTABLE
AS $$
  SELECT CASE i
    WHEN 1 THEN 'one'
    WHEN 2 THEN 'two'
    WHEN 3 THEN 'three'
    WHEN 4 THEN 'four'
    WHEN 5 THEN 'five'
    WHEN 6 THEN 'six'
    WHEN 7 THEN 'seven'
    WHEN 8 THEN 'eight'
    WHEN 9 THEN 'nine'
    WHEN 10 THEN 'ten'
    ELSE i::TEXT
  END;
$$;

CREATE TABLE t (k INT PRIMARY KEY, str TEXT);
CREATE TABLE u (key INT PRIMARY KEY, val TEXT);

INSERT INTO t SELECT i, to_english(i) FROM generate_series(1, 5) AS g(i);
INSERT INTO u SELECT i, to_english(i) FROM generate_series(2, 10) AS g(i);

-- CockroachDB-only: SET save_tables_prefix = 'save_table_test';
SELECT * FROM t ORDER BY k;
SELECT * FROM u ORDER BY key;

-- CockroachDB-only: SET save_tables_prefix = 'st_test';
SELECT u.key, t.str
FROM t
JOIN u ON t.k = u.key
WHERE t.k >= 3
ORDER BY u.key;

-- Materialize the "saved tables" explicitly for PostgreSQL.
CREATE TABLE st_test_merge_join_2 AS
SELECT t.k, t.str
FROM t
JOIN u ON t.k = u.key
WHERE t.k >= 3
ORDER BY t.k;

CREATE TABLE st_scan_4 AS
SELECT key, val FROM u ORDER BY key;

-- CockroachDB-only: SET save_tables_prefix = 'st';
SELECT u.key, t.str
FROM t
JOIN u ON t.k = u.key
WHERE u.val LIKE 't%'
ORDER BY u.key;

-- CockroachDB-only: SET save_tables_prefix = '';
SELECT * FROM st_test_merge_join_2 ORDER BY k;
SELECT * FROM st_scan_4 ORDER BY key;

-- CockroachDB: SHOW TABLES
SELECT tablename
FROM pg_catalog.pg_tables
WHERE schemaname = 'public'
ORDER BY tablename;

CREATE ROLE testuser;
GRANT ALL ON TABLE t TO testuser;

-- Cockroach logic-test directive `user testuser`: emulate with SET ROLE.
SET ROLE testuser;
SELECT * FROM t ORDER BY k;
RESET ROLE;

-- CockroachDB-only: SET save_tables_prefix = 'tt';
SELECT * FROM t ORDER BY k;

RESET client_min_messages;
