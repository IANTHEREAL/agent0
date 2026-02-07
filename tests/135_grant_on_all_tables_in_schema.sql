-- GRANT ... ON ALL TABLES IN SCHEMA
-- Verify via information_schema.table_privileges

SET client_min_messages = warning;

-- Cleanup from previous runs (cluster-scoped objects)
DROP SCHEMA IF EXISTS gaats_schema1 CASCADE;
DROP SCHEMA IF EXISTS gaats_schema2 CASCADE;
DROP ROLE IF EXISTS gaats_grantee;
DROP ROLE IF EXISTS gaats_owner;

-- Setup roles and schemas
CREATE ROLE gaats_owner;
CREATE ROLE gaats_grantee;
CREATE SCHEMA gaats_schema1 AUTHORIZATION gaats_owner;
CREATE SCHEMA gaats_schema2 AUTHORIZATION gaats_owner;

-- Create existing tables in two schemas
SET ROLE gaats_owner;
CREATE TABLE gaats_schema1.t1 (id INT);
CREATE TABLE gaats_schema1.t2 (id INT);
CREATE TABLE gaats_schema2.t1 (id INT);
RESET ROLE;

-- Grant affects existing tables in the specified schema only
SET ROLE gaats_owner;
GRANT SELECT ON ALL TABLES IN SCHEMA gaats_schema1 TO gaats_grantee;
RESET ROLE;

-- Table created AFTER the GRANT should not be affected
SET ROLE gaats_owner;
CREATE TABLE gaats_schema1.t_after (id INT);
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE grantee = 'gaats_grantee'
  AND table_schema = 'gaats_schema1'
  AND table_name IN ('t1', 't2', 't_after')
ORDER BY table_schema, table_name, grantee, privilege_type;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE grantee = 'gaats_grantee'
  AND table_schema = 'gaats_schema2'
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Revoke should remove the per-table grants created above
SET ROLE gaats_owner;
REVOKE SELECT ON ALL TABLES IN SCHEMA gaats_schema1 FROM gaats_grantee;
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE grantee = 'gaats_grantee'
  AND table_schema = 'gaats_schema1'
  AND table_name IN ('t1', 't2', 't_after')
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Cleanup
DROP SCHEMA IF EXISTS gaats_schema1 CASCADE;
DROP SCHEMA IF EXISTS gaats_schema2 CASCADE;
DROP ROLE IF EXISTS gaats_grantee;
DROP ROLE IF EXISTS gaats_owner;
