-- Regression test for issue #1396: schema-qualified wildcard must validate
-- the schema prefix. Previously, `wrong_schema.table.*` silently dropped
-- the schema part and succeeded.
--
-- db9-specific: For the four "does not exist" cases below, PG 17.7 returns
-- 'invalid reference to FROM-clause entry for table "..."'. db9 returns
-- 'column "..." does not exist'. The semantic is identical (reject the
-- invalid qualified wildcard) but the diagnostic wording differs.

DROP TABLE IF EXISTS qwsv_t;
CREATE TABLE qwsv_t(id INT PRIMARY KEY, val TEXT);
INSERT INTO qwsv_t VALUES (1, 'a');

-- Wrong schema prefix must error (PG: "invalid reference to FROM-clause entry").
-- db9-specific: returns "does not exist" instead.
SELECT bogus_schema.qwsv_t.* FROM qwsv_t;

-- Schema-qualified wildcard with alias must use the alias, not table name.
-- db9-specific: returns "does not exist" instead of PG's "invalid reference".
SELECT public.qwsv_t.* FROM qwsv_t AS x;

-- Alias cannot be schema-qualified (PG rejects schema.alias.*).
-- db9-specific: returns "does not exist" instead of PG's "invalid reference".
SELECT public.x.* FROM qwsv_t AS x;

-- CTE cannot be schema-qualified in wildcard.
-- db9-specific: returns "does not exist" instead of PG's "invalid reference".
WITH c AS (SELECT 1 AS x) SELECT public.c.* FROM c;

-- 4-part name: cross-database references are not implemented.
SELECT foo.public.qwsv_t.* FROM qwsv_t;

DROP TABLE qwsv_t;
