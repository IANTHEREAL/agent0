-- PostgreSQL compatible tests from index_recommendations
-- 29 tests

-- Test 1: statement (line 5)
-- CockroachDB-only setting (no PostgreSQL equivalent):
-- SET CLUSTER SETTING sql.metrics.statement_details.index_recommendation_collection.enabled = true;

-- Test 2: statement (line 8)
-- CockroachDB-only setting (no PostgreSQL equivalent):
-- SET enable_insert_fast_path = true;

-- Test 3: statement (line 11)
CREATE TABLE b (b INT PRIMARY KEY);

-- Test 4: statement (line 14)
CREATE TABLE c (c INT PRIMARY KEY);

-- Test 5: statement (line 17)
CREATE TABLE d (d INT PRIMARY KEY);

-- Test 6: statement (line 20)
CREATE TABLE abcd (
  a INT NOT NULL PRIMARY KEY,
  b INT NULL REFERENCES b (b),
  c INT NULL REFERENCES c (c),
  d INT NULL REFERENCES d (d)
);

-- CockroachDB supports inline secondary indexes in CREATE TABLE. In PostgreSQL,
-- create them separately.
CREATE INDEX abcd_b_idx ON abcd (b);
CREATE INDEX abcd_c_idx ON abcd (c);
CREATE INDEX abcd_d_idx ON abcd (d);

-- Test 7: statement (line 31)
INSERT INTO b VALUES (0), (1), (2), (3), (4), (5);

-- Test 8: statement (line 34)
INSERT INTO c VALUES (0), (1), (2), (3), (4), (5);

-- Test 9: statement (line 37)
INSERT INTO d VALUES (0), (1), (2), (3), (4), (5);

-- Test 10: statement (line 42)
PREPARE p0 AS INSERT INTO abcd VALUES ($1, $2, $3, $4);

-- Test 11: statement (line 45)
EXECUTE p0(0, 0, 0, 0);

-- Test 12: statement (line 48)
DEALLOCATE p0;

-- Test 13: statement (line 51)
PREPARE p1 AS INSERT INTO abcd VALUES ($1, $2, $3, $4);

-- Test 14: statement (line 54)
EXECUTE p1(1, 1, 1, 1);

-- Test 15: statement (line 57)
DEALLOCATE p1;

-- Test 16: statement (line 60)
PREPARE p2 AS INSERT INTO abcd VALUES ($1, $2, $3, $4);

-- Test 17: statement (line 63)
EXECUTE p2(2, 2, 2, 2);

-- Test 18: statement (line 66)
DEALLOCATE p2;

-- Test 19: statement (line 69)
PREPARE p3 AS INSERT INTO abcd VALUES ($1, $2, $3, $4);

-- Test 20: statement (line 72)
EXECUTE p3(3, 3, 3, 3);

-- Test 21: statement (line 75)
DEALLOCATE p3;

-- Test 22: statement (line 78)
PREPARE p4 AS INSERT INTO abcd VALUES ($1, $2, $3, $4);

-- Test 23: statement (line 81)
EXECUTE p4(4, 4, 4, 4);

-- Test 24: statement (line 84)
DEALLOCATE p4;

-- Test 25: statement (line 94)
PREPARE p5 AS INSERT INTO abcd VALUES ($1, $2, $3, $4);

-- Test 26: statement (line 97)
EXECUTE p5(5, 5, NULL, 5);

-- Test 27: statement (line 100)
DEALLOCATE p5;

-- Test 28: statement (line 103)
-- CockroachDB-only setting (no PostgreSQL equivalent):
-- RESET CLUSTER SETTING sql.metrics.statement_details.index_recommendation_collection.enabled;

-- Test 29: statement (line 106)
-- CockroachDB-only setting (no PostgreSQL equivalent):
-- RESET enable_insert_fast_path;
