-- PostgreSQL compatible tests from collatedstring_uniqueindex1
-- 12 tests

-- CockroachDB's collated string tests rely on ICU collations.
-- Match CockroachDB's Danish collation behavior (lowercase sorts before uppercase).
CREATE COLLATION da (provider = icu, locale = 'da-u-kf-lower');

CREATE TABLE t (
  a TEXT COLLATE da,
  b INT,
  c BOOL,
  PRIMARY KEY (a, b)
);

-- Test 1: statement (line 14)
INSERT INTO t VALUES
  ('A' COLLATE da, 1, TRUE),
  ('A' COLLATE da, 2, NULL),
  ('a' COLLATE da, 2, FALSE),
  ('a' COLLATE da, 3, TRUE),
  ('B' COLLATE da, 3, NULL),
  ('b' COLLATE da, 4, FALSE),
  ('ü' COLLATE da, 6, TRUE),
  ('ü' COLLATE da, 5, NULL),
  ('x' COLLATE da, 5, FALSE);

-- Test 2: statement (line 26)
CREATE UNIQUE INDEX ON t (b, a);

-- Test 3: query (line 29)
SELECT a, b FROM t ORDER BY a, b;

-- Test 4: query (line 42)
SELECT b, a FROM t ORDER BY b, a;

-- Test 5: query (line 55)
SELECT COUNT (a) FROM t WHERE a = ('a' COLLATE da);

-- Test 6: query (line 60)
SELECT COUNT (a) FROM t WHERE a = ('y' COLLATE da);

-- Test 7: query (line 65)
SELECT COUNT (a) FROM t WHERE a > ('a' COLLATE da) AND a < ('c' COLLATE da);

-- Update and try again.
UPDATE t SET a = (a || a) COLLATE da;

-- Test 8: query (line 75)
SELECT a, b FROM t ORDER BY a, b;

-- Test 9: query (line 88)
SELECT b, a FROM t ORDER BY b, a;

-- Test 11: query (line 106)
-- Delete and try again.
DELETE FROM t WHERE a > ('a' COLLATE da) AND a < ('c' COLLATE da);

-- Test 12: query (line 117)
SELECT a, b FROM t ORDER BY a, b;

SELECT b, a FROM t ORDER BY b, a;
