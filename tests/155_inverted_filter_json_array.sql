-- PostgreSQL compatible tests from inverted_filter_json_array
-- 25 tests

SET client_min_messages = warning;

DROP TABLE IF EXISTS json_tab CASCADE;
DROP TABLE IF EXISTS array_tab CASCADE;

-- Test 1: statement (line 4)
CREATE TABLE json_tab (
  a INT PRIMARY KEY,
  b JSONB
);

-- Test 2: statement (line 11)
-- CockroachDB uses INVERTED INDEX; PostgreSQL uses GIN for JSONB containment.
CREATE INDEX json_tab_foo_inv ON json_tab USING gin (b);

-- Test 3: statement (line 14)
CREATE TABLE array_tab (
  a INT PRIMARY KEY,
  b INT[]
);

-- Test 4: statement (line 21)
-- CockroachDB uses INVERTED INDEX; PostgreSQL uses GIN for array containment.
CREATE INDEX array_tab_foo_inv ON array_tab USING gin (b);

-- Test 5: statement (line 24)
INSERT INTO json_tab VALUES
  (1, '{"a": "b"}'),
  (2, '[1,2,3,4, "foo"]'),
  (3, '{"a": {"b": "c"}}'),
  (4, '{"a": {"b": [1]}}'),
  (5, '{"a": {"b": [1, [2]]}}'),
  (6, '{"a": {"b": [[2]]}}'),
  (7, '{"a": "b", "c": "d"}'),
  (8, '{"a": {"b":true}}'),
  (9, '{"a": {"b":false}}'),
  (10, '"a"'),
  (11, 'null'),
  (12, 'true'),
  (13, 'false'),
  (14, '1'),
  (15, '1.23'),
  (16, '[{"a": {"b": [1, [2]]}}, "d"]'),
  (17, '{}'),
  (18, '[]'),
  (19, '["a", "a"]'),
  (20, '[{"a": "a"}, {"a": "a"}]'),
  (21, '[[[["a"]]], [[["a"]]]]'),
  (22, '[1,2,3,1]'),
  (23, '{"a": 123.123}'),
  (24, '{"a": 123.123000}'),
  (25, '{"a": [{}]}'),
  (26, '[[], {}]'),
  (27, '[true, false, null, 1.23, "a"]'),
  (28, '{"a": {}}'),
  (29, NULL),
  (30, '{"a": []}'),
  (31, '{"a": {"b": "c", "d": "e"}, "f": "g"}'),
  (32, '{"a": [1]}'),
  (33, '[1, "bar"]'),
  (34, '{"a": 1}'),
  (35, '[1]'),
  (36, '[2]'),
  (37, '[[1]]'),
  (38, '[[2]]'),
  (39, '["a"]'),
  (40, '{"a": [[]]}'),
  (41, '[[1, 2]]'),
  (42, '[[1], [2]]'),
  (43, '[{"a": "b", "c": "d"}]'),
  (44, '[{"a": "b"}, {"c": "d"}]');

-- Test 6: query (line 72)
SELECT * FROM json_tab WHERE b @> '1' ORDER BY a;

-- Test 7: query (line 82)
SELECT a FROM json_tab WHERE b @> '1' ORDER BY a;

-- Test 8: query (line 92)
SELECT a FROM json_tab WHERE b @> '[1, 2]' OR b @> '[3, 4]' ORDER BY a;

-- Test 9: query (line 99)
SELECT * FROM json_tab WHERE b @> '{"a": {}}' ORDER BY a;

-- Test 10: query (line 111)
SELECT a FROM json_tab WHERE b @> '{"a": {}}' ORDER BY a;

-- Test 11: query (line 124)
SELECT * FROM json_tab WHERE b @> '{"a": []}' ORDER BY a;

-- Test 12: query (line 132)
SELECT a FROM json_tab WHERE b @> '{"a": []}' ORDER BY a;

-- Test 13: query (line 141)
SELECT a FROM json_tab WHERE b @> '[[1, 2]]' OR b @> '[[3, 4]]' ORDER BY a;

-- Test 14: query (line 147)
-- PostgreSQL does not support Cockroach index hints (table@index).
SELECT * FROM json_tab WHERE b @> '[1]' OR b @> '[2]' ORDER BY a;

-- Test 15: query (line 156)
SELECT a FROM json_tab WHERE b @> '[1]' OR b @> '[2]' ORDER BY a;

-- Test 16: query (line 166)
SELECT * FROM json_tab WHERE b @> '[3]' OR b @> '[[1, 2]]' ORDER BY a;

-- Test 17: query (line 174)
SELECT * FROM json_tab
WHERE (b @> '[1]'::jsonb OR b @> '[2]'::jsonb)
  AND (b @> '3'::jsonb OR b @> '"bar"'::jsonb)
ORDER BY a;

-- Test 18: query (line 184)
SELECT * FROM json_tab WHERE b @> '[1]' AND a % 2 = 0 ORDER BY a;

-- Test 19: query (line 191)
SELECT * FROM json_tab WHERE b @> '[1]' OR a = 44 ORDER BY a;

-- Test 20: query (line 201)
SELECT * FROM json_tab WHERE b @> '[1]' OR sqrt(a::decimal) = 2 ORDER BY a;

-- Test 21: statement (line 210)
INSERT INTO array_tab VALUES
  (1, '{}'),
  (2, '{1}'),
  (3, '{2}'),
  (4, '{1, 2}'),
  (5, '{1, 3}'),
  (6, '{1, 2, 3, 4}');

-- Test 22: query (line 220)
-- PostgreSQL does not support Cockroach index hints (table@index).
SELECT a FROM array_tab WHERE b @> '{}' ORDER BY a;

-- Test 23: query (line 231)
SELECT * FROM array_tab WHERE b @> '{1}' AND a % 2 = 0 ORDER BY a;

-- Test 24: query (line 239)
SELECT * FROM array_tab WHERE b @> '{1}' OR a = 1 ORDER BY a;

-- Test 25: query (line 249)
SELECT * FROM array_tab WHERE (b @> '{2}' AND a = 3) OR b[0] = a ORDER BY a;

RESET client_min_messages;
