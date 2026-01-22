-- PostgreSQL compatible tests from aggregate
-- 558 tests

SET client_min_messages = warning;

DROP TABLE IF EXISTS kv;
DROP TABLE IF EXISTS kv;
CREATE TABLE kv (
  k   INT PRIMARY KEY,
  v   INT,
  w   INT,
  s   TEXT,
  i   INTERVAL,
  arr INT[]
);

-- Test 1: query (line 16)
SELECT min(1), max(1), count(1), sum(1), avg(1), sum(1), stddev(1), stddev_samp(1), stddev_pop(1),
var_samp(1), variance(1), var_pop(1), bool_and(true), bool_and(false), bit_xor(get_byte('\x01'::bytea, 0)), bit_and(1), bit_or(1),
corr(1, 1), covar_pop(1, 1), covar_samp(1, 1), variance(1), regr_intercept(1, 1), regr_r2(1, 1), regr_slope(1, 1),
regr_sxx(1, 1), regr_sxy(1, 1), regr_syy(1, 1), regr_count(1, 1), regr_avgx(1, 1), regr_avgy(1, 1)
FROM kv;

-- Test 2: query (line 26)
SELECT min(NULL);

-- Test 3: query (line 32)
SELECT array_agg(1) FROM kv;

-- Test 4: query (line 37)
SELECT array_agg('{1}'::INT[]) FROM kv;

-- Test 5: query (line 42)
SELECT json_agg(1) FROM kv;

-- Test 6: query (line 47)
SELECT jsonb_agg(1) FROM kv;

-- Test 7: query (line 52)
SELECT min(i), avg(i), max(i), sum(i) FROM kv;

-- Test 8: query (line 57)
SELECT min(v), max(v), count(v), sum(1), avg(v), sum(v), stddev(v), stddev_pop(v), variance(v), var_pop(v), var_samp(v),
bool_and(v = 1), bool_and(v = 1), bit_xor(get_byte(s::bytea, 0)), corr(v,k), variance(v), covar_pop(v, k), covar_samp(v, k),
regr_intercept(v, k), regr_r2(v, k), regr_slope(v, k), regr_sxx(v, k), regr_sxy(v, k), regr_syy(v, k), regr_count(v, k),
regr_avgx(v, k), regr_avgy(v, k)
FROM kv;

-- Test 9: query (line 66)
SELECT array_agg(v) FROM kv;

-- Test 10: query (line 71)
SELECT array_agg(arr) FROM kv;

-- Test 11: query (line 76)
SELECT json_agg(v) FROM kv;

-- Test 12: query (line 81)
SELECT jsonb_agg(v) FROM kv;

-- Test 13: query (line 87)
SELECT min(1), count(1), max(1), sum(1), avg(1)::float, sum(1), stddev_samp(1), stddev_pop(1), variance(1),
var_pop(1), var_samp(1), bool_and(true), bool_or(true), to_hex(bit_xor(get_byte('\x01'::bytea, 0))), corr(1, 2), variance(1),
covar_pop(1, 2), covar_samp(1, 2), regr_intercept(1, 2), regr_r2(1, 2), regr_slope(1, 2), regr_sxx(1, 1), regr_sxy(1, 1),
regr_syy(1, 1), regr_count(1, 1), regr_avgx(1, 1), regr_avgy(1, 1);

-- Test 14: query (line 96)
SELECT array_agg(1);

-- Test 15: query (line 101)
SELECT array_agg('{1}'::INT[]);

-- Test 16: query (line 107)
SELECT array_agg(arr) FROM (SELECT ARRAY[1]::INT[] AS arr FROM generate_series(1, 3)) AS t;

-- Test 17: query (line 112)
SELECT json_agg(1);

-- Test 18: query (line 117)
SELECT jsonb_agg(1);

-- Test 19: query (line 124)
SELECT count(NULL);

-- Test 20: query (line 129)
SELECT json_agg(NULL::text);

-- Test 21: query (line 134)
SELECT jsonb_agg(NULL::text);

-- Test 22: statement (line 141)
SELECT array_agg(NULL::int);

-- Test 23: query (line 145)
SELECT array_agg(NULL::TEXT);

-- Test 24: query (line 152)
SELECT array_agg(NULL::TEXT) FROM (VALUES (1)) AS t(x);

-- Test 25: query (line 159)
SELECT COALESCE(max(1), 0) FROM generate_series(1,0);

-- Test 26: query (line 164)
SELECT count(*) FROM generate_series(1,100);

-- Test 27: query (line 170)
SELECT 1 + count(*) FROM generate_series(1,0);

-- Test 28: query (line 178)
SELECT count(*), COALESCE(max(k), 1) FROM kv;

-- Test 29: query (line 184)
SELECT (SELECT COALESCE(max(1), 0) FROM generate_series(1,0));

-- Test 30: statement (line 189)
INSERT INTO kv VALUES
(1, 2, 3, 'a', '1min', '{1, 2, NULL}'),
(3, 4, 5, 'a', '2sec', '{3, 4, 5}'),
(5, NULL, 5, NULL, NULL, NULL),
(6, 2, 3, 'b', '1ms', '{6, 2, 3}'),
(7, 2, 2, 'b', '4 days', '{7, 2, 2}'),
(8, 4, 2, 'A', '3 years', '{NULL, 4, 2}');

-- Test 31: query (line 200)
SELECT min(1), count(1), max(1), sum(1), avg(1)::float, sum(1), stddev(1), stddev_pop(1), variance(1)::float,
var_pop(1)::float, var_samp(1)::float, bool_and(true), bool_or(true), to_hex(bit_xor(get_byte('\x01'::bytea, 0))), variance(1)
FROM kv;

-- Test 32: query (line 208)
SELECT array_agg(1) FROM kv;

-- Test 33: query (line 213)
SELECT array_agg('{1, 2}'::INT[]) FROM kv;

-- Test 34: query (line 218)
SELECT json_agg(1) FROM kv;

-- Test 35: query (line 223)
SELECT jsonb_agg(1) FROM kv;

-- Test 36: query (line 229)
SELECT 1 FROM kv GROUP BY v;

-- Test 37: query (line 237)
SELECT 3 FROM kv HAVING TRUE;

-- Test 38: query (line 259)
SELECT count(*), k FROM kv GROUP BY 2;

-- Test 39: query (line 269)
-- SELECT * FROM kv GROUP BY v, count(DISTINCT w);

-- query error aggregate functions are not allowed in GROUP BY
-- SELECT count(DISTINCT w) FROM kv GROUP BY 1;

-- query error aggregate functions are not allowed in RETURNING
-- INSERT INTO kv (k, v) VALUES (99, 100) RETURNING sum(v);

-- query error column "v" does not exist
-- SELECT sum(v) FROM kv GROUP BY k LIMIT sum(v);

-- query error column "v" does not exist
-- SELECT sum(v) FROM kv GROUP BY k LIMIT 1 OFFSET sum(v);

-- query error aggregate functions are not allowed in VALUES
-- INSERT INTO kv (k, v) VALUES (99, count(1));

-- query error pgcode 42P10 GROUP BY position 5 is not in select list
-- SELECT count(*), k FROM kv GROUP BY 5;

-- query error pgcode 42P10 GROUP BY position 0 is not in select list
-- SELECT count(*), k FROM kv GROUP BY 0;

-- query error pgcode 42601 non-integer constant in GROUP BY
-- SELECT 1 GROUP BY 'a';

-- Qualifying a name in the SELECT, the GROUP BY, both or neither should not affect validation.
-- query IT rowsort
SELECT count(*), kv.s FROM kv GROUP BY s;

-- Test 40: query (line 305)
SELECT count(*), s FROM kv GROUP BY kv.s;

-- Test 41: query (line 313)
SELECT count(*), kv.s FROM kv GROUP BY kv.s;

-- Test 42: query (line 321)
SELECT count(*), s FROM kv GROUP BY s;

-- Test 43: query (line 330)
SELECT v, count(*), w FROM kv GROUP BY v, w;

-- Test 44: query (line 340)
SELECT v, count(*), w FROM kv GROUP BY 1, 3;

-- Test 45: query (line 350)
SELECT count(*), upper(s) FROM kv GROUP BY upper(s);

-- Test 46: query (line 358)
SELECT count(*) FROM kv GROUP BY 1+2;

-- Test 47: query (line 363)
SELECT count(*) FROM kv GROUP BY length('abc');

-- Test 48: query (line 369)
SELECT count(*), upper(s) FROM kv GROUP BY s;

-- Test 49: query (line 378)
SELECT count(*), upper(s) FROM kv GROUP BY upper(s);

-- Selecting and grouping on a more complex expression works.
-- query II rowsort
SELECT count(*), k+v FROM kv GROUP BY k+v;

-- Test 50: query (line 394)
SELECT count(*), k+v FROM kv GROUP BY k, v;

-- Test 51: query (line 404)
-- SELECT count(*), k+v FROM kv GROUP BY k;

-- Test 52: query (line 414)
-- SELECT count(*), k+v FROM kv GROUP BY v;

-- query error column "v" must appear in the GROUP BY clause or be used in an aggregate function
-- SELECT count(*), v/(k+v) FROM kv GROUP BY k+v;

-- query error aggregate functions are not allowed in WHERE
-- SELECT k FROM kv WHERE avg(k) > 1;

-- query error aggregate function calls cannot be nested
-- SELECT max(avg(k)) FROM kv;

-- Test case from #2761.
-- query II rowsort
SELECT count(kv.k) AS count_1, kv.v + kv.w AS lx FROM kv GROUP BY kv.v + kv.w;

-- Test 53: query (line 436)
SELECT s, count(*) FROM kv GROUP BY s HAVING count(*) > 1;

-- Test 54: query (line 442)
SELECT upper(s), count(DISTINCT s), count(DISTINCT upper(s)) FROM kv GROUP BY upper(s) HAVING count(DISTINCT s) > 1;

-- Test 55: query (line 447)
SELECT max(k), min(v) FROM kv HAVING min(v) > 2;

-- Test 56: query (line 451)
SELECT max(k), min(v) FROM kv HAVING max(v) > 2;

-- Test 57: query (line 456)
-- SELECT max(k), min(v) FROM kv HAVING max(min(v)) > 2;

-- query error argument of HAVING must be type bool, not type int
-- SELECT max(k), min(v) FROM kv HAVING k;

-- Expressions listed in the HAVING clause must conform to same validation as the SELECT clause (grouped or aggregated).
-- query error column "k" must appear in the GROUP BY clause or be used in an aggregate function
-- SELECT 3 FROM kv GROUP BY v HAVING k > 5;

-- Special case for grouping on primary key.
-- query I nosort
SELECT 3 FROM kv GROUP BY k HAVING max(v) > 2;

-- Test 58: query (line 473)
SELECT k FROM kv WHERE k > 7;

-- query error at or near ",": syntax error
-- SELECT count(*, 1) FROM kv;

-- query I
SELECT count(*);

-- Test 59: query (line 484)
SELECT count(k) FROM kv;

-- Test 60: query (line 489)
SELECT count(1);

-- Test 61: query (line 494)
SELECT count(1) FROM kv;

-- Test 62: query (line 499)
SELECT count(*) FROM kv;

-- query II
SELECT v, count(k) FROM kv GROUP BY v ORDER BY v;

-- Test 63: query (line 509)
SELECT v, count(k) FROM kv GROUP BY v ORDER BY v DESC;

