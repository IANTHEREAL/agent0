-- PostgreSQL compatible tests from truncate_with_concurrent_mutation
-- 30 tests

SET client_min_messages = warning;

-- Test 1: statement (line 3)
-- SET CLUSTER SETTING jobs.registry.interval.adopt = '50ms';

-- Test 2: statement (line 6)
-- SET CLUSTER SETTING jobs.registry.interval.cancel = '50ms';

-- Test 3: statement (line 11)
CREATE TABLE t1(a int primary key, b int);

-- Test 4: statement (line 14)
CREATE INDEX idx_b ON t1(b);

-- Test 5: statement (line 17)
-- SET CLUSTER SETTING jobs.debug.pausepoints = 'schemachanger.before.exec,newschemachanger.before.exec';

-- Test 6: statement (line 20)
DROP INDEX idx_b;

-- Test 7: statement (line 23)
TRUNCATE TABLE t1;

-- Test 8: statement (line 26)
-- SET CLUSTER SETTING jobs.debug.pausepoints = '';

-- Test 9: statement (line 31)
CREATE TYPE e AS ENUM ('v1', 'v2');
CREATE TABLE t2(a int primary key, b e);

-- Test 10: statement (line 35)
-- SET CLUSTER SETTING jobs.debug.pausepoints = 'schemachanger.before.exec,newschemachanger.before.exec';

-- Test 11: statement (line 38)
ALTER TABLE t2 DROP COLUMN b;

-- Test 12: statement (line 41)
TRUNCATE TABLE t2;

-- Test 13: statement (line 44)
-- SET CLUSTER SETTING jobs.debug.pausepoints = '';

-- Test 14: statement (line 49)
CREATE TABLE t3(a INT PRIMARY KEY, b INT);

-- Test 15: statement (line 52)
-- SET CLUSTER SETTING jobs.debug.pausepoints = 'schemachanger.before.exec,newschemachanger.before.exec';

-- Test 16: statement (line 55)
ALTER TABLE t3 ADD CONSTRAINT ckb CHECK (b > 3);

-- Test 17: statement (line 58)
TRUNCATE TABLE t3;

-- Test 18: statement (line 61)
-- SET CLUSTER SETTING jobs.debug.pausepoints = '';

-- Test 19: statement (line 65)
CREATE TABLE t4(a INT PRIMARY KEY, b INT NOT NULL);

-- Test 20: statement (line 68)
-- SET CLUSTER SETTING jobs.debug.pausepoints = 'schemachanger.before.exec,newschemachanger.before.exec';

-- Test 21: statement (line 71)
ALTER TABLE t4 DROP CONSTRAINT t4_pkey;
ALTER TABLE t4 ADD PRIMARY KEY (b);

-- Test 22: statement (line 78)
TRUNCATE TABLE t4;

-- Test 23: statement (line 81)
-- SET CLUSTER SETTING jobs.debug.pausepoints = '';

-- Test 24: statement (line 86)
CREATE TABLE t6(a int primary key, b int);

-- Test 25: statement (line 89)
-- SET CLUSTER SETTING jobs.debug.pausepoints = 'schemachanger.before.exec,newschemachanger.before.exec';

-- Test 26: statement (line 92)
-- ALTER TABLE t6 SET (ttl_expire_after = '00:10:00':::INTERVAL);

-- Test 27: statement (line 95)
TRUNCATE TABLE t6;

-- Test 28: statement (line 98)
-- SET CLUSTER SETTING jobs.debug.pausepoints = '';

-- Test 29: statement (line 101)
-- RESUME JOBS (SELECT job_id FROM crdb_internal.jobs WHERE status = 'paused');

-- Test 30: statement (line 104)
-- USE test;

RESET client_min_messages;
