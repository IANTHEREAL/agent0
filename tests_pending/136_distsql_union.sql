-- PostgreSQL compatible tests from distsql_union
-- 47 tests

-- Test 1: statement (line 3)
CREATE TABLE xyz (
  x INT,
  y INT,
  z TEXT
);

-- Test 2: statement (line 10)
INSERT INTO xyz VALUES
  (NULL, NULL, NULL),
  (1, 1, NULL),
  (2, 1, 'a'),
  (3, 1, 'b'),
  (4, 2, 'b'),
  (5, 2, 'c')
;

-- Test 3: query (line 45)
SELECT x FROM xyz UNION ALL SELECT x FROM xyz ORDER BY x;

-- Test 4: query (line 61)
SELECT x FROM xyz UNION SELECT x FROM xyz ORDER BY x;

-- Test 5: query (line 72)
SELECT x FROM xyz WHERE x < 3 UNION SELECT x FROM xyz WHERE x >= 3 ORDER BY x;

-- Test 6: query (line 82)
SELECT x FROM xyz WHERE x <= 4 UNION SELECT x FROM xyz WHERE x > 1 ORDER BY x;

-- Test 7: query (line 92)
SELECT x, y FROM xyz UNION ALL SELECT y, x from xyz;

-- Test 8: query (line 109)
(SELECT x FROM xyz ORDER BY y) UNION ALL (SELECT x FROM xyz ORDER BY z) ORDER BY x;

-- Test 9: query (line 125)
(SELECT x FROM xyz ORDER BY y) UNION (SELECT x FROM xyz ORDER BY z) ORDER BY x;

-- Test 10: query (line 136)
(SELECT x FROM xyz ORDER BY y) UNION ALL (SELECT x FROM xyz ORDER BY y, z) ORDER BY x;

-- Test 11: query (line 152)
(VALUES (1), (2)) UNION (VALUES (2), (3));

-- Test 12: query (line 162)
(SELECT y FROM xyz) INTERSECT ALL (SELECT y FROM xyz) ORDER BY y;

-- Test 13: query (line 172)
(SELECT y FROM xyz) INTERSECT (SELECT y FROM xyz) ORDER BY y;

-- Test 14: query (line 180)
(SELECT y FROM xyz ORDER BY y) INTERSECT ALL (SELECT y FROM xyz ORDER BY y);

-- Test 15: query (line 190)
(SELECT y FROM xyz ORDER BY y) INTERSECT (SELECT y FROM xyz ORDER BY y);

-- Test 16: query (line 198)
(SELECT x FROM xyz WHERE x < 2) INTERSECT ALL (SELECT x FROM xyz WHERE x >= 2) ORDER BY x;

-- Test 17: query (line 203)
(SELECT x FROM xyz WHERE x < 2) INTERSECT (SELECT x FROM xyz WHERE x >= 2) ORDER BY x;

-- Test 18: query (line 209)
(SELECT y FROM xyz WHERE x < 3) INTERSECT ALL (SELECT y FROM xyz WHERE x >= 1) ORDER BY y;

-- Test 19: query (line 215)
(SELECT y FROM xyz WHERE x < 3) INTERSECT (SELECT y FROM xyz WHERE x >= 1) ORDER BY y;

-- Test 20: query (line 221)
SELECT x, y FROM xyz INTERSECT ALL SELECT y, x from xyz;

-- Test 21: query (line 227)
SELECT x, y FROM xyz INTERSECT SELECT y, x from xyz;

-- Test 22: query (line 234)
(SELECT x FROM xyz ORDER BY y) INTERSECT ALL (SELECT x FROM xyz ORDER BY z) ORDER BY x;

-- Test 23: query (line 244)
(SELECT x FROM xyz ORDER BY y) INTERSECT (SELECT x FROM xyz ORDER BY z) ORDER BY x;

-- Test 24: query (line 255)
(SELECT x FROM xyz ORDER BY y) INTERSECT ALL (SELECT x FROM xyz ORDER BY y, z) ORDER BY x;

