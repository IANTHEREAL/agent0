-- PostgreSQL compatible tests from collatedstring_index2
-- 17 tests
-- Setup for PostgreSQL
CREATE COLLATION de (provider = icu, locale = 'de');
DROP TABLE IF EXISTS t CASCADE;
CREATE TABLE t (
  id SERIAL PRIMARY KEY,
  a TEXT COLLATE de,
  b INT,
  c BOOLEAN
);

-- Test 1: statement (line 14)
INSERT INTO t (a, b, c) VALUES
  ('A' COLLATE de, 1, TRUE),
  ('A' COLLATE de, 2, NULL),
  ('a' COLLATE de, 2, FALSE),
  ('a' COLLATE de, 3, TRUE),
  ('B' COLLATE de, 3, NULL),
  ('b' COLLATE de, 4, FALSE),
  ('ü' COLLATE de, 6, TRUE),
  ('ü' COLLATE de, 5, NULL),
  ('x' COLLATE de, 5, FALSE);

-- Test 2: query (line 26)
SELECT a, b FROM t ORDER BY a, b;

-- Test 3: query (line 39)
SELECT b, a FROM t ORDER BY b, a;

-- Test 4: query (line 52)
SELECT COUNT (a) FROM t WHERE a = ('a' COLLATE de);

-- Test 5: query (line 57)
SELECT COUNT (a) FROM t WHERE a = ('y' COLLATE de);

-- Test 6: query (line 62)
SELECT COUNT (a) FROM t WHERE a > ('a' COLLATE de) AND a < ('c' COLLATE de);

-- Test 7: statement (line 69)
CREATE INDEX ON t (a, b) INCLUDE (c);

-- Test 8: query (line 72)
SELECT a, b FROM t ORDER BY a, b;

-- Test 9: query (line 85)
SELECT b, a FROM t ORDER BY b, a;

-- Test 10: query (line 98)
SELECT COUNT (a) FROM t WHERE a = ('a' COLLATE de);

-- Test 11: query (line 103)
SELECT COUNT (a) FROM t WHERE a = ('y' COLLATE de);

-- Test 12: query (line 108)
SELECT COUNT (a) FROM t WHERE a > ('a' COLLATE de) AND a < ('c' COLLATE de);

-- Test 13: query (line 118)
SELECT a, b FROM t ORDER BY a, b;

-- Test 14: query (line 131)
SELECT b, a FROM t ORDER BY b, a;

-- Test 15: statement (line 146)
DELETE FROM t WHERE a > ('a' COLLATE de) AND a < ('c' COLLATE de);

-- Test 16: query (line 149)
SELECT a, b FROM t ORDER BY a, b;

-- Test 17: query (line 156)
SELECT b, a FROM t ORDER BY b, a;