-- Test 64: query (line 516)
SELECT v, count(k) FROM kv GROUP BY v ORDER BY count(k) DESC;

-- Test 65: query (line 523)
SELECT v, count(k) FROM kv GROUP BY v ORDER BY v-count(k);

-- Test 66: query (line 530)
SELECT v, count(k) FROM kv GROUP BY v ORDER BY 1 DESC;

-- Test 67: query (line 537)
SELECT count(*), count(k), count(kv.v) FROM kv;

-- Test 68: query (line 543)
SELECT count(kv.*) FROM kv;

-- Test 69: query (line 548)
SELECT count(DISTINCT k), count(DISTINCT v), count(DISTINCT (v)) FROM kv;

-- Test 70: query (line 553)
SELECT upper(s), count(DISTINCT k), count(DISTINCT v), count(DISTINCT (v)) FROM kv GROUP BY upper(s);

-- Test 71: query (line 561)
SELECT count((k, v)) FROM kv;

-- Test 72: query (line 566)
SELECT count(DISTINCT (k, v)) FROM kv;

-- Test 73: query (line 571)
SELECT count(DISTINCT (k, (v))) FROM kv;

-- Test 74: query (line 576)
SELECT count(*) FROM kv a, kv b;

-- Test 75: query (line 581)
SELECT count(DISTINCT a.*) FROM kv a, kv b;

-- Test 76: query (line 586)
SELECT count((k, v)) FROM kv LIMIT 1;

-- Test 77: query (line 591)
SELECT count((k, v)) FROM kv OFFSET 1;

-- Test 78: query (line 595)
SELECT count(k)+count(kv.v) FROM kv;

-- Test 79: query (line 600)
SELECT count(NULL::int), count((NULL, NULL));

-- Test 80: query (line 605)
SELECT min(k), max(k), min(v), max(v) FROM kv;

-- Test 81: query (line 611)
SELECT min(k), max(k), min(v), max(v) FROM kv WHERE k > 8;

-- Test 82: query (line 616)
SELECT array_agg(k), array_agg(s) FROM (SELECT k, s FROM kv ORDER BY k);

-- Test 83: query (line 621)
SELECT array_agg(k) || 1 FROM (SELECT k FROM kv ORDER BY k);

-- Test 84: query (line 626)
SELECT array_agg(arr) FROM (SELECT arr FROM kv WHERE arr IS NOT NULL ORDER BY k);

-- Test 85: query (line 631)
SELECT array_agg(s) FROM kv WHERE s IS NULL;

-- Test 86: query (line 636)
SELECT array_agg(arr) FROM kv WHERE arr IS NOT NULL;

-- Test 87: query (line 641)
SELECT array_agg(arr ORDER BY k), array_agg(ARRAY[NULL]::INT[]), array_agg('{NULL, NULL}'::INT[]) FROM kv WHERE arr IS NOT NULL;

-- Test 88: query (line 646)
SELECT json_agg(s) FROM kv WHERE s IS NULL;

-- Test 89: query (line 651)
SELECT jsonb_agg(s) FROM kv WHERE s IS NULL;

-- Test 90: query (line 656)
SELECT avg(k), avg(v), sum(k), sum(v) FROM kv;

-- Test 91: query (line 661)
SELECT min(i), avg(i), max(i), sum(i) FROM kv;

-- Test 92: query (line 666)
SELECT avg(k::decimal), avg(v::decimal), sum(k::decimal), sum(v::decimal) FROM kv;

-- Test 93: query (line 671)
SELECT avg(DISTINCT k), avg(DISTINCT v), sum(DISTINCT k), sum(DISTINCT v) FROM kv;

-- Test 94: query (line 676)
SELECT avg(k) * 2.0 + max(v)::DECIMAL FROM kv;

-- Test 95: query (line 683)
SELECT avg(k) * 2.0 + max(v)::DECIMAL FROM kv WHERE w*2 = k;

-- Test 96: query (line 690)
SELECT max(v) FROM kv GROUP BY k HAVING k=100;

-- Test 97: query (line 695)
SELECT max(v) FROM kv WHERE k=100;

-- Test 98: statement (line 700)
DROP TABLE IF EXISTS abc_tbl CASCADE;
DROP TABLE IF EXISTS abc_tbl;
CREATE TABLE abc_tbl (
  a VARCHAR PRIMARY KEY,
  b FLOAT,
  c BOOLEAN,
  d DECIMAL
);

-- Test 99: statement (line 708)
INSERT INTO abc_tbl VALUES ('one', 1.5, true, 5::decimal), ('two', 2.0, false, 1.1::decimal);

-- Test 100: query (line 711)
SELECT min(a), min(b), bool_and(c), min(d) FROM abc_tbl;

-- Test 101: query (line 716)
SELECT max(a), max(b), bool_or(c), max(d) FROM abc_tbl;

-- Test 102: query (line 721)
SELECT avg(b), sum(b), avg(d), sum(d) FROM abc_tbl;

-- Test 103: statement (line 727)
DROP TABLE IF EXISTS intervals;
CREATE TABLE intervals (
  a INTERVAL PRIMARY KEY
);

-- Test 104: statement (line 732)
INSERT INTO intervals VALUES (INTERVAL '1 year 2 months 3 days 4 seconds'), (INTERVAL '2 year 3 months 4 days 5 seconds'), (INTERVAL '10000ms');

-- Test 105: query (line 735)
SELECT sum(a) FROM intervals;

-- Test 106: query (line 741)
SELECT avg(length(a)) FROM abc_tbl;

-- query error unknown signature: avg\(bool\)
-- SELECT avg(c) FROM abc_tbl;

-- query error unknown signature: avg\(tuple{varchar, bool}\)
-- SELECT avg((a,c)) FROM abc_tbl;

-- query error unknown signature: sum\(varchar\)
-- SELECT sum(a) FROM abc_tbl;

-- query error unknown signature: sum\(bool\)
-- SELECT sum(c) FROM abc_tbl;

-- query error unknown signature: sum\(tuple{varchar, bool}\)
-- SELECT sum((a,c)) FROM abc_tbl;

-- statement ok
DROP TABLE IF EXISTS xyz;
CREATE TABLE xyz (
  x INT PRIMARY KEY,
  y INT,
  z FLOAT,
  w INT
);
DROP INDEX IF EXISTS xyz_xy_idx;
CREATE INDEX xyz_xy_idx ON xyz (x, y);
DROP INDEX IF EXISTS xyz_zyx_idx;
CREATE INDEX xyz_zyx_idx ON xyz (z, y, x);
DROP INDEX IF EXISTS xyz_w_idx;
CREATE INDEX xyz_w_idx ON xyz (w);

-- statement ok
INSERT INTO xyz VALUES (1, 2, 3.0, NULL), (4, 5, 6.0, 2), (7, NULL, 8.0, 3);

-- query I
SELECT min(x) FROM xyz;

-- Test 107: query (line 781)
SELECT min(y) FROM xyz;

-- Test 108: query (line 786)
SELECT min(w) FROM xyz;

-- Test 109: query (line 791)
SELECT min(x), max(z) FROM xyz;

-- Test 110: query (line 796)
SELECT min(x)+1, max(z)+1 FROM xyz;

-- Test 111: query (line 801)
SELECT min(x), max(z), sum(x) FROM xyz;

-- Test 112: query (line 806)
SELECT min(y), max(y) FROM xyz WHERE x IN (0, 4, 7);

-- Test 113: query (line 811)
SELECT min(x), max(x) FROM xyz WHERE x = 1;

-- Test 114: query (line 816)
SELECT min(z), max(y) FROM xyz WHERE z IN (3.0, 6.0, 8.0);

-- Test 115: query (line 821)
SELECT max(z), min(x) FROM (SELECT x,y,z FROM xyz a) dt WHERE dt.y > 0;

-- Test 116: query (line 826)
SELECT max(z), min(x) FROM xyz WHERE (z,x) = (SELECT max(z), min(x) FROM xyz);

-- Test 117: query (line 831)
SELECT max(z), min(x) FROM xyz HAVING (max(z), min(x)) = (SELECT max(z), min(x) FROM xyz);

-- Test 118: query (line 836)
SELECT min(x) FROM xyz WHERE x IN (0, 4, 7);

-- Test 119: query (line 841)
SELECT max(x) FROM xyz;

-- Test 120: query (line 846)
SELECT min(y) FROM xyz WHERE x = 1;

-- Test 121: query (line 851)
SELECT max(y) FROM xyz WHERE x = 1;

-- Test 122: query (line 856)
SELECT min(y) FROM xyz WHERE x = 7;

-- Test 123: query (line 861)
SELECT max(y) FROM xyz WHERE x = 7;

-- Test 124: query (line 866)
SELECT min(x) FROM xyz WHERE (y, z) = (2, 3.0);

-- Test 125: query (line 871)
SELECT max(x) FROM xyz WHERE (z, y) = (3.0, 2);

-- Test 126: query (line 878)
SELECT var_samp(x), variance(y::decimal), round(var_samp(z)::numeric, 14) FROM xyz;

-- Test 127: query (line 883)
SELECT variance(x) FROM xyz WHERE x = 10;

-- Test 128: query (line 888)
SELECT variance(x) FROM xyz WHERE x = 1;

-- Test 129: query (line 893)
SELECT var_pop(x), var_pop(y::decimal), round(var_pop(z)::numeric, 14) FROM xyz;

-- Test 130: query (line 898)
SELECT var_pop(x) FROM xyz WHERE x = 10;

-- Test 131: query (line 903)
SELECT var_pop(x) FROM xyz WHERE x = 1;

-- Test 132: query (line 908)
SELECT stddev_samp(x), stddev(y::decimal), round(stddev_samp(z)::numeric, 14) FROM xyz;

-- Test 133: query (line 913)
SELECT stddev(x) FROM xyz WHERE x = 1;

-- Test 134: query (line 918)
SELECT stddev_pop(x), stddev_pop(y::decimal), round(stddev_pop(z)::numeric, 14) FROM xyz;

-- Test 135: query (line 923)
SELECT stddev_pop(x) FROM xyz WHERE x = 1;

-- Test 136: query (line 929)
SELECT x > (SELECT avg(0)) FROM xyz LIMIT 1;

-- Test 137: statement (line 934)
DROP TABLE xyz;

-- Test 138: statement (line 939)
DROP TABLE IF EXISTS ifd;
DROP TABLE IF EXISTS ifd;
CREATE TABLE ifd
(
    i int,
    f float,
    d decimal
);
INSERT INTO ifd (i, f, d)
VALUES (1, 1.1, 1.1),
       (2, 2.2, 2.2),
       (5, 3.0, 3.0),
       (10, 7.8, 7.8),
       (11, 9.0, 9.0),
       (18, 11.2, 11.2);

-- Test 139: query (line 955)
SELECT variance(i), round(variance(f)::numeric, 12), variance(d)
FROM ifd;

-- Test 140: query (line 961)
SELECT variance(i), round(variance(f)::numeric, 2), variance(d)
FROM ifd
WHERE i < 10;

-- Test 141: statement (line 968)
DROP TABLE IF EXISTS sqrdiff;

-- Test 142: statement (line 976)
DROP TABLE IF EXISTS mnop;
CREATE TABLE mnop (
  m INT PRIMARY KEY,
  n FLOAT,
  o DECIMAL,
  p BIGINT
);

-- Test 143: statement (line 984)
INSERT INTO mnop (m, n) SELECT i, (1e9 + i/100.0)::float FROM
  generate_series(1, 100) AS i(i);

