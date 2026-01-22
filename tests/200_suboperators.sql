-- PostgreSQL compatible tests from suboperators
-- 94 tests

SET client_min_messages = warning;

-- Test 1: statement (line 1)
CREATE TABLE abc (a INT, b INT, C INT);

-- Test 2: statement (line 4)
INSERT INTO abc VALUES (1, 10, 100), (2, 20, 200), (3, 30, 300), (NULL, NULL, NULL);

-- Test 3: statement (line 7)
CREATE TYPE typ AS (x INT, y INT);

-- Test 4: statement (line 10)
CREATE TABLE comp (v typ);

-- Test 5: statement (line 13)
INSERT INTO comp VALUES (ROW(1, 2)), (ROW(3, 4));

-- Test 6: query (line 18)
SELECT 1 = ANY(ARRAY[1, 2]);

-- Test 7: query (line 23)
SELECT 1 = ANY (((ARRAY[1, 2])));

-- Test 8: query (line 28)
SELECT 1 = SOME(ARRAY[1, 2]);

-- Test 9: query (line 33)
SELECT 1 = ANY(ARRAY[3, 4]);

-- Test 10: query (line 38)
SELECT 1 = ANY (((ARRAY[3, 4])));

-- Test 11: query (line 43)
SELECT 1 < ANY(ARRAY[0, 5]);

-- Test 12: query (line 48)
SELECT 1 < ANY(ARRAY[0, 1]);

-- Test 13: query (line 53)
SELECT 1 = ANY(ARRAY[1.0, 1.1]);

-- Test 14: query (line 58)
SELECT 1 < ANY(ARRAY[1.0, 1.1]);

-- Test 15: query (line 63)
SELECT 1 = ANY(ARRAY[1, NULL]);

-- Test 16: query (line 68)
SELECT 1 = ANY(ARRAY[2, NULL]);

-- Test 17: query (line 73)
SELECT 1 = ANY(ARRAY[NULL, NULL]::int[]);

-- Test 18: query (line 78)
SELECT 1 = ANY(ARRAY[1,2] || 3);

-- Test 19: query (line 83)
SELECT 1 = ANY(ARRAY[2,3] || 1);

-- Test 20: query (line 88)
SELECT 1 = ANY(ARRAY[2,3] || 4);

-- Test 21: query (line 93)
SELECT * FROM abc WHERE a = ANY(ARRAY[1,3]) ORDER BY a;

-- Test 22: query (line 99)
SELECT * FROM abc WHERE a = ANY(ARRAY[4, 5]);

-- Test 23: query (line 103)
SELECT * FROM abc WHERE a = ANY(ARRAY[1, NULL]);

-- Test 24: query (line 108)
SELECT * FROM abc WHERE a = ANY(ARRAY[4, NULL]);

-- Test 25: query (line 112)
SELECT * FROM abc WHERE a = ANY(ARRAY[NULL, NULL]::int[]);

-- Test 26: query (line 135)
SELECT 1 = ANY(SELECT * FROM generate_series(2,4));

-- Test 27: query (line 140)
SELECT 1 < ANY(SELECT * FROM generate_series(1,3));

-- Test 28: query (line 145)
SELECT 1 < ANY(SELECT * FROM generate_series(0,1));

-- Test 29: query (line 150)
SELECT 1 = ANY(SELECT * FROM unnest(ARRAY[1.0, 1.1]));

-- Test 30: query (line 155)
SELECT 1 = ANY(SELECT * FROM unnest(ARRAY[1.0, 1.1]));

-- Test 31: query (line 160)
SELECT 1.0 < ANY(SELECT * FROM unnest(ARRAY[1.0, 1.1]));

-- Test 32: query (line 165)
SELECT 1.0 = ANY(SELECT * FROM unnest(ARRAY[1.0001, 2]));

-- Test 33: query (line 170)
SELECT 1 = ANY(SELECT * FROM unnest(ARRAY[1, NULL]));

-- Test 34: query (line 175)
SELECT 1 = ANY(SELECT * FROM unnest(ARRAY[2, NULL]));

