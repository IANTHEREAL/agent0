-- PostgreSQL compatible tests from distsql_expr
-- 3 tests

-- Test 1: statement (line 1)
SET client_min_messages = warning;

CREATE TABLE t (c INT PRIMARY KEY);

-- Test 2: statement (line 4)
INSERT INTO t VALUES (1), (2), (3);

-- Test 3: query (line 8)
SELECT c FROM t WHERE (c, c) > (2, -9223372036854775808) ORDER BY c;

RESET client_min_messages;