-- Test 144: statement (line 988)
UPDATE mnop SET o = n::decimal, p = (n * 10)::bigint;

-- Test 145: query (line 991)
SELECT round(variance(n)::numeric, 2), round(variance(o)::numeric, 2), round(variance(p)) FROM mnop;

-- Test 146: query (line 996)
SELECT round(var_pop(n)::numeric, 2), round(var_pop(o)::numeric, 2), round(var_pop(p)) FROM mnop;

-- Test 147: query (line 1001)
SELECT round(stddev_samp(n)::numeric, 2), round(stddev_samp(o)::numeric, 2), round(stddev_samp(p)) FROM mnop;

-- Test 148: query (line 1006)
SELECT round(stddev_pop(n)::numeric, 2), round(stddev_pop(o)::numeric, 2), round(stddev_pop(p)) FROM mnop;

-- Test 149: query (line 1011)
SELECT avg(1::int)::float, avg(2::float)::float, avg(3::decimal)::float;

-- Test 150: query (line 1016)
SELECT count(2::int), count(3::float), count(4::decimal);

-- Test 151: query (line 1021)
SELECT sum(1::int), sum(2::float), sum(3::decimal);

-- Test 152: query (line 1026)
SELECT variance(1::int), variance(1::float), variance(1::decimal);

-- Test 153: query (line 1031)
SELECT var_pop(1::int), var_pop(1::float), var_pop(1::decimal);

-- Test 154: query (line 1036)
SELECT stddev(1::int), stddev_samp(1::float), stddev(1::decimal);

-- Test 155: query (line 1041)
SELECT stddev_pop(1::int), stddev_pop(1::float), stddev_pop(1::decimal);

-- Test 156: statement (line 1046)
DROP TABLE IF EXISTS bits;
CREATE TABLE bits (b INT);

-- Test 157: query (line 1049)
SELECT bit_and(b), bit_or(b) FROM bits;

-- Test 158: statement (line 1054)
INSERT INTO bits VALUES (12), (25);

-- Test 159: query (line 1057)
SELECT bit_and(b), bit_or(b) FROM bits;

-- Test 160: statement (line 1062)
INSERT INTO bits VALUES(105);

-- Test 161: query (line 1065)
SELECT bit_and(b), bit_or(b) FROM bits;

-- Test 162: statement (line 1070)
INSERT INTO bits VALUES(NULL);

-- Test 163: query (line 1073)
SELECT bit_and(b), bit_or(b) FROM bits;

-- Test 164: statement (line 1078)
DROP TABLE IF EXISTS bools;
CREATE TABLE bools (b BOOL);

-- Test 165: query (line 1081)
SELECT bool_and(b), bool_or(b) FROM bools;

-- Test 166: statement (line 1086)
INSERT INTO bools VALUES (true), (true), (true);

-- Test 167: query (line 1089)
SELECT bool_and(b), bool_or(b) FROM bools;

-- Test 168: statement (line 1094)
INSERT INTO bools VALUES (false), (false);

-- Test 169: query (line 1097)
SELECT bool_and(b), bool_or(b) FROM bools;

-- Test 170: statement (line 1102)
DELETE FROM bools WHERE b;

-- Test 171: query (line 1105)
SELECT bool_and(b), bool_or(b) FROM bools;

-- Test 172: query (line 1110)
SELECT string_agg(s, '' ORDER BY k) FROM kv;

-- Test 173: query (line 1115)
SELECT json_agg(s) FROM (SELECT s FROM kv ORDER BY k);

-- Test 174: query (line 1120)
SELECT jsonb_agg(s) FROM (SELECT s FROM kv ORDER BY k);

-- Test 175: statement (line 1127)
DROP TABLE IF EXISTS filter_test;
CREATE TABLE filter_test (
  k INT,
  v INT,
  mark BOOL
);

-- Test 176: statement (line 1134)
INSERT INTO filter_test VALUES
(1, 2, false),
(3, 4, true),
(5, NULL, true),
(6, 2, true),
(7, 2, true),
(8, 4, true),
(NULL, 4, true);

-- Test 177: query (line 1145)
SELECT v, count(*) FILTER (WHERE k > 5) FROM filter_test GROUP BY v;

-- Test 178: query (line 1153)
SELECT v, mark, count(*) FILTER (WHERE k > 5), count(*), max(k) FILTER (WHERE k < 8) FROM filter_test GROUP BY v, mark;

-- Test 179: query (line 1161)
SELECT k, max(abs(k)) FILTER (WHERE k=1) FROM kv GROUP BY k;

-- query error at or near "filter": syntax error
-- SELECT k FILTER (WHERE k=1) FROM kv GROUP BY k;

-- query error aggregate functions are not allowed in FILTER
-- SELECT v, count(*) FILTER (WHERE count(*) > 5) FROM filter_test GROUP BY v;

-- Tests with * inside GROUP BY.
-- query I nosort
SELECT 1 FROM kv GROUP BY kv.*;

-- Test 180: query (line 1181)
SELECT sum(abc_tbl.d) FROM kv JOIN abc_tbl ON kv.k >= abc_tbl.d GROUP BY kv.*;

-- Test 181: statement (line 1191)
DROP TABLE IF EXISTS opt_test;
CREATE TABLE opt_test (k INT PRIMARY KEY, v INT);
DROP INDEX IF EXISTS opt_test_v_idx;
CREATE INDEX opt_test_v_idx ON opt_test (v);

-- Test 182: statement (line 1194)
INSERT INTO opt_test VALUES (1, NULL), (2, 10), (3, NULL), (4, 5);

-- Test 183: query (line 1199)
SELECT min(v) FROM opt_test;

-- Test 184: query (line 1205)
SELECT min(v) FROM opt_test;

-- Test 185: query (line 1211)
SELECT min(v) FROM opt_test WHERE k <> 4;

-- Test 186: query (line 1217)
SELECT min(v) FROM opt_test GROUP BY k;

-- Test 187: query (line 1225)
SELECT max(v) FROM opt_test GROUP BY k;

-- Test 188: statement (line 1233)
DROP TABLE IF EXISTS xor_bytes;
CREATE TABLE xor_bytes (a bytea, b int, c int);

-- Test 189: statement (line 1236)
INSERT INTO xor_bytes VALUES
  ('\x0101'::bytea, 1, 3),
  ('\x0201'::bytea, 1, 1),
  ('\x0401'::bytea, 2, -5),
  ('\x0801'::bytea, 2, -1),
  ('\x1001'::bytea, 2, 0);

-- Test 190: query (line 1244)
SELECT to_hex(bit_xor(get_byte(a, 0))), bit_xor(c) FROM xor_bytes;

-- Test 191: query (line 1249)
SELECT to_hex(bit_xor(get_byte(a, 0))), b, bit_xor(c) FROM xor_bytes GROUP BY b ORDER BY b;

-- Test 192: statement (line 1255)
SELECT bit_xor(get_byte(i, 0)) FROM (VALUES ('\x01'::bytea), ('\x0101'::bytea)) AS a(i);

-- Test 193: query (line 1258)
SELECT bool_or(true), bool_and(true);

-- Setup for ab/xy tests
DROP TABLE IF EXISTS ab;
DROP TABLE IF EXISTS xy;
DROP TABLE IF EXISTS ab;
CREATE TABLE ab (a INT, b INT);
DROP TABLE IF EXISTS xy;
CREATE TABLE xy (x INT, y INT);
INSERT INTO ab VALUES (1, 1), (2, 1), (3, 3);
INSERT INTO xy VALUES (1, 10), (2, 20), (3, 30);

-- Test 194: query (line 1277)
SELECT (b, a) FROM ab GROUP BY (b, a);

-- Test 195: query (line 1283)
SELECT min(y), (b, a)
 FROM ab, xy GROUP BY (x, (a, b));

-- Test 196: statement (line 1293)
DROP TABLE IF EXISTS group_ord;
CREATE TABLE group_ord (
  x INT PRIMARY KEY,
  y INT,
  z INT
);
DROP INDEX IF EXISTS group_ord_foo_idx;
CREATE INDEX group_ord_foo_idx ON group_ord (z);

-- Test 197: statement (line 1301)
INSERT INTO group_ord VALUES
(1, 2, 3),
(3, 4, 5),
(5, NULL, 5),
(6, 2, 3),
(7, 2, 2),
(8, 4, 2);

-- Test 198: query (line 1312)
SELECT x, max(y) FROM group_ord GROUP BY x;

-- Test 199: query (line 1324)
SELECT x, max(y) FROM group_ord GROUP BY x ORDER BY x;

-- Test 200: query (line 1336)
SELECT z, x, max(y) FROM group_ord GROUP BY x, z;

-- Test 201: query (line 1348)
SELECT z, x, max(y) FROM group_ord GROUP BY x, z ORDER BY x;

-- Test 202: query (line 1360)
SELECT z, max(y) FROM group_ord GROUP BY z;

-- Test 203: query (line 1370)
SELECT * FROM (SELECT x, max(y) FROM group_ord GROUP BY x) JOIN (SELECT z, min(y) FROM group_ord GROUP BY z) ON x = z;

-- Test 204: statement (line 1384)
DROP TABLE IF EXISTS index_tab;
DROP TABLE IF EXISTS index_tab;
CREATE TABLE index_tab (
  region TEXT,
  data INT
);
INSERT INTO index_tab
(VALUES
  ('US_WEST', 3),
  ('US_EAST', 23),
  ('US_EAST', -14),
  ('ASIA', 3294),
  ('ASIA', -3),
  ('US_WEST', 31),
  ('EUROPE', 123),
  ('US_EAST', -3000)
);

-- Test 205: query (line 1397)
SELECT max(data) FROM index_tab WHERE region = 'US_WEST' OR region = 'US_EAST';

-- Test 206: query (line 1402)
SELECT min(data) FROM index_tab WHERE region = 'US_WEST' OR region = 'US_EAST';

-- Test 207: statement (line 1407)
DROP TABLE index_tab;

-- Test 208: statement (line 1411)
-- SELECT final_variance(1.2, 1.2, 123) FROM kv;

-- Test 209: query (line 1415)
SELECT 1 FROM kv GROUP BY v, w::DECIMAL HAVING w::DECIMAL > 1;

-- Test 210: query (line 1425)
SELECT v, array_agg('a'::text) FROM kv GROUP BY v;

-- Test 211: query (line 1433)
SELECT 123 FROM kv ORDER BY max(v);

-- Test 212: statement (line 1440)
DROP TABLE IF EXISTS statistics_agg_test;
DROP TABLE IF EXISTS statistics_agg_test;
CREATE TABLE statistics_agg_test (
  y float,
  x float,
  int_y int,
  int_x int,
  dy decimal,
  dx decimal
);

-- Test 213: statement (line 1450)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,   10.0,    1,   10, 1.0,   10.0),
  (2.0,   25.0,    2,   25, 2.0,   25.0),
  (2.0,   25.0,    2,   25, 2.0,   25.0),
  (3.0,   40.0,    3,   40, 3.0,   40.0),
  (3.0,   40.0,    3,   40, 3.0,   40.0),
  (3.0,   40.0,    3,   40, 3.0,   40.0),
  (4.0,  100.0,    4,  100, 4.0,  100.0),
  (4.0,  100.0,    4,  100, 4.0,  100.0),
  (4.0,  100.0,    4,  100, 4.0,  100.0),
  (4.0,  100.0,    4,  100, 4.0,  100.0),
  (NULL,  NULL, NULL, NULL, NULL, NULL);