-- Test 35: query (line 180)
SELECT 1 = ANY(SELECT * FROM unnest(ARRAY[NULL, NULL]::int[]));

-- Test 36: query (line 185)
SELECT * FROM abc WHERE a = ANY(SELECT a FROM abc WHERE b = 10);

-- Test 37: query (line 190)
SELECT * FROM abc WHERE a < ANY(SELECT a FROM abc WHERE b = 30) ORDER BY a;

-- Test 38: query (line 196)
SELECT * FROM abc WHERE a > ANY(SELECT a FROM abc WHERE b = 30);

-- Test 39: query (line 201)
SELECT * FROM abc WHERE a > ANY(SELECT a FROM abc WHERE b = 20) OR b IS NULL;

-- Test 40: query (line 208)
SELECT * FROM abc WHERE a >= ALL(SELECT a FROM abc WHERE a IS NOT NULL) OR b=10;

-- Test 41: query (line 215)
SELECT * FROM abc WHERE a > ANY(SELECT a FROM abc WHERE b = 20) IS NULL;

-- Test 42: query (line 220)
SELECT * FROM abc WHERE a = ANY(SELECT * FROM unnest(ARRAY[1, NULL]));

-- Test 43: query (line 225)
SELECT * FROM abc WHERE a = ANY(SELECT * FROM unnest(ARRAY[4, NULL]));

-- Test 44: query (line 229)
SELECT * FROM abc WHERE a = ANY(SELECT * FROM unnest(ARRAY[NULL, NULL]::int[]));

-- Test 45: query (line 233)
SELECT '1' = ANY(SELECT * FROM unnest(ARRAY['foo', 'bar']));

-- ALL with arrays.

-- query B
SELECT 1 = ALL(ARRAY[1, 1, 1.0]);

-- Test 46: query (line 243)
SELECT 1 = ALL(ARRAY[1, 1.001, 1.0]);

-- Test 47: query (line 248)
SELECT 5 > ALL(ARRAY[1, 2, 3]);

-- Test 48: query (line 253)
SELECT 5 > ALL(ARRAY[6, 7, 8]);

-- Test 49: query (line 258)
SELECT 5 > ALL(ARRAY[4, 6, 7]);

-- Test 50: query (line 263)
SELECT 1 = ALL(ARRAY[2, NULL]);

-- Test 51: query (line 268)
SELECT 1 = ALL(ARRAY[1, NULL]);

-- Test 52: query (line 273)
SELECT 1 = ALL(ARRAY[NULL, NULL]::int[]);

-- Test 53: query (line 278)
SELECT 5 > ALL(ARRAY[1, 2] || 3);

-- Test 54: query (line 283)
SELECT 5 > ALL(ARRAY[6, 7] || 8);

-- Test 55: query (line 288)
SELECT * FROM abc WHERE a > ALL(ARRAY[0, 1]) ORDER BY a;

-- Test 56: query (line 294)
SELECT * FROM abc WHERE a > ALL(ARRAY[1, 4]);

-- Test 57: query (line 298)
SELECT * FROM abc WHERE a > ALL(ARRAY[1, NULL]);

-- Test 58: query (line 302)
SELECT * FROM abc WHERE a > ALL(ARRAY[NULL, NULL]::int[]);

-- Test 59: query (line 319)
SELECT 1 = ALL(SELECT * FROM unnest(ARRAY[1,2,3]));

-- Test 60: query (line 324)
SELECT 1 < ALL(SELECT * FROM generate_series(2,5));

-- Test 61: query (line 329)
SELECT 1 < ALL(SELECT * FROM generate_series(1,3));

-- Test 62: query (line 334)
SELECT 1 = ALL(SELECT * FROM unnest(ARRAY[2, NULL]));

-- Test 63: query (line 339)
SELECT 1 = ALL(SELECT * FROM unnest(ARRAY[1, NULL]));

-- Test 64: query (line 344)
SELECT 1 = ALL(SELECT * FROM unnest(ARRAY[NULL, NULL]::int[]));

