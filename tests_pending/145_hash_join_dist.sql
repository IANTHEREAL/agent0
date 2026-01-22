-- PostgreSQL compatible tests from hash_join_dist
-- 15 tests

-- Test 1: statement (line 3)
CREATE TABLE t (k INT, v INT);

-- Test 2: statement (line 6)
INSERT INTO t VALUES (1, 10), (2, 20), (3, 30);

-- retry (CockroachDB logic-test directive; not used by psql)

-- Test 3: statement (line 13)
CREATE TABLE xy (x INT PRIMARY KEY, y INT);

-- Test 4: statement (line 16)
INSERT INTO xy VALUES (2, 200), (3, 300), (4, 400);

-- Test 5: query (line 28)
SELECT * FROM t WHERE EXISTS(SELECT * FROM xy WHERE x=t.k);

-- Test 6: statement (line 35)
CREATE TABLE small (a INT PRIMARY KEY, b INT);

-- Test 7: statement (line 38)
CREATE TABLE large (c INT, d INT);

-- Test 8: statement (line 41)
INSERT INTO small SELECT x, 3*x FROM
  generate_series(1, 10) AS a(x);

-- Test 9: statement (line 45)
INSERT INTO large SELECT 2*x, 4*x FROM
  generate_series(1, 10) AS a(x);

-- Test 10: query (line 65)
SELECT small.b, large.d FROM large RIGHT JOIN small ON small.b = large.c AND large.d < 30 ORDER BY 1 LIMIT 5;

-- Test 11: query (line 74)
-- CockroachDB-specific introspection (no Postgres equivalent).
-- SELECT feature_name FROM crdb_internal.feature_usage WHERE feature_name='sql.exec.query.is-distributed' AND usage_count > 0;

-- Test 12: statement (line 81)
CREATE TABLE a (id TEXT PRIMARY KEY);
CREATE TABLE b (
  id TEXT PRIMARY KEY,
  a_id TEXT,
  status INT
);
CREATE INDEX b_a_id ON b (a_id ASC);
CREATE INDEX b_status_idx ON b (status ASC);
SELECT a.id FROM a
LEFT JOIN b AS b2 ON (a.id = b2.a_id AND b2.status = 2)
WHERE (a.id IN ('3f90e30a-c87a-4017-b9a0-8f964b91c4af', '3adaf3da-0368-461a-8437-ee448724b78d', 'd0c13b06-5368-4522-8126-105b0a9513cd'))
ORDER BY id DESC
LIMIT 2;

-- Test 13: statement (line 98)
CREATE TABLE t86075 (k INT PRIMARY KEY, c REGPROCEDURE, a REGPROCEDURE[]);
INSERT INTO t86075 VALUES (1, 1, ARRAY[1]), (2, 2, ARRAY[2]), (3, 3, ARRAY[3]);
CREATE TABLE t86075_2 (k INT PRIMARY KEY, c REGPROCEDURE, a REGPROCEDURE[]);
INSERT INTO t86075_2 VALUES (1, 1, ARRAY[1]), (2, 2, ARRAY[2]), (3, 3, ARRAY[3]);

-- Test 14: query (line 116)
SELECT t1.k FROM t86075 AS t1, t86075_2 AS t2 WHERE t1.c = t2.c;

-- Test 15: query (line 123)
SELECT t1.k FROM t86075 AS t1, t86075_2 AS t2 WHERE t1.a = t2.a;
