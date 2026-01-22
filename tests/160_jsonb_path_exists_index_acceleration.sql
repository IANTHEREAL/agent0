-- PostgreSQL compatible tests from jsonb_path_exists_index_acceleration
-- 77 tests

SET client_min_messages = warning;
DROP TABLE IF EXISTS json_tab;

-- Test 1: statement (line 4)
CREATE TABLE json_tab (
  a INT PRIMARY KEY,
  b JSONB,
  c JSONB
);

-- Test 2: statement (line 11)
CREATE INDEX foo_inv ON json_tab USING gin (b);

-- Test 3: statement (line 14)
INSERT INTO json_tab VALUES
  (1, '{"a": "b"}'),
  (2, '[1,2,3,4, "foo"]'),
  (3, '{"a": {"b": "c", "d": "e"}}'),
  (4, '{"a": {"b": [1, 2, 3, 4]}}'),
  (5, '{"a": {}}'),
  (6, '{"a": {"b": {"c": "d"}, "d": "e"}}'),
  (7, '{"a": [{"b": {"x": "y"}}, {"b": "e"}]}'),
  (8, '{"a": [{"b":[]}]}'),
  (9, '{"a": {"b": "c"}}'),
  (10, '{"a": {"d": "e"}}');

-- Test 4: query (line 28)
SELECT a, b FROM json_tab WHERE jsonb_path_exists(b, '$.a.b') ORDER BY a, b;

-- Test 5: query (line 38)
SELECT a, b FROM json_tab WHERE jsonb_path_exists(b, '$.a.b') ORDER BY a, b;

-- Test 6: query (line 49)
SELECT jsonb_path_query(b, '$.a.b') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b') ORDER BY a, jpq;

-- Test 7: query (line 60)
SELECT jsonb_path_query(b, '$.a.b') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b') ORDER BY a, jpq;

-- Test 8: query (line 71)
SELECT jsonb_path_query(b, '$.a.b.c') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b.c') ORDER BY a, jpq;

-- Test 9: query (line 76)
SELECT jsonb_path_query(b, '$.a.b.c') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b.c') ORDER BY a, jpq;

-- Test 10: query (line 81)
SELECT a, jsonb_path_query(b, '$') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$') ORDER BY a, jpq;

-- Test 11: statement (line 95)
SELECT a, jsonb_path_query(b, '$') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$') ORDER BY a, jpq;

-- Test 12: statement (line 98)
SELECT a, jsonb_path_query(b, '$[*]') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$[*]') ORDER BY a, jpq;

-- Test 13: statement (line 101)
SELECT a, jsonb_path_query(b, '$[*][*]') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$[*][*]') ORDER BY a, jpq;

-- Test 14: query (line 104)
SELECT a, jsonb_path_query(b, '$.a[*].b') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a[*].b') ORDER BY a, jpq;

-- Test 15: query (line 115)
SELECT a, jsonb_path_query(b, '$.a[*].b') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a[*].b') ORDER BY a, jpq;

-- Test 16: query (line 126)
SELECT a, jsonb_path_query(b, '$.a') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a') ORDER BY a, jpq;

-- Test 17: query (line 141)
SELECT a, jsonb_path_query(b, '$.a') as jpq FROM json_tab ORDER BY a, jpq;

-- Test 18: query (line 154)
SELECT a, jsonb_path_query(b, '$.a') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a') ORDER BY a, jpq;

-- Test 19: query (line 167)
SELECT a, jsonb_path_query(b, '$.a[*]') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a[*]') ORDER BY a, jpq;

-- Test 20: query (line 181)
SELECT a, jsonb_path_query(b, '$.a[*]') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a[*]') ORDER BY a, jpq;

-- Test 21: statement (line 196)
INSERT INTO json_tab VALUES
(11, '{"a": [{"b": [{"x": {"y": {"z": "y"}}}, {"x": {"y": {"z": "yuu"}}}, {"x": {"y": {"z": ["y", "yuu"]}}}, [{"x": "y"}], [[[{"x": "y"}]]], {"xx": [{"zz":"oo"}]}]}, {"b": "e"}]}'),
(12, '{"a": [{"b": [{"x": {"y": {"ztrue": true}}},  {"x": {"y": {"ztrue": [true, "y1"]}}}]}]}'),
(13, '{"a": [{"b": [{"x": {"y": {"zfalse": false}}},  {"x": {"y": {"zfalse": [false, "y1"]}}}]}]}'),
(14, '{"a": [{"b": [{"x": {"y": {"zint": 10}}},  {"x": {"y": {"zint": [10, "y1"]}}}]}]}'),
(15, '{"a": [{"b": [{"x": {"y": {"znull": null}}},  {"x": {"y": {"znull": [null, "y1"]}}}]}]}');

