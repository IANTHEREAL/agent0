-- ALTER DEFAULT PRIVILEGES ... GRANT ... WITH GRANT OPTION
-- Verify is_grantable=YES via information_schema.table_privileges

SET client_min_messages = warning;

-- Cleanup from previous runs (cluster-scoped objects)
DROP SCHEMA IF EXISTS adpgo_schema CASCADE;
DROP ROLE IF EXISTS adpgo_grantee;
DROP ROLE IF EXISTS adpgo_owner;

-- Setup roles and schema
CREATE ROLE adpgo_owner;
CREATE ROLE adpgo_grantee;
CREATE SCHEMA adpgo_schema AUTHORIZATION adpgo_owner;

-- Table created BEFORE altering default privileges: no grants for adpgo_grantee
SET ROLE adpgo_owner;
CREATE TABLE adpgo_schema.t_before (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type, is_grantable
FROM information_schema.table_privileges
WHERE table_schema = 'adpgo_schema'
  AND table_name = 't_before'
  AND grantee = 'adpgo_grantee'
ORDER BY table_schema, table_name, grantee, privilege_type, is_grantable;

-- Apply default privileges WITH GRANT OPTION for future tables created by adpgo_owner in adpgo_schema
ALTER DEFAULT PRIVILEGES FOR ROLE adpgo_owner IN SCHEMA adpgo_schema
  GRANT SELECT ON TABLES TO adpgo_grantee WITH GRANT OPTION;

-- Table created AFTER altering default privileges: should include SELECT with is_grantable=YES
SET ROLE adpgo_owner;
CREATE TABLE adpgo_schema.t_after (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type, is_grantable
FROM information_schema.table_privileges
WHERE table_schema = 'adpgo_schema'
  AND table_name = 't_after'
  AND grantee = 'adpgo_grantee'
ORDER BY table_schema, table_name, grantee, privilege_type, is_grantable;

-- Cleanup
DROP SCHEMA IF EXISTS adpgo_schema CASCADE;
DROP ROLE IF EXISTS adpgo_grantee;
DROP ROLE IF EXISTS adpgo_owner;
