-- PostgreSQL compatible tests from schema_change_retry
-- 8 tests

-- Test 1: statement (line 9)
-- CockroachDB-only cluster settings are not applicable in PostgreSQL.
-- SET CLUSTER SETTING kv.transaction.max_refresh_spans_bytes = 0;

-- Test 2: statement (line 13)
-- CockroachDB-only setting (PostgreSQL always runs DDL in the current txn).
-- SET autocommit_before_ddl = false;

-- Test 3: statement (line 16)
BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE;

-- Test 4: statement (line 19)
SAVEPOINT cockroach_restart;

-- Test 5: statement (line 22)
CREATE TABLE t (x INT PRIMARY KEY, y INT);

-- Test 6: statement (line 29)
CREATE INDEX y_idx ON t (y);

-- Test 7: statement (line 36)
COMMIT;

-- CockroachDB-only transaction priorities are not supported in PostgreSQL.
BEGIN TRANSACTION;
SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;

-- Test 8: statement (line 40)
-- SHOW TABLES
SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname = 'public' ORDER BY tablename;

COMMIT;

-- user root