-- Test 22: query (line 204)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.z == "y")') as jpq FROM json_tab ORDER BY a, jpq;

-- Test 23: query (line 210)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.z == "y")') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.z == "y")') ORDER BY a, jpq;

-- Test 24: query (line 217)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.z == "z")') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.z == "z")') ORDER BY a, jpq;

-- Test 25: query (line 221)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.ztrue == true)') as jpq FROM json_tab ORDER BY a, jpq;

-- Test 26: query (line 227)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.ztrue == true)') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.ztrue == true)') ORDER BY a, jpq;

-- Test 27: query (line 234)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.ztrue == false)') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.ztrue == false)') ORDER BY a, jpq;

-- Test 28: query (line 238)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.zfalse == false)') as jpq FROM json_tab ORDER BY a, jpq;

-- Test 29: query (line 244)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.zfalse == false)') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.zfalse == false)') ORDER BY a, jpq;

-- Test 30: query (line 251)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.zfalse == true)') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.zfalse == true)') ORDER BY a, jpq;

-- Test 31: query (line 255)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.zint == 10)') as jpq FROM json_tab ORDER BY a, jpq;

-- Test 32: query (line 261)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.zint == 10)') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.zint == 10)') ORDER BY a, jpq;

-- Test 33: query (line 268)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.zint == 12)') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.zint == 12)') ORDER BY a, jpq;

-- Test 34: query (line 272)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.znull == null)') as jpq FROM json_tab ORDER BY a, jpq;

-- Test 35: query (line 278)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.znull == null)') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.znull == null)') ORDER BY a, jpq;

-- Test 36: query (line 288)
SELECT a, jsonb_path_exists(b, '$.lllaaann.dddooo == 123123') FROM json_tab ORDER BY a;

-- Test 37: query (line 308)
SELECT a, jsonb_path_query(b, '$.lllaaann.dddooo == 123123') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.lllaaann.dddooo == 123123') ORDER BY a, jpq;

-- Test 38: query (line 327)
SELECT a, jsonb_path_exists(b, '$.a.b like_regex "hi.*"') FROM json_tab ORDER BY a;

-- Test 39: query (line 347)
SELECT a, jsonb_path_query(b, '$.a.b like_regex "hi.*"') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b like_regex "hi.*"') ORDER BY a, jpq;

-- Test 40: statement (line 367)
SELECT a, jsonb_path_query(b, '$.a.b like_regex "hi.*"') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b like_regex "hi.*"') ORDER BY a, jpq;

-- Test 41: statement (line 370)
SELECT a, jsonb_path_query(b, '$.lllaaann.dddooo == 123123') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.lllaaann.dddooo == 123123') ORDER BY a, jpq;

-- Test 42: query (line 374)
SELECT a, jsonb_path_query(b, '$.lllaaann.dddooo == 123123') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.lllaaann.dddooo == 123123') ORDER BY a, jpq;

-- Test 43: statement (line 394)
SELECT jsonb_path_query(b, '$.a.b') as jpq FROM json_tab ORDER BY a, jpq;

-- Test 44: query (line 400)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.z == "y")') as jpq1, jsonb_path_query(b, '$.a.b ? (@.x.y.ztrue == true)') as jpq2 FROM json_tab ORDER BY a, jpq1, jpq2;

-- Test 45: query (line 408)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.z == "y")') as jpq1, jsonb_path_query(b, '$.a.b ? (@.x.y.ztrue == true)') as jpq2 FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.z == "y")') ORDER BY a, jpq1, jpq2;

-- Test 46: query (line 414)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.z == "y")') as jpq1, jsonb_path_query(b, '$.a.b ? (@.x.y.ztrue == true)') as jpq2 FROM json_tab WHERE jsonb_path_exists(b, '$.a.b ? (@.x.y.z == "y")') OR jsonb_path_exists(b, '$.a.b ? (@.x.y.ztrue == true)') ORDER BY a, jpq1, jpq2;

-- Test 47: query (line 422)
SELECT a, jsonb_path_query(b, '$.a.b ? (@.x.y.z == "y")') as jpq1, jsonb_path_query(b, '$.a.b ? (@.x.y.ztrue == true)') as jpq2 FROM json_tab ORDER BY a, jpq1, jpq2;

-- Test 48: query (line 431)
SELECT a, jsonb_path_query(b, '$.a.b') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b') ORDER BY a, jpq;

-- Test 49: query (line 448)
SELECT a, jsonb_path_query(b, '$.a.d') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.d') ORDER BY a, jpq;

-- Test 50: query (line 455)
SELECT a, jsonb_path_query(b, '$.a.b'), jsonb_path_query(b, '$.a.d') as jpq FROM json_tab WHERE jsonb_path_exists(b, '$.a.b') AND jsonb_path_exists(b, '$.a.d') ORDER BY a, jpq;

