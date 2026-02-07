-- Regression: DROP SCHEMA ... CASCADE must remove stored types too (not just tables/views/sequences).

DROP SCHEMA IF EXISTS dsct_schema CASCADE;

CREATE SCHEMA dsct_schema;
CREATE TYPE dsct_schema.mood AS ENUM ('happy', 'sad');

DROP SCHEMA dsct_schema CASCADE;

SELECT 'SCHEMA_LEFT=' || count(*)
FROM information_schema.schemata
WHERE schema_name = 'dsct_schema';

SELECT 'TYPE_LEFT=' || count(*)
FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
WHERE n.nspname = 'dsct_schema' AND t.typname = 'mood';

