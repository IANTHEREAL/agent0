-- PostgreSQL compatible tests from alter_default_privileges_in_schema
-- 47 tests

SET client_min_messages = warning;

-- Setup roles used in the original CockroachDB tests.
DROP ROLE IF EXISTS root;
DROP ROLE IF EXISTS testuser2;
DROP ROLE IF EXISTS testuser;

CREATE ROLE root;
CREATE ROLE testuser LOGIN;
CREATE ROLE testuser2 LOGIN;

-- Ensure testuser can create schemas in this database (matches source "GRANT CREATE ON DATABASE test").
SELECT format('GRANT CREATE ON DATABASE %I TO testuser', current_database())
\gexec

-- Postgres does not grant CREATE on schema public to arbitrary roles by default.
GRANT CREATE ON SCHEMA public TO testuser;

-- Create a placeholder schema referenced by the source test.
CREATE SCHEMA crdb_internal;

-- Test 1: statement (line 3)
-- PG does not allow "IN SCHEMA ..." when changing default privileges ON SCHEMAS.
ALTER DEFAULT PRIVILEGES GRANT USAGE ON SCHEMAS TO root;

-- Test 2: statement (line 6)
ALTER DEFAULT PRIVILEGES IN SCHEMA crdb_internal GRANT SELECT ON TABLES TO root;

-- Run the main portion as testuser (default privileges apply to objects created by this role).
SET ROLE testuser;

-- Test 5: statement (line 18)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser IN SCHEMA public
  GRANT SELECT ON TABLES TO testuser2;

-- Test 6: statement (line 21)
CREATE TABLE t1();

-- Test 7: query (line 24)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 'public' AND table_name = 't1'
ORDER BY grantee, privilege_type;

-- Test 8: statement (line 36)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser
  GRANT INSERT ON TABLES TO testuser2;

-- Test 9: statement (line 39)
CREATE TABLE t2();

-- Test 10: query (line 42)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 'public' AND table_name = 't2'
ORDER BY grantee, privilege_type;

-- Test 11: statement (line 52)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser
  GRANT ALL ON TABLES TO testuser2;

-- Test 12: statement (line 55)
CREATE TABLE t3();

-- Test 13: query (line 58)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 'public' AND table_name = 't3'
ORDER BY grantee, privilege_type;

-- Test 14: statement (line 68)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser
  REVOKE ALL ON TABLES FROM testuser2;

-- Test 15: statement (line 71)
CREATE TABLE t4();

-- Test 16: query (line 74)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 'public' AND table_name = 't4'
ORDER BY grantee, privilege_type;

-- Test 17: statement (line 84)
CREATE SCHEMA s;

-- Test 18: statement (line 87)
GRANT CREATE, USAGE ON SCHEMA s TO testuser;

-- Test 19: statement (line 90)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser IN SCHEMA s, public
  GRANT ALL ON TABLES TO testuser2;

-- Test 20: statement (line 93)
CREATE TABLE public.t5();
CREATE TABLE s.t6();

-- Test 21: query (line 97)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 'public' AND table_name = 't5'
ORDER BY grantee, privilege_type;

-- Test 22: query (line 106)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 's' AND table_name = 't6'
ORDER BY grantee, privilege_type;

-- Test 23: statement (line 118)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser IN SCHEMA s, public
  REVOKE ALL ON TABLES FROM testuser2;

-- PG does not support ALTER DEFAULT PRIVILEGES FOR ALL ROLES; apply to testuser (the creator role).
ALTER DEFAULT PRIVILEGES FOR ROLE testuser IN SCHEMA s, public
  GRANT SELECT ON TABLES TO testuser2;

-- Test 24: statement (line 124)
CREATE TABLE public.t7();
CREATE TABLE s.t8();

-- Test 25: query (line 128)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 'public' AND table_name = 't7'
ORDER BY grantee, privilege_type;

-- Test 26: query (line 137)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 's' AND table_name = 't8'
ORDER BY grantee, privilege_type;

-- Test 27: statement (line 151)
CREATE TABLE public.t9();
CREATE TABLE s.t10();

-- Test 28: query (line 155)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 'public' AND table_name = 't9'
ORDER BY grantee, privilege_type;

-- Test 29: query (line 163)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 's' AND table_name = 't10'
ORDER BY grantee, privilege_type;

-- Test 30: statement (line 173)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser
  REVOKE ALL ON TABLES FROM testuser;

-- Test 31: statement (line 176)
CREATE TABLE t11();

-- Test 32: query (line 181)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 'public' AND table_name = 't11'
ORDER BY grantee, privilege_type;

-- Test 33: statement (line 192)
CREATE SCHEMA s2;

-- Test 34: statement (line 195)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser
  REVOKE ALL ON TABLES FROM testuser;
ALTER DEFAULT PRIVILEGES FOR ROLE testuser
  REVOKE ALL ON TABLES FROM testuser2;

-- Test 35: statement (line 199)
CREATE TABLE s2.t12();

