-- PostgreSQL compatible tests from citext
-- 51 tests

SET client_min_messages = warning;

CREATE EXTENSION IF NOT EXISTS citext;

DROP TABLE IF EXISTS t CASCADE;
DROP TABLE IF EXISTS r CASCADE;
DROP TABLE IF EXISTS s CASCADE;
DROP TABLE IF EXISTS composite_citext_tbl CASCADE;
DROP TABLE IF EXISTS citext_with_width_tbl CASCADE;
DROP TYPE IF EXISTS ctype CASCADE;

-- Test 1: query (line 3)
SELECT pg_typeof(CITEXT 'Foo');

-- Test 2: query (line 8)
SELECT pg_typeof('Foo'::CITEXT);

-- Test 3: query (line 13)
SELECT pg_typeof('Foo'::CITEXT::TEXT::CITEXT);

-- Test 4: query (line 18)
SELECT 'Foo'::CITEXT;

-- Test 5: statement (line 23)
CREATE TABLE t (
  c CITEXT
);

-- onlyif config schema-locked-disabled

-- Test 6: query (line 29)
SELECT a.attname AS column_name, format_type(a.atttypid, a.atttypmod) AS data_type
FROM pg_attribute a
JOIN pg_class c ON c.oid = a.attrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = current_schema()
  AND c.relname = 't'
  AND a.attnum > 0
  AND NOT a.attisdropped
ORDER BY a.attnum;

-- Test 7: statement (line 39)
INSERT INTO t VALUES ('test');

-- Test 8: query (line 42)
SELECT pg_typeof(c) FROM t LIMIT 1;

-- Test 9: query (line 47)
SELECT c FROM t WHERE c = 'tEsT';

-- Test 10: statement (line 52)
CREATE TABLE r (
  c CITEXT[]
);

-- onlyif config schema-locked-disabled

-- Test 11: query (line 58)
SELECT a.attname AS column_name, format_type(a.atttypid, a.atttypmod) AS data_type
FROM pg_attribute a
JOIN pg_class c ON c.oid = a.attrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = current_schema()
  AND c.relname = 'r'
  AND a.attnum > 0
  AND NOT a.attisdropped
ORDER BY a.attnum;

-- Test 12: statement (line 68)
INSERT INTO r VALUES (ARRAY['test', 'TESTER']);

-- Test 13: query (line 71)
SELECT pg_typeof(c) FROM r LIMIT 1;

-- Test 14: query (line 76)
SELECT c FROM r WHERE c = ARRAY['test', 'TESTER']::CITEXT[];

-- query T
SELECT c FROM r WHERE c = ARRAY['tEsT', 'tEsTeR']::CITEXT[];

-- Test 15: statement (line 86)
DROP TABLE IF EXISTS t;

-- Test 16: statement (line 89)
CREATE TABLE t (
  k INT PRIMARY KEY,
  c CITEXT
);

-- onlyif config schema-locked-disabled

-- Test 17: query (line 98)
SELECT a.attname AS column_name, format_type(a.atttypid, a.atttypmod) AS data_type
FROM pg_attribute a
JOIN pg_class c ON c.oid = a.attrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = current_schema()
  AND c.relname = 't'
  AND a.attnum > 0
  AND NOT a.attisdropped
ORDER BY a.attnum;

-- Test 18: statement (line 110)
INSERT INTO t VALUES (1, 'test');

-- Test 19: query (line 113)
SELECT pg_typeof(c) FROM t LIMIT 1;

-- Test 20: query (line 118)
SELECT c FROM t WHERE c = 'tEsT';

-- Test 21: statement (line 123)
DROP TABLE IF EXISTS r;

-- Test 22: statement (line 126)
CREATE TABLE r (
  k INT PRIMARY KEY,
  c CITEXT[]
);

-- onlyif config schema-locked-disabled

-- Test 23: query (line 135)
SELECT a.attname AS column_name, format_type(a.atttypid, a.atttypmod) AS data_type
FROM pg_attribute a
JOIN pg_class c ON c.oid = a.attrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = current_schema()
  AND c.relname = 'r'
  AND a.attnum > 0
  AND NOT a.attisdropped
