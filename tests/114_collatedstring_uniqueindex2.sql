-- PostgreSQL compatible tests from collatedstring_uniqueindex2
-- 12 tests

SET client_min_messages = warning;
DROP TABLE IF EXISTS t;
RESET client_min_messages;

CREATE TABLE t (
  a TEXT COLLATE "de-x-icu",
  b INT,
  c BOOLEAN
);

-- Test 1: statement (line 14)
INSERT INTO t VALUES
  ('A' COLLATE "de-x-icu", 1, TRUE),
  ('A' COLLATE "de-x-icu", 2, NULL),
  ('a' COLLATE "de-x-icu", 2, FALSE),
  ('a' COLLATE "de-x-icu", 3, TRUE),
  ('B' COLLATE "de-x-icu", 3, NULL),
  ('b' COLLATE "de-x-icu", 4, FALSE),
  ('ü' COLLATE "de-x-icu", 6, TRUE),
  ('ü' COLLATE "de-x-icu", 5, NULL),
  ('x' COLLATE "de-x-icu", 5, FALSE);

-- Test 2: statement (line 26)
CREATE UNIQUE INDEX ON t (a, b);

-- Test 3: query (line 29)
SELECT a, b FROM t ORDER BY a, b;

-- Test 4: query (line 42)
SELECT b, a FROM t ORDER BY b, a;

-- Test 5: query (line 55)
SELECT COUNT (a) FROM t WHERE a = ('a' COLLATE "de-x-icu");

-- Test 6: query (line 60)
SELECT COUNT (a) FROM t WHERE a = ('y' COLLATE "de-x-icu");

-- Test 7: query (line 65)
SELECT COUNT (a) FROM t WHERE a > ('a' COLLATE "de-x-icu") AND a < ('c' COLLATE "de-x-icu");

-- Test 8: query (line 75)
SELECT a, b FROM t ORDER BY a, b;

-- Test 9: query (line 88)
SELECT b, a FROM t ORDER BY b, a;

-- Test 10: statement (line 103)
DELETE FROM t WHERE a > ('a' COLLATE "de-x-icu") AND a < ('c' COLLATE "de-x-icu");

-- Test 11: query (line 106)
SELECT a, b FROM t ORDER BY a, b;

-- Test 12: query (line 113)
SELECT b, a FROM t ORDER BY b, a;
