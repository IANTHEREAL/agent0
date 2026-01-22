-- PostgreSQL compatible tests from alter_role
-- 3 tests

-- Test 1: statement (line 4)
SET client_min_messages = warning;
DROP ROLE IF EXISTS roach;
CREATE USER roach;

-- Test 2: statement (line 7)
ALTER ROLE roach WITH CREATEDB CREATEROLE NOLOGIN;

-- Test 3: statement (line 15)
ALTER ROLE roach WITH CREATEROLE CREATEDB NOLOGIN;

RESET client_min_messages;