-- Test 214: query (line 1464)
SELECT corr(y, x), corr(int_y, int_x), corr(y, int_x), corr(int_y, x), corr(dy, dx)
FROM statistics_agg_test;

-- Test 215: query (line 1470)
SELECT corr(y, dx), corr(int_y, dx), corr(dy, int_x), corr(dy, x)
FROM statistics_agg_test;

-- Test 216: query (line 1476)
SELECT corr(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 217: query (line 1482)
SELECT CAST(corr(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 30) AS decimal)
FROM statistics_agg_test;

-- Test 218: statement (line 1500)
TRUNCATE statistics_agg_test;

-- Test 219: statement (line 1503)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, 10.0, 1, 10, 1.0, 10.0),
  (2.0, 20.0, 2, 20, 2.0, 20.0);

-- Test 220: query (line 1508)
SELECT corr(y, x), corr(int_y, int_x), corr(dy, dx)
FROM statistics_agg_test;

-- Test 221: statement (line 1514)
TRUNCATE statistics_agg_test;

-- Test 222: statement (line 1517)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 223: query (line 1522)
SELECT corr(y, x), corr(int_y, int_x), corr(dy, dx)
FROM statistics_agg_test;

-- Test 224: statement (line 1528)
TRUNCATE statistics_agg_test;

-- Test 225: statement (line 1531)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 226: query (line 1536)
SELECT corr(y, x), corr(int_y, int_x), corr(dy, dx)
FROM statistics_agg_test;

-- Test 227: statement (line 1542)
TRUNCATE statistics_agg_test;

-- Test 228: statement (line 1547)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx)
VALUES (0.0,   0.09561,    1,   10,   0.0, 0.09561),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (NULL,     NULL, NULL, NULL,  NULL,    NULL);

-- Test 229: query (line 1561)
SELECT covar_pop(y, x), covar_pop(int_y, int_x), covar_pop(y, int_x), covar_pop(int_y, x), round(covar_pop(dy, dx)::numeric, 7)
FROM statistics_agg_test;

-- Test 230: query (line 1567)
SELECT covar_pop(y, dx), covar_pop(int_y, dx), covar_pop(dy, int_x), covar_pop(dy, x)
FROM statistics_agg_test;

-- Test 231: query (line 1573)
SELECT covar_pop(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 232: query (line 1579)
SELECT CAST(covar_pop(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 100) AS decimal)
FROM statistics_agg_test;

-- Test 233: statement (line 1609)
TRUNCATE statistics_agg_test;

-- Test 234: statement (line 1612)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 235: query (line 1617)
SELECT covar_pop(y, x), covar_pop(int_y, int_x), covar_pop(dy, dx)
FROM statistics_agg_test;

-- Test 236: statement (line 1623)
TRUNCATE statistics_agg_test;

-- Test 237: statement (line 1626)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 238: query (line 1631)
SELECT covar_pop(y, x), covar_pop(int_y, int_x), covar_pop(dy, dx)
FROM statistics_agg_test;

-- Test 239: statement (line 1637)
TRUNCATE statistics_agg_test;

-- Test 240: statement (line 1642)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx)
VALUES (0.0,   0.09561,    1,   10,   0.0, 0.09561),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (NULL,     NULL, NULL, NULL,  NULL,    NULL);

-- Test 241: query (line 1656)
SELECT covar_samp(y, x), covar_samp(int_y, int_x), covar_samp(y, int_x), covar_samp(int_y, x), round(covar_samp(dy, dx)::numeric, 6)
FROM statistics_agg_test;

-- Test 242: query (line 1662)
SELECT covar_samp(y, dx), covar_samp(int_y, dx), covar_samp(dy, int_x), covar_samp(dy, x)
FROM statistics_agg_test;

-- Test 243: query (line 1668)
SELECT covar_samp(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 244: query (line 1674)
SELECT CAST(covar_samp(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 100) AS decimal)
FROM statistics_agg_test;

-- Test 245: statement (line 1704)
TRUNCATE statistics_agg_test;

-- Test 246: statement (line 1707)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 247: query (line 1712)
SELECT covar_samp(y, x), covar_samp(int_y, int_x), covar_samp(dy, dx)
FROM statistics_agg_test;

-- Test 248: statement (line 1718)
TRUNCATE statistics_agg_test;

-- Test 249: statement (line 1721)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 250: query (line 1726)
SELECT covar_samp(y, x), covar_samp(int_y, int_x), covar_samp(dy, dx)
FROM statistics_agg_test;

-- Test 251: statement (line 1732)
TRUNCATE statistics_agg_test;

-- Test 252: statement (line 1737)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx)
VALUES (0.0,   0.09561,    1,   10,   0.0, 0.09561),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (NULL,     NULL, NULL, NULL,  NULL,    NULL);

-- Test 253: query (line 1751)
SELECT regr_intercept(y, x), regr_intercept(int_y, int_x), regr_intercept(y, int_x), regr_intercept(int_y, x), regr_intercept(dy, dx)
FROM statistics_agg_test;

-- Test 254: query (line 1757)
SELECT regr_intercept(y, dx), regr_intercept(int_y, dx), regr_intercept(dy, int_x), regr_intercept(dy, x)
FROM statistics_agg_test;

-- Test 255: query (line 1763)
SELECT regr_intercept(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 256: query (line 1769)
SELECT CAST(regr_intercept(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 100) AS decimal)
FROM statistics_agg_test;

-- Test 257: statement (line 1787)
TRUNCATE statistics_agg_test;

-- Test 258: statement (line 1790)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, 10.0, 1, 10, 1.0, 10.0),
  (2.0, 20.0, 2, 20, 2.0, 20.0);

-- Test 259: query (line 1795)
SELECT regr_intercept(y, x), regr_intercept(int_y, int_x), regr_intercept(dy, dx)
FROM statistics_agg_test;

-- Test 260: statement (line 1801)
TRUNCATE statistics_agg_test;

-- Test 261: statement (line 1804)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 262: query (line 1809)
SELECT regr_intercept(y, x), regr_intercept(int_y, int_x), regr_intercept(dy, dx)
FROM statistics_agg_test;

-- Test 263: statement (line 1815)
TRUNCATE statistics_agg_test;

-- Test 264: statement (line 1818)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 265: query (line 1823)
SELECT regr_intercept(y, x), regr_intercept(int_y, int_x), regr_intercept(dy, dx)
FROM statistics_agg_test;

-- Test 266: statement (line 1829)
TRUNCATE statistics_agg_test;

-- Test 267: statement (line 1834)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx)
VALUES (0.0,   0.09561,    1,   10,   0.0, 0.09561),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (NULL,     NULL, NULL, NULL,  NULL,    NULL);

-- Test 268: query (line 1848)
SELECT regr_r2(y, x), regr_r2(int_y, int_x), regr_r2(y, int_x), round(regr_r2(int_y, x)::numeric, 15), regr_r2(dy, dx)
FROM statistics_agg_test;

-- Test 269: query (line 1854)
SELECT regr_r2(y, dx), round(regr_r2(int_y, x)::numeric, 15), regr_r2(dy, int_x), regr_r2(dy, x)
FROM statistics_agg_test;

-- Test 270: query (line 1860)
SELECT regr_r2(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 271: query (line 1866)
SELECT CAST(regr_r2(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 100) AS decimal)
FROM statistics_agg_test;

-- Test 272: statement (line 1884)
TRUNCATE statistics_agg_test;

-- Test 273: statement (line 1887)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, 10.0, 1, 10, 1.0, 10.0),
  (2.0, 20.0, 2, 20, 2.0, 20.0);

-- Test 274: query (line 1892)
SELECT regr_r2(y, x), regr_r2(int_y, int_x), regr_r2(dy, dx)
FROM statistics_agg_test;

-- Test 275: statement (line 1898)
TRUNCATE statistics_agg_test;

-- Test 276: statement (line 1901)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 277: query (line 1906)
SELECT regr_r2(y, x), regr_r2(int_y, int_x), regr_r2(dy, dx)
FROM statistics_agg_test;

-- Test 278: statement (line 1912)
TRUNCATE statistics_agg_test;

-- Test 279: statement (line 1915)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 280: query (line 1920)
SELECT regr_r2(y, x), regr_r2(int_y, int_x), regr_r2(dy, dx)
FROM statistics_agg_test;

-- Test 281: statement (line 1926)
TRUNCATE statistics_agg_test;

-- Test 282: statement (line 1931)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx)
VALUES (0.0,   0.09561,    1,   10,   0.0, 0.09561),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (NULL,     NULL, NULL, NULL,  NULL,    NULL);

-- Test 283: query (line 1945)
SELECT regr_slope(y, x), regr_slope(int_y, int_x), regr_slope(y, int_x), regr_slope(int_y, x), regr_slope(dy, dx)
FROM statistics_agg_test;

-- Test 284: query (line 1951)
SELECT regr_slope(y, dx), regr_slope(int_y, dx), regr_slope(dy, int_x), regr_slope(dy, x)
FROM statistics_agg_test;

-- Test 285: query (line 1957)
SELECT regr_slope(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 286: query (line 1963)
SELECT CAST(regr_slope(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 100) AS decimal)
FROM statistics_agg_test;

-- Test 287: statement (line 1981)
TRUNCATE statistics_agg_test;

-- Test 288: statement (line 1984)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, 10.0, 1, 10, 1.0, 10.0),
  (2.0, 20.0, 2, 20, 2.0, 20.0);

-- Test 289: query (line 1989)
SELECT regr_slope(y, x), regr_slope(int_y, int_x), regr_slope(dy, dx)
FROM statistics_agg_test;

-- Test 290: statement (line 1995)
TRUNCATE statistics_agg_test;

-- Test 291: statement (line 1998)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 292: query (line 2003)
SELECT regr_slope(y, x), regr_slope(int_y, int_x), regr_slope(dy, dx)
FROM statistics_agg_test;

-- Test 293: statement (line 2009)
TRUNCATE statistics_agg_test;

-- Test 294: statement (line 2012)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 295: query (line 2017)
SELECT regr_slope(y, x), regr_slope(int_y, int_x), regr_slope(dy, dx)
FROM statistics_agg_test;

-- Test 296: statement (line 2023)
TRUNCATE statistics_agg_test;

-- Test 297: statement (line 2028)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx)
VALUES (0.0,   0.09561,    1,   10,   0.0, 0.09561),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (NULL,     NULL, NULL, NULL,  NULL,    NULL);

-- Test 298: query (line 2042)
SELECT regr_sxx(y, x), regr_sxx(int_y, int_x), regr_sxx(y, int_x), regr_sxx(int_y, x), regr_sxx(dy, dx)
FROM statistics_agg_test;

-- Test 299: query (line 2048)
SELECT regr_sxx(y, dx), regr_sxx(int_y, dx), regr_sxx(dy, int_x), regr_sxx(dy, x)
FROM statistics_agg_test;

