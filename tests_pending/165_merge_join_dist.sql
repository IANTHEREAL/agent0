-- PostgreSQL compatible tests from merge_join_dist
-- 6 tests

SET client_min_messages = warning;

DROP TABLE IF EXISTS r;
DROP TABLE IF EXISTS l;

-- Test 1: statement (line 5)
CREATE TABLE l (a INT PRIMARY KEY, b INT);

-- Test 2: statement (line 8)
CREATE TABLE r (a INT PRIMARY KEY, b INT);

-- Test 3: statement (line 11)
INSERT INTO l VALUES (1, 10), (2, 20), (3, 30);

-- Test 4: statement (line 14)
INSERT INTO r VALUES (2, 200), (3, 300), (4, 400);

-- Test 5: query (line 47)
SELECT * FROM l LEFT OUTER JOIN r USING(a) WHERE a = 2;

-- Test 6: query (line 54)
SELECT * FROM l WHERE EXISTS(SELECT * FROM r WHERE r.a=l.a);

RESET client_min_messages;
