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

SELECT pg_get_serial_sequence('pgss_missing', 'id');
SELECT pg_get_serial_sequence('pgss_basic', 'missing_col');
SELECT pg_get_serial_sequence('db.public.pgss_basic', 'id');
SELECT pg_get_serial_sequence('', 'id');
SELECT pg_get_serial_sequence('pgss_case', 'casecol');
SELECT public.pg_get_serial_sequence('t', 'id');

DROP TABLE IF EXISTS pgss_basic CASCADE;
DROP TABLE IF EXISTS pgss_collision CASCADE;
DROP TABLE IF EXISTS pgss_case CASCADE;
DROP TABLE IF EXISTS pgss_long_table_name_component_aaaaaaaaaaaaaaaa CASCADE;
DROP TABLE IF EXISTS "select".pgss_kw CASCADE;
DROP SCHEMA IF EXISTS "select" CASCADE;
DROP SEQUENCE IF EXISTS public.pgss_collision_id_seq;
