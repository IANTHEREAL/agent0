-- PostgreSQL compatible tests from vectorize_agg
-- 14 tests

SET client_min_messages = warning;

-- Test 1: statement (line 1)
DROP TABLE IF EXISTS bools;
CREATE TABLE bools (a INT, b BOOL);

-- Test 2: query (line 4)
SELECT bool_and(b), bool_or(b) FROM bools;

-- Test 3: query (line 9)
SELECT bool_and(b), bool_or(b) FROM bools GROUP BY a;

-- Test 4: statement (line 13)
INSERT INTO bools VALUES
(0, NULL),
(1, true),  (1, true),
(2, false), (2, false),
(3, false), (3, true), (3, true),
(4, NULL),  (4, true),
(5, false), (5, NULL);

-- Test 5: query (line 22)
SELECT bool_and(b), bool_or(b) FROM bools GROUP BY a ORDER BY a;

DROP TABLE IF EXISTS bytes_string;
CREATE TABLE bytes_string (_group INT, _bytes BYTEA, _string TEXT);

-- Test 6: query (line 35)
SELECT
  string_agg(_bytes, ''::bytea ORDER BY _group, _bytes),
  string_agg(_string, '' ORDER BY _group, _string)
FROM bytes_string;

-- Test 7: query (line 40)
SELECT
  string_agg(_bytes, ''::bytea ORDER BY _bytes),
  string_agg(_string, '' ORDER BY _string)
FROM bytes_string
GROUP BY _group;

-- Test 8: statement (line 44)
INSERT INTO bytes_string VALUES
(0, NULL, NULL),
(1, convert_to('1', 'UTF8'), '1'),
(2, convert_to('2', 'UTF8'), '2'), (2, convert_to('2', 'UTF8'), '2'),
(3, convert_to('3', 'UTF8'), '3'), (3, NULL, NULL), (3, convert_to('3', 'UTF8'), '3');

-- Test 9: query (line 51)
SELECT
  string_agg(_bytes, ''::bytea ORDER BY _bytes),
  string_agg(_string, '' ORDER BY _string)
FROM bytes_string
GROUP BY _group
ORDER BY _group;

-- Test 10: statement (line 61)
DROP TABLE IF EXISTS t87619;
CREATE TABLE t87619 (b) AS SELECT true;
-- CockroachDB-only session settings.
-- SET testing_optimizer_random_seed = 3164997759865821235;
-- SET testing_optimizer_disable_rule_probability = 0.500000;

-- Test 11: query (line 66)
SELECT true FROM t87619 GROUP BY b HAVING b;

-- Test 12: statement (line 71)
-- CockroachDB-only session settings.
-- RESET testing_optimizer_random_seed;
-- RESET testing_optimizer_disable_rule_probability;

-- Test 13: statement (line 77)
DROP TABLE IF EXISTS t97603;
CREATE TABLE t97603 (id INT PRIMARY KEY);
INSERT INTO t97603 SELECT generate_series(1, 50000);

-- Test 14: statement (line 83)
SELECT count(*)
FROM (
  SELECT
    var_pop(txid_current()::numeric),
    1::oid
  FROM t97603
  GROUP BY id
  HAVING bool_or(true)
) AS q;

RESET client_min_messages;
