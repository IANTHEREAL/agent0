-- PostgreSQL compatible tests from distsql_event_log
-- 7 tests

SET client_min_messages = warning;

DROP TABLE IF EXISTS a;

-- Test 1: statement (line 8)
-- CockroachDB cluster setting (no PostgreSQL equivalent).
-- SET CLUSTER SETTING sql.stats.system_tables_autostats.enabled = FALSE;

-- Test 2: statement (line 12)
-- CockroachDB cluster setting (no PostgreSQL equivalent).
-- SET CLUSTER SETTING sql.stats.post_events.enabled = TRUE;

-- Test 3: statement (line 15)
CREATE TABLE a (id INT PRIMARY KEY, x INT, y INT);
CREATE INDEX x_idx ON a (x, y);

-- Test 4: statement (line 18)
CREATE STATISTICS s1 ON x, y FROM a;

-- logic-test directive (CockroachDB): retry

-- Test 5: statement (line 22)
-- CockroachDB auto-statistics collection has no direct PG equivalent.
ANALYZE a;

-- Test 6: query (line 27)
-- CockroachDB's system.eventlog does not exist in PostgreSQL.
-- Verify that the statistics object was created instead.
SELECT stxname, stxrelid::regclass::text AS relname, stxkeys
FROM pg_statistic_ext
WHERE stxname = 's1';

-- Test 7: statement (line 36)
DROP TABLE a;

RESET client_min_messages;
