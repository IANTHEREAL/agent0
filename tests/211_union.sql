-- PostgreSQL compatible tests from union
-- 117 tests

-- Test 1: query (line 1)
VALUES (1), (1), (1), (2), (2) UNION VALUES (1), (3), (1);

-- Test 2: query (line 8)
VALUES (1), (1), (1), (2), (2) UNION ALL VALUES (1), (3), (1);

-- Test 3: query (line 20)
VALUES (1), (1), (1), (2), (2) INTERSECT VALUES (1), (3), (1);

-- Test 4: query (line 25)
VALUES (1), (1), (1), (2), (2) INTERSECT ALL VALUES (1), (3), (1);

-- Test 5: query (line 31)
VALUES (1), (1), (1), (2), (2) EXCEPT VALUES (1), (3), (1);

-- Test 6: query (line 36)
VALUES (1), (1), (1), (2), (2) EXCEPT ALL VALUES (1), (3), (1);

-- Test 7: query (line 43)
VALUES (1, 2), (1, 1), (1, 2), (2, 1), (2, 1) UNION VALUES (1, 3), (3, 4), (1, 1);

-- Test 8: query (line 53)
VALUES (1), (1), (1), (2), (2) UNION ALL VALUES (1), (3), (1) ORDER BY 1 DESC LIMIT 2;

-- Test 9: query (line 60)
VALUES (1) UNION ALL VALUES (NULL::INT) ORDER BY 1;

-- Test 10: query (line 66)
VALUES (NULL::INT) UNION ALL VALUES (1) ORDER BY 1;

-- Test 11: query (line 72)
VALUES (NULL) UNION ALL VALUES (NULL);

-- Test 12: query (line 78)
SELECT x, pg_typeof(y) FROM (SELECT 1, NULL UNION ALL SELECT 2, 4) AS t(x, y);

-- Test 13: query (line 84)
SELECT x, pg_typeof(y) FROM (SELECT 1, 3 UNION ALL SELECT 2, NULL) AS t(x, y);

-- Test 14: query (line 91)
VALUES (1) INTERSECT VALUES (NULL::INT) ORDER BY 1;

-- Test 15: query (line 95)
VALUES (NULL::INT) INTERSECT VALUES (1) ORDER BY 1;

-- Test 16: query (line 99)
VALUES (NULL) INTERSECT VALUES (NULL);

-- Test 17: query (line 105)
VALUES (1) EXCEPT VALUES (NULL::INT) ORDER BY 1;

-- Test 18: query (line 110)
VALUES (NULL::INT) EXCEPT VALUES (1) ORDER BY 1;

-- Test 19: query (line 115)
VALUES (NULL) EXCEPT VALUES (NULL);

-- Test 20: statement (line 119)
CREATE TABLE uniontest (
  k INT,
  v INT
);

-- Test 21: statement (line 125)
INSERT INTO uniontest VALUES
(1, 1),
(1, 1),
(1, 1),
(1, 2),
(1, 2),
(2, 1),
(2, 3),
(2, 1);

-- Test 22: query (line 136)
SELECT v FROM uniontest WHERE k = 1 UNION SELECT v FROM uniontest WHERE k = 2;

-- Test 23: query (line 143)
SELECT v FROM uniontest WHERE k = 1 UNION ALL SELECT v FROM uniontest WHERE k = 2;

-- Test 24: query (line 155)
SELECT v FROM uniontest WHERE k = 1 INTERSECT SELECT v FROM uniontest WHERE k = 2;

-- Test 25: query (line 160)
SELECT v FROM uniontest WHERE k = 1 INTERSECT ALL SELECT v FROM uniontest WHERE k = 2;

-- Test 26: query (line 166)
SELECT v FROM uniontest WHERE k = 1 EXCEPT SELECT v FROM uniontest WHERE k = 2;

-- Test 27: query (line 171)
SELECT v FROM uniontest WHERE k = 1 EXCEPT ALL SELECT v FROM uniontest WHERE k = 2;

-- Test 28: query (line 178)
(SELECT v FROM uniontest WHERE k = 1 UNION ALL SELECT v FROM uniontest WHERE k = 2) ORDER BY 1 DESC LIMIT 2;

-- Test 29: query (line 185)
SELECT v FROM uniontest WHERE k = 1 UNION ALL SELECT v FROM uniontest WHERE k = 2 ORDER BY 1 DESC LIMIT 2;

-- Test 30: query (line 191)
SELECT * FROM (SELECT * FROM (VALUES (1)) a LEFT JOIN (VALUES (1) UNION VALUES (2)) b on a.column1 = b.column1);

-- Test 31: query (line 196)
SELECT * FROM (VALUES (1)) a LEFT JOIN (VALUES (1) UNION VALUES (2)) b on a.column1 = b.column1;

