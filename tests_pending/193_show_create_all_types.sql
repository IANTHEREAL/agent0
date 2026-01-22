-- PostgreSQL compatible tests from show_create_all_types
-- 20 tests

SET client_min_messages = warning;

-- Emulate CockroachDB's `SHOW CREATE ALL TYPES` using PostgreSQL catalogs.
-- This is scoped to the current `search_path` to approximate Cockroach's
-- "current database" behavior in a single Postgres database.
CREATE OR REPLACE FUNCTION public.pg_show_create_all_types()
RETURNS TABLE (statement TEXT)
LANGUAGE sql
AS $$
WITH user_schemas AS (
  SELECT unnest(current_schemas(false)) AS nspname
),
user_types AS (
  SELECT
    t.oid,
    n.nspname,
    t.typname,
    t.typtype,
    t.typrelid,
    obj_description(t.oid, 'pg_type') AS comment
  FROM pg_type AS t
  JOIN pg_namespace AS n ON n.oid = t.typnamespace
  JOIN user_schemas AS us ON us.nspname = n.nspname
  WHERE t.typtype IN ('e', 'c')
    AND n.nspname NOT IN ('pg_catalog', 'information_schema')
),
enum_types AS (
  SELECT
    nspname,
    typname,
    1 AS ord,
    format(
      'CREATE TYPE %I.%I AS ENUM (%s);',
      nspname,
      typname,
      (
        SELECT string_agg(quote_literal(e.enumlabel), ', ' ORDER BY e.enumsortorder)
        FROM pg_enum AS e
        WHERE e.enumtypid = user_types.oid
      )
    ) AS stmt,
    comment
  FROM user_types
  WHERE typtype = 'e'
),
composite_types AS (
  SELECT
    ut.nspname,
    ut.typname,
    1 AS ord,
    format(
      'CREATE TYPE %I.%I AS (%s);',
      ut.nspname,
      ut.typname,
      (
        SELECT string_agg(
                 format('%I %s', a.attname, format_type(a.atttypid, a.atttypmod)),
                 ', '
                 ORDER BY a.attnum
               )
        FROM pg_attribute AS a
        WHERE a.attrelid = ut.typrelid
          AND a.attnum > 0
          AND NOT a.attisdropped
      )
    ) AS stmt,
    ut.comment
  FROM user_types AS ut
  JOIN pg_class AS c ON c.oid = ut.typrelid
  WHERE ut.typtype = 'c'
    AND c.relkind = 'c'  -- exclude table row-types
),
create_stmts AS (
  SELECT nspname, typname, ord, stmt, comment FROM enum_types
  UNION ALL
  SELECT nspname, typname, ord, stmt, comment FROM composite_types
),
comment_stmts AS (
  SELECT
    nspname,
    typname,
    2 AS ord,
    format('COMMENT ON TYPE %I.%I IS %s;', nspname, typname, quote_literal(comment)) AS stmt
  FROM create_stmts
  WHERE comment IS NOT NULL
),
all_stmts AS (
  SELECT nspname, typname, ord, stmt FROM create_stmts
  UNION ALL
  SELECT nspname, typname, ord, stmt FROM comment_stmts
)
SELECT stmt AS statement
FROM all_stmts
ORDER BY nspname, typname, ord;
$$;

-- Test 1: statement (line 3)
DROP SCHEMA IF EXISTS d CASCADE;
CREATE SCHEMA d;

-- Test 2: statement (line 6)
-- `USE d` (CockroachDB) -> set a dedicated schema as our namespace.
SET search_path TO d;

-- Test 3: query (line 9)
SELECT * FROM public.pg_show_create_all_types();

-- Test 4: statement (line 14)
CREATE TYPE status AS ENUM ('open', 'closed', 'inactive');

-- Test 5: query (line 17)
SELECT * FROM public.pg_show_create_all_types();

-- Test 6: statement (line 23)
CREATE TYPE tableObj AS ENUM('row', 'col');

-- Test 7: query (line 26)
SELECT * FROM public.pg_show_create_all_types();

-- Test 8: statement (line 33)
DROP TYPE status;

-- Test 9: query (line 36)
SELECT * FROM public.pg_show_create_all_types();

-- Test 10: statement (line 43)
CREATE SCHEMA s;
-- Keep "database d" visible by including both schemas on the search_path.
SET search_path TO d, s;

-- Test 11: statement (line 46)
CREATE TYPE s.status AS ENUM ('a', 'b', 'c');

-- Test 12: query (line 49)
SELECT * FROM public.pg_show_create_all_types();

-- Test 13: statement (line 57)
DROP SCHEMA IF EXISTS "d-d" CASCADE;
CREATE SCHEMA "d-d";
SET search_path TO "d-d";
SELECT * FROM public.pg_show_create_all_types();

-- Test 14: statement (line 63)
DROP SCHEMA IF EXISTS "a""bc" CASCADE;
CREATE SCHEMA "a""bc";
SET search_path TO "a""bc";
SELECT * FROM public.pg_show_create_all_types();

-- Back to our "database d" namespace.
SET search_path TO d, s;

-- Test 15: statement (line 80)
DROP TYPE IF EXISTS address;
CREATE TYPE address AS (street TEXT, city TEXT);
COMMENT ON TYPE address IS 'comment for composite type address';

-- skipif config local-legacy-schema-changer

-- Test 16: query (line 84)
SELECT * FROM public.pg_show_create_all_types();

-- Test 17: statement (line 92)
DROP TYPE address;

-- skipif config local-legacy-schema-changer

-- Test 18: statement (line 96)
CREATE TYPE roaches AS ENUM('papa_roach','mama_roach','baby_roach');

-- skipif config local-legacy-schema-changer

-- Test 19: statement (line 100)
COMMENT ON TYPE roaches IS 'comment for enum type roaches';

-- skipif config local-legacy-schema-changer

-- Test 20: query (line 104)
SELECT * FROM public.pg_show_create_all_types();

RESET client_min_messages;
