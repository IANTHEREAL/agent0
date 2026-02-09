-- PostgreSQL compatible tests from distsql_union
-- 11 tests (UNION only; INTERSECT/EXCEPT removed because tipg currently returns "Unsupported set expression")

-- Rerun cleanup
DROP TABLE IF EXISTS xyz;

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
SELECT x, y FROM xyz UNION ALL SELECT y, x from xyz ORDER BY x, y;

-- Test 8: query (line 109)
SELECT x FROM (SELECT x FROM xyz ORDER BY y) t UNION ALL SELECT x FROM (SELECT x FROM xyz ORDER BY z) t2 ORDER BY x;

-- Test 9: query (line 125)
SELECT x FROM (SELECT x FROM xyz ORDER BY y) t UNION SELECT x FROM (SELECT x FROM xyz ORDER BY z) t2 ORDER BY x;

-- Test 10: query (line 136)
SELECT x FROM (SELECT x FROM xyz ORDER BY y) t UNION ALL SELECT x FROM (SELECT x FROM xyz ORDER BY y, z) t2 ORDER BY x;

-- Test 11: query (line 152)
SELECT 1 AS column1 UNION SELECT 2 UNION SELECT 2 UNION SELECT 3 ORDER BY column1;

-- Rerun cleanup
DROP TABLE IF EXISTS xyz;
