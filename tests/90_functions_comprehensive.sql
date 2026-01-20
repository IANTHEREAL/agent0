-- Comprehensive function tests for pg-tikv
-- Tests string, math, date/time, conditional, array, and JSON functions

--------------------------------------------------------------------------------
-- STRING FUNCTIONS
--------------------------------------------------------------------------------

-- Basic string manipulation
SELECT UPPER('hello world');
SELECT LOWER('HELLO WORLD');
SELECT INITCAP('hello world');
SELECT LENGTH('hello');
SELECT CHAR_LENGTH('hello');
SELECT OCTET_LENGTH('hello');
SELECT BIT_LENGTH('hello');

-- Concatenation
SELECT CONCAT('a', 'b', 'c', 'd');
SELECT CONCAT_WS('-', 'a', 'b', 'c');
SELECT CONCAT_WS(',', 'one', NULL, 'three');
SELECT 'Hello' || ' ' || 'World';

-- Substring extraction
SELECT LEFT('PostgreSQL', 4);
SELECT RIGHT('PostgreSQL', 3);
SELECT SUBSTR('PostgreSQL', 5);
SELECT SUBSTR('PostgreSQL', 5, 3);
SELECT SUBSTRING('PostgreSQL' FROM 5);
SELECT SUBSTRING('PostgreSQL' FROM 5 FOR 3);

-- Padding
SELECT LPAD('42', 5, '0');
SELECT RPAD('hi', 5, '*');
SELECT LPAD('hello', 3);

-- Trimming
SELECT TRIM('  hello  ');
SELECT LTRIM('  hello');
SELECT RTRIM('hello  ');
SELECT BTRIM('xxhelloxx', 'x');

-- Search and replace
SELECT REPLACE('hello world', 'world', 'PostgreSQL');
SELECT TRANSLATE('hello', 'el', 'ip');
SELECT STRPOS('hello', 'll');
SELECT POSITION('ll' IN 'hello');

-- Other string functions
SELECT REVERSE('hello');
SELECT REPEAT('ab', 3);
SELECT SPLIT_PART('a.b.c.d', '.', 2);
SELECT SPLIT_PART('a.b.c.d', '.', 5);
SELECT ASCII('A');
SELECT CHR(65);
SELECT MD5('hello');

-- Encoding
SELECT ENCODE('hello'::bytea, 'base64');
SELECT ENCODE('hello'::bytea, 'hex');

--------------------------------------------------------------------------------
-- MATH FUNCTIONS
--------------------------------------------------------------------------------

-- Basic math
SELECT ABS(-42);
SELECT ABS(-3.14);
SELECT SIGN(-5);
SELECT SIGN(0);
SELECT SIGN(10);

-- Rounding
SELECT CEIL(4.2);
SELECT CEILING(4.2);
SELECT FLOOR(4.8);
SELECT ROUND(4.567);
SELECT ROUND(4.567, 2);
SELECT TRUNC(4.567);
SELECT TRUNC(4.567, 1);

-- Powers and roots
SELECT SQRT(16);
SELECT CBRT(27);
SELECT POWER(2, 10);
SELECT POW(2, 8);
SELECT EXP(1);

-- Logarithms
SELECT LN(2.718281828);
SELECT LOG(100);
SELECT LOG10(1000);

-- Trigonometry
SELECT SIN(0);
SELECT COS(0);
SELECT TAN(0);
SELECT PI();
SELECT DEGREES(PI());
SELECT RADIANS(180);

-- Modulo
SELECT MOD(17, 5);
SELECT MOD(10, 3);

-- Random (just check it returns something)
SELECT RANDOM() >= 0 AND RANDOM() < 1 AS random_in_range;

--------------------------------------------------------------------------------
-- CONDITIONAL FUNCTIONS
--------------------------------------------------------------------------------

-- COALESCE
SELECT COALESCE(NULL, NULL, 'default');
SELECT COALESCE('first', NULL, 'default');
SELECT COALESCE(NULL, 42, 100);

-- NULLIF
SELECT NULLIF(5, 5);
SELECT NULLIF(5, 3);
SELECT NULLIF('hello', 'hello');
SELECT NULLIF('hello', 'world');

-- GREATEST / LEAST
SELECT GREATEST(1, 5, 3, 9, 2);
SELECT LEAST(1, 5, 3, 9, 2);
SELECT GREATEST('apple', 'banana', 'cherry');
SELECT LEAST('apple', 'banana', 'cherry');

-- CASE expressions
SELECT CASE WHEN 1 = 1 THEN 'yes' ELSE 'no' END;
SELECT CASE WHEN 1 = 2 THEN 'yes' ELSE 'no' END;
SELECT CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' WHEN 3 THEN 'three' ELSE 'other' END;
SELECT CASE WHEN NULL IS NULL THEN 'null is null' ELSE 'unexpected' END;

