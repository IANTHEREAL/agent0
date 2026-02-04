-- ported from pg_tests PR#58: compatible/case_sensitive_names.sql
--
-- Quoted identifier case sensitivity (schema/table/column/index/view names).

SET client_min_messages = warning;

-- Cleanup from prior runs.
DROP VIEW IF EXISTS t146_csn_v;
DROP VIEW IF EXISTS "T146_CSN_V";
DROP TABLE IF EXISTS t146_csn_foo;
DROP TABLE IF EXISTS t146_csn_idx_test;
DROP TABLE IF EXISTS t146_csn_schema.a;
DROP TABLE IF EXISTS t146_csn_schema."A";
DROP SCHEMA IF EXISTS t146_csn_schema;
DROP SCHEMA IF EXISTS "T146_CSN_SCHEMA";

-- Schema names (unquoted identifiers fold to lower-case).
CREATE SCHEMA t146_csn_schema;
CREATE SCHEMA "T146_CSN_SCHEMA";

-- Initially empty.
SELECT table_schema, table_name
FROM information_schema.tables
WHERE table_schema IN ('t146_csn_schema', 'T146_CSN_SCHEMA')
  AND table_type = 'BASE TABLE'
ORDER BY table_schema, table_name;

-- Table names: a vs "A" are distinct.
CREATE TABLE t146_csn_schema.a (x INT);
CREATE TABLE t146_csn_schema."A" (x INT);
INSERT INTO t146_csn_schema.a VALUES (1);
INSERT INTO t146_csn_schema."A" VALUES (2);

SELECT table_schema, table_name
FROM information_schema.tables
WHERE table_schema = 't146_csn_schema'
  AND table_type = 'BASE TABLE'
ORDER BY CASE table_name WHEN 'a' THEN 1 WHEN 'A' THEN 2 ELSE 3 END;

SELECT x FROM t146_csn_schema.a ORDER BY x;
SELECT x FROM t146_csn_schema."A" ORDER BY x;

-- Column names: y vs "Y" are distinct; unquoted references fold to lower-case.
CREATE TABLE t146_csn_foo (x INT, y INT, "Y" INT);
INSERT INTO t146_csn_foo (x, y, "Y") VALUES (1, 10, 20);

SELECT x, y, "Y" FROM t146_csn_foo;
SELECT "Y" FROM t146_csn_foo;

-- Views: v vs "V" are distinct.
CREATE VIEW t146_csn_v AS SELECT x, "Y" FROM t146_csn_foo;
CREATE VIEW "T146_CSN_V" AS SELECT x, "Y" FROM t146_csn_foo;

SELECT x, "Y" FROM t146_csn_v ORDER BY x;
SELECT x, "Y" FROM "T146_CSN_V" ORDER BY x;

-- Index names: i vs "I" are distinct.
CREATE TABLE t146_csn_idx_test (x INT, y INT);
CREATE INDEX t146_csn_i ON t146_csn_idx_test (x);
CREATE INDEX "T146_CSN_I" ON t146_csn_idx_test (y);

SELECT indexname
FROM pg_indexes
WHERE tablename = 't146_csn_idx_test'
ORDER BY CASE indexname WHEN 't146_csn_i' THEN 1 WHEN 'T146_CSN_I' THEN 2 ELSE 3 END;

-- Function names are case-insensitive.
SELECT LENGTH('abc') AS len, length('abc') AS len2;

-- Cleanup.
DROP VIEW t146_csn_v;
DROP VIEW "T146_CSN_V";
DROP TABLE t146_csn_foo;
DROP TABLE t146_csn_idx_test;
DROP TABLE t146_csn_schema.a;
DROP TABLE t146_csn_schema."A";
DROP SCHEMA t146_csn_schema;
DROP SCHEMA "T146_CSN_SCHEMA";
