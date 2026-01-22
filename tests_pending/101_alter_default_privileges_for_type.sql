-- PostgreSQL compatible tests from alter_default_privileges_for_type
-- 28 tests

SET client_min_messages = warning;

-- Capture the runner-created database name so we can \connect back after hopping DBs.
SELECT current_database() AS orig_db \gset

DROP DATABASE IF EXISTS d;

DROP ROLE IF EXISTS testuser2;
DROP ROLE IF EXISTS testuser;
CREATE ROLE testuser LOGIN;

-- Test 1: statement (line 1)
CREATE DATABASE d;
GRANT CREATE ON DATABASE d TO testuser;

-- Test 2: statement (line 9)
\connect d
GRANT CREATE ON SCHEMA public TO testuser;

-- Test 3: statement (line 12)
CREATE TYPE testuser_t AS ENUM ('v');

-- Test 4: query (line 15)
SELECT
  CASE WHEN x.grantee = 0 THEN 'public' ELSE x.grantee::regrole::text END AS grantee,
  x.privilege_type,
  x.is_grantable
FROM (
  SELECT (aclexplode(COALESCE(t.typacl, acldefault('T', t.typowner)))).*
  FROM pg_type AS t
  WHERE t.oid = 'public.testuser_t'::regtype
) AS x
ORDER BY 1, 2;

-- Test 5: statement (line 24)
ALTER DEFAULT PRIVILEGES REVOKE ALL ON TYPES FROM testuser;
ALTER DEFAULT PRIVILEGES REVOKE USAGE ON TYPES FROM public;
ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON TYPES FROM testuser;
ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE USAGE ON TYPES FROM public;

-- Test 6: statement (line 30)
CREATE TYPE testuser_t2 AS ENUM ('v');

-- Test 7: query (line 33)
SELECT
  CASE WHEN x.grantee = 0 THEN 'public' ELSE x.grantee::regrole::text END AS grantee,
  x.privilege_type,
  x.is_grantable
FROM (
  SELECT (aclexplode(COALESCE(t.typacl, acldefault('T', t.typowner)))).*
  FROM pg_type AS t
  WHERE t.oid = 'public.testuser_t2'::regtype
) AS x
ORDER BY 1, 2;

-- Test 8: statement (line 43)
\connect :orig_db

-- Test 9: statement (line 46)
CREATE ROLE testuser2 LOGIN;

-- Test 10: statement (line 49)
ALTER DEFAULT PRIVILEGES GRANT ALL ON TYPES TO testuser, testuser2;

-- Test 11: statement (line 52)
CREATE TYPE t AS ENUM ('v');

-- Test 12: query (line 55)
SELECT
  CASE WHEN x.grantee = 0 THEN 'public' ELSE x.grantee::regrole::text END AS grantee,
  x.privilege_type,
  x.is_grantable
FROM (
  SELECT (aclexplode(COALESCE(typacl, acldefault('T', typowner)))).*
  FROM pg_type
  WHERE oid = 'public.t'::regtype
) AS x
ORDER BY 1, 2;

-- Test 13: statement (line 65)
ALTER DEFAULT PRIVILEGES REVOKE USAGE ON TYPES FROM testuser, testuser2;

-- Test 14: statement (line 68)
CREATE TYPE t2 AS ENUM ('v');

-- Test 15: query (line 71)
SELECT
  CASE WHEN x.grantee = 0 THEN 'public' ELSE x.grantee::regrole::text END AS grantee,
  x.privilege_type,
  x.is_grantable
FROM (
  SELECT (aclexplode(COALESCE(typacl, acldefault('T', typowner)))).*
  FROM pg_type
  WHERE oid = 'public.t2'::regtype
) AS x
ORDER BY 1, 2;

-- Test 16: statement (line 79)
ALTER DEFAULT PRIVILEGES REVOKE ALL ON TYPES FROM testuser, testuser2;

-- Test 17: statement (line 82)
CREATE TYPE t3 AS ENUM ('v');

-- Test 18: query (line 85)
SELECT
  CASE WHEN x.grantee = 0 THEN 'public' ELSE x.grantee::regrole::text END AS grantee,
  x.privilege_type,
  x.is_grantable
FROM (
  SELECT (aclexplode(COALESCE(typacl, acldefault('T', typowner)))).*
  FROM pg_type
  WHERE oid = 'public.t3'::regtype
) AS x
ORDER BY 1, 2;

-- Test 19: statement (line 93)
GRANT CREATE ON DATABASE d TO testuser;

-- user testuser

-- Test 20: statement (line 97)
\connect d
SET ROLE testuser;

-- Test 21: statement (line 100)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser REVOKE ALL ON TYPES FROM testuser, testuser2;

-- Test 22: statement (line 103)
CREATE TYPE t4 AS ENUM ('v');

-- Test 23: query (line 106)
SELECT
  CASE WHEN x.grantee = 0 THEN 'public' ELSE x.grantee::regrole::text END AS grantee,
  x.privilege_type,
  x.is_grantable
FROM (
  SELECT (aclexplode(COALESCE(typacl, acldefault('T', typowner)))).*
  FROM pg_type
  WHERE oid = 'public.t4'::regtype
) AS x
ORDER BY 1, 2;

-- Test 24: statement (line 115)
\connect d
RESET ROLE;

-- Test 25: statement (line 118)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser REVOKE ALL ON TYPES FROM testuser, testuser2;

SET ROLE testuser;

-- Test 26: statement (line 122)
\connect d

-- Test 27: statement (line 125)
CREATE TYPE t5 AS ENUM ('v');

-- Test 28: query (line 128)
SELECT
  CASE WHEN x.grantee = 0 THEN 'public' ELSE x.grantee::regrole::text END AS grantee,
  x.privilege_type,
  x.is_grantable
FROM (
  SELECT (aclexplode(COALESCE(typacl, acldefault('T', typowner)))).*
  FROM pg_type
  WHERE oid = 'public.t5'::regtype
) AS x
ORDER BY 1, 2;

RESET ROLE;
\connect postgres
DROP DATABASE d;

RESET client_min_messages;
