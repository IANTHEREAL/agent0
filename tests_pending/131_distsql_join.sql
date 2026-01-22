-- PostgreSQL compatible tests from distsql_join
-- 13 tests

SET client_min_messages = warning;

DROP TABLE IF EXISTS distsql_mj_test;
DROP TABLE IF EXISTS tab0;

-- Test 1: statement (line 4)
CREATE TABLE distsql_mj_test (k INT, v INT);

-- Test 2: statement (line 7)
INSERT INTO distsql_mj_test VALUES (0, NULL), (0, 1), (2, 4), (NULL, 4);

-- Test 3: query (line 12)
SELECT l.k, l.v, r.k, r.v
FROM (SELECT * FROM distsql_mj_test ORDER BY k, v) l
INNER JOIN (SELECT * FROM distsql_mj_test ORDER BY k, v) r
  ON l.k = r.k AND l.v = r.v
ORDER BY l.k, l.v, r.k, r.v;

-- Test 4: statement (line 18)
DELETE FROM distsql_mj_test WHERE TRUE;

-- Test 5: statement (line 21)
INSERT INTO distsql_mj_test VALUES (0, NULL), (1, NULL), (2, NULL);

-- Test 6: query (line 25)
SELECT l.k, l.v, r.k, r.v
FROM (SELECT * FROM distsql_mj_test ORDER BY k, v) l
INNER JOIN (SELECT * FROM distsql_mj_test ORDER BY k, v) r
  ON l.k = r.k AND l.v = r.v
ORDER BY l.k, l.v, r.k, r.v;

-- Test 7: statement (line 29)
DELETE FROM distsql_mj_test WHERE TRUE;

-- Test 8: statement (line 32)
INSERT INTO distsql_mj_test VALUES (NULL, NULL);

-- Test 9: query (line 36)
SELECT l.k, r.k
FROM (SELECT * FROM distsql_mj_test ORDER BY k) l
INNER JOIN (SELECT * FROM distsql_mj_test ORDER BY k) r
  ON l.k = r.k
ORDER BY l.k, r.k;

-- Test 10: statement (line 42)
CREATE TABLE tab0(pk INTEGER PRIMARY KEY, a INTEGER, b INTEGER);

-- Test 11: statement (line 45)
INSERT INTO tab0 VALUES(0,1,2);

-- Test 12: statement (line 48)
CREATE INDEX on tab0 (a);

-- Test 13: query (line 51)
SELECT pk, a, b FROM tab0 WHERE a < 10 AND b = 2 ORDER BY a DESC, pk;

RESET client_min_messages;
