-- PostgreSQL compatible tests from inner-join
-- 25 tests

-- Test 1: statement (line 1)
CREATE TABLE abc (a INT, b INT, c INT, PRIMARY KEY (a, b));
INSERT INTO abc VALUES (1, 1, 2), (2, 1, 1), (2, 2, NULL);

-- Test 2: statement (line 5)
CREATE TABLE def (d INT, e INT, f INT, PRIMARY KEY (d, e));
INSERT INTO def VALUES (1, 1, 2), (2, 1, 0), (1, 2, NULL);

-- Test 3: query (line 9)
SELECT * from abc WHERE EXISTS (SELECT * FROM def WHERE a=d);

-- Test 4: query (line 17)
SELECT * from abc WHERE EXISTS (SELECT * FROM def WHERE a=f);

-- Test 5: query (line 23)
SELECT * from abc WHERE EXISTS (SELECT * FROM def WHERE a=d AND c=e);

-- Test 6: query (line 30)
SELECT a, b, c FROM abc WHERE EXISTS (SELECT * FROM def WHERE a=d OR a=e);

-- Test 7: query (line 38)
SELECT c FROM abc WHERE EXISTS (SELECT * FROM def WHERE a=d OR a=e);

-- Test 8: query (line 46)
SELECT a, b, c FROM abc WHERE NOT EXISTS (SELECT * FROM def WHERE a=d OR a=e);

-- Test 9: query (line 51)
SELECT c FROM abc WHERE NOT EXISTS (SELECT * FROM def WHERE a=d OR a=e);

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
SELECT a, b, c FROM abc WHERE EXISTS (SELECT * FROM def WHERE a=d OR a=e);

-- Test 19: query (line 91)
SELECT c FROM abc WHERE EXISTS (SELECT * FROM def WHERE a=d OR a=e);

-- Test 20: query (line 97)
SELECT a, b, c FROM abc WHERE NOT EXISTS (SELECT * FROM def WHERE a=d OR a=e);

-- Test 21: query (line 102)
SELECT c FROM abc WHERE NOT EXISTS (SELECT * FROM def WHERE a=d OR a=e);

-- Test 22: query (line 123)
SELECT a, b, c FROM abc, def WHERE a=d OR a=e;

-- Test 23: query (line 130)
SELECT c FROM abc, def WHERE a=d OR a=e;

-- Test 24: statement (line 136)
CREATE TABLE abc_decimal (a DECIMAL, b DECIMAL, c DECIMAL);
INSERT INTO abc_decimal VALUES (1, 1, 1), (1, 1, 1), (1.0, 1.0, 1.0), (1.00, 1.00, 1.00);

-- Test 25: statement (line 140)
CREATE TABLE def_decimal (d DECIMAL, e DECIMAL, f DECIMAL);
INSERT INTO def_decimal VALUES (1, 1, 1), (1.0, 1.0, 1.0), (1.00, 1.00, 1.00);
