-- PostgreSQL compatible tests from partial_txn_commit
-- 12 tests

-- Test 1: statement (line 7)
-- CockroachDB-only session setting.
-- SET create_table_with_schema_locked=false;

-- Test 2: statement (line 10)
-- CockroachDB-only session setting.
-- SET autocommit_before_ddl = false;

-- Test 3: statement (line 13)
CREATE TABLE t (x INT);

-- Test 4: statement (line 16)
INSERT INTO t (x) VALUES (0);

-- Test 5: statement (line 19)
BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE;

-- Test 6: statement (line 22)
ALTER TABLE t ADD COLUMN z INT DEFAULT 123;

-- Test 7: statement (line 25)
INSERT INTO t (x) VALUES (1);

-- Test 8: statement (line 28)
-- PostgreSQL uses GENERATED columns; make division-by-zero well-defined.
ALTER TABLE t
  ADD COLUMN y FLOAT GENERATED ALWAYS AS (1::FLOAT / NULLIF(x::FLOAT, 0::FLOAT)) STORED;

-- Test 9: statement (line 31)
COMMIT;

-- Test 10: query (line 35)
SELECT * FROM t ORDER BY x;

-- Test 11: query (line 42)
-- CockroachDB has SHOW CREATE; approximate via information_schema.
SELECT column_name, data_type, column_default, is_generated, generation_expression
FROM information_schema.columns
WHERE table_schema = 'public' AND table_name = 't'
ORDER BY ordinal_position;

-- Test 12: statement (line 51)
-- CockroachDB-only session setting.
-- RESET autocommit_before_ddl;
