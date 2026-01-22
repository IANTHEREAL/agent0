-- PostgreSQL compatible tests from alias_types
-- 10 tests

-- Test 1: statement (line 1)
SET client_min_messages = warning;
DROP TABLE IF EXISTS aliases;
CREATE TABLE aliases (
    a OID,
    b NAME
);

-- onlyif config schema-locked-disabled

-- Test 2: query (line 10)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'aliases'
ORDER BY ordinal_position;

-- Test 3: query (line 24)
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = 'public'
  AND table_name = 'aliases'
ORDER BY ordinal_position;

-- Test 4: statement (line 37)
INSERT INTO aliases VALUES (100, 'abc');

-- Test 5: statement (line 40)
INSERT INTO aliases VALUES (2, 'def');

-- Test 6: query (line 46)
SELECT a, b FROM aliases ORDER BY a;

-- Test 7: query (line 53)
SELECT pg_typeof(a), pg_typeof(b) FROM aliases LIMIT 1;

-- Test 8: query (line 58)
SELECT b || 'cat' FROM aliases ORDER BY a;

-- Test 9: query (line 65)
SELECT reverse(b) FROM aliases ORDER BY a;

-- Test 10: query (line 72)
SELECT length(convert_to(b, 'UTF8')) FROM aliases ORDER BY a;

RESET client_min_messages;
