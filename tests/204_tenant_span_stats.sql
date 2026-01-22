-- PostgreSQL compatible tests from tenant_span_stats
-- 8 tests

SET client_min_messages = warning;
DROP ROLE IF EXISTS testuser2;
DROP SCHEMA IF EXISTS a CASCADE;

-- Test 1: statement (line 4)
-- CockroachDB databases are closer to PostgreSQL schemas in terms of
-- namespacing within a single test database.
CREATE SCHEMA a;

-- Test 2: statement (line 8)
CREATE TABLE a.b (id INT PRIMARY KEY);

-- Test 3: statement (line 12)
INSERT INTO a.b SELECT generate_series(1, 10);

-- Test 4: statement (line 16)
CREATE TABLE a.c (id INT PRIMARY KEY);

-- Test 5: statement (line 79)
SELECT * FROM a.b;

-- Test 6: statement (line 90)
CREATE USER testuser2;

-- Test 7: statement (line 119)
-- CockroachDB-only role options (no PostgreSQL equivalent).
-- ALTER ROLE testuser2 WITH NOVIEWACTIVITY;

-- Test 8: statement (line 123)
-- ALTER ROLE testuser2 WITH VIEWACTIVITYREDACTED;

RESET client_min_messages;
