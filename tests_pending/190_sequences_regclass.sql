-- PostgreSQL compatible tests from sequences_regclass
-- 117 tests

SET client_min_messages = warning;

-- Test 1: statement (line 4)
-- CockroachDB-only setting (PostgreSQL always uses sequences for SERIAL).
-- SET serial_normalization = 'sql_sequence';

-- Test 2: statement (line 7)
CREATE SEQUENCE test_seq;

-- Test 3: statement (line 10)
-- PostgreSQL has no cross-database object references; use schemas instead.
CREATE SCHEMA diff_db;

-- Test 4: statement (line 13)
CREATE SEQUENCE diff_db.test_seq;

-- let $test_seq_id
SELECT 'test_seq'::regclass::int AS test_seq_id \gset

-- Test 5: statement (line 19)
CREATE TABLE foo (i SERIAL PRIMARY KEY);

-- Test 6: statement (line 22)
ALTER TABLE foo
  ADD COLUMN j INT NOT NULL DEFAULT nextval('test_seq'::regclass);

-- Test 7: statement (line 25)
ALTER TABLE foo ADD COLUMN k SERIAL;

-- Test 8: statement (line 28)
ALTER TABLE foo ADD COLUMN l INT NOT NULL;

-- Test 9: statement (line 31)
ALTER TABLE foo ALTER COLUMN l SET DEFAULT currval('diff_db.test_seq');

-- Test 10: statement (line 34)
-- CockroachDB-only cluster setting.
-- SET CLUSTER SETTING sql.cross_db_sequence_references.enabled = TRUE;

-- Test 11: statement (line 37)
ALTER TABLE foo ALTER COLUMN l SET DEFAULT currval('diff_db.test_seq');

-- Test 12: statement (line 40)
SELECT nextval('diff_db.test_seq');

-- skipif config schema-locked-disabled

-- Test 13: query (line 44)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'foo'
ORDER BY ordinal_position;

-- Test 14: query (line 56)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'foo'
ORDER BY ordinal_position;

-- Test 15: statement (line 67)
INSERT INTO foo VALUES (default, default, default, default);

-- Test 16: statement (line 75)
-- CockroachDB-only setting.
-- SET serial_normalization = 'sql_sequence';

-- Test 17: statement (line 78)
CREATE SEQUENCE dep_seq;

-- Test 18: statement (line 81)
CREATE SEQUENCE dep_seq2;

-- let $dep_seq_id
SELECT 'dep_seq'::regclass::int AS dep_seq_id \gset

-- Test 19: statement (line 87)
CREATE TABLE seq_table (
  i SERIAL PRIMARY KEY,
  j INT NOT NULL DEFAULT nextval('dep_seq'::regclass),
  k INT NOT NULL DEFAULT nextval('dep_seq2')
);
ALTER SEQUENCE dep_seq OWNED BY seq_table.j;
ALTER SEQUENCE dep_seq2 OWNED BY seq_table.k;

-- Test 20: query (line 93)
SELECT
  pg_get_serial_sequence('seq_table', 'i'),
  pg_get_serial_sequence('seq_table', 'j'),
  pg_get_serial_sequence('seq_table', 'k');

-- Test 21: statement (line 98)
DROP SEQUENCE seq_table_i_seq CASCADE;

-- Test 22: statement (line 101)
DROP SEQUENCE dep_seq CASCADE;

-- Test 23: statement (line 104)
DROP SEQUENCE dep_seq2 CASCADE;

-- Test 24: statement (line 112)
-- CockroachDB-only setting.
-- SET serial_normalization = 'sql_sequence';

-- Test 25: statement (line 115)
CREATE SEQUENCE s1;

-- let $s1_id
SELECT 's1'::regclass::int AS s1_id \gset

-- Test 26: statement (line 121)
CREATE SEQUENCE s2;

-- Test 27: statement (line 124)
CREATE TABLE bar (
  i SERIAL PRIMARY KEY,
  j INT NOT NULL DEFAULT currval('s1'::regclass),
  k INT NOT NULL DEFAULT nextval('s2')
);

-- Test 28: statement (line 131)
SELECT nextval(:s1_id::regclass);

-- Test 29: statement (line 134)
INSERT INTO bar VALUES (default, default, default);

-- Test 30: statement (line 137)
ALTER SEQUENCE bar_i_seq RENAME TO new_bar_i_seq;

-- Test 31: statement (line 140)
ALTER SEQUENCE s1 RENAME TO new_s1;

-- Test 32: statement (line 143)
ALTER SEQUENCE s2 RENAME TO new_s2;

-- Test 33: query (line 148)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'bar'
ORDER BY ordinal_position;

-- Test 34: query (line 160)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'bar'
ORDER BY ordinal_position;

-- Test 35: statement (line 172)
INSERT INTO bar VALUES (default, default, default);

-- Test 36: query (line 175)
SELECT i, j, k FROM bar ORDER BY i, j, k;