-- Setup: tables used for tests 32-40.
CREATE TABLE a (a INT);
CREATE TABLE c (a INT, b INT);

-- Test 32: query (line 250)
select *,1 from (values(1,2) union all select 2,2 from c);

-- Test 33: statement (line 255)
INSERT INTO a VALUES (1);

-- Test 34: statement (line 258)
INSERT INTO c VALUES (1,2);

-- Test 35: statement (line 261)
INSERT INTO c VALUES (3,4);

-- Test 36: query (line 265)
SELECT a FROM (SELECT a FROM a UNION ALL SELECT a FROM c) ORDER BY a;

-- Test 37: query (line 272)
SELECT a FROM (SELECT a FROM a WHERE a > 3 AND a < 1 UNION ALL SELECT a FROM c) ORDER BY a;

-- Test 38: query (line 278)
SELECT a FROM (SELECT a FROM c UNION ALL SELECT a FROM a WHERE a > 3 AND a < 1) ORDER BY a;

-- Test 39: query (line 284)
SELECT a FROM (SELECT a FROM c UNION ALL SELECT a FROM a) WHERE a > 0 AND a < 3;

-- Test 40: query (line 290)
SELECT 1 FROM (SELECT a FROM a WHERE a > 3 UNION ALL SELECT a FROM c);

-- Test 41: statement (line 301)
CREATE TABLE tab41973 (rowid BIGINT);

-- Test 42: statement (line 304)
INSERT INTO tab41973 (rowid) VALUES (1), (2), (3);

-- Test 43: query (line 307)
SELECT 1 FROM ((SELECT * FROM tab41973) UNION ALL (SELECT * FROM tab41973));

-- Test 44: statement (line 317)
DROP TABLE IF EXISTS t1, t2;
CREATE TABLE t1 (j JSONB);
CREATE TABLE t2 (j JSONB);
INSERT INTO t1 VALUES ('{"a": "b"}'), ('{"foo": "bar"}'), (NULL);
INSERT INTO t2 VALUES ('{"c": "d"}'), ('{"foo": "bar"}'), (NULL);

-- Test 45: query (line 324)
(SELECT j FROM t1) UNION (SELECT j FROM t2);

-- Test 46: statement (line 332)
DROP TABLE IF EXISTS t1, t2;

-- Test 47: statement (line 335)
CREATE TABLE t1 (a INT[]);
CREATE TABLE t2 (b INT[]);
INSERT INTO t1 VALUES (ARRAY[1]), (ARRAY[2]), (NULL);
INSERT INTO t2 VALUES (ARRAY[2]), (ARRAY[3]), (NULL);

-- Test 48: query (line 341)
(SELECT a FROM t1) UNION (SELECT b FROM t2);

-- Test 49: statement (line 350)
CREATE TABLE ab (a INT, b INT);

-- Test 50: statement (line 353)
SELECT a, b, NULL::BIGINT AS rowid FROM ab UNION VALUES (1, 2, 3);

-- Test 51: statement (line 356)
DROP TABLE ab;

-- Test 52: statement (line 360)
CREATE TABLE ab (a INT4, b INT8);

-- Test 53: statement (line 363)
INSERT INTO ab VALUES (1, 1), (1, 2), (2, 1), (2, 2);

-- Test 54: query (line 366)
SELECT a FROM ab UNION SELECT b FROM ab;

-- Test 55: query (line 372)
SELECT b FROM ab UNION SELECT a FROM ab;

-- Test 56: statement (line 378)
DROP TABLE ab;

-- Test 57: statement (line 383)
CREATE TABLE t59611 (a INT);

-- Test 58: statement (line 386)
INSERT INTO t59611 VALUES (1);

-- Test 59: query (line 389)
WITH
  cte (cte_col)
    AS (
      SELECT
        *
      FROM
        (VALUES ((SELECT NULL::record FROM t59611 LIMIT 1)))
      UNION SELECT * FROM (VALUES ((1, 2)))
    )
SELECT
  NULL
FROM
  cte;

-- Test 60: statement (line 407)
CREATE TABLE t34524 (a INT PRIMARY KEY);

-- Test 61: query (line 411)
(SELECT NULL FROM t34524) EXCEPT (VALUES((SELECT 1 FROM t34524 LIMIT 1)), (1));

-- Test 62: statement (line 415)
CREATE TABLE ab (a INT PRIMARY KEY, b INT);
CREATE INDEX ON ab (b, a);
INSERT INTO ab VALUES
  (1, 1),
  (2, 2),
  (3, 1),
  (4, 2),
  (5, 1),
  (6, 2),
  (7, 3);

