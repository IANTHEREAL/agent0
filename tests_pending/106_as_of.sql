-- PostgreSQL compatible tests from as_of
-- 20 tests

SET client_min_messages = warning;
SET TIME ZONE 'UTC';
DROP TABLE IF EXISTS v00, t;

-- CockroachDB helper functions used with `AS OF SYSTEM TIME` are not available
-- in PostgreSQL. Provide minimal stubs so the test can run.
CREATE OR REPLACE FUNCTION with_min_timestamp(ts timestamptz)
RETURNS timestamptz
LANGUAGE sql
IMMUTABLE
AS $$ SELECT ts $$;

CREATE OR REPLACE FUNCTION with_max_staleness(i interval)
RETURNS interval
LANGUAGE sql
IMMUTABLE
AS $$ SELECT i $$;

-- Test 1: statement (line 2)
CREATE TABLE t (i INT);

-- Test 2: statement (line 5)
INSERT INTO t VALUES (2);

-- Test 3: query (line 35)
SELECT pg_sleep(5); -- we need to sleep so that the 4.8s elapses and the SELECT * FROM t returns something.

-- Test 4: statement (line 105)
SELECT with_min_timestamp('2020-01-15 15:16:17+00'::timestamptz);

-- skipif config enterprise-configs

-- Test 5: statement (line 109)
SELECT with_min_timestamp(statement_timestamp()) IS NOT NULL;

-- skipif config enterprise-configs

-- Test 6: statement (line 113)
SELECT with_max_staleness('1s'::interval);

-- skipif config enterprise-configs

-- Test 7: statement (line 128)
BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE;

-- Test 8: statement (line 131)
SELECT * FROM t;

-- Test 9: statement (line 137)
ROLLBACK;

-- Test 10: statement (line 140)
-- CockroachDB session/cluster settings are not supported in PostgreSQL.
-- SET kv_transaction_buffered_writes_enabled = false;

-- Test 11: statement (line 143)
BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE;

-- Test 12: statement (line 146)
INSERT INTO t VALUES (1);

-- Test 13: statement (line 152)
ROLLBACK;

-- Test 14: statement (line 155)
-- SET kv_transaction_buffered_writes_enabled = true;

-- Test 15: statement (line 159)
-- SET CLUSTER SETTING kv.transaction.write_buffering.max_buffer_size = '2KiB';

-- Test 16: statement (line 162)
BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE;

-- Test 17: statement (line 165)
INSERT INTO t VALUES (1);

-- Test 18: statement (line 171)
ROLLBACK;

-- Test 19: statement (line 174)
-- RESET kv_transaction_buffered_writes_enabled;

-- Test 20: statement (line 233)
CREATE TABLE v00 (c01 INT);

RESET TIME ZONE;
RESET client_min_messages;
