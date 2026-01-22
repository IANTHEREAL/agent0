-- PostgreSQL compatible tests from alter_default_privileges_for_all_roles
-- 30 tests

SET client_min_messages = warning;

DROP ROLE IF EXISTS testuser2;
DROP ROLE IF EXISTS testuser;
CREATE ROLE testuser;

-- Test 1: statement (line 1)
-- PostgreSQL does not support `FOR ALL ROLES`; scope to the current role.
ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO testuser;

-- Test 2: statement (line 4)
CREATE TABLE t();

-- Test 3: query (line 7)
SELECT r.rolname AS grantee, a.privilege_type
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL aclexplode(c.relacl) AS a
JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'public'
  AND c.relname = 't'
  AND r.rolname IN ('testuser', 'testuser2')
ORDER BY 1, 2;

-- Test 4: statement (line 15)
-- PostgreSQL doesn't have a `CREATE` table privilege; use a valid table privilege.
ALTER DEFAULT PRIVILEGES GRANT INSERT ON TABLES TO testuser;

-- Test 5: statement (line 18)
CREATE TABLE t2();

-- Test 6: query (line 23)
SELECT r.rolname AS grantee, a.privilege_type
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL aclexplode(c.relacl) AS a
JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'public'
  AND c.relname = 't2'
  AND r.rolname IN ('testuser', 'testuser2')
ORDER BY 1, 2;

-- Test 7: statement (line 32)
ALTER DEFAULT PRIVILEGES REVOKE SELECT ON TABLES FROM testuser;

-- Test 8: statement (line 35)
CREATE TABLE t3();

-- Test 9: query (line 38)
SELECT r.rolname AS grantee, a.privilege_type
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL aclexplode(c.relacl) AS a
JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'public'
  AND c.relname = 't3'
  AND r.rolname IN ('testuser', 'testuser2')
ORDER BY 1, 2;

-- Test 10: statement (line 46)
ALTER DEFAULT PRIVILEGES GRANT INSERT ON TABLES TO testuser;

-- Test 11: statement (line 49)
CREATE TABLE t4();

-- Test 12: query (line 54)
SELECT r.rolname AS grantee, a.privilege_type
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL aclexplode(c.relacl) AS a
JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'public'
  AND c.relname = 't4'
  AND r.rolname IN ('testuser', 'testuser2')
ORDER BY 1, 2;

-- Test 13: statement (line 62)
CREATE USER testuser2;

-- Test 14: statement (line 65)
ALTER DEFAULT PRIVILEGES GRANT INSERT ON TABLES TO testuser, testuser2;

-- Test 15: statement (line 68)
CREATE TABLE t5();

-- Test 16: query (line 71)
SELECT r.rolname AS grantee, a.privilege_type
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL aclexplode(c.relacl) AS a
JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'public'
  AND c.relname = 't5'
  AND r.rolname IN ('testuser', 'testuser2')
ORDER BY 1, 2;

-- Test 17: statement (line 80)
ALTER DEFAULT PRIVILEGES GRANT ALL ON TABLES TO testuser, testuser2;

-- Test 18: statement (line 83)
CREATE TABLE t6();

-- Test 19: query (line 86)
SELECT r.rolname AS grantee, a.privilege_type
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL aclexplode(c.relacl) AS a
JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'public'
  AND c.relname = 't6'
  AND r.rolname IN ('testuser', 'testuser2')
ORDER BY 1, 2;

-- Test 20: statement (line 97)
ALTER DEFAULT PRIVILEGES REVOKE ALL ON TABLES FROM testuser, testuser2;

-- Test 21: statement (line 100)
CREATE TABLE t7();

-- Test 22: query (line 103)
SELECT r.rolname AS grantee, a.privilege_type
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL aclexplode(c.relacl) AS a
JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'public'
  AND c.relname = 't7'
  AND r.rolname IN ('testuser', 'testuser2')
ORDER BY 1, 2;

-- Test 23: statement (line 115)
ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO testuser;

-- user root (cockroach logic test directive)

-- Test 24: statement (line 122)
ALTER DEFAULT PRIVILEGES GRANT ALL ON SEQUENCES TO testuser, testuser2;
ALTER DEFAULT PRIVILEGES GRANT ALL ON SCHEMAS TO testuser, testuser2;
ALTER DEFAULT PRIVILEGES GRANT ALL ON TYPES TO testuser, testuser2;

-- Test 25: statement (line 127)
CREATE SCHEMA s;

-- Test 26: query (line 130)
SELECT r.rolname AS grantee, a.privilege_type
FROM pg_namespace n
CROSS JOIN LATERAL aclexplode(n.nspacl) AS a
JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 's'
  AND r.rolname IN ('testuser', 'testuser2')
ORDER BY 1, 2;

-- Test 27: statement (line 139)
CREATE SEQUENCE seq;

-- Test 28: query (line 142)
SELECT r.rolname AS grantee, a.privilege_type
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL aclexplode(c.relacl) AS a
JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'public'
  AND c.relname = 'seq'
  AND c.relkind = 'S'
  AND r.rolname IN ('testuser', 'testuser2')
ORDER BY 1, 2;

-- Test 29: statement (line 151)
CREATE TYPE typ AS ENUM ('v');

-- Test 30: query (line 154)
SELECT r.rolname AS grantee, a.privilege_type
FROM pg_type t
JOIN pg_namespace n ON n.oid = t.typnamespace
CROSS JOIN LATERAL aclexplode(t.typacl) AS a
JOIN pg_roles r ON r.oid = a.grantee
WHERE n.nspname = 'public'
  AND t.typname = 'typ'
  AND r.rolname IN ('testuser', 'testuser2')
ORDER BY 1, 2;

RESET client_min_messages;
