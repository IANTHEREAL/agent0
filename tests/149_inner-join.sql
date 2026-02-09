-- PostgreSQL compatible tests from inner-join
-- 25 tests

DROP TABLE IF EXISTS abc;
DROP TABLE IF EXISTS def;

-- Test 1: statement (line 1)
CREATE TABLE abc (a INT, b INT, c INT, PRIMARY KEY (a, b));
INSERT INTO abc VALUES (1, 1, 2), (2, 1, 1), (2, 2, NULL);

-- Test 2: statement (line 5)
CREATE TABLE def (d INT, e INT, f INT, PRIMARY KEY (d, e));
INSERT INTO def VALUES (1, 1, 2), (2, 1, 0), (1, 2, NULL);

-- Test 3: query (line 9)
SELECT * from abc WHERE a IN (SELECT d FROM def) ORDER BY a, b, c;

-- Test 4: query (line 17)
SELECT * from abc WHERE a IN (SELECT f FROM def) ORDER BY a, b, c;

-- Test 5: query (line 23)
SELECT DISTINCT abc.* FROM abc INNER JOIN def ON abc.a = def.d AND abc.c = def.e ORDER BY a, b, c;

-- Test 6: query (line 30)
SELECT a, b, c FROM abc WHERE a IN (SELECT d FROM def UNION SELECT e FROM def) ORDER BY a, b, c;

-- Test 7: query (line 38)
SELECT c FROM abc WHERE a IN (SELECT d FROM def UNION SELECT e FROM def) ORDER BY c;

-- Test 8: query (line 46)
SELECT a, b, c FROM abc WHERE a NOT IN (SELECT d FROM def UNION SELECT e FROM def) ORDER BY a, b, c;

-- Test 9: query (line 51)
SELECT c FROM abc WHERE a NOT IN (SELECT d FROM def UNION SELECT e FROM def) ORDER BY c;

-- Test 10: statement (line 60)
-- ALTER TABLE abc SET (schema_locked=false)

-- Test 11: statement (line 63)
TRUNCATE TABLE abc;

-- Test 12: statement (line 66)
-- ALTER TABLE abc RESET (schema_locked)

-- Test 13: statement (line 69)
-- ALTER TABLE def SET (schema_locked=false)

-- Test 14: statement (line 72)
TRUNCATE TABLE def;

-- Test 15: statement (line 75)
-- ALTER TABLE def RESET (schema_locked)

-- Test 16: statement (line 78)
INSERT INTO abc VALUES (1, 1, 1);

-- Test 17: statement (line 81)
INSERT INTO def VALUES (1, 1, 1), (2, 1, 1);

-- Test 18: query (line 85)
SELECT a, b, c FROM abc WHERE a IN (SELECT d FROM def UNION SELECT e FROM def) ORDER BY a, b, c;

-- Test 19: query (line 91)
SELECT c FROM abc WHERE a IN (SELECT d FROM def UNION SELECT e FROM def) ORDER BY c;

-- Test 20: query (line 97)
SELECT a, b, c FROM abc WHERE a NOT IN (SELECT d FROM def UNION SELECT e FROM def) ORDER BY a, b, c;

-- Test 21: query (line 102)
SELECT c FROM abc WHERE a NOT IN (SELECT d FROM def UNION SELECT e FROM def) ORDER BY c;

-- Test 22: query (line 123)
SELECT a, b, c FROM abc, def WHERE a=d OR a=e ORDER BY a, b, c, d, e;

-- Test 23: query (line 130)
SELECT c FROM abc, def WHERE a=d OR a=e ORDER BY c, d, e;