-- Test 300: query (line 2054)
SELECT regr_sxx(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 301: query (line 2060)
SELECT CAST(regr_sxx(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 100) AS decimal)
FROM statistics_agg_test;

-- Test 302: statement (line 2090)
TRUNCATE statistics_agg_test;

-- Test 303: statement (line 2093)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 304: query (line 2098)
SELECT regr_sxx(y, x), regr_sxx(int_y, int_x), regr_sxx(dy, dx)
FROM statistics_agg_test;

-- Test 305: statement (line 2104)
TRUNCATE statistics_agg_test;

-- Test 306: statement (line 2107)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 307: query (line 2112)
SELECT regr_sxx(y, x), regr_sxx(int_y, int_x), regr_sxx(dy, dx)
FROM statistics_agg_test;

-- Test 308: statement (line 2118)
TRUNCATE statistics_agg_test;

-- Test 309: statement (line 2123)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx)
VALUES (0.0,   0.09561,    1,   10,   0.0, 0.09561),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (NULL,     NULL, NULL, NULL,  NULL,    NULL);

-- Test 310: query (line 2137)
SELECT regr_sxy(y, x), regr_sxy(int_y, int_x), regr_sxy(y, int_x), regr_sxy(int_y, x), regr_sxy(dy, dx)
FROM statistics_agg_test;

-- Test 311: query (line 2143)
SELECT regr_sxy(y, dx), regr_sxy(int_y, dx), regr_sxy(dy, int_x), regr_sxy(dy, x)
FROM statistics_agg_test;

-- Test 312: query (line 2149)
SELECT regr_sxy(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 313: query (line 2155)
SELECT CAST(regr_sxy(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 100) AS decimal)
FROM statistics_agg_test;

-- Test 314: statement (line 2185)
TRUNCATE statistics_agg_test;

-- Test 315: statement (line 2188)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 316: query (line 2193)
SELECT regr_sxy(y, x), regr_sxy(int_y, int_x), regr_sxy(dy, dx)
FROM statistics_agg_test;

-- Test 317: statement (line 2199)
TRUNCATE statistics_agg_test;

-- Test 318: statement (line 2202)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 319: query (line 2207)
SELECT regr_sxy(y, x), regr_sxy(int_y, int_x), regr_sxy(dy, dx)
FROM statistics_agg_test;

-- Test 320: statement (line 2213)
TRUNCATE statistics_agg_test;

-- Test 321: statement (line 2218)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx)
VALUES (0.0,   0.09561,    1,   10,   0.0, 0.09561),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (NULL,     NULL, NULL, NULL,  NULL,    NULL);

-- Test 322: query (line 2232)
SELECT regr_syy(y, x), regr_syy(int_y, int_x), regr_syy(y, int_x), regr_syy(int_y, x), regr_syy(dy, dx)
FROM statistics_agg_test;

-- Test 323: query (line 2238)
SELECT regr_syy(y, dx), regr_syy(int_y, dx), regr_syy(dy, int_x), regr_syy(dy, x)
FROM statistics_agg_test;

-- Test 324: query (line 2244)
SELECT regr_syy(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 325: query (line 2250)
SELECT CAST(regr_syy(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 100) AS decimal)
FROM statistics_agg_test;

-- Test 326: statement (line 2280)
TRUNCATE statistics_agg_test;

-- Test 327: statement (line 2283)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 328: query (line 2288)
SELECT regr_syy(y, x), regr_syy(int_y, int_x), regr_syy(dy, dx)
FROM statistics_agg_test;

-- Test 329: statement (line 2294)
TRUNCATE statistics_agg_test;

-- Test 330: statement (line 2297)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 331: query (line 2302)
SELECT regr_syy(y, x), regr_syy(int_y, int_x), regr_syy(dy, dx)
FROM statistics_agg_test;

-- Test 332: statement (line 2308)
TRUNCATE statistics_agg_test;

-- Test 333: statement (line 2313)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx)
VALUES (0.0,   0.09561,    1,   10,   0.0, 0.09561),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (NULL,     NULL, NULL, NULL,  NULL,    NULL);

-- Test 334: query (line 2327)
SELECT regr_count(y, x), regr_count(int_y, int_x), regr_count(y, int_x), regr_count(int_y, x), regr_count(dy, dx)
FROM statistics_agg_test;

-- Test 335: query (line 2333)
SELECT regr_count(y, dx), regr_count(int_y, dx), regr_count(dy, int_x), regr_count(dy, x)
FROM statistics_agg_test;

-- Test 336: query (line 2339)
SELECT regr_count(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 337: query (line 2345)
SELECT regr_count(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 100)
FROM statistics_agg_test;

-- Test 338: statement (line 2363)
TRUNCATE statistics_agg_test;

-- Test 339: query (line 2366)
SELECT regr_count(y, x) FROM statistics_agg_test;

-- Test 340: statement (line 2371)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (NULL, NULL, NULL, NULL, NULL, NULL),
  (NULL, NULL, NULL, NULL, NULL, NULL);

-- Test 341: query (line 2376)
SELECT regr_count(y, x)
FROM statistics_agg_test;

-- Test 342: statement (line 2382)
TRUNCATE statistics_agg_test;

-- Test 343: statement (line 2387)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,   10.0,    1,   10, 1.0,   10.0),
  (2.0,   25.0,    2,   25, 2.0,   25.0),
  (2.0,   25.0,    2,   25, 2.0,   25.0),
  (3.0,   40.0,    3,   40, 3.0,   40.0),
  (3.0,   40.0,    3,   40, 3.0,   40.0),
  (3.0,   40.0,    3,   40, 3.0,   40.0),
  (4.0,  100.0,    4,  100, 4.0,  100.0),
  (4.0,  100.0,    4,  100, 4.0,  100.0),
  (4.0,  100.0,    4,  100, 4.0,  100.0),
  (4.0,  100.0,    4,  100, 4.0,  100.0),
  (NULL,  NULL, NULL, NULL, NULL, NULL);

-- Test 344: query (line 2401)
SELECT regr_avgx(y, x)::decimal, regr_avgx(int_y, int_x)::decimal, regr_avgx(y, int_x)::decimal,
regr_avgx(int_y, x)::decimal
FROM statistics_agg_test;

-- Test 345: query (line 2408)
SELECT regr_avgx(y, dx), regr_avgx(int_y, dx), regr_avgx(dy, int_x), regr_avgx(dy, x)
FROM statistics_agg_test;

-- Test 346: query (line 2414)
SELECT regr_avgx(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 347: query (line 2420)
SELECT CAST(regr_avgx(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 30) AS decimal)
FROM statistics_agg_test;

-- Test 348: statement (line 2450)
TRUNCATE statistics_agg_test;

-- Test 349: statement (line 2453)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 350: query (line 2458)
SELECT regr_avgx(y, x), regr_avgx(int_y, int_x), regr_avgx(dy, dx)
FROM statistics_agg_test;

-- Test 351: statement (line 2464)
TRUNCATE statistics_agg_test;

-- Test 352: statement (line 2467)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 353: query (line 2472)
SELECT regr_avgx(y, x), regr_avgx(int_y, int_x), regr_avgx(dy, dx)
FROM statistics_agg_test;

-- Test 354: statement (line 2478)
TRUNCATE statistics_agg_test;

-- Test 355: query (line 2481)
SELECT regr_avgx(y, x) FROM statistics_agg_test;

-- Test 356: statement (line 2488)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx)
VALUES (0.0,   0.09561,    1,   10,   0.0, 0.09561),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (42.0,   324.78,    2,   25,  42.0,  324.78),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (56.0,      7.8,    3,   40,  56.0,     7.8),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (100.0,  99.097,    4,  100, 100.0,  99.097),
       (NULL,     NULL, NULL, NULL,  NULL,    NULL);

-- Test 357: query (line 2502)
SELECT regr_avgy(y, x), regr_avgy(int_y, int_x), regr_avgy(y, int_x), regr_avgy(int_y, x), regr_avgy(dy, dx)
FROM statistics_agg_test;

-- Test 358: query (line 2508)
SELECT regr_avgy(y, dx), regr_avgy(int_y, dx), regr_avgy(dy, int_x), regr_avgy(dy, x)
FROM statistics_agg_test;

-- Test 359: query (line 2514)
SELECT regr_avgy(DISTINCT y, x)
FROM statistics_agg_test;

-- Test 360: query (line 2520)
SELECT CAST(regr_avgy(DISTINCT y, x) FILTER (WHERE x > 3 AND y < 100) AS decimal)
FROM statistics_agg_test;

-- Test 361: statement (line 2550)
TRUNCATE statistics_agg_test;

-- Test 362: statement (line 2553)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0,  10.0, 1,  10, 1.0,  10.0),
  (2.0, -20.0, 2, -20, 2.0, -20.0);

-- Test 363: query (line 2558)
SELECT regr_avgy(y, x), regr_avgy(int_y, int_x), regr_avgy(dy, dx)
FROM statistics_agg_test;

-- Test 364: statement (line 2564)
TRUNCATE statistics_agg_test;

-- Test 365: statement (line 2567)
INSERT INTO statistics_agg_test (y, x, int_y, int_x, dy, dx) VALUES
  (1.0, -1.0, 1, -1, 1.0, -1.0),
  (1.0,  1.0, 1,  1, 1.0,  1.0);

-- Test 366: query (line 2572)
SELECT regr_avgy(y, x), regr_avgy(int_y, int_x), regr_avgy(dy, dx)
FROM statistics_agg_test;

-- Test 367: statement (line 2578)
TRUNCATE statistics_agg_test;

-- Setup for string_agg_test
DROP TABLE IF EXISTS string_agg_test;
DROP TABLE IF EXISTS string_agg_test;
CREATE TABLE string_agg_test (
  company_id int,
  emp_id int,
  employee text
);
INSERT INTO string_agg_test VALUES
  (1, 1, 'A'),
  (2, 2, 'B'),
  (3, 3, 'C'),
  (4, 4, 'D'),
  (5, 3, 'C'),
  (6, 4, 'D'),
  (7, 4, 'D'),
  (8, 4, 'D'),
  (9, 3, 'C'),
  (10, 2, 'B');

-- Test 368: query (line 2590)
SELECT company_id, string_agg(employee, ',')
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 369: query (line 2598)
SELECT company_id, string_agg(employee::BYTEA, ','::bytea)
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 370: query (line 2606)
SELECT company_id, string_agg(employee, NULL)
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 371: query (line 2614)
SELECT company_id, string_agg(employee::BYTEA, NULL)
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 372: statement (line 2622)
TRUNCATE string_agg_test;
INSERT INTO string_agg_test VALUES
  (1, 1, 'A'),
  (2, 2, 'B'),
  (3, 3, 'C'),
  (4, 4, 'D'),
  (5, 3, 'C'),
  (6, 4, 'D'),
  (7, 4, 'D'),
  (8, 4, 'D'),
  (9, 3, 'C'),
  (10, 2, 'B');

-- Test 373: query (line 2636)
SELECT company_id, string_agg(employee, employee)
FROM string_agg_test
GROUP BY company_id;

-- Test 374: query (line 2646)
SELECT company_id, string_agg(employee, ',')
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 375: query (line 2658)
SELECT company_id, string_agg(DISTINCT employee, ',')
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 376: query (line 2670)
SELECT company_id, string_agg(employee::BYTEA, ','::bytea)
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 377: query (line 2682)
SELECT company_id, string_agg(employee, '')
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 378: query (line 2694)
SELECT company_id, string_agg(employee::BYTEA, ''::bytea)
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 379: query (line 2706)
SELECT company_id, string_agg(employee, NULL)
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 380: query (line 2718)
SELECT company_id, string_agg(employee::BYTEA, NULL)
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 381: query (line 2742)
SELECT company_id, string_agg(NULL::BYTEA, ','::bytea)
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 382: query (line 2766)
SELECT company_id, string_agg(NULL::BYTEA, NULL)
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- Test 383: query (line 2778)
SELECT company_id, string_agg(NULL, NULL)
FROM string_agg_test
GROUP BY company_id
ORDER BY company_id;

