-- PostgreSQL compatible tests from alter_database_convert_to_schema
-- 2 tests

-- Test 1: statement (line 1)
SET client_min_messages = warning;
DROP DATABASE IF EXISTS parent;
DROP DATABASE IF EXISTS pgdatabase;
RESET client_min_messages;

CREATE DATABASE parent;
\c parent
CREATE DATABASE pgdatabase;
\c template1

-- Test 2: statement (line 7)
-- CockroachDB supports converting a database into a schema inside another
-- database; PostgreSQL has no direct equivalent, so emulate the effect.
-- ALTER DATABASE parent CONVERT TO SCHEMA WITH PARENT pgdatabase;
\c pgdatabase
CREATE SCHEMA parent;
\c template1
DROP DATABASE parent;
DROP DATABASE pgdatabase;
