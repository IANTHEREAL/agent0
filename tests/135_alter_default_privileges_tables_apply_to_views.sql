-- ported from pg_tests PR#58 compatible/alter_default_privileges_for_table.sql
-- views: TABLES default privileges also apply to views
-- Verify via information_schema.table_privileges

SET client_min_messages = warning;

-- Cleanup from previous runs (cluster-scoped objects)
DROP SCHEMA IF EXISTS adp135_schema CASCADE;
DROP ROLE IF EXISTS adp135_grantee;
DROP ROLE IF EXISTS adp135_owner;

-- Setup roles and schema
CREATE ROLE adp135_owner;
CREATE ROLE adp135_grantee;
CREATE SCHEMA adp135_schema AUTHORIZATION adp135_owner;

-- Default TABLES privileges should apply to both tables and views.
ALTER DEFAULT PRIVILEGES FOR ROLE adp135_owner IN SCHEMA adp135_schema
  GRANT SELECT ON TABLES TO adp135_grantee;

SET ROLE adp135_owner;
CREATE TABLE adp135_schema.t_base (id INT);
CREATE VIEW adp135_schema.v_after AS
  SELECT id FROM adp135_schema.t_base;
RESET ROLE;

SELECT table_schema, table_name, grantee, privilege_type
FROM information_schema.table_privileges
WHERE table_schema = 'adp135_schema'
  AND table_name = 'v_after'
  AND grantee = 'adp135_grantee'
ORDER BY table_schema, table_name, grantee, privilege_type;

-- Cleanup
DROP SCHEMA IF EXISTS adp135_schema CASCADE;
DROP ROLE IF EXISTS adp135_grantee;
DROP ROLE IF EXISTS adp135_owner;

