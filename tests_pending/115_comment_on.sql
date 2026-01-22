-- PostgreSQL compatible tests from comment_on
-- 88 tests
--
-- Note: This file is adapted from CockroachDB tests. Cockroach-specific
-- catalogs/commands like system.comments, crdb_internal.invalid_objects,
-- SHOW ... WITH COMMENT, USE, and schema changer toggles have no direct
-- PostgreSQL equivalent, so this version validates comments via PostgreSQL
-- catalogs (pg_database/pg_namespace/pg_type/pg_description).

SET client_min_messages = warning;

-- Capture the current database name so we can COMMENT ON DATABASE without
-- creating/dropping extra databases (keeps the runner isolated).
SELECT current_database() AS current_db \gset

-- Database comments.
COMMENT ON DATABASE :"current_db" IS 'A';
SELECT datname AS database_name,
       shobj_description(oid, 'pg_database') AS comment
FROM pg_database
WHERE datname = :'current_db';

COMMENT ON DATABASE :"current_db" IS 'AAA';
SELECT datname AS database_name,
       shobj_description(oid, 'pg_database') AS comment
FROM pg_database
WHERE datname = :'current_db';

COMMENT ON DATABASE :"current_db" IS NULL;
SELECT datname AS database_name,
       shobj_description(oid, 'pg_database') AS comment
FROM pg_database
WHERE datname = :'current_db';

-- Schema comments.
CREATE SCHEMA sc;
COMMENT ON SCHEMA sc IS 'SC';
SELECT obj_description('sc'::regnamespace, 'pg_namespace') AS comment;

COMMENT ON SCHEMA sc IS 'SC_AGAIN';
SELECT obj_description('sc'::regnamespace, 'pg_namespace') AS comment;

SELECT nspname AS schema_name,
       obj_description(oid, 'pg_namespace') AS comment
FROM pg_namespace
WHERE nspname IN ('public', 'sc')
ORDER BY nspname;

-- Cockroach "db.schema" naming: emulate using a quoted schema name containing a dot.
CREATE SCHEMA "db.schema1";
COMMENT ON SCHEMA "db.schema1" IS 'Database_Schema';
SELECT nspname AS schema_name,
       obj_description(oid, 'pg_namespace') AS comment
FROM pg_namespace
WHERE nspname = 'db.schema1';

-- Table/column/index/constraint comments.
CREATE TABLE t(
  a INT PRIMARY KEY,
  b INT NOT NULL,
  CONSTRAINT ckb CHECK (b > 1)
);
CREATE INDEX idxb ON t(b);

CREATE VIEW t_comments AS
SELECT
  obj_description('t'::regclass, 'pg_class') AS table_comment,
  col_description(
    't'::regclass,
    (SELECT attnum FROM pg_attribute WHERE attrelid = 't'::regclass AND attname = 'b')
  ) AS column_b_comment,
  obj_description('idxb'::regclass, 'pg_class') AS index_comment,
  obj_description(
    (SELECT oid FROM pg_constraint WHERE conname = 'ckb' AND conrelid = 't'::regclass),
    'pg_constraint'
  ) AS constraint_comment;

COMMENT ON TABLE t IS 'table t';
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON TABLE t IS 'table t AGAIN';
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON TABLE t IS NULL;
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON COLUMN t.b IS 'column b';
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON COLUMN t.b IS 'column b AGAIN';
-- PostgreSQL requires table qualification for COMMENT ON COLUMN.
COMMENT ON COLUMN t.b IS 'unqualified column b';
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON COLUMN t.b IS NULL;
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON INDEX idxb IS 'index b';
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON INDEX idxb IS 'index b AGAIN';
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON INDEX idxb IS NULL;
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON CONSTRAINT ckb ON t IS 'cst b';
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON CONSTRAINT ckb ON t IS 'cst b AGAIN';
SELECT * FROM t_comments;
SELECT * FROM t_comments;

COMMENT ON CONSTRAINT ckb ON t IS NULL;
SELECT * FROM t_comments;
SELECT * FROM t_comments;

-- Clear schema comment.
COMMENT ON SCHEMA sc IS NULL;
SELECT obj_description('sc'::regnamespace, 'pg_namespace') AS comment;

-- Type comments.
CREATE TYPE roach_dwellings AS ENUM ('roach_motel','roach_kitchen','roach_bathroom','roach_house');
CREATE TYPE roach_legs AS (legs INT);

COMMENT ON TYPE roach_dwellings IS 'First-CRDB-comment-on-types';
COMMENT ON TYPE roach_legs IS 'Second-CRDB-comment-on-types';
COMMENT ON TYPE roach_dwellings IS 'First-CRDB-comment-on-types-again';
COMMENT ON TYPE roach_legs IS 'Second-CRDB-comment-on-types-again';

SELECT n.nspname AS schema_name,
       t.typname AS type_name,
       obj_description(t.oid, 'pg_type') AS comment
FROM pg_type t
JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE n.nspname = 'public'
  AND t.typname IN ('roach_dwellings','roach_legs')
ORDER BY type_name;

COMMENT ON TYPE roach_dwellings IS NULL;
COMMENT ON TYPE roach_legs IS NULL;

SELECT n.nspname AS schema_name,
       t.typname AS type_name,
       obj_description(t.oid, 'pg_type') AS comment
FROM pg_type t
JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE n.nspname = 'public'
  AND t.typname IN ('roach_dwellings','roach_legs')
ORDER BY type_name;

-- Cockroach's CREATE DATABASE/USE test_db: emulate with a schema in PostgreSQL.
CREATE SCHEMA test_db;
SET search_path TO test_db;

CREATE TYPE roach_type AS ENUM ('option1', 'option2');
COMMENT ON TYPE roach_type IS 'This is a test comment on a type';

SELECT n.nspname AS schema_name,
       t.typname AS type_name,
       obj_description(t.oid, 'pg_type') AS comment
FROM pg_type t
JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE n.nspname = 'test_db'
  AND t.typname = 'roach_type';

SET search_path TO public;
DROP SCHEMA test_db CASCADE;

SELECT schema_name FROM information_schema.schemata WHERE schema_name = 'test_db';

RESET client_min_messages;