-- Test 63: statement (line 426)
CREATE TABLE xy (x INT PRIMARY KEY, y INT);
CREATE INDEX ON xy (y, x);
INSERT INTO xy VALUES
  (1, 1),
  (2, 3),
  (3, 2),
  (4, 3),
  (5, 1),
  (6, 3);

-- Test 64: query (line 438)
SELECT a, b FROM ab UNION SELECT x AS a, y AS b FROM xy ORDER BY b, a;

-- Test 65: query (line 453)
SELECT a FROM ab UNION ALL SELECT x AS a FROM xy ORDER BY a;

-- Test 66: query (line 470)
SELECT a, b FROM ab INTERSECT SELECT x AS a, y AS b FROM xy ORDER BY b, a;

-- Test 67: query (line 476)
SELECT b FROM ab INTERSECT ALL SELECT y AS b FROM xy ORDER BY b;

-- Test 68: query (line 484)
SELECT b, a FROM ab EXCEPT SELECT y AS b, x AS a FROM xy ORDER BY b, a;

-- Test 69: query (line 493)
SELECT b FROM ab EXCEPT ALL SELECT y AS b FROM xy ORDER BY b;

-- Test 70: query (line 502)
SELECT a, b FROM ab UNION SELECT x AS a, y AS b FROM xy;

-- Test 71: query (line 517)
SELECT a, b FROM ab INTERSECT SELECT x AS a, y AS b FROM xy;

-- Test 72: query (line 523)
SELECT b FROM ab INTERSECT ALL SELECT y AS b FROM xy;

-- Test 73: query (line 531)
SELECT b, a FROM ab EXCEPT SELECT y AS b, x AS a FROM xy;

-- Test 74: query (line 540)
SELECT b FROM ab EXCEPT ALL SELECT y AS b FROM xy;

-- Test 75: query (line 548)
SELECT a FROM ab UNION ALL SELECT x AS a FROM xy;

-- Test 76: statement (line 568)
-- CockroachDB-only table setting.
-- ALTER TABLE ab SET (schema_locked=false);

-- Test 77: statement (line 571)
TRUNCATE ab;

-- Test 78: statement (line 574)
-- CockroachDB-only table setting.
-- ALTER TABLE ab RESET (schema_locked);

-- Test 79: statement (line 577)
-- CockroachDB-only table setting.
-- ALTER TABLE xy SET (schema_locked=false);

-- Test 80: statement (line 580)
TRUNCATE xy;

-- Test 81: statement (line 583)
-- CockroachDB-only table setting.
-- ALTER TABLE xy RESET (schema_locked);

-- Test 82: statement (line 586)
INSERT INTO ab VALUES (1, 1), (2, 2), (3, 3), (4, 4), (5, 5);
INSERT INTO xy VALUES (1, 1), (3, 3), (5, 5), (7, 7);

-- Test 83: query (line 590)
SELECT a, b FROM ab UNION SELECT x, y FROM xy ORDER BY a;

-- Test 84: query (line 600)
SELECT a, b FROM ab UNION ALL SELECT x, y FROM xy ORDER BY a;

-- Test 85: query (line 613)
SELECT a, b FROM ab INTERSECT SELECT x, y FROM xy ORDER BY a;

-- Test 86: query (line 620)
SELECT a, b FROM ab INTERSECT ALL SELECT x, y FROM xy ORDER BY a;

-- Test 87: query (line 627)
SELECT a, b FROM ab EXCEPT SELECT x, y FROM xy ORDER BY a;

-- Test 88: query (line 633)
SELECT a, b FROM ab EXCEPT ALL SELECT x, y FROM xy ORDER BY a;

-- Test 89: query (line 641)
WITH q (x, y) AS (
  SELECT * FROM (VALUES ('a', 'a'), ('b', 'b'), ('c', 'c'))
  UNION ALL
  SELECT * FROM (VALUES ('d', 'd'))
)
SELECT 'e', y FROM q
ORDER BY x;

-- Test 90: statement (line 657)
CREATE TABLE abc (a INT, b INT, c INT);
CREATE INDEX ON abc (a, b, c DESC);
INSERT INTO abc VALUES (1, 1, 1), (1, 2, 2), (1, 2, 3), (2, 2, 2), (2, 2, 3);
CREATE TABLE def (d INT PRIMARY KEY, e INT, f INT);
INSERT INTO def VALUES (1, 1, 1), (2, 2, 2), (3, 2, 3), (4, 2, 2), (5, 2, 3);

-- Test 91: query (line 663)
SELECT d, e, f FROM def EXCEPT SELECT a, b, c FROM abc ORDER by d, e;

