-- PostgreSQL compatible tests from internal_executor
-- 3 tests

-- Test 1: statement (line 13)
-- CockroachDB: SET avoid_buffering = true;
-- No PostgreSQL equivalent (no-op).

DO $do$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'testuser') THEN
    CREATE ROLE testuser LOGIN;
  END IF;
END
$do$;

-- Test 2: statement (line 16)
CREATE TABLE t (i INT PRIMARY KEY);

-- Test 3: statement (line 19)
SELECT has_table_privilege('testuser', 't', 'SELECT');
