-- PostgreSQL compatible tests from inverted_index_multi_column
-- 75 tests

SET client_min_messages = warning;
CREATE EXTENSION IF NOT EXISTS btree_gin;

-- Test 1: statement (line 6)
DROP TABLE IF EXISTS m_err;
CREATE TABLE m_err (k INT PRIMARY KEY, a INT, b INT, geom JSONB);
CREATE INDEX m_err_ab_gin ON m_err USING GIN (a, b);

-- Test 2: statement (line 10)
DROP TABLE IF EXISTS m_err;
CREATE TABLE m_err (k INT PRIMARY KEY, geom1 JSONB, geom JSONB);
CREATE INDEX m_err_geom1_geom_gin ON m_err USING GIN (geom1, geom);

-- Test 3: statement (line 14)
DROP TABLE IF EXISTS l;
CREATE TABLE l (k INT PRIMARY KEY, a INT, j JSONB);
CREATE INDEX l_a_j_gin ON l USING GIN (a, j);

-- Test 4: statement (line 17)
DROP TABLE IF EXISTS m_err;
CREATE TABLE m_err (k INT PRIMARY KEY, a INT, j JSONB);
CREATE INDEX m_err_a_j_gin ON m_err USING GIN (a, j);

-- Test 5: statement (line 20)
DROP TABLE IF EXISTS m;
CREATE TABLE m (k INT PRIMARY KEY, a INT, geom JSONB);
CREATE INDEX m_a_geom_gin ON m USING GIN (a, geom);

-- Test 6: statement (line 23)
DROP TABLE IF EXISTS n;
CREATE TABLE n (k INT PRIMARY KEY, a INT, geom JSONB);
CREATE INDEX n_a_geom_gin ON n USING GIN (a, geom);

-- Test 7: statement (line 27)
-- Postgres GIN indexes do not support per-column ASC/DESC semantics.
-- CREATE INDEX n_a_geom_gin_desc ON n USING GIN (a, geom);

-- Test 8: statement (line 30)
-- Postgres GIN indexes do not support per-column ASC/DESC semantics.
-- CREATE INDEX n_a_geom_gin_asc ON n USING GIN (a, geom);

-- statement
DROP TABLE IF EXISTS s;
CREATE TABLE s (
  k INT PRIMARY KEY,
  a INT,
  geom JSONB
);
CREATE INDEX s_a_geom_gin ON s USING GIN (a, geom);

-- onlyif config schema-locked-disabled

-- Test 9: query (line 45)
SELECT indexdef FROM pg_indexes WHERE schemaname = 'public' AND tablename = 's' ORDER BY indexname;

-- Test 10: query (line 60)
SELECT indexdef FROM pg_indexes WHERE schemaname = 'public' AND tablename = 's' ORDER BY indexname;

-- Test 11: statement (line 75)
DROP TABLE IF EXISTS drop_j;
CREATE TABLE drop_j (
  a INT,
  b INT,
  j JSONB
);
CREATE INDEX drop_j_a_j_gin ON drop_j USING GIN (a, j);
CREATE INDEX drop_j_b_a_j_gin ON drop_j USING GIN (b, a, j);
ALTER TABLE drop_j DROP COLUMN j;

-- onlyif config schema-locked-disabled

-- Test 12: query (line 87)
SELECT indexdef FROM pg_indexes WHERE schemaname = 'public' AND tablename = 'drop_j' ORDER BY indexname;

-- Test 13: query (line 99)
SELECT indexdef FROM pg_indexes WHERE schemaname = 'public' AND tablename = 'drop_j' ORDER BY indexname;

-- Test 14: statement (line 111)
DROP TABLE IF EXISTS drop_a;
CREATE TABLE drop_a (
  a INT,
  b INT,
  j JSONB
);
CREATE INDEX drop_a_a_j_gin ON drop_a USING GIN (a, j);
CREATE INDEX drop_a_b_a_j_gin ON drop_a USING GIN (b, a, j);
ALTER TABLE drop_a DROP COLUMN a;

-- onlyif config schema-locked-disabled

-- Test 15: query (line 123)
SELECT indexdef FROM pg_indexes WHERE schemaname = 'public' AND tablename = 'drop_a' ORDER BY indexname;

-- Test 16: query (line 135)
SELECT indexdef FROM pg_indexes WHERE schemaname = 'public' AND tablename = 'drop_a' ORDER BY indexname;

-- Test 17: statement (line 147)
DROP TABLE IF EXISTS dst;
DROP TABLE IF EXISTS src;
CREATE TABLE src (a INT, b INT, j JSONB);
CREATE INDEX src_a_j_gin ON src USING GIN (a, j);
CREATE INDEX src_a_b_j_gin ON src USING GIN (a, b, j);
CREATE TABLE dst (LIKE src INCLUDING INDEXES);

-- onlyif config schema-locked-disabled

-- Test 18: query (line 152)
SELECT indexdef FROM pg_indexes WHERE schemaname = 'public' AND tablename = 'dst' ORDER BY indexname;

