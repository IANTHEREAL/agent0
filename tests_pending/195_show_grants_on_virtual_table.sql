-- PostgreSQL compatible tests from show_grants_on_virtual_table
-- 7 tests

-- CockroachDB's `crdb_internal.tables` virtual table doesn't exist in PostgreSQL.
-- Create a lightweight view to exercise GRANT/privilege visibility semantics.
CREATE SCHEMA IF NOT EXISTS crdb_internal;
CREATE OR REPLACE VIEW crdb_internal.tables AS SELECT 1 AS dummy;
GRANT SELECT ON crdb_internal.tables TO PUBLIC;

-- Test 1: query (line 1)
WITH grants AS (
  SELECT
    current_database() AS database_name,
    table_schema AS schema_name,
    table_name,
    lower(grantee) AS grantee,
    CASE
      WHEN COUNT(DISTINCT privilege_type) > 1 THEN 'ALL'
      ELSE MIN(privilege_type)
    END AS privilege_type,
    CASE
      WHEN bool_or(is_grantable = 'YES') THEN 'true'
      ELSE 'false'
    END AS is_grantable
  FROM information_schema.role_table_grants
  WHERE table_schema = 'crdb_internal'
    AND table_name = 'tables'
    AND lower(grantee) IN ('public', 'testuser', 'testuser2')
  GROUP BY current_database(), table_schema, table_name, lower(grantee)
)
SELECT database_name, schema_name, table_name, grantee, privilege_type, is_grantable
FROM grants
ORDER BY grantee, privilege_type;

-- Test 2: statement (line 7)
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'testuser') THEN
    CREATE ROLE testuser LOGIN;
  END IF;
END
$$;

GRANT SELECT ON crdb_internal.tables TO testuser;

-- Test 3: query (line 10)
WITH grants AS (
  SELECT
    current_database() AS database_name,
    table_schema AS schema_name,
    table_name,
    lower(grantee) AS grantee,
    CASE
      WHEN COUNT(DISTINCT privilege_type) > 1 THEN 'ALL'
      ELSE MIN(privilege_type)
    END AS privilege_type,
    CASE
      WHEN bool_or(is_grantable = 'YES') THEN 'true'
      ELSE 'false'
    END AS is_grantable
  FROM information_schema.role_table_grants
  WHERE table_schema = 'crdb_internal'
    AND table_name = 'tables'
    AND lower(grantee) IN ('public', 'testuser', 'testuser2')
  GROUP BY current_database(), table_schema, table_name, lower(grantee)
)
SELECT database_name, schema_name, table_name, grantee, privilege_type, is_grantable
FROM grants
ORDER BY grantee, privilege_type;

-- Test 4: statement (line 17)
GRANT ALL ON crdb_internal.tables TO testuser;

-- Test 5: statement (line 20)
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'testuser2') THEN
    CREATE ROLE testuser2 LOGIN;
  END IF;
END
$$;

-- Test 6: statement (line 23)
GRANT SELECT ON crdb_internal.tables TO testuser2 WITH GRANT OPTION;

-- Test 7: query (line 26)
WITH grants AS (
  SELECT
    current_database() AS database_name,
    table_schema AS schema_name,
    table_name,
    lower(grantee) AS grantee,
    CASE
      WHEN COUNT(DISTINCT privilege_type) > 1 THEN 'ALL'
      ELSE MIN(privilege_type)
    END AS privilege_type,
    CASE
      WHEN bool_or(is_grantable = 'YES') THEN 'true'
      ELSE 'false'
    END AS is_grantable
  FROM information_schema.role_table_grants
  WHERE table_schema = 'crdb_internal'
    AND table_name = 'tables'
    AND lower(grantee) IN ('public', 'testuser', 'testuser2')
  GROUP BY current_database(), table_schema, table_name, lower(grantee)
)
SELECT database_name, schema_name, table_name, grantee, privilege_type, is_grantable
FROM grants
ORDER BY grantee, privilege_type;
