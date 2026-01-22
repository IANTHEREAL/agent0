-- PostgreSQL compatible tests from distinct
-- 35 tests

SET client_min_messages = warning;

-- Test 1: statement (line 1)
CREATE TABLE xyz (
  x INT PRIMARY KEY,
  y INT,
  z INT
);
CREATE INDEX foo ON xyz (z, y);

-- Test 2: statement (line 9)
INSERT INTO xyz VALUES
  (1, 2, 3),
  (2, 5, 6),
  (3, 2, 3),
  (4, 5, 6),
  (5, 2, 6),
  (6, 3, 5),
  (7, 2, 9);

-- Test 3: query (line 19)
SELECT y, z FROM xyz ORDER BY y, z, x;

-- Test 4: query (line 30)
SELECT DISTINCT y, z FROM xyz ORDER BY y, z;

-- Test 5: query (line 39)
SELECT y FROM (SELECT DISTINCT y, z FROM xyz) AS s ORDER BY y, z;

-- Test 6: query (line 48)
SELECT DISTINCT y, z FROM xyz ORDER BY z, y;

-- Test 7: query (line 57)
SELECT DISTINCT y, z FROM xyz ORDER BY y, z;

-- Test 8: query (line 66)
SELECT DISTINCT y, z FROM xyz ORDER BY y, z;

-- Test 9: query (line 75)
SELECT DISTINCT y + z FROM xyz ORDER by (y + z);

-- Test 10: query (line 82)
SELECT DISTINCT y AS w, z FROM xyz ORDER by z, w;

-- Test 11: query (line 91)
SELECT DISTINCT y AS w FROM xyz ORDER by y;

-- Test 12: statement (line 99)
INSERT INTO xyz (x, y) VALUES (8, 2), (9, 2);

-- Test 13: query (line 102)
SELECT DISTINCT y,z FROM xyz ORDER BY y, z;

-- Test 14: query (line 112)
SELECT DISTINCT (y,z) FROM xyz ORDER BY 1;

-- Test 15: query (line 122)
SELECT count(*) FROM (SELECT DISTINCT y FROM xyz) AS s;

-- Test 16: statement (line 127)
CREATE TABLE kv (k INT PRIMARY KEY, v INT);
CREATE UNIQUE INDEX idx ON kv (v);

-- Test 17: statement (line 130)
INSERT INTO kv VALUES (1, 1), (2, 2), (3, NULL), (4, NULL), (5, 5), (6, NULL);

-- Test 18: query (line 133)
SELECT DISTINCT v FROM kv ORDER BY v;

-- Test 19: query (line 141)
-- CockroachDB index hint (@idx) not supported in PostgreSQL.
SELECT DISTINCT v FROM kv ORDER BY v;

-- Test 20: query (line 149)
-- CockroachDB index hint (@idx) not supported in PostgreSQL.
SELECT DISTINCT v FROM kv WHERE v > 0 ORDER BY v;

-- Test 21: statement (line 157)
CREATE TABLE t0(c0 INT UNIQUE);

-- Test 22: statement (line 160)
CREATE VIEW v0(c0) AS SELECT DISTINCT t0.c0 FROM t0;

-- Test 23: statement (line 163)
INSERT INTO t0 (c0) VALUES (NULL), (NULL);

-- Test 24: query (line 166)
SELECT * FROM v0 WHERE v0.c0 IS NULL;

-- Test 25: statement (line 172)
CREATE TABLE t44079 (x INT[]);
INSERT INTO t44079 VALUES (NULL), (ARRAY[NULL]::INT[]);

-- Test 26: query (line 176)
SELECT DISTINCT * FROM t44079 ORDER BY x;

-- Test 27: statement (line 182)
DROP TABLE IF EXISTS t;
CREATE TABLE t (x JSONB);
INSERT INTO t VALUES
  ('{"foo" : "bar"}'),
  ('{"foo" : "bar"}'),
  ('[1, 2]'),
  ('[2, 1]'),
  ('[1, 2]'),
  ('{"foo": {"bar" : "baz"}}');

-- Test 28: query (line 193)
SELECT DISTINCT (x) FROM t ORDER BY x;

-- Test 29: statement (line 201)
DROP TABLE IF EXISTS t;

-- Test 30: statement (line 204)
CREATE TABLE t (x DECIMAL);

-- Test 31: statement (line 207)
INSERT INTO t VALUES (1.0), (1.00), (1.000);

-- Test 32: query (line 213)
SELECT COUNT (*) FROM (SELECT DISTINCT (array[x]) FROM t) AS s;

-- Test 33: statement (line 219)
DROP TABLE IF EXISTS t;

-- Test 34: statement (line 225)
CREATE TABLE t (i INT, x INT, y INT, z TEXT);
INSERT INTO t VALUES
  (1, 1, 2, 'hello'),
  (2, 1, 2, 'hello'),
  (3, 1, 2, 'hello there');

-- Test 35: query (line 231)
SELECT
  x,
  jsonb_agg(DISTINCT jsonb_build_object('y', y, 'z', z))
FROM (SELECT * FROM t ORDER BY i) AS ordered
GROUP BY x;

RESET client_min_messages;
