-- Schemas + search_path + CURRENT_SCHEMA + sequence resolution

-- Cleanup from prior runs
DROP TABLE IF EXISTS app.users;
DROP TABLE IF EXISTS app.t1;
DROP SEQUENCE IF EXISTS app.s;
DROP SEQUENCE IF EXISTS public.s;
DROP SCHEMA IF EXISTS app;

CREATE SCHEMA app;

CREATE TABLE app.users (
  id INT PRIMARY KEY,
  name TEXT
);
INSERT INTO app.users (id, name) VALUES (1, 'a');

SET search_path TO app, public;

-- Unqualified resolution should hit app.users
SELECT 1 / (CASE WHEN (SELECT COUNT(*) FROM users) = 1 THEN 1 ELSE 0 END);

-- Unqualified CREATE TABLE should create in search_path[0]
CREATE TABLE t1 (id INT PRIMARY KEY);
SELECT 1 / (CASE WHEN EXISTS (
  SELECT 1 FROM information_schema.tables
  WHERE table_schema = 'app' AND table_name = 't1'
) THEN 1 ELSE 0 END);

-- information_schema.columns should return schema-qualified table info
SELECT 1 / (CASE WHEN (
  SELECT COUNT(*) FROM information_schema.columns
  WHERE table_schema = 'app' AND table_name = 'users'
) = 2 THEN 1 ELSE 0 END);

-- CURRENT_SCHEMA() should reflect search_path[0] in expressions
SELECT 1 / (CASE WHEN current_schema() = 'app' THEN 1 ELSE 0 END);

-- Sequence resolution should use search_path
CREATE SEQUENCE public.s START WITH 100;
CREATE SEQUENCE app.s START WITH 1;

SELECT 1 / (CASE WHEN nextval('s') = 1 THEN 1 ELSE 0 END);
SELECT 1 / (CASE WHEN currval('s') = 1 THEN 1 ELSE 0 END);

SET search_path TO public, app;
SELECT 1 / (CASE WHEN nextval('s') = 100 THEN 1 ELSE 0 END);

-- DROP SCHEMA should be RESTRICT (fail when not empty)
DROP SCHEMA app;
SELECT 1 / (CASE WHEN EXISTS (
  SELECT 1 FROM information_schema.schemata WHERE schema_name = 'app'
) THEN 1 ELSE 0 END);

-- Cleanup
DROP TABLE app.users;
DROP TABLE app.t1;
DROP SEQUENCE app.s;
DROP SEQUENCE public.s;
DROP SCHEMA app;

SET search_path TO DEFAULT;
SELECT 1 / (CASE WHEN current_schema() = 'public' THEN 1 ELSE 0 END);