-- Test 25: query (line 265)
(SELECT x FROM xyz ORDER BY y) INTERSECT (SELECT x FROM xyz ORDER BY y, z) ORDER BY x;

-- Test 26: query (line 276)
(SELECT y FROM xyz ORDER BY z) INTERSECT ALL (SELECT y FROM xyz ORDER BY z);

-- Test 27: query (line 286)
(SELECT y FROM xyz ORDER BY z) INTERSECT (SELECT y FROM xyz ORDER BY z);

-- Test 28: query (line 294)
SELECT x FROM ((SELECT x, y FROM xyz) INTERSECT ALL (SELECT x, y FROM xyz)) AS t(x, y);

-- Test 29: query (line 304)
SELECT x FROM ((SELECT x, y FROM xyz) INTERSECT (SELECT x, y FROM xyz)) AS t(x, y);

-- Test 30: query (line 317)
(SELECT y FROM xyz) EXCEPT ALL (SELECT x AS y FROM xyz) ORDER BY y;

-- Test 31: query (line 324)
(SELECT y FROM xyz) EXCEPT (SELECT x AS y FROM xyz) ORDER BY y;

-- Test 32: query (line 330)
(SELECT y FROM xyz ORDER BY y) EXCEPT ALL (SELECT y FROM xyz ORDER BY y);

-- Test 33: query (line 335)
(SELECT y FROM xyz ORDER BY y) EXCEPT (SELECT y FROM xyz ORDER BY y);

-- Test 34: query (line 341)
(SELECT x FROM xyz WHERE x < 2) EXCEPT ALL (SELECT x FROM xyz WHERE x >= 2) ORDER BY x;

-- Test 35: query (line 346)
(SELECT x FROM xyz WHERE x < 2) EXCEPT (SELECT x FROM xyz WHERE x >= 2) ORDER BY x;

-- Test 36: query (line 352)
(SELECT y FROM xyz WHERE x >= 1) EXCEPT ALL (SELECT y FROM xyz WHERE x < 3) ORDER BY y;

-- Test 37: query (line 359)
(SELECT y FROM xyz WHERE x >= 1) EXCEPT (SELECT y FROM xyz WHERE x < 3) ORDER BY y;

-- Test 38: query (line 365)
SELECT x, y FROM xyz EXCEPT ALL SELECT y, x from xyz;

-- Test 39: query (line 373)
SELECT x, y FROM xyz EXCEPT SELECT y, x from xyz;

-- Test 40: query (line 382)
(SELECT x FROM xyz ORDER BY y) EXCEPT ALL (SELECT y AS x FROM xyz ORDER BY z) ORDER BY x;

-- Test 41: query (line 389)
(SELECT x FROM xyz ORDER BY y) EXCEPT (SELECT y AS x FROM xyz ORDER BY z) ORDER BY x;

-- Test 42: query (line 397)
(SELECT x FROM xyz ORDER BY y) EXCEPT ALL (SELECT x FROM xyz ORDER BY y, z) ORDER BY x;

-- Test 43: query (line 401)
(SELECT x FROM xyz ORDER BY y) EXCEPT (SELECT x FROM xyz ORDER BY y, z) ORDER BY x;

-- Test 44: query (line 406)
(SELECT y FROM xyz ORDER BY z) EXCEPT ALL (SELECT y FROM xyz ORDER BY z);

-- Test 45: query (line 410)
(SELECT y FROM xyz ORDER BY z) EXCEPT (SELECT y FROM xyz ORDER BY z);

-- Test 46: query (line 415)
SELECT x FROM ((SELECT x, y FROM xyz) EXCEPT ALL (SELECT x, y FROM xyz)) AS t(x, y);

-- Test 47: query (line 419)
SELECT x FROM ((SELECT x, y FROM xyz) EXCEPT (SELECT x, y FROM xyz)) AS t(x, y);