-- Test 65: query (line 349)
SELECT * FROM abc WHERE a < ALL(SELECT b FROM abc WHERE b IS NOT NULL) ORDER BY a;

-- Test 66: query (line 356)
SELECT * FROM abc WHERE a < ALL(SELECT a FROM abc WHERE a >= 2);

-- Test 67: query (line 361)
SELECT * FROM abc WHERE a < ALL(SELECT a FROM abc);

-- Test 68: query (line 365)
SELECT * FROM abc WHERE a > ALL(SELECT * FROM unnest(ARRAY[1, NULL]));

-- Test 69: query (line 369)
SELECT * FROM abc WHERE a > ALL(SELECT * FROM unnest(ARRAY[NULL, NULL]::int[]));

-- Test 70: query (line 373)
SELECT '1' = ALL(SELECT * FROM unnest(ARRAY['foo', 'bar']));

-- ANY/ALL with tuples.

-- query B
SELECT 1 = ANY (ARRAY[1, 2, 3]);

-- Test 71: query (line 383)
SELECT '1' = ANY (ARRAY['1', '2', '3.3', 'foo']);

-- query B
SELECT 1 = ANY (ARRAY[1, 2, 3]);

-- Test 72: query (line 391)
SELECT 1 = ANY (ARRAY[2, 3, 4]);

-- Test 73: query (line 396)
SELECT 1 = ANY (ARRAY[2, 3, 4]);

-- Test 74: query (line 401)
SELECT 1 = ANY (ARRAY[1::numeric, 1.1]);

-- Test 75: query (line 406)
SELECT 1::decimal = ANY (ARRAY[1::numeric, 1.1]);

-- Test 76: query (line 411)
SELECT 1 = ANY (ARRAY[1.0, 1.1]);

-- Test 77: query (line 416)
SELECT 1 = ANY (ARRAY[1.0, 1.1]);

-- Test 78: query (line 421)
SELECT 1::decimal = ANY (ARRAY[1.0, 1.1]);

-- Test 79: query (line 426)
SELECT 1::decimal = ANY (ARRAY[1.0, 1.1]);

-- Test 80: query (line 431)
SELECT '1' = ANY (ARRAY['1', 'hello', '3']);

-- query B
-- Cockroach supports ANY/ALL against a tuple-list; in PG use an empty array.
SELECT 1 = ANY (ARRAY[]::int[]);

-- Test 81: query (line 441)
SELECT NULL = ANY(ARRAY []::INTEGER[]);

-- Test 82: query (line 446)
SELECT NULL = SOME(ARRAY []::INTEGER[]);

-- Test 83: query (line 451)
SELECT NULL = ALL(ARRAY []::INTEGER[]);

-- Test 84: query (line 456)
SELECT NULL = ANY(ARRAY [1]::INTEGER[]);

-- Test 85: query (line 461)
SELECT NULL = SOME(ARRAY [1]::INTEGER[]);

-- Test 86: query (line 466)
SELECT NULL = ALL(ARRAY [1]::INTEGER[]);

-- Test 87: query (line 471)
SELECT NULL = ANY(NULL::INTEGER[]);

-- Test 88: query (line 476)
SELECT NULL = SOME(NULL::INTEGER[]);

-- Test 89: query (line 481)
SELECT NULL = ALL(NULL::INTEGER[]);

-- Test 90: query (line 487)
SELECT * FROM (VALUES (1, 2)) foo(a, b)
WHERE (a, b) = ANY(ARRAY(SELECT ROW(x, y) FROM (VALUES (1, 2)) bar(x, y)));

-- Test 91: query (line 493)
SELECT a, b FROM abc WHERE (a, b) = ANY(ARRAY(SELECT ROW(a, b) FROM abc LIMIT 1));

-- Test 92: statement (line 500)
-- CockroachDB-only setting.
-- SET vectorize = on;

-- Test 93: query (line 503)
SELECT * FROM comp WHERE v IN (SELECT v FROM comp);

-- Test 94: statement (line 509)
-- CockroachDB-only setting.
-- RESET vectorize;

RESET client_min_messages;