-- Test 37: statement (line 186)
-- CockroachDB-only setting.
-- SET serial_normalization = 'sql_sequence';

-- Test 38: statement (line 189)
CREATE SCHEMA other_db;

-- Test 39: statement (line 192)
CREATE SEQUENCE other_db.s;

-- let $s_id
SELECT 'other_db.s'::regclass::int AS s_id \gset

-- Test 40: statement (line 198)
CREATE SEQUENCE other_db.s2;

-- Test 41: statement (line 201)
CREATE TABLE other_db.t (
  i INT NOT NULL DEFAULT nextval('other_db.s'::regclass),
  j INT NOT NULL DEFAULT currval('other_db.s2')
);

-- Test 42: statement (line 207)
SELECT nextval('other_db.s2');

-- Test 43: statement (line 210)
INSERT INTO other_db.t VALUES (default, default);

-- Test 44: statement (line 213)
ALTER SCHEMA other_db RENAME TO new_other_db;

-- Test 45: query (line 218)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'new_other_db'
  AND table_name = 't'
ORDER BY ordinal_position;

-- Test 46: query (line 230)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'new_other_db'
  AND table_name = 't'
ORDER BY ordinal_position;

-- Test 47: statement (line 242)
INSERT INTO new_other_db.t VALUES (default, default);

-- Test 48: query (line 245)
SELECT i, j FROM new_other_db.t ORDER BY i, j;

-- Test 49: statement (line 256)
-- CockroachDB-only setting.
-- SET serial_normalization = 'sql_sequence';

-- Test 50: statement (line 259)
CREATE SEQUENCE sc_s1;

-- let $sc_s1_id
SELECT 'sc_s1'::regclass::int AS sc_s1_id \gset

-- Test 51: statement (line 265)
CREATE SEQUENCE sc_s2;

-- Test 52: statement (line 268)
CREATE SCHEMA test_schema;

-- Test 53: statement (line 271)
CREATE TABLE tb (
  i SERIAL PRIMARY KEY,
  j INT NOT NULL DEFAULT nextval('sc_s1'::regclass),
  k INT NOT NULL DEFAULT currval('sc_s2')
);

-- Test 54: statement (line 278)
SELECT nextval('sc_s2');

-- Test 55: statement (line 281)
INSERT INTO tb VALUES (default, default, default);

-- Test 56: statement (line 284)
ALTER SEQUENCE tb_i_seq OWNED BY NONE;
ALTER SEQUENCE tb_i_seq SET SCHEMA test_schema;

-- Test 57: statement (line 287)
ALTER SEQUENCE sc_s1 SET SCHEMA test_schema;

-- Test 58: statement (line 290)
ALTER SEQUENCE sc_s2 SET SCHEMA test_schema;

-- Test 59: query (line 295)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'tb'
ORDER BY ordinal_position;

-- Test 60: query (line 307)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'tb'
ORDER BY ordinal_position;

-- Test 61: statement (line 319)
INSERT INTO tb VALUES (default, default, default);

-- Test 62: query (line 322)
SELECT i, j, k FROM tb ORDER BY i, j, k;

-- Test 63: statement (line 333)
SET search_path TO test_schema;

-- Test 64: statement (line 336)
CREATE SEQUENCE s3;

-- let $s3_id
SELECT 's3'::regclass::int AS s3_id \gset

-- Test 65: statement (line 342)
CREATE SEQUENCE s4;

-- Test 66: statement (line 345)
CREATE TABLE foo (
  i SERIAL PRIMARY KEY,
  j INT NOT NULL DEFAULT nextval('s3'::regclass),
  k INT NOT NULL DEFAULT currval('test_schema.s4')
);

-- Test 67: statement (line 352)
SELECT nextval('test_schema.s4');

-- Test 68: statement (line 355)
INSERT INTO foo VALUES (default, default, default);

-- Test 69: statement (line 358)
ALTER SCHEMA test_schema RENAME TO new_test_schema;

-- Test 70: query (line 363)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'new_test_schema'
  AND table_name = 'foo'
ORDER BY ordinal_position;

-- Test 71: query (line 375)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'new_test_schema'
  AND table_name = 'foo'
ORDER BY ordinal_position;

-- Test 72: statement (line 387)
INSERT INTO new_test_schema.foo VALUES (default, default, default);

-- Test 73: query (line 390)
SELECT i, j, k FROM new_test_schema.foo ORDER BY i, j, k;

-- Test 74: statement (line 396)
SET search_path TO public;

-- Test 75: statement (line 404)
-- CockroachDB-only setting.
-- SET serial_normalization = 'sql_sequence';

-- Test 76: statement (line 407)
CREATE SCHEMA other_db;

-- Test 77: statement (line 410)
CREATE TABLE other_db.t (s SERIAL PRIMARY KEY, i INT);

-- Test 78: statement (line 413)
INSERT INTO other_db.t (i) VALUES (1);

