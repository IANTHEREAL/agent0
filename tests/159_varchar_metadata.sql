-- Regression: TEXT vs VARCHAR(n) must be distinguishable in catalog surfaces.

DROP TABLE IF EXISTS public.varchar_meta;

CREATE TABLE public.varchar_meta (
    t TEXT,
    v VARCHAR(3)
);

-- information_schema.columns.data_type + character_maximum_length must match PostgreSQL:
-- - TEXT → data_type = text, character_maximum_length = NULL, udt_name = text
-- - VARCHAR(n) → data_type = character varying, character_maximum_length = n, udt_name = varchar
SELECT column_name, data_type, character_maximum_length, udt_name
FROM information_schema.columns
WHERE table_schema = 'public' AND table_name = 'varchar_meta'
ORDER BY ordinal_position;

-- pg_attribute.atttypid must not collapse VARCHAR to TEXT (OID=25).
SELECT a.attname,
       a.atttypid,
       format_type(a.atttypid, a.atttypmod) AS formatted
  FROM pg_attribute a
  JOIN pg_class c ON a.attrelid = c.oid
  JOIN pg_namespace n ON c.relnamespace = n.oid
 WHERE n.nspname = 'public'
   AND c.relname = 'varchar_meta'
 ORDER BY a.attnum;

DROP TABLE public.varchar_meta;