ORDER BY a.attnum;

-- Test 24: statement (line 147)
INSERT INTO r VALUES (1, ARRAY['test', 'TESTER']);

-- Test 25: query (line 150)
SELECT pg_typeof(c) FROM r LIMIT 1;

-- Test 26: query (line 155)
SELECT c FROM r WHERE c = ARRAY['tEsT', 'tEsTeR']::CITEXT[];

-- Test 27: query (line 162)
-- Use the built-in "C" collation for portability (some systems may not have "en_US").
CREATE TABLE s (c CITEXT COLLATE "C");

-- query B
SELECT 'test'::CITEXT = 'TEST'::CITEXT;

-- Test 28: query (line 170)
SELECT 'test'::CITEXT = 'TEST';

-- Test 29: query (line 175)
SELECT 'test' = 'TEST'::CITEXT;

-- Test 30: query (line 180)
SELECT 'test'::CITEXT = 'TESTER';

-- Test 31: query (line 186)
SELECT e'\u047D'::CITEXT = e'\u047C'::CITEXT;

-- Test 32: query (line 192)
SELECT e'\u00E9'::CITEXT = e'\u00E8'::CITEXT;

-- Test 33: query (line 198)
SELECT e'\u00E9'::CITEXT = e'\u0065\u0301'::CITEXT;

-- Test 34: query (line 204)
SELECT e'\u00C9'::CITEXT = e'\u0065\u0301'::CITEXT;

-- Test 35: query (line 210)
SELECT e'\u0065\u0301'::CITEXT = e'\u0045\u0301'::CITEXT;

-- Test 36: query (line 216)
SELECT e'\u00E9'::CITEXT = e'\u00C9'::CITEXT;

-- Test 37: query (line 224)
SELECT 'I'::CITEXT = 'ı'::CITEXT;

-- Test 38: query (line 229)
SELECT 'ß'::CITEXT = 'SS'::CITEXT;

-- Test 39: query (line 234)
SELECT NULL::CITEXT = NULL::CITEXT IS NULL;

-- Test 40: query (line 239)
SELECT 'A'::CITEXT < 'a'::CITEXT;

-- Test 41: query (line 244)
SELECT 'a'::CITEXT < 'A'::CITEXT;

-- Test 42: query (line 249)
SELECT 'test'::CITEXT LIKE 'TEST';

-- query error unsupported comparison operator: <citext> ILIKE <citext>
SELECT 'test'::CITEXT ILIKE 'TEST'::CITEXT;

-- query B
SELECT ARRAY['test', 'TESTER']::CITEXT[] = ARRAY['tEsT', 'tEsTeR']::CITEXT[];

-- Test 43: query (line 260)
SELECT ARRAY['test', 'TESTER']::CITEXT[] = ARRAY['TESTING', 'TESTER']::CITEXT[];

-- Test 44: query (line 265)
SELECT ARRAY[]::CITEXT[] = ARRAY['TESTING', 'TESTER']::CITEXT[];

-- Test 45: statement (line 270)
CREATE TYPE ctype AS (id INT, c CITEXT);

-- Test 46: statement (line 273)
CREATE TABLE composite_citext_tbl (k INT PRIMARY KEY, a ctype);

-- Test 47: statement (line 276)
INSERT INTO composite_citext_tbl VALUES
  (1, ROW(1, 'TeSt')),
  (2, ROW(2, 'TESTER')),
  (3, ROW(3, 'tEsT'));

-- Test 48: query (line 279)
SELECT (a).c FROM composite_citext_tbl WHERE (a).c = 'test' ORDER BY (a).id;

-- Test 49: query (line 285)
SELECT pg_typeof((a).c) FROM composite_citext_tbl LIMIT 1;

-- Test 50: query (line 290)
-- PostgreSQL citext does not support typmods like CITEXT(10).
CREATE TABLE citext_with_width_tbl (a CITEXT);

-- query T
SELECT cast('test'::TEXT AS CITEXT);

-- Test 51: query (line 298)
SELECT pg_collation_for('foo'::CITEXT);

RESET client_min_messages;
