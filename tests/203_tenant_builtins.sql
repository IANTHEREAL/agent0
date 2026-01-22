-- PostgreSQL compatible tests from tenant_builtins
-- 3 tests

SET client_min_messages = warning;

-- Test 1: statement (line 185)
-- CockroachDB cluster setting (no PostgreSQL equivalent).
-- RESET CLUSTER SETTING sql.defaults.vectorize;

-- Test 2: query (line 188)
-- CockroachDB cluster setting (no PostgreSQL equivalent).
-- SHOW CLUSTER SETTING sql.defaults.vectorize;

-- Test 3: statement (line 221)
-- CockroachDB cluster setting (no PostgreSQL equivalent).
-- SET CLUSTER SETTING jobs.registry.interval.adopt = '1s';

RESET client_min_messages;
