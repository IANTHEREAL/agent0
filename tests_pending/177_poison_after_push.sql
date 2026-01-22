-- PostgreSQL compatible tests from poison_after_push
-- 18 tests

-- Test 1: statement (line 20)
-- CockroachDB cluster setting; not applicable in PostgreSQL.
-- SET CLUSTER SETTING kv.transaction.write_pipelining.enabled = false;

-- Test 2: statement (line 23)
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'testuser') THEN
    CREATE ROLE testuser LOGIN;
  END IF;
END
$$;

-- Test 3: statement (line 26)
CREATE TABLE t (id INT PRIMARY KEY);

-- Test 4: statement (line 29)
INSERT INTO t VALUES (1);

-- Test 5: statement (line 32)
GRANT ALL ON TABLE t TO testuser;

-- Use two backend connections via dblink to model the upstream multi-session
-- transaction interleavings (PostgreSQL doesn't support PRIORITY).
CREATE EXTENSION IF NOT EXISTS dblink;
SELECT dblink_connect('conn_low', 'dbname=' || current_database()) AS ignore \gset
SELECT dblink_connect('conn_high', 'dbname=' || current_database()) AS ignore \gset

-- Test 6: statement (line 35)
SELECT dblink_exec('conn_low', 'BEGIN ISOLATION LEVEL SERIALIZABLE') AS ignore \gset
SELECT dblink_exec('conn_low', 'INSERT INTO t VALUES (2)') AS ignore \gset

-- Test 7: statement (line 41)
SELECT dblink_exec('conn_high', 'BEGIN ISOLATION LEVEL SERIALIZABLE') AS ignore \gset

-- Test 8: query (line 45)
SELECT * FROM dblink('conn_high', 'SELECT * FROM t') AS t(id int);

-- Test 9: statement (line 50)
SELECT dblink_exec('conn_high', 'COMMIT') AS ignore \gset

-- Test 10: query (line 57)
SELECT * FROM t ORDER BY id;

-- Test 11: statement (line 65)
SELECT dblink_exec('conn_low', 'COMMIT') AS ignore \gset

-- Test 12: statement (line 70)
SELECT dblink_exec('conn_low', 'BEGIN ISOLATION LEVEL REPEATABLE READ') AS ignore \gset

-- Test 13: statement (line 73)
SELECT dblink_exec('conn_low', 'INSERT INTO t VALUES (3)') AS ignore \gset

SELECT dblink_exec('conn_high', 'SET ROLE testuser') AS ignore \gset

-- Test 14: statement (line 78)
SELECT dblink_exec('conn_high', 'BEGIN ISOLATION LEVEL SERIALIZABLE') AS ignore \gset

-- Test 15: query (line 82)
SELECT * FROM dblink('conn_high', 'SELECT * FROM t ORDER BY id') AS t(id int);

-- Test 16: statement (line 88)
SELECT dblink_exec('conn_high', 'COMMIT') AS ignore \gset

-- Test 17: query (line 93)
SELECT * FROM dblink('conn_low', 'SELECT * FROM t ORDER BY id') AS t(id int);

-- Test 18: statement (line 100)
SELECT dblink_exec('conn_low', 'COMMIT') AS ignore \gset

SELECT dblink_disconnect('conn_high') AS ignore \gset
SELECT dblink_disconnect('conn_low') AS ignore \gset