--------------------------------------------------------------------------------
-- TYPE CASTING
--------------------------------------------------------------------------------

SELECT CAST(123 AS TEXT);
SELECT CAST('456' AS INTEGER);
SELECT CAST(3.14 AS INTEGER);
SELECT CAST('3.14' AS DOUBLE PRECISION);
SELECT CAST(TRUE AS INTEGER);
SELECT CAST(1 AS BOOLEAN);
SELECT 123::TEXT;
SELECT '456'::INTEGER;

--------------------------------------------------------------------------------
-- ARRAY FUNCTIONS
--------------------------------------------------------------------------------

SELECT ARRAY[1, 2, 3];
SELECT ARRAY['a', 'b', 'c'];
SELECT ARRAY_LENGTH(ARRAY[1, 2, 3, 4, 5], 1);
SELECT ARRAY_UPPER(ARRAY[1, 2, 3], 1);
SELECT ARRAY_LOWER(ARRAY[1, 2, 3], 1);
SELECT CARDINALITY(ARRAY[1, 2, 3, 4]);
SELECT ARRAY_POSITION(ARRAY['a', 'b', 'c'], 'b');
SELECT ARRAY_CAT(ARRAY[1, 2], ARRAY[3, 4]);
SELECT ARRAY_APPEND(ARRAY[1, 2, 3], 4);
SELECT ARRAY_PREPEND(0, ARRAY[1, 2, 3]);
SELECT ARRAY_REMOVE(ARRAY[1, 2, 3, 2, 1], 2);
SELECT ARRAY_TO_STRING(ARRAY[1, 2, 3], ',');
SELECT ARRAY_TO_STRING(ARRAY['a', 'b', NULL, 'c'], ',', 'N/A');
SELECT STRING_TO_ARRAY('a,b,c', ',');

--------------------------------------------------------------------------------
-- JSON FUNCTIONS
--------------------------------------------------------------------------------

-- JSON creation
SELECT JSON_BUILD_OBJECT('name', 'Alice', 'age', 30);
SELECT JSONB_BUILD_OBJECT('key', 'value');
SELECT JSON_BUILD_ARRAY(1, 2, 'three', true);
SELECT JSONB_BUILD_ARRAY('a', 'b', 'c');

-- JSON inspection
SELECT JSON_TYPEOF('{"a": 1}'::json);
SELECT JSONB_TYPEOF('"hello"'::jsonb);
SELECT JSONB_TYPEOF('123'::jsonb);
SELECT JSONB_TYPEOF('[1,2,3]'::jsonb);
SELECT JSONB_TYPEOF('true'::jsonb);
SELECT JSONB_TYPEOF('null'::jsonb);

-- JSON array operations
SELECT JSONB_ARRAY_LENGTH('[1, 2, 3, 4, 5]'::jsonb);
SELECT JSONB_ARRAY_LENGTH('["a", "b", "c"]'::jsonb);

-- JSON object operations
SELECT JSONB_EXISTS('{"a": 1, "b": 2}'::jsonb, 'a');
SELECT JSONB_EXISTS('{"a": 1, "b": 2}'::jsonb, 'c');
SELECT JSONB_EXISTS_ANY('{"a": 1, "b": 2}'::jsonb, ARRAY['c', 'd', 'a']);
SELECT JSONB_EXISTS_ALL('{"a": 1, "b": 2}'::jsonb, ARRAY['a', 'b']);
SELECT JSONB_EXISTS_ALL('{"a": 1, "b": 2}'::jsonb, ARRAY['a', 'c']);

-- JSON extraction
SELECT JSONB_EXTRACT_PATH('{"a": {"b": {"c": 1}}}'::jsonb, 'a', 'b', 'c');
SELECT JSONB_EXTRACT_PATH_TEXT('{"name": "Alice"}'::jsonb, 'name');

-- JSON operators
SELECT '{"a": 1}'::jsonb -> 'a';
SELECT '{"a": {"b": 2}}'::jsonb -> 'a' -> 'b';
SELECT '{"name": "Alice"}'::jsonb ->> 'name';
SELECT '[1, 2, 3]'::jsonb -> 0;
SELECT '[1, 2, 3]'::jsonb ->> 1;

--------------------------------------------------------------------------------
-- DATE/TIME FUNCTIONS (using fixed values for determinism)
--------------------------------------------------------------------------------

-- Date arithmetic
SELECT DATE '2024-01-15' + INTERVAL '10 days';
SELECT DATE '2024-01-15' - INTERVAL '5 days';

