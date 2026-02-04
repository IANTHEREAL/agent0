-- ported from pg_tests PR#58 compatible/alter_default_privileges_for_table.sql
-- multi-owner: default privileges are per object owner
-- Verify via information_schema.table_privileges

SET client_min_messages = warning;

-- Cleanup from previous runs (cluster-scoped objects)
DROP SCHEMA IF EXISTS adp133_schema CASCADE;
DROP ROLE IF EXISTS adp133_grantee;
DROP ROLE IF EXISTS adp133_owner2;
DROP ROLE IF EXISTS adp133_owner1;

-- Setup roles and schema
CREATE ROLE adp133_owner1;
CREATE ROLE adp133_owner2;
CREATE ROLE adp133_grantee;

CREATE SCHEMA adp133_schema AUTHORIZATION adp133_owner1;
GRANT USAGE, CREATE ON SCHEMA adp133_schema TO adp133_owner2;

-- Default privileges only for objects created by adp133_owner1
ALTER DEFAULT PRIVILEGES FOR ROLE adp133_owner1 IN SCHEMA adp133_schema
  GRANT SELECT ON TABLES TO adp133_grantee;

-- Table created by owner1: should include SELECT for adp133_grantee
SET ROLE adp133_owner1;
CREATE TABLE adp133_schema.t_by_owner1 (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE table_schema = 'adp133_schema'
  AND table_name = 't_by_owner1'
  AND grantee = 'adp133_grantee'
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Table created by owner2: should NOT include owner1's default privileges
SET ROLE adp133_owner2;
CREATE TABLE adp133_schema.t_by_owner2 (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE table_schema = 'adp133_schema'
  AND table_name = 't_by_owner2'
  AND grantee = 'adp133_grantee'
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Cleanup
DROP SCHEMA IF EXISTS adp133_schema CASCADE;
DROP ROLE IF EXISTS adp133_grantee;
DROP ROLE IF EXISTS adp133_owner2;
DROP ROLE IF EXISTS adp133_owner1;