-- statement ok
TRUNCATE string_agg_test;

-- statement ok
INSERT INTO string_agg_test VALUES
  (1, 1, 'A'),
  (2, 1, 'B'),
  (3, 1, 'C'),
  (4, 1, 'D');

-- query IT colnames
SELECT e.company_id, string_agg(e.employee, ', ')
FROM (
  SELECT employee, company_id
  FROM string_agg_test
  ORDER BY employee
  ) AS e
GROUP BY e.company_id
ORDER BY e.company_id;

-- Test 384: query (line 2807)
SELECT e.company_id, string_agg(e.employee, ', '::bytea)
FROM (
  SELECT employee::BYTEA, company_id
  FROM string_agg_test
  ORDER BY employee
  ) AS e
GROUP BY e.company_id
ORDER BY e.company_id;

-- Test 385: query (line 2820)
SELECT e.company_id, string_agg(e.employee, ', ')
FROM (
  SELECT employee, company_id
  FROM string_agg_test
  ORDER BY employee DESC
  ) AS e
GROUP BY e.company_id
ORDER BY e.company_id;

-- Test 386: query (line 2833)
SELECT e.company_id, string_agg(e.employee, ', '::bytea)
FROM (
  SELECT employee::BYTEA, company_id
  FROM string_agg_test
  ORDER BY employee DESC
  ) AS e
GROUP BY e.company_id
ORDER BY e.company_id;

-- Test 387: query (line 2846)
SELECT e.company_id, string_agg(e.employee, NULL)
FROM (
  SELECT employee, company_id
  FROM string_agg_test
  ORDER BY employee DESC
  ) AS e
GROUP BY e.company_id
ORDER BY e.company_id;

-- Test 388: query (line 2859)
SELECT e.company_id, string_agg(e.employee, NULL)
FROM (
  SELECT employee::BYTEA, company_id
  FROM string_agg_test
  ORDER BY employee DESC
  ) AS e
GROUP BY e.company_id
ORDER BY e.company_id;

-- Test 389: statement (line 2872)
DROP TABLE string_agg_test;

-- Test 390: query (line 2877)
SELECT string_agg('foo', CAST ((SELECT NULL) AS BYTEA)) OVER ();

-- Test 391: statement (line 2883)
SELECT array_agg(x) FROM generate_series(1, 2) AS g(x);

-- Test 392: statement (line 2888)
DROP TABLE IF EXISTS uvw;
CREATE TABLE uvw (u INT, v INT, w INT);
DROP INDEX IF EXISTS uvw_uvw_idx;
CREATE INDEX uvw_uvw_idx ON uvw (u, v, w);

-- Test 393: statement (line 2891)
INSERT INTO uvw VALUES (1, 2, 3), (1, 2, 3), (3, 2, 1), (3, 2, 3);

-- Test 394: query (line 2894)
SELECT u, v, array_agg(w) AS s FROM (SELECT * FROM uvw ORDER BY w) GROUP BY u, v;

-- Test 395: statement (line 2919)
DROP TABLE IF EXISTS tab;
DROP TABLE IF EXISTS tab;
CREATE TABLE tab (
  col1 INT,
  col2 INT,
  col3 TEXT,
  arr INT[]
);
INSERT INTO tab VALUES (-3, 7, 'a', '{-3, 7}'), (-2, 6, 'a', '{-2, 6}'), (-1, 5, 'a', '{-1, 5}'),
  (0, 7, 'b', '{0, 7}'), (1, 5, 'b', '{1, 5}'), (2, 6, 'b', '{2, 6}');

-- Test 396: query (line 2923)
SELECT array_agg(col1 ORDER BY col1) FROM tab;

-- Test 397: query (line 2929)
SELECT array_agg(arr ORDER BY col1) FROM tab;

-- Test 398: query (line 2935)
SELECT array_agg(col1 ORDER BY col2*100+col1) FROM tab;

-- Test 399: query (line 2941)
SELECT array_agg(arr ORDER BY col2*100+col1) FROM tab;

-- Test 400: query (line 2947)
SELECT json_agg(col1 ORDER BY col1) FROM tab;

-- Test 401: query (line 2953)
SELECT jsonb_agg(col1 ORDER BY col1) FROM tab;

-- Test 402: query (line 2959)
SELECT jsonb_agg(col1 ORDER BY col2, col1) FROM tab;

-- Test 403: query (line 2965)
SELECT string_agg(col3, '' ORDER BY col1) FROM tab;

-- Test 404: query (line 2971)
SELECT string_agg(col3, '' ORDER BY col1 DESC) FROM tab;

-- Test 405: query (line 2977)
SELECT string_agg(col3, ', ' ORDER BY col3) FROM tab;

-- Test 406: query (line 2983)
SELECT string_agg(col3, ', ' ORDER BY col3 DESC) FROM tab;

-- Test 407: query (line 2989)
SELECT array_agg(col1 ORDER BY col1), array_agg(col1 ORDER BY col2, col1), array_agg(col1 ORDER BY col3, col1) FROM tab;

-- Test 408: query (line 2995)
SELECT array_agg(col1 ORDER BY col1), array_agg(col1 ORDER BY col2, col1), col3 FROM tab GROUP BY col3 ORDER BY col3;

-- Test 409: query (line 3002)
SELECT array_agg(col1 ORDER BY col1), array_agg(col1 ORDER BY col2, col1), count(col3), count(*) FROM tab;

-- Test 410: query (line 3008)
SELECT array_agg(col1 ORDER BY col1), array_agg(col1 ORDER BY col1) FILTER (WHERE col1 < 0) FROM tab;

-- Test 411: query (line 3014)
SELECT array_agg(col1 ORDER BY col3, col1) FILTER (WHERE col1 < 0), array_agg(col1 ORDER BY col3, col1) FROM tab;

-- Test 412: query (line 3020)
SELECT count(1), string_agg(col3, '' ORDER BY col1) FROM tab;

-- Test 413: query (line 3026)
SELECT
    *
FROM
    (
        SELECT
            count(1) AS count_1,
            count(lower(col3)) AS count_lower,
            count(upper(col3)) AS count_upper,
            string_agg(col3, '' ORDER BY col1) AS concat
        FROM
            tab
        GROUP BY
            upper(col3)
    )
ORDER BY
    concat;

-- Test 414: statement (line 3049)
DELETE FROM ab WHERE true;

-- Test 415: statement (line 3052)
INSERT INTO ab VALUES (1,1), (2,1), (3,3), (4, 7);

-- Test 416: query (line 3055)
SELECT b FROM ab GROUP BY b;

-- Test 417: query (line 3063)
SELECT a+b, count(*) FROM ab JOIN tab ON b=col2 GROUP BY a, b;

-- Test 418: query (line 3068)
SELECT a, col1, b+col2, count(*) FROM ab JOIN tab ON b=col2 GROUP BY a, b, col1, col2;

-- Test 419: query (line 3074)
SELECT a, b, count(*), count(col2) FROM ab LEFT JOIN tab ON b=col2 GROUP BY a, b;

-- Test 420: query (line 3082)
SELECT a, b, count(*) FROM ab RIGHT JOIN tab ON b=col2 GROUP BY a, b;

-- Test 421: statement (line 3089)
DROP TABLE IF EXISTS xyz;
CREATE TABLE xyz (
  x INT PRIMARY KEY,
  y INT,
  z INT
);
DROP INDEX IF EXISTS xyz_yz_idx;
CREATE INDEX xyz_yz_idx ON xyz (y, z);

-- Test 422: statement (line 3097)
INSERT INTO xyz VALUES (1, 2, 3), (2, 2, 7), (3, 2, 1), (4, 2, NULL), (5, 3, -1);

-- Test 423: query (line 3100)
SELECT min(z) FROM xyz WHERE y = 2 GROUP BY y;

-- Test 424: query (line 3105)
SELECT min(z) FROM xyz WHERE y = 2 AND z IS NOT NULL GROUP BY y;

-- Test 425: query (line 3110)
SELECT min(z) FROM xyz WHERE y = 2 AND z IS NULL GROUP BY y;

-- Test 426: query (line 3115)
SELECT min(z) FROM xyz WHERE y = 100 AND z IS NULL GROUP BY y;

-- Test 427: query (line 3119)
SELECT max(z) FROM xyz WHERE y = 2 GROUP BY y;

-- Test 428: query (line 3124)
SELECT max(z) FROM xyz WHERE y = 2 AND z IS NOT NULL GROUP BY y;

-- Test 429: query (line 3129)
SELECT max(z) FROM xyz WHERE y = 2 AND z IS NULL GROUP BY y;

-- Test 430: query (line 3134)
SELECT max(z) FROM xyz WHERE y = 100 GROUP BY y;

-- Test 431: statement (line 3138)
DROP TABLE xyz;

-- Test 432: statement (line 3142)
DROP TABLE IF EXISTS t44469_a;
CREATE TABLE t44469_a (a INT);
DROP INDEX IF EXISTS t44469_a_a_idx;
CREATE INDEX t44469_a_a_idx ON t44469_a (a);

-- Test 433: statement (line 3145)
DROP TABLE IF EXISTS t44469_b;
CREATE TABLE t44469_b (b INT);
DROP INDEX IF EXISTS t44469_b_b_idx;
CREATE INDEX t44469_b_b_idx ON t44469_b (b);

-- Test 434: statement (line 3148)
DROP TABLE IF EXISTS t44469_cd;
CREATE TABLE t44469_cd (c INT, d INT);
DROP INDEX IF EXISTS t44469_cd_cd_idx;
CREATE INDEX t44469_cd_cd_idx ON t44469_cd (c, d);

-- Test 435: statement (line 3151)
SELECT DISTINCT ON (b) b
FROM t44469_a INNER JOIN t44469_b ON a = b INNER JOIN t44469_cd ON c = 1 AND d = a
ORDER BY b;

-- Test 436: statement (line 3156)
DROP TABLE IF EXISTS t;
DROP TABLE IF EXISTS t;
CREATE TABLE t (x JSONB, y INT);
INSERT INTO t VALUES
  ('{"foo": "bar"}', 5),
  ('{"foo": "bar"}', 10),
  ('[1, 2]', 5),
  ('[1, 2]', 20),
  ('{"foo": "bar", "bar": "baz"}', 5),
  ('{"foo": "bar", "bar": "baz"}', 30),
  ('{"foo": {"bar" : "baz"}}', 5),
  ('{"foo": {"bar" : "baz"}}', 40);

-- Test 437: query (line 3169)
SELECT x, SUM (y) FROM t GROUP BY (x) ORDER BY SUM (y);

-- Test 438: statement (line 3180)
DROP TABLE IF EXISTS t_every;
CREATE TABLE t_every (x BOOL);

-- Test 439: query (line 3183)
SELECT every (x) FROM t_every;

-- Test 440: statement (line 3188)
INSERT INTO t_every VALUES (true), (true);

-- Test 441: query (line 3191)
SELECT every (x) FROM t_every;

-- Test 442: statement (line 3196)
INSERT INTO t_every VALUES (NULL), (true);

-- Test 443: query (line 3199)
SELECT every (x) FROM t_every;

-- Test 444: statement (line 3204)
INSERT INTO t_every VALUES (false), (NULL);

-- Test 445: query (line 3207)
SELECT every (x) FROM t_every;