-- Test 36: query (line 204)
SELECT grantee, privilege_type, is_grantable
FROM information_schema.role_table_grants
WHERE table_schema = 's2' AND table_name = 't12'
ORDER BY grantee, privilege_type;

-- Test 37: query (line 212)
-- PostgreSQL equivalent of crdb_internal.default_privileges (schema-specific entries).
SELECT d.defaclrole::regrole AS role_name,
       n.nspname AS schema_name,
       d.defaclobjtype AS object_type,
       d.defaclacl
FROM pg_default_acl AS d
JOIN pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY role_name, schema_name, object_type;

-- Test 38: query (line 219)
SELECT d.defaclrole::regrole AS role_name,
       n.nspname AS schema_name,
       d.defaclobjtype AS object_type,
       d.defaclacl
FROM pg_default_acl AS d
JOIN pg_namespace AS n ON n.oid = d.defaclnamespace
WHERE n.nspname = 'public'
ORDER BY role_name, schema_name, object_type;

-- Test 39: query (line 224)
SELECT d.defaclrole::regrole AS role_name,
       n.nspname AS schema_name,
       d.defaclobjtype AS object_type,
       d.defaclacl
FROM pg_default_acl AS d
JOIN pg_namespace AS n ON n.oid = d.defaclnamespace
WHERE n.nspname = 's'
ORDER BY role_name, schema_name, object_type;

-- Test 40: query (line 229)
SELECT d.defaclrole::regrole AS role_name,
       n.nspname AS schema_name,
       d.defaclobjtype AS object_type,
       d.defaclacl
FROM pg_default_acl AS d
JOIN pg_namespace AS n ON n.oid = d.defaclnamespace
WHERE n.nspname = 's2'
ORDER BY role_name, schema_name, object_type;

-- Test 41: statement (line 234)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser IN SCHEMA public
  GRANT ALL ON TABLES TO testuser;
ALTER DEFAULT PRIVILEGES FOR ROLE testuser IN SCHEMA s
  GRANT USAGE ON TYPES TO testuser2;
-- PG has no DROP privilege for tables.
ALTER DEFAULT PRIVILEGES FOR ROLE testuser IN SCHEMA s2
  GRANT SELECT, UPDATE ON TABLES TO testuser2;

-- Test 42: query (line 239)
SELECT d.defaclrole::regrole AS role_name,
       n.nspname AS schema_name,
       d.defaclobjtype AS object_type,
       d.defaclacl
FROM pg_default_acl AS d
JOIN pg_namespace AS n ON n.oid = d.defaclnamespace
ORDER BY role_name, schema_name, object_type;

-- Test 43: query (line 252)
SELECT d.defaclrole::regrole AS role_name,
       n.nspname AS schema_name,
       d.defaclobjtype AS object_type,
       d.defaclacl
FROM pg_default_acl AS d
JOIN pg_namespace AS n ON n.oid = d.defaclnamespace
WHERE n.nspname = 'public'
ORDER BY role_name, schema_name, object_type;

-- Test 44: query (line 258)
SELECT d.defaclrole::regrole AS role_name,
       n.nspname AS schema_name,
       d.defaclobjtype AS object_type,
       d.defaclacl
FROM pg_default_acl AS d
JOIN pg_namespace AS n ON n.oid = d.defaclnamespace
WHERE n.nspname = 's'
ORDER BY role_name, schema_name, object_type;

-- Test 45: query (line 264)
SELECT d.defaclrole::regrole AS role_name,
       n.nspname AS schema_name,
       d.defaclobjtype AS object_type,
       d.defaclacl
FROM pg_default_acl AS d
JOIN pg_namespace AS n ON n.oid = d.defaclnamespace
WHERE n.nspname = 's2'
ORDER BY role_name, schema_name, object_type;

-- Test 46: query (line 274)
SELECT d.defaclrole::regrole AS role_name,
       n.nspname AS schema_name,
       d.defaclobjtype AS object_type,
       d.defaclacl
FROM pg_default_acl AS d
JOIN pg_namespace AS n ON n.oid = d.defaclnamespace
WHERE n.nspname = 's2' AND d.defaclrole = 'testuser'::regrole
ORDER BY role_name, schema_name, object_type;

-- Test 47: query (line 282)
-- Ensure schema-name input is treated as data (no SQL injection).
SELECT nspname
FROM pg_namespace
WHERE nspname = '''; drop database test; SELECT ''';

RESET ROLE;

-- Cleanup: drop owned objects and default privileges to allow role drops.
ALTER DEFAULT PRIVILEGES REVOKE ALL ON SCHEMAS FROM root;
ALTER DEFAULT PRIVILEGES IN SCHEMA crdb_internal REVOKE ALL ON TABLES FROM root;

DROP OWNED BY root;
DROP OWNED BY testuser;
DROP OWNED BY testuser2;

SELECT format('REVOKE ALL ON DATABASE %I FROM testuser', current_database())
\gexec
SELECT format('REVOKE ALL ON DATABASE %I FROM testuser2', current_database())
\gexec

DROP ROLE testuser2;
DROP ROLE testuser;
DROP ROLE root;

RESET client_min_messages;