-- Test 19: query (line 166)
SELECT indexdef FROM pg_indexes WHERE schemaname = 'public' AND tablename = 'dst' ORDER BY indexname;

DROP TABLE IF EXISTS t;
CREATE TABLE t (
  i INT PRIMARY KEY,
  s TEXT,
  j JSONB
);
CREATE INDEX t_idx_gin ON t USING GIN (i, s, j);

-- Test 20: statement (line 194)
INSERT INTO t VALUES
    (1, 'foo', '{"x": "y", "num": 1}'),
    (2, 'bar', '{"x": "y", "num": 2}'),
    (3, 'baz', '{"x": "y", "num": 3}');

-- Test 21: query (line 200)
SELECT * FROM t WHERE i IN (1, 2, 3) AND s = 'foo' AND j @> '{"x": "y"}' ORDER BY i;

-- Test 22: query (line 205)
SELECT * FROM t WHERE i = 1 AND s IN ('foo', 'bar') AND j @> '{"x": "y"}' ORDER BY i;

-- Test 23: query (line 210)
SELECT * FROM t WHERE i IN (1, 2, 3) AND s IN ('foo', 'bar') AND j @> '{"num": 1}' ORDER BY i;

-- Test 24: query (line 215)
SELECT * FROM t WHERE i IN (1, 2, 3) AND s IN ('foo', 'baz') AND j @> '{"x": "y"}' ORDER BY i;

-- Test 25: statement (line 222)
DELETE FROM t WHERE i = 3;

-- Test 26: query (line 225)
SELECT * FROM t WHERE i IN (1, 2, 3) AND s IN ('foo', 'baz') AND j @> '{"x": "y"}' ORDER BY i;

-- Test 27: statement (line 231)
UPDATE t SET j = '{"x": "y", "num": 10}' WHERE i = 1;

-- Test 28: query (line 234)
SELECT * FROM t WHERE i IN (1, 2, 3) AND s = 'foo' AND j @> '{"num": 10}' ORDER BY i;

-- Test 29: statement (line 240)
UPDATE t SET i = 10 WHERE i = 1;
UPDATE t SET s = 'bar' WHERE i = 2;

-- Test 30: query (line 244)
SELECT * FROM t WHERE i IN (2, 10) AND s IN ('foo', 'bar') AND j @> '{"x": "y"}' ORDER BY i;

-- Test 31: statement (line 251)
INSERT INTO t (i, s, j)
VALUES (3, 'bar', '{"x": "y", "num": 3}')
ON CONFLICT (i) DO UPDATE SET s = EXCLUDED.s, j = EXCLUDED.j;

-- Test 32: query (line 254)
SELECT * FROM t WHERE i IN (1, 2, 3) AND s = 'bar' AND j @> '{"x": "y"}' ORDER BY i;

-- Test 33: statement (line 261)
INSERT INTO t (i, s, j)
VALUES (3, 'bar', '{"x": "y", "num": 4}')
ON CONFLICT (i) DO UPDATE SET s = EXCLUDED.s, j = EXCLUDED.j;

-- Test 34: query (line 264)
SELECT * FROM t WHERE i IN (1, 2, 3) AND s = 'bar' AND j @> '{"num": 4}' ORDER BY i;

-- Test 35: statement (line 270)
SELECT * FROM t WHERE i = 1 AND j @> '{"num": 2}' ORDER BY i;

-- Test 36: statement (line 273)
SELECT * FROM t WHERE s = 'foo' AND j @> '{"num": 2}' ORDER BY i;

-- Test 37: statement (line 281)
DROP TABLE IF EXISTS backfill_a;
CREATE TABLE backfill_a (i INT PRIMARY KEY, s TEXT, j JSONB);
INSERT INTO backfill_a VALUES
    (1, 'foo', '[7]'),
    (2, 'bar', '[7, 0, 7]'),
    (3, 'baz', '{"a": "b"}'),
    (4, 'baz', '["a", "b"]');

-- Test 38: statement (line 288)
CREATE INDEX backfill_a_idx_gin ON backfill_a USING GIN (i, s, j);

-- Test 39: query (line 291)
SELECT * FROM backfill_a WHERE i = 1 AND s = 'foo' AND j @> '7'::JSONB ORDER BY i;

-- Test 40: query (line 296)
SELECT * FROM backfill_a WHERE i IN (1, 2, 3, 4) AND s IN ('foo', 'bar', 'baz') AND j @> '7'::JSONB ORDER BY i;

-- Test 41: query (line 302)
SELECT * FROM backfill_a WHERE i IN (3, 4) AND s = 'baz' AND j @> '{"a": "b"}'::JSONB ORDER BY i;

-- Test 42: statement (line 309)
-- CockroachDB-only setting.
-- SET autocommit_before_ddl = false;

-- Test 43: statement (line 312)
BEGIN;

-- Test 44: statement (line 318)
DROP TABLE IF EXISTS backfill_b;
CREATE TABLE backfill_b (i INT PRIMARY KEY, s TEXT, j JSONB);
INSERT INTO backfill_b VALUES
    (1, 'foo', '[7]'),
    (2, 'bar', '[7, 0, 7]'),
    (3, 'baz', '{"a": "b"}');

