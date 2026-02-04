-- ported from pg_tests PR#58 compatible/alter_default_privileges_for_table.sql
-- revoke: default privileges changes affect future objects only
-- Verify via information_schema.table_privileges

SET client_min_messages = warning;

-- Cleanup from previous runs (cluster-scoped objects)
DROP SCHEMA IF EXISTS adp134_schema CASCADE;
DROP ROLE IF EXISTS adp134_grantee;
DROP ROLE IF EXISTS adp134_owner;

-- Setup roles and schema
CREATE ROLE adp134_owner;
CREATE ROLE adp134_grantee;
CREATE SCHEMA adp134_schema AUTHORIZATION adp134_owner;

-- Grant default SELECT privilege for future tables created by adp134_owner.
ALTER DEFAULT PRIVILEGES FOR ROLE adp134_owner IN SCHEMA adp134_schema
  GRANT SELECT ON TABLES TO adp134_grantee;

-- Table created after GRANT: should include SELECT for adp134_grantee.
SET ROLE adp134_owner;
CREATE TABLE adp134_schema.t_before_revoke (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE table_schema = 'adp134_schema'
  AND table_name = 't_before_revoke'
  AND grantee = 'adp134_grantee'
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Revoke default privileges; existing objects should keep their explicit GRANTs.
ALTER DEFAULT PRIVILEGES FOR ROLE adp134_owner IN SCHEMA adp134_schema
  REVOKE SELECT ON TABLES FROM adp134_grantee;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE table_schema = 'adp134_schema'
  AND table_name = 't_before_revoke'
  AND grantee = 'adp134_grantee'
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Table created after REVOKE: should NOT include SELECT for adp134_grantee.
SET ROLE adp134_owner;
CREATE TABLE adp134_schema.t_after_revoke (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE table_schema = 'adp134_schema'
  AND table_name = 't_after_revoke'
  AND grantee = 'adp134_grantee'
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Cleanup
DROP SCHEMA IF EXISTS adp134_schema CASCADE;
DROP ROLE IF EXISTS adp134_grantee;
DROP ROLE IF EXISTS adp134_owner;

