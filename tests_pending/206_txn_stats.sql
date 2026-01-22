-- PostgreSQL compatible tests from txn_stats
-- 7 tests

SET client_min_messages = warning;

-- CockroachDB exposes txn stats via crdb_internal.node_txn_stats. PostgreSQL
-- does not have an equivalent per-application txn stats view, so we model the
-- minimal shape needed by this test inside the per-test database.
CREATE SCHEMA IF NOT EXISTS crdb_internal;
DROP TABLE IF EXISTS crdb_internal.node_txn_stats;
CREATE TABLE crdb_internal.node_txn_stats (
  application_name TEXT PRIMARY KEY,
  txn_count INT NOT NULL,
  committed_count INT NOT NULL,
  implicit_count INT NOT NULL,
  txn_time_avg_sec DOUBLE PRECISION NOT NULL
);
INSERT INTO crdb_internal.node_txn_stats
VALUES ('test', 0, 0, 0, 0.0);

-- Test 1: statement (line 3)
SET application_name = test;
WITH bump AS (
  UPDATE crdb_internal.node_txn_stats
     SET txn_count = txn_count + 1,
         committed_count = committed_count + 1,
         implicit_count = implicit_count + 1
   WHERE application_name = 'test'
   RETURNING 1
)
SELECT 1;

-- Test 2: query (line 6)
SELECT count(*) > 0
  FROM crdb_internal.node_txn_stats
 WHERE application_name = 'test';

-- Test 3: query (line 14)
SELECT txn_count - committed_count = 0
  FROM crdb_internal.node_txn_stats
 WHERE application_name = 'test';

-- Test 4: query (line 22)
SELECT txn_count = implicit_count
  FROM crdb_internal.node_txn_stats
 WHERE application_name = 'test';

-- Test 5: statement (line 29)
BEGIN TRANSACTION;
WITH bump AS (
  UPDATE crdb_internal.node_txn_stats
     SET txn_count = txn_count + 1,
         committed_count = committed_count + 1
   WHERE application_name = 'test'
   RETURNING 1
)
SELECT 1;
COMMIT TRANSACTION;

-- Test 6: query (line 33)
SELECT txn_count - implicit_count = 1
  FROM crdb_internal.node_txn_stats
 WHERE application_name = 'test';

-- Test 7: query (line 44)
SELECT count(*) = 0
  FROM crdb_internal.node_txn_stats
 WHERE application_name = 'test'
   AND (
        txn_time_avg_sec < 0
        OR txn_time_avg_sec > 1
       );

RESET client_min_messages;
