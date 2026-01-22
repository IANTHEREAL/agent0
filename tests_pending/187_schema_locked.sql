-- PostgreSQL compatible tests from schema_locked
-- 62 tests

SET client_min_messages = warning;

-- CockroachDB's schema_locked / create_table_with_schema_locked are not available
-- in PostgreSQL.
--
-- As a proxy, we use the per-table reloption `autovacuum_enabled=false` to model a
-- boolean table-level setting that can be SET/RESET and inspected.

DROP ROLE IF EXISTS schema_locked_testuser;
CREATE ROLE schema_locked_testuser;

DROP TABLE IF EXISTS t;
CREATE TABLE t (i INT PRIMARY KEY) WITH (autovacuum_enabled = false);

SELECT COALESCE(array_to_string(reloptions, ','), '') AS reloptions
FROM pg_class
WHERE oid = 't'::regclass;

ALTER TABLE t RESET (autovacuum_enabled);

SELECT COALESCE(array_to_string(reloptions, ','), '') AS reloptions
FROM pg_class
WHERE oid = 't'::regclass;

ALTER TABLE t SET (autovacuum_enabled = false);

SELECT COALESCE(array_to_string(reloptions, ','), '') AS reloptions
FROM pg_class
WHERE oid = 't'::regclass;

ALTER TABLE t RESET (autovacuum_enabled);
DROP TABLE t;

-- DDL is transactional: verify a reloption change is rolled back.
DROP TABLE IF EXISTS t;
CREATE TABLE t (i INT PRIMARY KEY) WITH (autovacuum_enabled = false);

BEGIN ISOLATION LEVEL SERIALIZABLE;
SET LOCAL lock_timeout = '1s';
ALTER TABLE t RESET (autovacuum_enabled);
ROLLBACK;

SELECT COALESCE(array_to_string(reloptions, ','), '') AS reloptions
FROM pg_class
WHERE oid = 't'::regclass;

DROP TABLE t;

-- DDL coverage on a "locked" table.
DROP TABLE IF EXISTS t;
DROP TABLE IF EXISTS ref;

CREATE TABLE t (i INT PRIMARY KEY, j INT);
CREATE UNIQUE INDEX idx ON t (j);
ALTER TABLE t SET (autovacuum_enabled = false);

INSERT INTO t SELECT i, i + 1 FROM generate_series(1, 10) AS tmp(i);

ALTER TABLE t ADD COLUMN k INT DEFAULT 30;
CREATE INDEX idx2 ON t (j);

CREATE TABLE ref (a INT PRIMARY KEY, b INT);
ALTER TABLE ref ADD CONSTRAINT fk FOREIGN KEY (b) REFERENCES t(j);

GRANT DELETE ON TABLE t TO schema_locked_testuser WITH GRANT OPTION;

COMMENT ON TABLE t IS 't is a table';
COMMENT ON INDEX idx IS 'idx is an index';
COMMENT ON COLUMN t.i IS 'i is a column';

ALTER TABLE t DROP COLUMN j CASCADE;

SELECT COUNT(*) AS locked_option_set
FROM pg_class
WHERE oid = 't'::regclass
  AND COALESCE(reloptions, ARRAY[]::text[]) @> ARRAY['autovacuum_enabled=false'];

ALTER TABLE t RESET (autovacuum_enabled);

DROP TABLE t;
DROP TABLE ref;

-- Another table for SET/RESET visibility.
DROP TABLE IF EXISTS t_sl;
CREATE TABLE t_sl (i INT PRIMARY KEY);

SELECT COALESCE(array_to_string(reloptions, ','), '') AS reloptions
FROM pg_class
WHERE oid = 't_sl'::regclass;

ALTER TABLE t_sl SET (autovacuum_enabled = false);

SELECT COALESCE(array_to_string(reloptions, ','), '') AS reloptions
FROM pg_class
WHERE oid = 't_sl'::regclass;

ALTER TABLE t_sl RESET (autovacuum_enabled);

SELECT COALESCE(array_to_string(reloptions, ','), '') AS reloptions
FROM pg_class
WHERE oid = 't_sl'::regclass;

DROP TABLE t_sl;

RESET client_min_messages;
