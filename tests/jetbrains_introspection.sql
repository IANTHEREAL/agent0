-- JetBrains IDE (DataGrip / IntelliJ) introspection compatibility (#1373).
-- Verifies that the key catalog queries sent by the PostgreSQL JDBC driver
-- and DataGrip's own introspection engine execute without errors.

-- Setup: a user table to be discovered.
DROP TABLE IF EXISTS jb_test;
CREATE TABLE jb_test (id INT PRIMARY KEY, name TEXT);

-- 1. JDBC startup query: current_database() + current_schemas(false)
SELECT 'jdbc_startup='
       || current_database()
       || ','
       || array_length(current_schemas(false), 1)::text;

-- 2. current_schemas(true) should include pg_catalog
SELECT 'schemas_true_has_pg_catalog='
       || ('pg_catalog' = ANY(current_schemas(true)))::text;

-- 3. current_schemas(false) should not include pg_catalog
SELECT 'schemas_false_no_pg_catalog='
       || (NOT ('pg_catalog' = ANY(current_schemas(false))))::text;

-- 4. DataGrip schema listing query (simplified — uses pg_get_userbyid)
-- Uses N.xmin (PG system column) for incremental change detection.
SELECT 'schema_listing='
       || count(*)::text
FROM (
    SELECT N.oid::bigint AS id,
           N.xmin AS state_number,
           nspname AS name,
           D.description,
           pg_catalog.pg_get_userbyid(N.nspowner) AS "owner"
    FROM pg_catalog.pg_namespace N
    LEFT JOIN pg_catalog.pg_description D ON N.oid = D.objoid
    WHERE N.nspname NOT LIKE 'pg_toast%'
      AND N.nspname NOT LIKE 'pg_temp%'
    ORDER BY CASE WHEN nspname = current_schema() THEN -1::bigint
                  ELSE N.oid::bigint END
) sub;

-- 5. DataGrip database listing query (uses pg_shdescription)
SELECT 'db_listing='
       || count(*)::text
FROM (
    SELECT N.oid::bigint AS id,
           datname AS name,
           D.description,
           datistemplate AS is_template,
           datallowconn AS allow_connections,
           pg_catalog.pg_get_userbyid(N.datdba) AS "owner"
    FROM pg_catalog.pg_database N
    LEFT JOIN pg_catalog.pg_shdescription D ON N.oid = D.objoid
    ORDER BY CASE WHEN datname = pg_catalog.current_database()
                  THEN -1::bigint ELSE N.oid::bigint END
) sub;

-- 6. JDBC getTables() core join (uses 'pg_class'::regclass cast)
SELECT 'jdbc_get_tables='
       || count(*)::text
FROM pg_catalog.pg_namespace n, pg_catalog.pg_class c
LEFT JOIN pg_catalog.pg_description d
    ON (c.oid = d.objoid AND d.objsubid = 0
        AND d.classoid = 'pg_class'::regclass)
WHERE c.relnamespace = n.oid
  AND c.relkind = 'r'
  AND n.nspname = 'public';

-- 7. JDBC getSchemas() (uses current_schemas(true) with array subscript)
SELECT 'jdbc_get_schemas='
       || count(*)::text
FROM pg_catalog.pg_namespace
WHERE nspname <> 'pg_toast'
  AND nspname NOT LIKE 'pg_temp_%'
  AND nspname NOT LIKE 'pg_toast_temp_%';

-- 8. pg_namespace has nspacl column (used by some tools)
SELECT 'nspacl_exists='
       || (count(*) >= 0)::text
FROM pg_catalog.pg_namespace
WHERE nspacl IS NULL;

-- 9. SELECT * on pg_namespace should still succeed with hidden system columns
SELECT 'pg_namespace_star='
       || count(*)::text
FROM (
    SELECT * FROM pg_catalog.pg_namespace LIMIT 1
) s;

-- 10. pg_namespace.xmin remains queryable as a qualified system column
SELECT 'pg_namespace_xmin_qualified='
       || (count(*) > 0)::text
FROM pg_catalog.pg_namespace n
WHERE n.xmin IS NOT NULL;

-- Cleanup
DROP TABLE jb_test;
