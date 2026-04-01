-- PostgreSQL compatible tests from collatedstring_index1
-- 17 tests
-- Setup for PostgreSQL (idempotent cleanup)
DROP TABLE IF EXISTS colltest_da CASCADE;
DROP COLLATION IF EXISTS da CASCADE;
CREATE COLLATION da (provider = icu, locale = 'da-u-kf-lower');
CREATE TABLE colltest_da (
  id SERIAL PRIMARY KEY,
  a TEXT COLLATE da,
  b INT,
  c BOOLEAN
);

-- Test 1: statement (line 14)
INSERT INTO colltest_da (a, b, c) VALUES
  ('A' COLLATE da, 1, TRUE),
  ('A' COLLATE da, 2, NULL),
  ('a' COLLATE da, 2, FALSE),
  ('a' COLLATE da, 3, TRUE),
  ('B' COLLATE da, 3, NULL),
  ('b' COLLATE da, 4, FALSE),
  ('ü' COLLATE da, 6, TRUE),
  ('ü' COLLATE da, 5, NULL),
  ('x' COLLATE da, 5, FALSE);

-- Test 2: query (line 26)
SELECT a, b FROM colltest_da ORDER BY a, b;

-- Test 3: query (line 39)
SELECT b, a FROM colltest_da ORDER BY b, a;

-- Test 4: query (line 52)
SELECT COUNT (a) FROM colltest_da WHERE a = ('a' COLLATE da);

-- Test 5: query (line 57)
SELECT COUNT (a) FROM colltest_da WHERE a = ('y' COLLATE da);

-- Test 6: query (line 62)
SELECT COUNT (a) FROM colltest_da WHERE a > ('a' COLLATE da) AND a < ('c' COLLATE da);

-- Test 7: statement (line 69)
CREATE INDEX ON colltest_da (b, a) INCLUDE (c);

-- Test 8: query (line 72)
SELECT a, b FROM colltest_da ORDER BY a, b;

-- Test 9: query (line 85)
SELECT b, a FROM colltest_da ORDER BY b, a;

-- Test 10: query (line 98)
SELECT COUNT (a) FROM colltest_da WHERE a = ('a' COLLATE da);

-- Test 11: query (line 103)
SELECT COUNT (a) FROM colltest_da WHERE a = ('y' COLLATE da);

-- Test 12: query (line 108)
SELECT COUNT (a) FROM colltest_da WHERE a > ('a' COLLATE da) AND a < ('c' COLLATE da);

-- Test 13: query (line 118)
SELECT a, b FROM colltest_da ORDER BY a, b;

-- Test 14: query (line 131)
SELECT b, a FROM colltest_da ORDER BY b, a;

-- Test 15: statement (line 146)
DELETE FROM colltest_da WHERE a > ('a' COLLATE da) AND a < ('c' COLLATE da);

-- Test 16: query (line 149)
SELECT a, b FROM colltest_da ORDER BY a, b;

-- Test 17: query (line 160)
SELECT b, a FROM colltest_da ORDER BY b, a;
