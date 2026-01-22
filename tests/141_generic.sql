-- PostgreSQL compatible tests from generic
-- 120 tests

SET client_min_messages = warning;
SET jit = off;

-- Test fixture setup (each file runs in an isolated temporary database, but keep it re-runnable).
DEALLOCATE ALL;
DROP TABLE IF EXISTS t159124;
DROP TABLE IF EXISTS t158945;
DROP TABLE IF EXISTS t155159;
DROP TABLE IF EXISTS t135151;
DROP TABLE IF EXISTS a;
DROP TABLE IF EXISTS t;
DROP TABLE IF EXISTS g;
DROP TABLE IF EXISTS c;

-- Test 1: statement (line 16)
CREATE TABLE c (
  k INT PRIMARY KEY,
  a INT
);
CREATE INDEX c_a_idx ON c (a);

-- Test 2: statement (line 23)
CREATE TABLE g (
  k INT PRIMARY KEY,
  a INT,
  b INT
);
CREATE INDEX g_a_b_idx ON g (a, b);

-- Table referenced by most PREPARE/EXECUTE statements below.
CREATE TABLE t (
  k INT PRIMARY KEY,
  a INT,
  b INT,
  c INT,
  s TEXT,
  t TIMESTAMPTZ
);
CREATE INDEX t_a_b_c_idx ON t (a, b, c);
CREATE INDEX t_a_c_idx ON t (a, c);
CREATE INDEX t_s_idx ON t (s);
CREATE INDEX t_c_idx ON t (c);

INSERT INTO t (k, a, b, c, s, t) VALUES
  (1, 1, 2, 3, 'foo', '2000-01-01 00:00:00+00'),
  (10, 10, 20, 30, 'foo10', '2000-01-01 00:00:00+00'),
  (33, 33, 44, 55, 'foo33', '2000-01-01 00:00:00+00'),
  (100, 100, 200, 300, 'foo100', '2000-01-01 00:00:00+00'),
  (1000, 1000, 2000, 3000, 'foo1000', '2000-01-01 00:00:00+00'),
  (10000, 10000, 20000, 30000, 'foo10000', '2000-01-01 00:00:00+00');

-- Keep c/g non-empty for JOIN tests.
INSERT INTO c (k, a) SELECT k, k FROM t;
INSERT INTO g (k, a, b) SELECT k, k, b FROM t;

-- Test 3: statement (line 31)
SET plan_cache_mode = force_generic_plan;

-- Test 4: statement (line 34)
PREPARE p AS SELECT * FROM t WHERE a = 1 AND b = 2 AND c = 3;

-- Test 5: query (line 39)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p;

-- Test 6: statement (line 44)
SET plan_cache_mode = force_custom_plan;

-- Test 7: query (line 49)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p;

-- Test 8: statement (line 54)
DEALLOCATE p;

-- Test 9: statement (line 58)
PREPARE p AS SELECT * FROM t WHERE a = 1 AND b = 2 AND c = 3;

-- Test 10: query (line 62)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p;

-- Test 11: statement (line 67)
SET plan_cache_mode = force_generic_plan;

-- Test 12: query (line 72)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p;

-- Test 13: statement (line 77)
DEALLOCATE p;

-- Test 14: statement (line 80)
PREPARE p AS SELECT * FROM t WHERE k = $1;

-- Test 15: query (line 85)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(33);

-- Test 16: statement (line 90)
SET plan_cache_mode = force_custom_plan;

-- Test 17: query (line 95)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(33);

-- Test 18: statement (line 100)
SET plan_cache_mode = force_generic_plan;

-- Test 19: statement (line 103)
DEALLOCATE p;

-- Test 20: statement (line 106)
PREPARE p AS SELECT * FROM t WHERE a = $1 AND c = $2;

-- Test 21: query (line 110)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1, 2);

-- Test 22: query (line 116)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(11, 22);

-- Test 23: statement (line 121)
SET plan_cache_mode = force_custom_plan;

-- Test 24: query (line 125)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1, 2);

-- Test 25: statement (line 130)
SET plan_cache_mode = force_generic_plan;

-- Test 26: statement (line 133)
DEALLOCATE p;

-- Test 27: statement (line 136)
PREPARE p AS SELECT * FROM t WHERE t = now();

-- Test 28: query (line 140)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p;

-- Test 29: query (line 146)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p;

-- Test 30: statement (line 151)
DEALLOCATE p;

-- Test 31: statement (line 154)
PREPARE p AS SELECT k FROM t WHERE s LIKE $1;

-- Test 32: query (line 158)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p('foo%');

