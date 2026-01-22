-- PostgreSQL compatible tests from collatedstring_nullinindex
-- 5 tests

SET client_min_messages = warning;

CREATE COLLATION IF NOT EXISTS en (provider = icu, locale = 'en');

DROP TABLE IF EXISTS t;
CREATE TABLE t (
    a INT,
    b TEXT COLLATE en
);

-- Test 1: statement (line 7)
INSERT INTO t VALUES (1, 'foo' COLLATE en), (2, NULL), (3, 'bar' COLLATE en);

-- Test 2: statement (line 10)
CREATE INDEX ON t (b, a);

-- Test 3: statement (line 14)
INSERT INTO t (a) VALUES(4);

-- Test 4: statement (line 17)
INSERT INTO t VALUES(5, NULL);

-- Test 5: query (line 20)
SELECT b FROM t ORDER BY b;

RESET client_min_messages;