-- Test 446: statement (line 3212)
TRUNCATE t_every;
INSERT INTO t_every VALUES (false);

-- Test 447: query (line 3216)
SELECT every (x) FROM t_every;

-- Test 448: statement (line 3221)
TRUNCATE t_every;

-- Test 449: statement (line 3224)
INSERT INTO t_every VALUES (NULL), (NULL), (NULL);

-- Test 450: query (line 3227)
SELECT every (x) FROM t_every;

-- Test 451: statement (line 3233)
DROP TABLE IF EXISTS t46423;
CREATE TABLE t46423(c0 INT);
INSERT INTO t46423(c0) VALUES(0);

-- Test 452: query (line 3237)
SELECT c0 FROM t46423 GROUP BY c0 HAVING NOT (variance(0) IS NULL);

-- Test 453: statement (line 3243)
DROP TABLE IF EXISTS t45453;
CREATE TABLE t45453(c INT);

-- Test 454: query (line 3246)
SELECT count(*) FROM t45453 GROUP BY 0 + 0;

-- Test 455: statement (line 3254)
DROP TABLE IF EXISTS vals;

-- Test 456: statement (line 3257)
DROP TABLE IF EXISTS vals;
CREATE TABLE vals (
  v VARBIT,
  b BIT(8)
);

-- Test 457: query (line 3265)
SELECT bit_and(v) FROM vals;

-- Test 458: query (line 3270)
SELECT bit_or(v) FROM vals;

-- Test 459: query (line 3278)
SELECT bit_and('1000'::varbit) FROM vals;

-- Test 460: query (line 3283)
SELECT bit_or('1000'::varbit) FROM vals;

-- Test 461: query (line 3291)
SELECT bit_and('1'::varbit), bit_and('1000'::bit(4)), bit_and('1010'::varbit);

-- Test 462: query (line 3296)
SELECT bit_or('1'::varbit), bit_or('1000'::bit(4)), bit_or('1010'::varbit);

-- Test 463: query (line 3303)
SELECT bit_and(NULL::varbit);

-- Test 464: query (line 3308)
SELECT bit_or(NULL::varbit);

-- Test 465: statement (line 3315)
INSERT INTO vals VALUES
('11111110'::varbit, '11111110'::bit(8)),
('01111111'::varbit, '01111110'::bit(8)),
('10111111'::varbit, '10111110'::bit(8)),
('11011111'::varbit, '11011110'::bit(8)),
('11101111'::varbit, '11101110'::bit(8));

-- Test 466: query (line 3323)
SELECT bit_and(v), bit_and(b) FROM vals;

-- Test 467: query (line 3328)
SELECT bit_or(v), bit_or(b) FROM vals;

-- Test 468: statement (line 3335)
INSERT INTO vals VALUES
(NULL::varbit, NULL::bit),
(NULL::varbit, NULL::bit);

-- Test 469: query (line 3340)
SELECT bit_and(v), bit_and(b) FROM vals;

-- Test 470: query (line 3345)
SELECT bit_or(v), bit_or(b) FROM vals;

-- Test 471: statement (line 3352)
DELETE FROM vals;

-- Test 472: statement (line 3355)
INSERT INTO vals VALUES
(NULL::varbit),
(NULL::varbit),
(NULL::varbit),
(NULL::varbit);

-- Test 473: query (line 3362)
SELECT bit_and(v) FROM vals;

-- Test 474: query (line 3367)
SELECT bit_or(v) FROM vals;

-- Test 475: statement (line 3374)
SELECT bit_and(NULL::int);

-- Test 476: statement (line 3377)
SELECT bit_or(NULL::int);

-- Test 477: statement (line 3383)
SELECT bit_and(x::bit(2)) FROM (VALUES ('1'), ('11')) t(x);

-- Test 478: statement (line 3386)
SELECT bit_and(x) FROM (VALUES ('100'::bit(3)), ('101'::bit(3))) t(x);

-- Test 479: statement (line 3389)
SELECT bit_and(x) FROM (VALUES ('0'::bit(1)), ('1'::bit(1))) t(x);

-- Test 480: statement (line 3392)
SELECT bit_or(x::bit(2)) FROM (VALUES ('1'), ('11')) t(x);

-- Test 481: statement (line 3395)
SELECT bit_or(x) FROM (VALUES ('100'::bit(3)), ('101'::bit(3))) t(x);

-- Test 482: statement (line 3398)
SELECT bit_or(x) FROM (VALUES ('0'::bit(1)), ('1'::bit(1))) t(x);

-- Test 483: statement (line 3403)
DROP TABLE IF EXISTS t46981_0;
CREATE TABLE t46981_0(c0 INT);
DROP VIEW IF EXISTS v46981_0;
CREATE VIEW v46981_0(c0) AS SELECT count(*) FROM t46981_0;

-- Test 484: statement (line 3407)
SELECT * FROM v46981_0 WHERE '' !~ '\\+';

-- Test 485: statement (line 3413)
DROP TABLE IF EXISTS osagg;

-- Test 486: statement (line 3423)
DROP TABLE IF EXISTS osagg;
CREATE TABLE osagg (
  f float,
  v text,
  i interval
);
INSERT INTO osagg VALUES
(NULL, NULL, NULL),
(0.00, NULL, '1 months'),
(0.05, NULL, '1 months'),
(1.0, 'v1', '1 year 1 months'),
(3.0, 'v3', '1 year 3 months'),
(5.0, 'v5', '1 year 5 months'),
(2.0, 'v2', '1 year 2 months'),
(4.0, 'v4', '1 year 4 months'),
(6.0, 'v6', '1 year 6 months');

-- Test 487: query (line 3436)
SELECT
  percentile_disc(0.95) WITHIN GROUP (ORDER BY f)
FROM osagg;

-- Test 488: query (line 3443)
SELECT
  percentile_disc(0.95) WITHIN GROUP (ORDER BY f),
  percentile_disc(0.95) WITHIN GROUP (ORDER BY v)
FROM osagg;

-- Test 489: query (line 3451)
SELECT
  percentile_cont(0.95) WITHIN GROUP (ORDER BY f),
  percentile_cont(0.95) WITHIN GROUP (ORDER BY f DESC),
  percentile_cont(0.95) WITHIN GROUP (ORDER BY i)
FROM osagg;

-- Test 490: query (line 3461)
SELECT
  percentile_disc(0.00) WITHIN GROUP (ORDER BY v),
  percentile_disc(0.1) WITHIN GROUP (ORDER BY f),
  percentile_disc(0.15) WITHIN GROUP (ORDER BY f)
FROM osagg;

-- Test 491: query (line 3470)
SELECT
  percentile_cont(0.05) WITHIN GROUP (ORDER BY f),
  percentile_cont(0.05) WITHIN GROUP (ORDER BY f DESC),
  percentile_cont(0.05) WITHIN GROUP (ORDER BY i),
  percentile_cont(0.05) WITHIN GROUP (ORDER BY i DESC)
FROM osagg;

-- Test 492: query (line 3481)
SELECT
  percentile_disc(0.25) WITHIN GROUP (ORDER BY f),
  percentile_disc(0.5) WITHIN GROUP (ORDER BY f),
  percentile_disc(0.75) WITHIN GROUP (ORDER BY f)
FROM osagg;

-- Test 493: query (line 3490)
SELECT
  percentile_cont(0.25) WITHIN GROUP (ORDER BY f),
  percentile_cont(0.5) WITHIN GROUP (ORDER BY f),
  percentile_cont(0.75) WITHIN GROUP (ORDER BY f)
FROM osagg;

-- Test 494: query (line 3500)
SELECT
  percentile_disc(ARRAY[0.25]::float[]) WITHIN GROUP (ORDER BY f)
FROM osagg;

-- Test 495: query (line 3507)
SELECT
  percentile_disc(ARRAY[0.25, 0.5, 0.75]::float[]) WITHIN GROUP (ORDER BY f)
FROM osagg;

-- Test 496: query (line 3514)
SELECT
  percentile_cont(ARRAY[0.25, 0.5, 0.75]::float[]) WITHIN GROUP (ORDER BY f)
FROM osagg;

-- Test 497: query (line 3521)
SELECT
  percentile_disc(ARRAY[0.25, 0.5, 0.75]::float[]) WITHIN GROUP (ORDER BY i)
FROM osagg;

-- Test 498: statement (line 3528)
-- SELECT
--   percentile_disc(ARRAY[1.25]::float[]) WITHIN GROUP (ORDER BY f)
-- FROM osagg;

-- Test 499: statement (line 3533)
-- SELECT
--   percentile_disc(ARRAY[0.25, 0.50, 1.25]::float[]) WITHIN GROUP (ORDER BY f)
-- FROM osagg;

-- Test 500: query (line 3538)
SELECT
  percentile_cont(ARRAY[0.25, 0.5, 0.75]::float[]) WITHIN GROUP (ORDER BY i)
FROM osagg;

-- Test 501: statement (line 3546)
DROP VIEW IF EXISTS osagg_view;
CREATE VIEW osagg_view (disc, cont) AS
  SELECT percentile_disc(0.50) WITHIN GROUP (ORDER BY f),
         percentile_cont(0.50) WITHIN GROUP (ORDER BY f DESC) FROM osagg;

-- Test 502: query (line 3551)
SELECT pg_get_viewdef('osagg_view'::regclass, true);

-- Test 503: statement (line 3564)
-- SELECT percentile_disc(0.50) FROM osagg;

-- Test 504: statement (line 3567)
-- SELECT percentile_cont(0.50) FROM osagg;

-- Test 505: statement (line 3594)
DROP TABLE IF EXISTS t_collate;
DROP TABLE IF EXISTS t_collate;
CREATE TABLE t_collate (x text);
INSERT INTO t_collate VALUES ('a'), ('b');

-- Test 506: query (line 3575)
SELECT min(x), max(x) FROM t_collate;

DROP TABLE IF EXISTS profiles;
DROP TABLE IF EXISTS profiles;
CREATE TABLE profiles (
  userid int,
  property text,
  value text
);
INSERT INTO profiles VALUES
(1, 'email', 'user1@gmail.com'),
(1, 'phone', '111111111111111'),
(1, 'home_page', 'user1.org1.com'),
(2, 'email', 'user2@gmail.com'),
(2, 'phone', '222222222222222'),
(2, 'home_page', 'user2.org1.com');

-- Test 507: query (line 3603)
SELECT json_object_agg(property, value) FROM profiles GROUP BY userid ORDER BY userid;

-- Test 508: statement (line 3622)
DROP TABLE IF EXISTS users;
DROP TABLE IF EXISTS users;
CREATE TABLE users (
  userid int,
  user_name text
);
INSERT INTO users VALUES
(1, 'Alice'),
(2, 'Bob');

-- Test 509: statement (line 3627)
DROP TABLE IF EXISTS user_networks;
DROP TABLE IF EXISTS user_networks;
CREATE TABLE user_networks (
  userid int,
  network_name text,
  user_account_id text
);
INSERT INTO user_networks VALUES
(1, 'Facebook', 'Alice_fb'),
(1, 'Twitter', '@Alice'),
(1, 'Instagram', 'AliceInst'),
(2, 'Facebook', 'Bob_fb'),
(2, 'LinkedIn', 'Bob The Builder');