-- Test 33: statement (line 163)
DEALLOCATE p;

-- Test 34: statement (line 166)
PREPARE p AS SELECT k FROM t WHERE c = $1;

-- Test 35: query (line 170)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1);

-- Test 36: statement (line 175)
ALTER TABLE t ADD COLUMN z INT;

-- Test 37: query (line 179)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1);

-- Test 38: statement (line 184)
DEALLOCATE p;

-- Test 39: statement (line 187)
ALTER TABLE t DROP COLUMN z;

-- Test 40: statement (line 190)
SET plan_cache_mode = auto;

-- Test 41: statement (line 193)
PREPARE p AS SELECT * FROM t WHERE a = 1 AND b = 2 AND c = 3;

-- Test 42: query (line 197)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p;

-- Test 43: statement (line 202)
DEALLOCATE p;

-- Test 44: statement (line 205)
PREPARE p AS SELECT * FROM t WHERE a = $1 AND c = $2;

-- Test 45: statement (line 208)
EXECUTE p(1, 2);
EXECUTE p(10, 20);
EXECUTE p(100, 200);
EXECUTE p(1000, 2000);
EXECUTE p(10000, 20000);

-- Test 46: query (line 218)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(10000, 20000);

-- Test 47: statement (line 223)
DEALLOCATE p;

-- Test 48: statement (line 227)
PREPARE p AS
SELECT t.k, c.k, g.k FROM t
JOIN c ON t.k = c.a
JOIN g ON c.k = g.a
WHERE t.a = $1 AND t.c = $2;

-- Test 49: statement (line 234)
EXECUTE p(1, 2);
EXECUTE p(10, 20);
EXECUTE p(100, 200);
EXECUTE p(1000, 2000);

-- Test 50: query (line 241)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(10000, 20000);

-- Test 51: query (line 249)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(10000, 20000);

-- Test 52: query (line 255)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(10000, 20000);

-- Test 53: statement (line 260)
ALTER TABLE t ADD COLUMN z INT;

-- Test 54: query (line 265)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1, 2);

-- Test 55: statement (line 270)
EXECUTE p(1, 2);
EXECUTE p(10, 20);
EXECUTE p(100, 200);
EXECUTE p(1000, 2000);

-- Test 56: query (line 278)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1, 2);

-- Test 57: query (line 284)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1, 2);

-- Test 58: statement (line 289)
DEALLOCATE p;

-- Test 59: statement (line 293)
PREPARE p AS
SELECT * FROM g WHERE a = $1 ORDER BY b LIMIT 10;

-- Test 60: statement (line 297)
EXECUTE p(1);
EXECUTE p(10);
EXECUTE p(100);
EXECUTE p(1000);
EXECUTE p(10000);

-- Test 61: query (line 306)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(10);

-- Test 62: statement (line 311)
SET plan_cache_mode = force_generic_plan;

-- Test 63: query (line 315)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(10);

-- Test 64: statement (line 320)
DEALLOCATE p;

-- Test 65: statement (line 323)
SET plan_cache_mode = auto;

-- Test 66: statement (line 326)
-- CockroachDB-only: ALTER TABLE ... INJECT STATISTICS.
-- ALTER TABLE g INJECT STATISTICS ...;
ANALYZE g;

-- Test 67: statement (line 337)
PREPARE p AS
UPDATE t SET b = 0 WHERE a IN ($1, $2) AND b > $3;

-- Test 68: statement (line 341)
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);

-- Test 69: query (line 350)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1, 10, 100);

-- Test 70: statement (line 355)
DEALLOCATE p;

-- Test 71: statement (line 358)
PREPARE p AS
SELECT * FROM g WHERE a IN ($1, $2) AND b > $3;

-- Test 72: statement (line 362)
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);

-- Test 73: query (line 371)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1, 10, 100);

-- Test 74: statement (line 376)
DEALLOCATE p;

-- Test 75: statement (line 379)
PREPARE p AS
SELECT * FROM g
UNION ALL
SELECT * FROM g WHERE a IN ($1, $2) AND b > $3;

-- Test 76: statement (line 385)
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);
EXECUTE p(1, 10, 100);

-- Test 77: query (line 394)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1, 10, 100);

-- Test 78: statement (line 400)
DEALLOCATE p;

-- Test 79: statement (line 404)
SET plan_cache_mode = auto;

-- Test 80: statement (line 407)
CREATE TABLE a (a INT PRIMARY KEY);

