-- pg_get_serial_sequence canonical resolver contract

DROP TABLE IF EXISTS pgss_basic CASCADE;
DROP TABLE IF EXISTS pgss_collision CASCADE;
DROP TABLE IF EXISTS pgss_case CASCADE;
DROP TABLE IF EXISTS pgss_long_table_name_component_aaaaaaaaaaaaaaaa CASCADE;
DROP TABLE IF EXISTS "select".pgss_kw CASCADE;
DROP SCHEMA IF EXISTS "select" CASCADE;
DROP SEQUENCE IF EXISTS public.pgss_collision_id_seq;

CREATE TABLE pgss_basic (
    id SERIAL PRIMARY KEY,
    payload TEXT
);

SELECT 'basic=' || COALESCE(pg_get_serial_sequence('pgss_basic', 'id'), 'NULL') AS probe;
SELECT 'basic_schema=' || COALESCE(pg_get_serial_sequence('public.pgss_basic', 'id'), 'NULL') AS probe;
SELECT 'non_serial=' || COALESCE(pg_get_serial_sequence('pgss_basic', 'payload'), 'NULL') AS probe;

CREATE SEQUENCE public.pgss_collision_id_seq;
CREATE TABLE pgss_collision (id SERIAL PRIMARY KEY);
SELECT 'collision=' || COALESCE(pg_get_serial_sequence('pgss_collision', 'id'), 'NULL') AS probe;

CREATE TABLE pgss_case ("CaseCol" SERIAL PRIMARY KEY);
SELECT 'case_exact=' || COALESCE(pg_get_serial_sequence('pgss_case', 'CaseCol'), 'NULL') AS probe;

CREATE SCHEMA "select";
CREATE TABLE "select".pgss_kw (id SERIAL PRIMARY KEY);
SELECT 'keyword_schema=' || COALESCE(pg_get_serial_sequence('"select".pgss_kw', 'id'), 'NULL') AS probe;

CREATE TABLE pgss_long_table_name_component_aaaaaaaaaaaaaaaa (
    long_column_name_component_bbbbbbbbbbbbbbbb SERIAL
);
SELECT 'long_name=' || COALESCE(
    pg_get_serial_sequence(
        'pgss_long_table_name_component_aaaaaaaaaaaaaaaa',
        'long_column_name_component_bbbbbbbbbbbbbbbb'
    ),
    'NULL'
) AS probe;

-- name type implicit cast to text (PG allows name -> text)
SELECT 'name_cast=' || COALESCE(pg_get_serial_sequence('pgss_basic'::name, 'id'::name), 'NULL') AS probe;

SELECT pg_get_serial_sequence('pgss_missing', 'id');
SELECT pg_get_serial_sequence('pgss_basic', 'missing_col');
SELECT pg_get_serial_sequence('db.public.pgss_basic', 'id');
SELECT pg_get_serial_sequence('', 'id');
SELECT pg_get_serial_sequence('pgss_case', 'casecol');
SELECT pg_get_serial_sequence(1, 2);
SELECT public.pg_get_serial_sequence('t', 'id');
-- P1: schema-qualified call with wrong arg types must include schema in error
SELECT public.pg_get_serial_sequence(1, 2);
-- quoted "PG_CATALOG" is case-sensitive: schema does not exist
SELECT "PG_CATALOG".pg_get_serial_sequence('pgss_basic', 'id');
-- 3-part qualifier is a cross-database reference
SELECT foo.public.pg_get_serial_sequence('pgss_basic', 'id');
-- P1: existing schema outside search_path → function-not-found (42883), not schema-not-found (3F000)
DROP SCHEMA IF EXISTS pgss_s1 CASCADE;
CREATE SCHEMA pgss_s1;
SELECT pgss_s1.pg_get_serial_sequence(1, 2);
-- nonexistent schema → schema-not-found (3F000)
SELECT pgss_nosuch.pg_get_serial_sequence(1, 2);
DROP SCHEMA IF EXISTS pgss_s1 CASCADE;

DROP TABLE IF EXISTS pgss_basic CASCADE;
DROP TABLE IF EXISTS pgss_collision CASCADE;
DROP TABLE IF EXISTS pgss_case CASCADE;
DROP TABLE IF EXISTS pgss_long_table_name_component_aaaaaaaaaaaaaaaa CASCADE;
DROP TABLE IF EXISTS "select".pgss_kw CASCADE;
DROP SCHEMA IF EXISTS "select" CASCADE;
DROP SEQUENCE IF EXISTS public.pgss_collision_id_seq;
