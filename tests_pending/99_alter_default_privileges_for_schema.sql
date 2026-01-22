-- PostgreSQL compatible tests from alter_default_privileges_for_schema
-- 32 tests

SET client_min_messages = warning;
\set orig_db :DBNAME

DROP ROLE IF EXISTS testuser2;
DROP ROLE IF EXISTS testuser;
CREATE ROLE testuser;

DROP DATABASE IF EXISTS d;

-- Test 1: statement (line 1)
CREATE DATABASE d;
GRANT CREATE ON DATABASE d TO testuser;

-- Test 2: statement (line 8)
\set QUIET 1
\connect d
\unset QUIET
SET client_min_messages = warning;

-- Test 3: query (line 14)
SELECT
  CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE r.rolname END AS grantee,
  a.privilege_type
FROM pg_namespace n
CROSS JOIN LATERAL aclexplode(n.nspacl) AS a
LEFT JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'public'
  AND (a.grantee = 0 OR r.rolname IN ('testuser', 'testuser2'))
ORDER BY 1, 2;

-- Test 4: statement (line 23)
CREATE SCHEMA testuser_s;

-- Test 5: query (line 26)
SELECT
  CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE r.rolname END AS grantee,
  a.privilege_type
FROM pg_namespace n
CROSS JOIN LATERAL aclexplode(n.nspacl) AS a
LEFT JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'testuser_s'
  AND (a.grantee = 0 OR r.rolname IN ('testuser', 'testuser2'))
ORDER BY 1, 2;

-- Test 6: statement (line 34)
ALTER DEFAULT PRIVILEGES REVOKE ALL ON SCHEMAS FROM testuser;

-- Test 7: statement (line 37)
CREATE SCHEMA testuser_s2;

-- Test 8: query (line 43)
SELECT
  CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE r.rolname END AS grantee,
  a.privilege_type
FROM pg_namespace n
CROSS JOIN LATERAL aclexplode(n.nspacl) AS a
LEFT JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'testuser_s2'
  AND (a.grantee = 0 OR r.rolname IN ('testuser', 'testuser2'))
ORDER BY 1, 2;

-- Test 9: statement (line 53)
\set QUIET 1
\connect :orig_db
\unset QUIET
SET client_min_messages = warning;

-- Test 10: statement (line 56)
CREATE USER testuser2;

-- Test 11: statement (line 59)
ALTER DEFAULT PRIVILEGES GRANT ALL ON SCHEMAS TO testuser, testuser2;

-- Test 12: statement (line 62)
CREATE SCHEMA s;

-- Test 13: query (line 65)
SELECT
  CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE r.rolname END AS grantee,
  a.privilege_type
FROM pg_namespace n
CROSS JOIN LATERAL aclexplode(n.nspacl) AS a
LEFT JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 's'
  AND (a.grantee = 0 OR r.rolname IN ('testuser', 'testuser2'))
ORDER BY 1, 2;

-- Test 14: statement (line 74)
ALTER DEFAULT PRIVILEGES REVOKE USAGE ON SCHEMAS FROM testuser, testuser2;

-- Test 15: statement (line 77)
CREATE SCHEMA s2;

-- Test 16: query (line 80)
SELECT
  CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE r.rolname END AS grantee,
  a.privilege_type
FROM pg_namespace n
CROSS JOIN LATERAL aclexplode(n.nspacl) AS a
LEFT JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 's2'
  AND (a.grantee = 0 OR r.rolname IN ('testuser', 'testuser2'))
ORDER BY 1, 2;

-- Test 17: statement (line 91)
ALTER DEFAULT PRIVILEGES REVOKE ALL ON SCHEMAS FROM testuser, testuser2;

-- Test 18: statement (line 94)
CREATE SCHEMA s3;

-- Test 19: query (line 97)
SELECT
  CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE r.rolname END AS grantee,
  a.privilege_type
FROM pg_namespace n
CROSS JOIN LATERAL aclexplode(n.nspacl) AS a
LEFT JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 's3'
  AND (a.grantee = 0 OR r.rolname IN ('testuser', 'testuser2'))
ORDER BY 1, 2;

-- Test 20: statement (line 104)
GRANT CREATE ON DATABASE d TO testuser;

-- user testuser (cockroach logic test directive)

-- Test 21: statement (line 108)
\set QUIET 1
\connect d
\unset QUIET
SET client_min_messages = warning;
SET ROLE testuser;

-- Test 22: statement (line 111)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser REVOKE ALL ON SCHEMAS FROM testuser, testuser2;

-- Test 23: statement (line 114)
CREATE SCHEMA s4;

-- Test 24: query (line 118)
SELECT
  CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE r.rolname END AS grantee,
  a.privilege_type
FROM pg_namespace n
CROSS JOIN LATERAL aclexplode(n.nspacl) AS a
LEFT JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 's4'
  AND (a.grantee = 0 OR r.rolname IN ('testuser', 'testuser2'))
ORDER BY 1, 2;

-- Test 25: statement (line 127)
-- USE d (already connected)

-- Test 26: statement (line 130)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser REVOKE ALL ON SCHEMAS FROM testuser, testuser2;

-- user testuser (already SET ROLE testuser)

-- Test 27: statement (line 134)
-- USE d (already connected)

-- Test 28: statement (line 137)
CREATE SCHEMA s5;

-- Test 29: query (line 141)
SELECT
  CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE r.rolname END AS grantee,
  a.privilege_type
FROM pg_namespace n
CROSS JOIN LATERAL aclexplode(n.nspacl) AS a
LEFT JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 's5'
  AND (a.grantee = 0 OR r.rolname IN ('testuser', 'testuser2'))
ORDER BY 1, 2;

-- Test 30: statement (line 149)
RESET ROLE;
ALTER DEFAULT PRIVILEGES GRANT ALL ON SCHEMAS TO testuser, testuser2;

-- user root (cockroach logic test directive)

-- Test 31: statement (line 154)
CREATE SCHEMA s_72322;

-- Test 32: query (line 158)
SELECT
  CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE r.rolname END AS grantee,
  a.privilege_type
FROM pg_namespace n
CROSS JOIN LATERAL aclexplode(n.nspacl) AS a
LEFT JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 's_72322'
  AND (a.grantee = 0 OR r.rolname IN ('testuser', 'testuser2'))
ORDER BY 1, 2;

\set QUIET 1
\connect :orig_db
\unset QUIET
DROP DATABASE d;
RESET client_min_messages;