-- Test 51: statement (line 462)
INSERT INTO json_tab VALUES
(16, '{"a": [[{"b": 1, "c": "aaa"}, {"b": 2, "c": "bbb"}, {"b": 1, "c": "ccc"}], [{"b": 1, "c": "zzz"}, {"b": 2, "c": "xxx"}, {"b": 1, "c": "yyy"}]]}'),
(17, '{"a": [[[{"b": 1, "c": "aaa"}, {"b": 2, "c": "bbb"}, {"b": 1, "c": "ccc"}], [{"b": 1, "c": "zzz"}, {"b": 2, "c": "xxx"}, {"b": 1, "c": "yyy"}]]]}'),
(18, '{"a": [[[[{"b": 1, "c": "aaa"}, {"b": 2, "c": "bbb"}, {"b": 1, "c": "ccc"}], [{"b": 1, "c": "zzz"}, {"b": 2, "c": "xxx"}, {"b": 1, "c": "yyy"}]]]]}');

-- Test 52: query (line 468)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$.a ? (@.b == 1)') ORDER BY a;

-- Test 53: query (line 474)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$.a ? (@.b == 1)') ORDER BY a;

-- Test 54: query (line 480)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$.a[*] ? (@.b == 1)') ORDER BY a;

-- Test 55: query (line 487)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$.a[*] ? (@.b == 1)') ORDER BY a;

-- Test 56: query (line 494)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$.a[*][*] ? (@.b == 1)') ORDER BY a;

-- Test 57: query (line 502)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$.a[*][*] ? (@.b == 1)') ORDER BY a;

-- Test 58: statement (line 512)
INSERT INTO json_tab VALUES (19, '{"a": {"b": "c"}}', '{"a": {"b": "c"}}');

-- Test 59: query (line 515)
SELECT a FROM json_tab WHERE jsonb_path_exists(c, '$.a.b') ORDER BY a;

-- Test 60: statement (line 520)
SELECT a FROM json_tab WHERE jsonb_path_exists(c, '$.a.b') ORDER BY a;

-- Test 61: query (line 523)
SELECT a FROM json_tab WHERE jsonb_path_exists('{"a": {"b": "c"}}', '$.a.b') ORDER BY a;

-- Test 62: statement (line 546)
SELECT a FROM json_tab WHERE jsonb_path_exists('{"a": {"b": "c"}}', '$.a.b') ORDER BY a;

-- Test 63: query (line 550)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$.a ? (@.b == $x)', '{"x": "c"}') ORDER BY a;

-- Test 64: statement (line 557)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$.a ? (@.b == $x)', '{"x": "c"}') ORDER BY a;

-- Test 65: statement (line 560)
INSERT INTO json_tab VALUES
(20,'[{"b": {"x": "y"}, "a": "x"}, null]'),
(21,'[[{"b": {"x": "y"}, "a": "x"}], null]');

-- Test 66: query (line 565)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$."b"[*]');

-- Test 67: query (line 570)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$.b');

-- Test 68: query (line 575)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$."b"[*]');

-- Test 69: query (line 580)
SELECT a FROM json_tab WHERE jsonb_path_exists(b, '$."b"[*]');

-- Test 70: statement (line 587)
DROP TABLE IF EXISTS anykey_json_tab;

-- Test 71: statement (line 590)
CREATE TABLE anykey_json_tab (
  a INT PRIMARY KEY,
  b JSONB
);

-- Test 72: statement (line 596)
CREATE INDEX anykey_inv ON anykey_json_tab USING gin (b);

-- Test 73: statement (line 599)
INSERT INTO anykey_json_tab VALUES
(1, '{"a": {"b": {"c": "d"}}}'),
(2, '{"a": {"b": {"c": {"d": "e"}}}}'),
(3, '{"a": {"b": [{"c": {"d": "e"}}]}}'),
(4, '{"a": {"b": ["c", "d"]}}'),
(5, '{"a": {"b": "d"}}'),
(6, '{"a": {"b1": "d"}}'),
(7, '{"a": {"b1": {"c": {"d": "e"}}}}');

-- Test 74: query (line 609)
SELECT a, b FROM anykey_json_tab WHERE jsonb_path_exists(b, '$.a.b.*') ORDER BY a;

-- Test 75: query (line 616)
SELECT a, b FROM anykey_json_tab WHERE jsonb_path_exists(b, '$.a.b.*') ORDER BY a;

-- Test 76: statement (line 624)
SELECT a, b FROM anykey_json_tab WHERE jsonb_path_exists(b, '$.a.*.b') ORDER BY a;

-- Test 77: statement (line 627)
SELECT a, b FROM anykey_json_tab WHERE jsonb_path_exists(b, '$.a.b.*.*') ORDER BY a;

RESET client_min_messages;
