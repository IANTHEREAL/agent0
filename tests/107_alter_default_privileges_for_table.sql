-- ALTER DEFAULT PRIVILEGES ... GRANT ... ON TABLES
-- Verify via information_schema.table_privileges

SET client_min_messages = warning;

-- Cleanup from previous runs (cluster-scoped objects)
DROP SCHEMA IF EXISTS adp_schema CASCADE;
DROP ROLE IF EXISTS adp_grantee;
DROP ROLE IF EXISTS adp_owner;

-- Setup roles and schema
CREATE ROLE adp_owner;
CREATE ROLE adp_grantee;
CREATE SCHEMA adp_schema AUTHORIZATION adp_owner;

-- Table created BEFORE altering default privileges: no grants for adp_grantee
SET ROLE adp_owner;
CREATE TABLE adp_schema.t_before (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE table_schema = 'adp_schema'
  AND table_name = 't_before'
  AND grantee = 'adp_grantee'
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Apply default privileges for future tables created by adp_owner in adp_schema
ALTER DEFAULT PRIVILEGES FOR ROLE adp_owner IN SCHEMA adp_schema
  GRANT SELECT ON TABLES TO adp_grantee;

-- Table created AFTER altering default privileges: should include SELECT for adp_grantee
SET ROLE adp_owner;
CREATE TABLE adp_schema.t_after (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE table_schema = 'adp_schema'
  AND table_name = 't_after'
  AND grantee = 'adp_grantee'
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Cleanup
DROP SCHEMA IF EXISTS adp_schema CASCADE;
DROP ROLE IF EXISTS adp_grantee;
DROP ROLE IF EXISTS adp_owner;
