-- Issue #2018: PostgreSQL custom typed-string literals (e.g. mood 'happy')
-- must parse and execute in direct SELECTs, nested expressions, and DDL defaults.

DROP TABLE IF EXISTS qg2018_default_direct CASCADE;
DROP TABLE IF EXISTS qg2018_default_nested CASCADE;
DROP TYPE IF EXISTS mood CASCADE;
DROP SCHEMA IF EXISTS qg2018 CASCADE;

CREATE TYPE mood AS ENUM ('happy', 'sad');

SELECT 'direct_select' AS check_name, mood 'happy'::text;
SELECT 'nested_select' AS check_name, coalesce(mood 'happy', mood 'sad')::text;

CREATE TABLE qg2018_default_direct (c mood DEFAULT mood 'happy');
INSERT INTO qg2018_default_direct DEFAULT VALUES;
SELECT 'default_direct' AS check_name, c::text FROM qg2018_default_direct;

CREATE TABLE qg2018_default_nested (c mood DEFAULT coalesce(mood 'happy', mood 'sad'));
INSERT INTO qg2018_default_nested DEFAULT VALUES;
SELECT 'default_nested' AS check_name, c::text FROM qg2018_default_nested;

CREATE SCHEMA qg2018;
CREATE TYPE qg2018.mood AS ENUM ('happy', 'sad');
SELECT 'qualified_select' AS check_name, qg2018.mood 'sad'::text;

DROP TABLE IF EXISTS qg2018_default_direct CASCADE;
DROP TABLE IF EXISTS qg2018_default_nested CASCADE;
DROP TYPE IF EXISTS mood CASCADE;
DROP SCHEMA IF EXISTS qg2018 CASCADE;