-- Test 45: statement (line 324)
CREATE INDEX backfill_b_idx_gin ON backfill_b USING GIN (i, s, j);

-- Test 46: statement (line 327)
COMMIT;

-- Test 47: statement (line 330)
-- CockroachDB-only setting.
-- RESET autocommit_before_ddl;

-- Test 48: query (line 333)
SELECT * FROM backfill_b WHERE i IN (1, 2, 3, 4) AND s = 'bar' AND j @> '7'::JSONB ORDER BY i;

-- Test 49: statement (line 340)
DROP TABLE IF EXISTS backfill_c;
CREATE TABLE backfill_c (i INT, s TEXT, j JSONB);

-- Test 50: statement (line 343)
INSERT INTO backfill_c (i, j) VALUES
    (1, '[7]'),
    (2, '[7, 0, 7]'),
    (3, '{"a": "b"}');

-- Test 51: statement (line 349)
BEGIN;

-- Test 52: statement (line 355)
CREATE INDEX backfill_c_idx_gin ON backfill_c USING GIN (i, s, j);

-- Test 53: statement (line 358)
COMMIT;

-- Test 54: query (line 361)
SELECT * FROM backfill_c WHERE i IN (1, 2, 3, 4) AND s IS NULL AND j @> '7'::JSONB ORDER BY i;

-- Test 55: statement (line 369)
CREATE TYPE enum AS ENUM ('foo', 'bar', 'baz');

-- Test 56: statement (line 372)
DROP TABLE IF EXISTS backfill_d;
CREATE TABLE backfill_d (i INT PRIMARY KEY, s enum, j JSONB);

-- Test 57: statement (line 375)
INSERT INTO backfill_d VALUES
    (1, 'foo', '[7]'),
    (2, 'bar', '[7, 0, 7]'),
    (3, 'baz', '{"a": "b"}');

-- Test 58: statement (line 381)
CREATE INDEX backfill_d_idx_gin ON backfill_d USING GIN (i, s, j);

-- Test 59: query (line 384)
SELECT * FROM backfill_d WHERE i IN (1, 2, 3, 4) AND s = 'bar' AND j @> '7'::JSONB ORDER BY i;

-- Test 60: statement (line 391)
DROP TABLE IF EXISTS d;
CREATE TABLE d (
  id INT PRIMARY KEY,
  foo JSONB,
  bar JSONB
);
CREATE INDEX d_idx_gin ON d USING GIN (foo, bar);

-- Test 61: statement (line 400)
INSERT into d VALUES
    (1, '"foo"', '[7]'),
    (2, '"bar"', '[7, 0, 7]'),
    (3, '"baz"', '{"a": "b"}'),
    (4, '"foo"', '[7, 8, 9, 10]'),
    (5, '"foo"', '[[0], [7, 8, 9, 10]]');

-- Test 62: query (line 408)
SELECT id, foo, bar FROM d WHERE foo = '"foo"' AND bar->0 = '7' ORDER BY id;

-- Test 63: query (line 414)
SELECT id, foo, bar FROM d WHERE foo = '"foo"' AND bar->1 = '0' ORDER BY id;

-- Test 64: query (line 418)
SELECT id, foo, bar FROM d WHERE foo = '"foo"' AND bar->1 = '8' ORDER BY id;

-- Test 65: query (line 423)
SELECT id, foo, bar FROM d WHERE foo = '"foo"' AND bar->0 @> '[0]' ORDER BY id;

-- Test 66: query (line 428)
SELECT id, foo, bar FROM d WHERE foo = '"foo"' AND bar->0 <@ '[0]' ORDER BY id;

-- Test 67: statement (line 434)
DELETE FROM d WHERE  id = 5;

-- Test 68: query (line 437)
SELECT id, foo, bar FROM d WHERE foo = '"foo"' AND bar->0 <@ '[0]' ORDER BY id;

-- Test 69: statement (line 442)
UPDATE d SET foo = '"updated"' WHERE id = 2;

-- Test 70: query (line 445)
SELECT id, foo, bar FROM d WHERE foo = '"updated"' AND bar->0 @> '7' ORDER BY id;

-- Test 71: statement (line 452)
DROP INDEX d_idx_gin;

-- Test 72: statement (line 455)
INSERT into d VALUES
    (6, '"backfilling"', '[[0], [1], 2, 3]'),
    (7, '"q"', '[[0], [1], [2], []]'),
    (8, '"backfilling"', '[[0], [1], [2], []]');

-- Test 73: statement (line 462)
CREATE INDEX d_idx_gin ON d USING GIN (foo, bar);

-- Test 74: query (line 465)
SELECT id, foo, bar FROM d WHERE foo = '"backfilling"' AND bar->2 @> '2' ORDER BY id;

-- Test 75: query (line 471)
SELECT id, foo, bar FROM d WHERE foo = '"foo"' AND bar->0 = '7' ORDER BY id;
