-- ALTER DEFAULT PRIVILEGES ... GRANT ... ON TABLES (global, no IN SCHEMA)
-- Verify via information_schema.table_privileges

SET client_min_messages = warning;

-- Cleanup from previous runs (cluster-scoped objects)
DROP SCHEMA IF EXISTS adpg_schema1 CASCADE;
DROP SCHEMA IF EXISTS adpg_schema2 CASCADE;
DROP ROLE IF EXISTS adpg_owner;
DROP ROLE IF EXISTS adpg_grantee;

-- Setup roles and schemas
CREATE ROLE adpg_owner;
CREATE ROLE adpg_grantee;
CREATE SCHEMA adpg_schema1 AUTHORIZATION adpg_owner;
CREATE SCHEMA adpg_schema2 AUTHORIZATION adpg_owner;

-- Tables created BEFORE altering default privileges: no grants for adpg_grantee
SET ROLE adpg_owner;
CREATE TABLE adpg_schema1.t_before (id INT);
CREATE TABLE adpg_schema2.t_before (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE grantee = 'adpg_grantee'
  AND table_name = 't_before'
  AND table_schema IN ('adpg_schema1', 'adpg_schema2')
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Apply default privileges WITHOUT IN SCHEMA: should affect future tables in multiple schemas
ALTER DEFAULT PRIVILEGES FOR ROLE adpg_owner
  GRANT SELECT ON TABLES TO adpg_grantee;

-- Tables created AFTER altering default privileges: should include SELECT for adpg_grantee in both schemas
SET ROLE adpg_owner;
CREATE TABLE adpg_schema1.t_after (id INT);
CREATE TABLE adpg_schema2.t_after (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE grantee = 'adpg_grantee'
  AND table_name = 't_after'
  AND table_schema IN ('adpg_schema1', 'adpg_schema2')
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Cleanup
ALTER DEFAULT PRIVILEGES FOR ROLE adpg_owner
  REVOKE SELECT ON TABLES FROM adpg_grantee;

DROP SCHEMA IF EXISTS adpg_schema1 CASCADE;
DROP SCHEMA IF EXISTS adpg_schema2 CASCADE;
DROP ROLE IF EXISTS adpg_owner;
DROP ROLE IF EXISTS adpg_grantee;
