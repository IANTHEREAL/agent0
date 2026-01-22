-- PostgreSQL compatible tests from create_as_non_metamorphic
-- 3 tests

-- Test 1: statement (line 7)
-- CockroachDB-only cluster setting (no PostgreSQL equivalent).
-- SET CLUSTER SETTING kv.raft.command.max_size='4.01MiB';

-- Test 2: statement (line 10)
BEGIN;
DROP TABLE IF EXISTS source_tbl_huge;
CREATE TABLE source_tbl_huge AS SELECT 1::CHAR(256) FROM generate_series(1, 50000);
COMMIT;

-- Test 3: statement (line 15)
-- CockroachDB-only cluster setting (no PostgreSQL equivalent).
-- SET CLUSTER SETTING kv.raft.command.max_size to default;