-- Test 92: statement (line 671)
CREATE TABLE abcd (a INT, b INT, c INT, d INT);
CREATE INDEX ON abcd (a, b, c DESC) INCLUDE (d);
INSERT INTO abcd VALUES (1, 1, 1, 1), (1, 2, 2, 2), (1, 2, 3, 3), (2, 2, 2, 2), (2, 2, 3, 3);
CREATE TABLE efgh (e INT PRIMARY KEY, f INT, g INT, h INT);
INSERT INTO efgh VALUES (1, 1, 1, 1), (2, 2, 2, 2), (3, 2, 3, 3), (4, 2, 2, 2), (5, 2, 3, 3);

-- Test 93: query (line 677)
SELECT e, f, g, h FROM efgh EXCEPT SELECT a, b, c, d FROM abcd ORDER by e, f;

-- Test 94: query (line 686)
SELECT a, b AS b1, b AS b2 FROM abc EXCEPT SELECT a, b, c FROM abc ORDER by a, b1;

-- Test 95: query (line 690)
SELECT a, b AS b1, b AS b2 FROM abc INTERSECT SELECT a, c, b FROM abc ORDER by a, b2;

-- Test 96: statement (line 699)
CREATE TABLE t127043_1 (k1 INT, v1 INT);
CREATE INDEX ON t127043_1 (k1);
INSERT INTO t127043_1 VALUES (1, 1);
CREATE TABLE t127043_2 (k2 INT, v2 INT);
CREATE INDEX ON t127043_2 (k2);
INSERT INTO t127043_2 VALUES (1, 1);
CREATE TABLE t127043_3 (k3 INT, v3 INT);
CREATE INDEX ON t127043_3 (k3);
INSERT INTO t127043_3 VALUES (1, 1);
CREATE VIEW v127043_3 (k, v) AS
  SELECT
    k1 AS k, v1 AS v FROM t127043_1
  UNION SELECT
    k2 AS k, v2 AS v FROM t127043_2
  UNION SELECT
    k3 AS k, v3 AS v FROM t127043_3;
CREATE VIEW v127043_3_idx (k, v) AS
  SELECT
    k1 AS k, v1 AS v FROM t127043_1
  UNION SELECT
    k2 AS k, v2 AS v FROM t127043_2
  UNION SELECT
    k3 AS k, v3 AS v FROM t127043_3;
CREATE VIEW v127043_2 (k, v) AS
  SELECT
    k1 AS k, v1 AS v FROM t127043_1
  UNION SELECT
    k2 AS k, v2 AS v FROM t127043_2;

-- Test 97: statement (line 726)
ANALYZE t127043_1;

-- Test 98: statement (line 729)
ANALYZE t127043_2;

-- Test 99: statement (line 732)
ANALYZE t127043_3;

-- Test 100: query (line 736)
SELECT k, v FROM v127043_3 WHERE k = 1 LIMIT 1;

-- Test 101: query (line 742)
SELECT k, v FROM v127043_3_idx WHERE k = 1 LIMIT 1;

-- Test 102: query (line 749)
SELECT k, v FROM v127043_2 INNER JOIN t127043_3 ON k = k3 WHERE k = 1 LIMIT 1;

-- Test 103: statement (line 755)
CREATE TABLE t130591_1 AS SELECT ALL;
CREATE TABLE t130591_2 AS SELECT ALL;

-- Test 104: statement (line 759)
TABLE t130591_1 EXCEPT DISTINCT TABLE t130591_1;

-- Test 105: statement (line 762)
TABLE t130591_1 INTERSECT DISTINCT TABLE t130591_1;

-- Test 106: statement (line 765)
TABLE t130591_1 UNION DISTINCT TABLE t130591_1;

-- Test 107: statement (line 768)
TABLE t130591_1 EXCEPT DISTINCT TABLE t130591_2;

-- Test 108: statement (line 771)
TABLE t130591_1 INTERSECT DISTINCT TABLE t130591_2;

-- Test 109: statement (line 774)
TABLE t130591_1 UNION DISTINCT TABLE t130591_2;

-- Test 110: statement (line 777)
INSERT INTO t130591_1 DEFAULT VALUES;

-- Test 111: query (line 780)
SELECT count(*) FROM t130591_1;

-- Test 112: statement (line 785)
TABLE t130591_1 EXCEPT DISTINCT TABLE t130591_1;

-- Test 113: statement (line 788)
TABLE t130591_1 INTERSECT DISTINCT TABLE t130591_1;

-- Test 114: statement (line 791)
TABLE t130591_1 UNION DISTINCT TABLE t130591_1;

-- Test 115: statement (line 794)
TABLE t130591_1 EXCEPT TABLE t130591_1;

-- Test 116: statement (line 797)
TABLE t130591_1 INTERSECT TABLE t130591_1;

-- Test 117: statement (line 800)
TABLE t130591_1 UNION TABLE t130591_1;
