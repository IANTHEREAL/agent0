-- Test: schema-qualified COLLATE syntax (psql \d compatibility)
-- psql uses COLLATE pg_catalog."default" in its \d queries.

-- Positive: pg_catalog-qualified and unqualified built-in collations
SELECT 'hello' COLLATE pg_catalog."default" AS result;
SELECT 'hello' COLLATE "default" AS result;
SELECT 'hello' COLLATE pg_catalog."C" AS result;
SELECT 'hello' COLLATE pg_catalog."POSIX" AS result;

-- Negative: non-pg_catalog schema must be rejected (PG parity)
SELECT 'hello' COLLATE no_such_schema."default" AS result;
SELECT 'hello' COLLATE public."default" AS result;

-- Negative: 3-part names are cross-database references (PG parity)
SELECT 'hello' COLLATE pg_catalog.foo."default" AS result;