-- Test 510: query (line 3635)
SELECT json_object_agg(user_name, networks) FROM
  (SELECT u.userid as userid, u.user_name, json_object_agg(un.network_name, un.user_account_id) as networks FROM
    users u, user_networks un
      WHERE u.userid = un.userid
      GROUP BY u.userid, u.user_name
      ORDER BY u.userid)
  GROUP BY userid
  ORDER BY userid;

-- Test 511: statement (line 3648)
SELECT json_object_agg('key', NULL);

-- Test 512: statement (line 3651)
SELECT json_object_agg('key', 1);

-- Test 513: statement (line 3674)
DROP TABLE IF EXISTS persons;
DROP TABLE IF EXISTS persons;
CREATE TABLE persons (
  id text,
  name text
);
INSERT INTO persons VALUES
('1', 'Alice'),
('2', 'Bob');

-- Test 514: statement (line 3679)
DROP TABLE IF EXISTS companies;
DROP TABLE IF EXISTS companies;
CREATE TABLE companies (
  id text,
  company_name text
);
INSERT INTO companies VALUES
('1', 'Facebook'),
('2', 'Google'),
('3', 'Twitter'),
('4', 'IBM'),
('5', 'Cockroach Labs');

-- Test 515: statement (line 3687)
DROP TABLE IF EXISTS jobs;
DROP TABLE IF EXISTS jobs;
CREATE TABLE jobs (
  id text,
  person_id text,
  company_id text,
  job_title text
);
INSERT INTO jobs VALUES
('1', '1', '1', 'Developer'),
('2', '1', '2', 'Full Stack'),
('3', '1', '4', 'Research'),
('4', '2', '3', 'Frontend'),
('5', '2', '5', 'DB Developer'),
('6', '2', '2', 'DevOps');

-- Test 516: query (line 3696)
SELECT json_build_object(p.name, json_object_agg(c.company_name, j.job_title))
FROM persons p
  LEFT OUTER JOIN jobs j ON p.id = j.person_id
    LEFT OUTER JOIN companies c ON c.id = j.company_id
GROUP BY p.name
ORDER BY p.name;

-- Test 517: statement (line 3713)
DROP TABLE IF EXISTS blog;
DROP TABLE IF EXISTS blog;
CREATE TABLE blog (
  id text,
  name text
);
INSERT INTO blog VALUES ('1', 'Test Blog');

-- Test 518: statement (line 3723)
DROP TABLE IF EXISTS blog_properties;
DROP TABLE IF EXISTS blog_properties;
CREATE TABLE blog_properties (
  blog_id text,
  property_name text,
  property_value text
);
INSERT INTO blog_properties VALUES
('1', 'Application Name', 'Instagram'),
('1', 'Admin Email', 'admin@email.com'),
('1', 'Blog Name', 'Wordpress Blog'),
('1', 'Application Name', 'Twitter'),
('1', 'KeepAlive', 'true'),
('1', 'Session Timeout', '1000ms');

-- Test 519: query (line 3732)
SELECT json_build_object(b.name, json_object_agg(p.property_name, p.property_value ORDER BY p.property_value))
FROM blog b, blog_properties p WHERE b.id = p.blog_id
GROUP BY b.name;

-- Test 520: statement (line 3741)
DROP TABLE IF EXISTS t55776;
CREATE TABLE t55776 (i BIGINT PRIMARY KEY, y FLOAT8, x FLOAT8);
INSERT INTO t55776 (i, y, x) VALUES
  (1, 1.0, 1),
  (2, 2.0, 2),
  (3, 1.0, 2),
  (4, 1.0, 2),
  (5, 3.0, 2);

-- Test 521: query (line 3750)
SELECT corr(DISTINCT y, x), count(DISTINCT y) FROM t55776;

-- Test 522: statement (line 3757)
DROP TABLE IF EXISTS t63159;
CREATE TABLE t63159 (a INT, b INT);
DROP INDEX IF EXISTS t63159_a_idx;
CREATE INDEX t63159_a_idx ON t63159 (a) INCLUDE (b);
INSERT INTO t63159 VALUES (1,1), (3,3), (2,2), (5,5), (0,0), (1,1);

-- Test 523: query (line 3761)
SELECT a, b, count(*) FROM t63159 GROUP BY a,b ORDER BY a;

-- Test 524: statement (line 3772)
DROP TABLE IF EXISTS t63436;
CREATE TABLE t63436 (a INT, b FLOAT, c DECIMAL);
DROP INDEX IF EXISTS t63436_a_idx;
CREATE INDEX t63436_a_idx ON t63436 (a);
SELECT count(*) FROM t63436 GROUP BY b, c ORDER BY c;

-- Test 525: query (line 3779)
SELECT percentile_disc(0.95) WITHIN GROUP (ORDER BY 33) FROM osagg;

-- Test 526: query (line 3784)
SELECT percentile_disc(0.95) WITHIN GROUP (ORDER BY 33.0) FROM osagg;

-- Test 527: query (line 3789)
SELECT percentile_disc(0.95) WITHIN GROUP (ORDER BY 33::INT) FROM osagg;

-- Test 528: query (line 3794)
SELECT percentile_disc(0.95) WITHIN GROUP (ORDER BY '33'::INT) FROM osagg;

-- Test 529: query (line 3803)
SELECT percentile_disc(0.95) WITHIN GROUP (ORDER BY 'foo'::text) FROM osagg;

-- Test 530: query (line 3808)
SELECT percentile_disc(0.95) WITHIN GROUP (ORDER BY current_database()) FROM osagg;

-- Test 531: statement (line 3815)
DROP TABLE IF EXISTS corrupt_combine;
CREATE TABLE corrupt_combine (
  y float,
  x float
);

-- Test 532: statement (line 3821)
INSERT INTO corrupt_combine (y, x) VALUES
  (1.0, 10.0),
  (2.0, 25.0),
  (3.0, 35.0),
  (4.0, 50.0),
  (5.0, 70.0),
  (6.0, 70.0);

-- Test 533: query (line 3841)
select covar_pop(y, x), covar_samp(y, x), regr_sxx(y, x), regr_syy(y, x) from corrupt_combine;

-- Test 534: statement (line 3848)
DROP TABLE IF EXISTS t2;
DROP TABLE IF EXISTS t2;
CREATE TABLE t2 (
  a INT,
  b INT,
  c INT
);
DROP INDEX IF EXISTS t2_bca_idx;
CREATE INDEX t2_bca_idx ON t2 (b, c, a);
INSERT INTO t2 (a, b, c) VALUES (1, 10, 20), (0, 11, 100);

-- Test 535: query (line 3857)
SELECT min(a) FROM t2 WHERE (b <= 11 AND c < 50) OR (b = 11 AND c = 50) OR (b >= 11 AND c > 50);

-- Test 536: statement (line 3863)
DROP TABLE IF EXISTS nulls_last_test;
CREATE TABLE nulls_last_test (
    id INT NULL,
    k INT NULL,
    v VARCHAR(3) NULL
);
INSERT INTO nulls_last_test VALUES
  (1, 1, 'foo'),
  (2, null, null),
  (null, null, 'bar'),
  (3, 3, 'baz');

-- Test 537: query (line 3875)
SELECT array_agg(id ORDER BY id NULLS LAST) FROM nulls_last_test;

-- Test 538: query (line 3881)
SELECT array_agg((k, v) ORDER BY (k, v)) FROM nulls_last_test;

-- Test 539: query (line 3886)
SELECT array_agg((k, v) ORDER BY (k, v) NULLS LAST) FROM nulls_last_test;

-- Test 540: query (line 3892)
SELECT array_agg((k, v) ORDER BY (k+1, v||'foo')) FROM nulls_last_test;

-- Test 541: query (line 3897)
SELECT array_agg((k, v) ORDER BY (k+1, v||'foo') NULLS LAST) FROM nulls_last_test;

-- Test 542: statement (line 3903)
-- SET null_ordered_last = true;

-- Test 543: query (line 3906)
SELECT array_agg(id ORDER BY id) FROM nulls_last_test;

-- Test 544: query (line 3911)
SELECT array_agg((k, v) ORDER BY (k, v)) FROM nulls_last_test;

-- Test 545: query (line 3919)
SELECT array_agg((k, v) ORDER BY (k, v) NULLS FIRST) FROM nulls_last_test;

-- Test 546: query (line 3924)
SELECT array_agg((k, v) ORDER BY (k+1, v||'foo')) FROM nulls_last_test;

-- Test 547: query (line 3932)
SELECT array_agg((k, v) ORDER BY (k+1, v||'foo') NULLS FIRST) FROM nulls_last_test;

-- Test 548: query (line 3941)
WITH t (x, y) AS (
  VALUES
    ((1, 1), 1),
    ((NULL::RECORD), 2),
    ((1, NULL::INT), 3),
    ((NULL::INT, NULL::INT), 4)
)
SELECT array_agg(x ORDER BY x)
FROM t;

-- Test 549: query (line 3958)
WITH t (x, y) AS (
  VALUES
    ((1, 1), 1),
    ((NULL::RECORD), 2),
    ((1, NULL::INT), 3),
    ((NULL::INT, NULL::INT), 4)
)
SELECT array_agg(x ORDER BY x NULLS FIRST)
FROM t;

-- Test 550: statement (line 3971)
-- RESET null_ordered_last;

-- Test 551: query (line 3976)
-- SELECT array_agg(ARRAY[(1::INT,), (1::FLOAT8,)]);

-- Test 552: query (line 3981)
-- SELECT array_agg(
--   ARRAY[(416644234484367676::BIGINT,),(NULL,),((-0.12116245180368423)::FLOAT8,)]
-- );

-- Test 553: statement (line 3988)
DROP TABLE IF EXISTS __test_array_agg;
CREATE TABLE __test_array_agg(a TEXT PRIMARY KEY, b TEXT, c TEXT);
INSERT INTO __test_array_agg VALUES ('a', '', '::byteac'), ('aa', 'b', '::byteacc'), ('aaa', 'bb', '::byteaccc');

-- Test 554: query (line 3992)
SELECT array_agg(array[a, b, c]) FROM __test_array_agg;

-- Test 555: query (line 3999)
WITH
    foo(f) AS (SELECT array_agg(x) FROM generate_series(1, 3) g(x)),
    bar(b) AS (SELECT array_agg(f) FROM foo, generate_series(1, 3)),
    baz(z) AS (SELECT array_agg(b) FROM bar, generate_series(1, 3))
SELECT z FROM baz;

-- Regression test for incorrectly picking row-by-row ordered aggregator when
-- some optimizer rules are disabled (#124101).
-- statement ok
DROP TABLE IF EXISTS t124101;
CREATE TABLE t124101 (
  a INT,
  b INT,
  c INT,
  PRIMARY KEY (a, b),
  UNIQUE (a, c)
);
INSERT INTO t124101 VALUES (0, 0, 1), (0, 1, 2);
-- SET testing_optimizer_disable_rule_probability = 1.000000;

-- query II
SELECT a, sum(c) AS s FROM t124101 GROUP BY a, c ORDER BY a, a, s, c LIMIT 2;

-- Test 556: statement (line 4025)
-- RESET testing_optimizer_disable_rule_probability;

-- Test 557: statement (line 4029)
DROP TABLE IF EXISTS t90519;
CREATE TABLE t90519 (i int);
INSERT INTO t90519 VALUES (1),(2),(3),(4);

-- Test 558: query (line 4033)
SELECT percentile_cont(ARRAY[.4::FLOAT]) WITHIN GROUP (ORDER BY i::FLOAT4) FROM t90519;

RESET client_min_messages;
