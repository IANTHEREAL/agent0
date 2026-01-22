-- PostgreSQL compatible tests from alter_default_privileges_for_sequence
-- 30 tests

SET client_min_messages = warning;

-- This file creates cluster-wide objects (roles + a database). Keep it
-- rerunnable so iterative adaptation doesn't get stuck on leftovers.
DROP DATABASE IF EXISTS d;
DROP ROLE IF EXISTS testuser2;
DROP ROLE IF EXISTS testuser;
CREATE USER testuser;

-- Remember the harness-provided isolated database so we can "USE test" later.
\set orig_db :DBNAME

-- Test 1: statement (line 1)
CREATE DATABASE d;
GRANT CREATE ON DATABASE d TO testuser;

-- Test 2: statement (line 8)
\set QUIET on
\c d
\set QUIET off
GRANT USAGE, CREATE ON SCHEMA public TO testuser;

-- Test 3: statement (line 11)
CREATE SEQUENCE testuser_s;

-- Test 4: query (line 14)
SELECT
  'testuser' AS role,
  has_sequence_privilege('testuser', 'testuser_s', 'USAGE')  AS usage,
  has_sequence_privilege('testuser', 'testuser_s', 'SELECT') AS select,
  has_sequence_privilege('testuser', 'testuser_s', 'UPDATE') AS update;

-- Test 5: statement (line 22)
ALTER DEFAULT PRIVILEGES REVOKE ALL ON SEQUENCES FROM testuser;
ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON SEQUENCES FROM testuser;

-- Test 6: statement (line 26)
CREATE SEQUENCE testuser_s2;

-- Test 7: query (line 32)
SELECT
  'testuser' AS role,
  has_sequence_privilege('testuser', 'testuser_s2', 'USAGE')  AS usage,
  has_sequence_privilege('testuser', 'testuser_s2', 'SELECT') AS select,
  has_sequence_privilege('testuser', 'testuser_s2', 'UPDATE') AS update;

-- Test 8: statement (line 42)
\set QUIET on
\c :orig_db
\set QUIET off

-- Test 9: statement (line 45)
CREATE USER testuser2;

-- Test 10: statement (line 48)
ALTER DEFAULT PRIVILEGES GRANT ALL ON SEQUENCES TO testuser, testuser2;

-- Test 11: statement (line 51)
CREATE SEQUENCE s;

-- Test 12: query (line 54)
SELECT
  r.role,
  has_sequence_privilege(r.role, 's', 'USAGE')  AS usage,
  has_sequence_privilege(r.role, 's', 'SELECT') AS select,
  has_sequence_privilege(r.role, 's', 'UPDATE') AS update
FROM (VALUES ('testuser'), ('testuser2')) AS r(role)
ORDER BY r.role;

-- Test 13: statement (line 64)
CREATE TABLE t();

-- Test 14: query (line 67)
SELECT
  r.role,
  has_table_privilege(r.role, 't', 'SELECT') AS select,
  has_table_privilege(r.role, 't', 'INSERT') AS insert,
  has_table_privilege(r.role, 't', 'UPDATE') AS update,
  has_table_privilege(r.role, 't', 'DELETE') AS delete
FROM (VALUES ('testuser'), ('testuser2')) AS r(role)
ORDER BY r.role;

-- Test 15: statement (line 75)
ALTER DEFAULT PRIVILEGES REVOKE SELECT ON SEQUENCES FROM testuser, testuser2;

-- Test 16: statement (line 78)
CREATE SEQUENCE s2;

-- Test 17: query (line 81)
SELECT
  r.role,
  has_sequence_privilege(r.role, 's2', 'USAGE')  AS usage,
  has_sequence_privilege(r.role, 's2', 'SELECT') AS select,
  has_sequence_privilege(r.role, 's2', 'UPDATE') AS update
FROM (VALUES ('testuser'), ('testuser2')) AS r(role)
ORDER BY r.role;

-- Test 18: statement (line 104)
ALTER DEFAULT PRIVILEGES REVOKE ALL ON SEQUENCES FROM testuser, testuser2;

-- Test 19: statement (line 107)
CREATE SEQUENCE s3;

-- Test 20: query (line 110)
SELECT
  r.role,
  has_sequence_privilege(r.role, 's3', 'USAGE')  AS usage,
  has_sequence_privilege(r.role, 's3', 'SELECT') AS select,
  has_sequence_privilege(r.role, 's3', 'UPDATE') AS update
FROM (VALUES ('testuser'), ('testuser2')) AS r(role)
ORDER BY r.role;

-- Test 21: statement (line 117)
GRANT CREATE ON DATABASE d TO testuser;
-- user testuser

-- Test 22: statement (line 121)
\set QUIET on
\c d
\set QUIET off
SET ROLE testuser;

-- Test 23: statement (line 124)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser REVOKE ALL ON SEQUENCES FROM testuser, testuser2;

-- Test 24: statement (line 127)
CREATE SEQUENCE s4;

-- Test 25: query (line 131)
SELECT
  r.role,
  has_sequence_privilege(r.role, 's4', 'USAGE')  AS usage,
  has_sequence_privilege(r.role, 's4', 'SELECT') AS select,
  has_sequence_privilege(r.role, 's4', 'UPDATE') AS update
FROM (VALUES ('testuser'), ('testuser2')) AS r(role)
ORDER BY r.role;

-- Test 26: statement (line 140)
RESET ROLE;
\set QUIET on
\c d
\set QUIET off

-- Test 27: statement (line 143)
ALTER DEFAULT PRIVILEGES FOR ROLE testuser REVOKE ALL ON SEQUENCES FROM testuser, testuser2;
-- user testuser

-- Test 28: statement (line 147)
\set QUIET on
\c d
\set QUIET off
SET ROLE testuser;

-- Test 29: statement (line 150)
CREATE SEQUENCE s5;

-- Test 30: query (line 154)
SELECT
  r.role,
  has_sequence_privilege(r.role, 's5', 'USAGE')  AS usage,
  has_sequence_privilege(r.role, 's5', 'SELECT') AS select,
  has_sequence_privilege(r.role, 's5', 'UPDATE') AS update
FROM (VALUES ('testuser'), ('testuser2')) AS r(role)
ORDER BY r.role;

RESET ROLE;
\set QUIET on
\c :orig_db
\set QUIET off

DROP DATABASE d;
DROP OWNED BY testuser2;
DROP OWNED BY testuser;
DROP ROLE testuser2;
DROP ROLE testuser;