-- EXTRACT
SELECT EXTRACT(YEAR FROM TIMESTAMP '2024-06-15 10:30:00');
SELECT EXTRACT(MONTH FROM TIMESTAMP '2024-06-15 10:30:00');
SELECT EXTRACT(DAY FROM TIMESTAMP '2024-06-15 10:30:00');
SELECT EXTRACT(HOUR FROM TIMESTAMP '2024-06-15 10:30:00');
SELECT EXTRACT(MINUTE FROM TIMESTAMP '2024-06-15 10:30:00');
SELECT EXTRACT(SECOND FROM TIMESTAMP '2024-06-15 10:30:45');
SELECT EXTRACT(DOW FROM DATE '2024-06-15');
SELECT EXTRACT(DOY FROM DATE '2024-06-15');

-- DATE_TRUNC
SELECT DATE_TRUNC('year', TIMESTAMP '2024-06-15 10:30:45');
SELECT DATE_TRUNC('month', TIMESTAMP '2024-06-15 10:30:45');
SELECT DATE_TRUNC('day', TIMESTAMP '2024-06-15 10:30:45');
SELECT DATE_TRUNC('hour', TIMESTAMP '2024-06-15 10:30:45');

-- AGE
SELECT AGE(TIMESTAMP '2024-06-15', TIMESTAMP '2020-01-01');

-- DATE function
SELECT DATE(TIMESTAMP '2024-06-15 10:30:00');
SELECT DATE('2024-06-15');

--------------------------------------------------------------------------------
-- UUID FUNCTIONS
--------------------------------------------------------------------------------

-- gen_random_uuid returns valid UUID format
SELECT LENGTH(gen_random_uuid()::text) AS uuid_length;

--------------------------------------------------------------------------------
-- FORMAT FUNCTION
--------------------------------------------------------------------------------

SELECT FORMAT('Hello, %s!', 'World');
SELECT FORMAT('Value: %s, Count: %s', 'test', 42);
SELECT FORMAT('Name: %I', 'table_name');
SELECT FORMAT('Literal: %L', 'it''s a test');
SELECT FORMAT('Width: %10s', 'hi');
SELECT FORMAT('Left: %-10s', 'hi');

--------------------------------------------------------------------------------
-- PATTERN MATCHING
--------------------------------------------------------------------------------

CREATE TABLE test_patterns (id INT PRIMARY KEY, val TEXT);
INSERT INTO test_patterns VALUES (1, 'Hello World');
INSERT INTO test_patterns VALUES (2, 'hello world');
INSERT INTO test_patterns VALUES (3, 'HELLO WORLD');
INSERT INTO test_patterns VALUES (4, 'PostgreSQL');
INSERT INTO test_patterns VALUES (5, 'pg-tikv');

-- LIKE
SELECT id, val FROM test_patterns WHERE val LIKE 'Hello%' ORDER BY id;
SELECT id, val FROM test_patterns WHERE val LIKE '%World' ORDER BY id;
SELECT id, val FROM test_patterns WHERE val LIKE '%lo%' ORDER BY id;
SELECT id, val FROM test_patterns WHERE val LIKE 'H_llo%' ORDER BY id;

-- ILIKE (case insensitive)
SELECT id, val FROM test_patterns WHERE val ILIKE 'hello%' ORDER BY id;
SELECT id, val FROM test_patterns WHERE val ILIKE '%WORLD' ORDER BY id;

-- NOT LIKE
SELECT id, val FROM test_patterns WHERE val NOT LIKE '%World%' ORDER BY id;

DROP TABLE test_patterns;

--------------------------------------------------------------------------------
-- COMPARISON OPERATORS
--------------------------------------------------------------------------------

-- BETWEEN
SELECT 5 BETWEEN 1 AND 10;
SELECT 15 BETWEEN 1 AND 10;
SELECT 5 NOT BETWEEN 10 AND 20;
SELECT 'b' BETWEEN 'a' AND 'c';

-- IN
SELECT 5 IN (1, 3, 5, 7, 9);
SELECT 4 IN (1, 3, 5, 7, 9);
SELECT 'b' IN ('a', 'b', 'c');
SELECT 'd' NOT IN ('a', 'b', 'c');

-- IS NULL / IS NOT NULL
SELECT NULL IS NULL;
SELECT 5 IS NULL;
SELECT NULL IS NOT NULL;
SELECT 5 IS NOT NULL;



--------------------------------------------------------------------------------
-- BOOLEAN OPERATORS
--------------------------------------------------------------------------------

SELECT TRUE AND TRUE;
SELECT TRUE AND FALSE;
SELECT TRUE OR FALSE;
SELECT FALSE OR FALSE;
SELECT NOT TRUE;
SELECT NOT FALSE;

--------------------------------------------------------------------------------
-- SYSTEM FUNCTIONS
--------------------------------------------------------------------------------

SELECT VERSION() IS NOT NULL AS has_version;
SELECT CURRENT_DATABASE();
SELECT CURRENT_SCHEMA();
SELECT PG_BACKEND_PID() > 0 AS has_pid;