-- Test 79: statement (line 416)
SET search_path TO other_db;

-- Test 80: statement (line 419)
INSERT INTO t (i) VALUES (2);

-- Test 81: statement (line 422)
SET search_path TO public;

-- Test 82: statement (line 429)
CREATE TABLE t2 (i SERIAL PRIMARY KEY, j INT NOT NULL);
CREATE SEQUENCE s5 OWNED BY t2.j;
ALTER TABLE t2 ALTER COLUMN j SET DEFAULT nextval('s5');

-- Test 83: statement (line 438)
ALTER SEQUENCE s5 RENAME TO s5_new;

-- skipif config schema-locked-disabled

-- Test 84: query (line 442)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't2'
ORDER BY ordinal_position;

-- Test 85: query (line 454)
SELECT column_name, column_default
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 't2'
ORDER BY ordinal_position;

-- Test 86: query (line 465)
SELECT pg_get_serial_sequence('t2', 'i'), pg_get_serial_sequence('t2', 'j');

-- Test 87: statement (line 470)
INSERT INTO t2 VALUES (default, default);

-- Test 88: statement (line 478)
CREATE TABLE t3 (i INT NOT NULL, j INT NOT NULL);

-- Test 89: statement (line 481)
CREATE TABLE t4 (i INT NOT NULL, j INT NOT NULL);

-- Test 90: statement (line 484)
CREATE SEQUENCE view_seq;

-- let $view_seq_id
SELECT 'view_seq'::regclass::int AS view_seq_id \gset

-- Test 91: statement (line 491)
CREATE VIEW v AS (SELECT i, nextval('view_seq') FROM t3);

-- Test 92: query (line 494)
SELECT pg_get_viewdef('v'::regclass, true);

-- Test 93: statement (line 503)
CREATE VIEW v2 AS
SELECT currval
FROM (SELECT currval('view_seq'::regclass) AS currval FROM t3) AS sub;

-- Test 94: query (line 506)
SELECT pg_get_viewdef('v2'::regclass, true);

-- Test 95: statement (line 517)
CREATE VIEW v3 AS
SELECT nextval('view_seq'::regclass), i FROM t3
UNION
SELECT nextval('view_seq'), i FROM t4;

-- Test 96: query (line 520)
SELECT pg_get_viewdef('v3'::regclass, true);

-- Test 97: statement (line 529)
CREATE VIEW v4 AS
SELECT t3.i, nextval('view_seq')
FROM t3
INNER JOIN (SELECT j, currval('view_seq'::regclass) FROM t4) AS t5 ON t3.i = t5.j;

-- Test 98: query (line 533)
SELECT pg_get_viewdef('v4'::regclass, true);

-- Test 99: statement (line 547)
CREATE MATERIALIZED VIEW v5 AS SELECT currval('view_seq'), i FROM t3 WITH NO DATA;

-- Test 100: query (line 550)
SELECT pg_get_viewdef('v5'::regclass, true);

-- Test 101: statement (line 560)
CREATE VIEW v6 AS SELECT nextval('view_seq') AS i;

-- Test 102: statement (line 563)
CREATE OR REPLACE VIEW v6 AS SELECT currval('view_seq'::regclass) AS i;

-- Test 103: query (line 566)
SELECT pg_get_viewdef('v6'::regclass, true);

-- Test 104: statement (line 574)
CREATE VIEW v7 AS (SELECT i, (SELECT nextval('view_seq')) FROM t3);

-- Test 105: query (line 577)
SELECT pg_get_viewdef('v7'::regclass, true);

-- Test 106: statement (line 586)
CREATE VIEW v8 AS SELECT i FROM t3 WHERE i = nextval('view_seq'::regclass);

-- Test 107: query (line 589)
SELECT pg_get_viewdef('v8'::regclass, true);

-- Test 108: statement (line 597)
CREATE VIEW v9 AS (WITH w AS (SELECT nextval('view_seq')) SELECT nextval FROM w);

-- Test 109: query (line 600)
SELECT pg_get_viewdef('v9'::regclass, true);

-- Test 110: statement (line 608)
CREATE VIEW v10 AS (SELECT i FROM t3 LIMIT nextval('view_seq'));

-- Test 111: query (line 611)
SELECT pg_get_viewdef('v10'::regclass, true);

-- Test 112: statement (line 619)
ALTER SEQUENCE view_seq RENAME TO view_seq2;

-- Test 113: query (line 622)
SELECT pg_get_viewdef('v7'::regclass, true);

-- Test 114: statement (line 630)
INSERT INTO t3 VALUES (8, 2);

-- Test 115: statement (line 633)
INSERT INTO t4 VALUES (35, 6);

-- Test 116: query (line 636)
SELECT * FROM v;

-- Test 117: query (line 641)
SELECT * FROM v3 ORDER BY nextval;

RESET client_min_messages;
