-- Ported from pg_tests PR#58: compatible/create_statements.sql

SET client_min_messages = warning;

-- Cleanup from prior runs.
DROP TABLE IF EXISTS t_pgtests_create_statements_t CASCADE;
DROP TABLE IF EXISTS t_pgtests_create_statements_c CASCADE;

-- Basic tables.
CREATE TABLE t_pgtests_create_statements_t (
  a INT PRIMARY KEY
);

CREATE TABLE t_pgtests_create_statements_c (
  a INT NOT NULL,
  b INT NULL,
  PRIMARY KEY (a)
);

-- Index + comments.
CREATE INDEX t_pgtests_create_statements_c_a_b_idx
  ON t_pgtests_create_statements_c (a ASC, b ASC);

COMMENT ON TABLE t_pgtests_create_statements_c IS 'table';
COMMENT ON COLUMN t_pgtests_create_statements_c.a IS 'column';

-- Verify index metadata via pg_indexes.
SELECT indexname,
       (lower(indexdef) LIKE '%(a, b)%') AS has_cols
FROM pg_catalog.pg_indexes
WHERE schemaname = 'public'
  AND tablename = 't_pgtests_create_statements_c'
  AND indexname = 't_pgtests_create_statements_c_a_b_idx'
ORDER BY indexname;

-- Verify nullability metadata (explicit NULL should be accepted).
SELECT a.attname, a.attnotnull
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
WHERE c.relname = 't_pgtests_create_statements_c'
  AND a.attname IN ('a', 'b')
ORDER BY a.attname;

-- Verify comments are persisted and discoverable via pg_description.
SELECT c.relname, d.description
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.objsubid = 0
WHERE c.relname = 't_pgtests_create_statements_c'
ORDER BY c.relname;

SELECT a.attname, d.description
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.objsubid = a.attnum
WHERE c.relname = 't_pgtests_create_statements_c'
  AND a.attname = 'a'
ORDER BY a.attname;

SELECT 'create_statements tests completed successfully' AS result;

-- Cleanup.
DROP TABLE IF EXISTS t_pgtests_create_statements_t CASCADE;
DROP TABLE IF EXISTS t_pgtests_create_statements_c CASCADE;
