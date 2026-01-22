-- PostgreSQL compatible tests from multi_region
-- 2 tests

-- CockroachDB multi-region DDL has no direct PostgreSQL equivalent.
-- For PostgreSQL runs, emulate the intent by storing region metadata as
-- database-level custom GUCs.

SET client_min_messages = warning;

-- Test 1: statement (line 3)
DROP DATABASE IF EXISTS region_test_db;
CREATE DATABASE region_test_db;
ALTER DATABASE region_test_db SET crdb.primary_region = 'ap-southeast-2';
ALTER DATABASE region_test_db SET crdb.survive_goal = 'zone failure';

-- Test 2: statement (line 6)
-- Equivalent of "ALTER DATABASE test PRIMARY REGION ...": update the current
-- database's metadata.
SELECT format(
  'ALTER DATABASE %I SET crdb.primary_region = %L;',
  current_database(),
  'ap-southeast-2'
) \gexec

DROP DATABASE region_test_db;

RESET client_min_messages;