-- Test 81: statement (line 410)
-- CRDB: SELECT create_statement FROM [SHOW CREATE TABLE a]
-- PG: capture table OID and select via pg_class to stay stable across rename.
SELECT 'a'::regclass::oid AS a_oid \gset
PREPARE p AS
  SELECT oid::regclass::text AS create_statement
  FROM pg_class
  WHERE oid = :a_oid::oid;

-- Test 82: query (line 413)
EXECUTE p;

-- Test 83: statement (line 421)
ALTER TABLE a RENAME TO b;

-- Test 84: statement (line 424)
EXECUTE p;

-- Test 85: statement (line 427)
DEALLOCATE p;

-- Test 86: statement (line 432)
CREATE TABLE t135151 (
  k INT PRIMARY KEY,
  a INT,
  b INT
);
CREATE INDEX t135151_a_b_idx ON t135151 (a, b);

-- Test 87: statement (line 440)
SET plan_cache_mode = force_custom_plan;

-- Test 88: statement (line 443)
PREPARE p AS SELECT k FROM t135151 WHERE a = $1 AND b = $2;

-- Test 89: query (line 446)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1, 2);

-- Test 90: statement (line 451)
-- CockroachDB-only: ALTER TABLE ... INJECT STATISTICS.
-- ALTER TABLE t135151 INJECT STATISTICS ...;
ANALYZE t135151;

-- Test 91: query (line 469)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p(1, 2);

-- Test 92: statement (line 474)
DEALLOCATE p;

-- Test 93: statement (line 480)
CREATE TABLE t155159 (
  id INT PRIMARY KEY,
  a INT,
  b INT
);
CREATE INDEX t155159_a_b_idx ON t155159 (a, b);

-- Test 94: statement (line 488)
SET plan_cache_mode = auto;

-- Test 95: statement (line 491)
-- CockroachDB-only setting.
-- SET optimizer_prefer_bounded_cardinality = false;

-- Test 96: statement (line 494)
PREPARE p AS
SELECT id FROM t155159 WHERE a = $1 AND b >= $2 ORDER BY b, id LIMIT 250;

-- Test 97: statement (line 497)
EXECUTE p (33, 44);

-- Test 98: statement (line 500)
EXECUTE p (33, 44);

-- Test 99: statement (line 503)
EXECUTE p (33, 44);

-- Test 100: statement (line 506)
EXECUTE p (33, 44);

-- Test 101: statement (line 509)
EXECUTE p (33, 44);

-- Test 102: query (line 512)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p (33, 44);

-- Test 103: statement (line 533)
DEALLOCATE p;

-- Test 104: statement (line 536)
PREPARE p AS
SELECT * FROM (
  SELECT id FROM t155159 WHERE a = $1 AND b >= $2
  ORDER BY b, id
  LIMIT 250
)
UNION
SELECT * FROM (
  SELECT id FROM t155159 WHERE a = $3 AND b >= $4
);

-- Test 105: statement (line 548)
EXECUTE p (33, 44, 55, 66);

-- Test 106: statement (line 551)
EXECUTE p (33, 44, 55, 66);

-- Test 107: statement (line 554)
EXECUTE p (33, 44, 55, 66);

-- Test 108: statement (line 557)
EXECUTE p (33, 44, 55, 66);

-- Test 109: statement (line 560)
EXECUTE p (33, 44, 55, 66);

-- Test 110: query (line 563)
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p (33, 44, 55, 66);

-- Test 111: statement (line 601)
DEALLOCATE p;

-- Test 112: statement (line 606)
CREATE TABLE t158945 (
  k INT PRIMARY KEY,
  a INT,
  b INT,
  UNIQUE (a)
);
CREATE INDEX t158945_b_idx ON t158945 (b);

-- Test 113: statement (line 615)
INSERT INTO t158945 VALUES (1, NULL, NULL);

-- Test 114: statement (line 618)
PREPARE p158945a AS SELECT k FROM t158945 WHERE a = $1;

-- Test 115: query (line 621)
EXECUTE p158945a(NULL);

-- query T
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p158945a(NULL);

-- Test 116: statement (line 639)
PREPARE p158945b AS SELECT k FROM t158945 WHERE a = $1;

-- Test 117: query (line 642)
EXECUTE p158945b(NULL);

-- query T
EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF) EXECUTE p158945b(NULL);

-- Test 118: statement (line 668)
SET plan_cache_mode = force_generic_plan;

-- Test 119: statement (line 671)
CREATE TABLE t159124 (k INT PRIMARY KEY, s TEXT);
INSERT INTO t159124 VALUES (1, current_user);
PREPARE p159124 AS SELECT k FROM t159124 WHERE s = current_user;

-- Test 120: statement (line 674)
EXECUTE p159124;

RESET client_min_messages;
