-- Comprehensive User-Defined Function (UDF) Integration Tests

-- ============================================
-- SECTION 1: Basic PL/pgSQL Functions
-- ============================================

DROP FUNCTION IF EXISTS basic_add(integer, integer);
CREATE FUNCTION basic_add(a integer, b integer) RETURNS integer AS $$
BEGIN
    RETURN a + b;
END;
$$ LANGUAGE plpgsql;

SELECT basic_add(10, 20) AS basic_sum;

DROP FUNCTION IF EXISTS basic_multiply(integer, integer);
CREATE FUNCTION basic_multiply(x integer, y integer) RETURNS integer AS $$
BEGIN
    RETURN x * y;
END;
$$ LANGUAGE plpgsql;

SELECT basic_multiply(6, 7) AS basic_product;

-- ============================================
-- SECTION 2: CREATE OR REPLACE FUNCTION
-- ============================================

DROP FUNCTION IF EXISTS replace_test();
CREATE FUNCTION replace_test() RETURNS integer AS $$
BEGIN
    RETURN 1;
END;
$$ LANGUAGE plpgsql;

SELECT replace_test() AS version1;

CREATE OR REPLACE FUNCTION replace_test() RETURNS integer AS $$
BEGIN
    RETURN 2;
END;
$$ LANGUAGE plpgsql;

SELECT replace_test() AS version2;

CREATE OR REPLACE FUNCTION replace_test() RETURNS integer AS $$
BEGIN
    RETURN 3;
END;
$$ LANGUAGE plpgsql;

SELECT replace_test() AS version3;

DROP FUNCTION replace_test();

-- ============================================
-- SECTION 3: Functions in SELECT Context
-- ============================================

DROP TABLE IF EXISTS udf_nums_test;
CREATE TABLE udf_nums_test (id INTEGER PRIMARY KEY, val INTEGER);
INSERT INTO udf_nums_test (id, val) VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50);

DROP FUNCTION IF EXISTS double_val(integer);
CREATE FUNCTION double_val(x integer) RETURNS integer AS $$
BEGIN
    RETURN x * 2;
END;
$$ LANGUAGE plpgsql;

SELECT id, double_val(val) AS doubled FROM udf_nums_test ORDER BY id;

-- ============================================
-- SECTION 4: Function in WHERE clause
-- ============================================

DROP FUNCTION IF EXISTS is_large(integer);
CREATE FUNCTION is_large(x integer) RETURNS boolean AS $$
BEGIN
    RETURN x > 25;
END;
$$ LANGUAGE plpgsql;

SELECT id, val FROM udf_nums_test WHERE is_large(val) ORDER BY id;

-- ============================================
-- SECTION 5: Nested function calls
-- ============================================

SELECT double_val(double_val(10)) AS quad_10;

-- ============================================
-- SECTION 6: Function with text return
-- ============================================

DROP FUNCTION IF EXISTS greet(text);
CREATE FUNCTION greet(name text) RETURNS text AS $$
DECLARE
    greeting text;
BEGIN
    greeting := 'Hello, ' || name || '!';
    RETURN greeting;
END;
$$ LANGUAGE plpgsql;

SELECT greet('World') AS greeting;
SELECT greet('PostgreSQL') AS greeting2;

-- ============================================
-- SECTION 7: SQL Language Functions
-- ============================================

DROP FUNCTION IF EXISTS sql_add(integer, integer);
CREATE FUNCTION sql_add(a integer, b integer) RETURNS integer AS $$
SELECT a + b;
$$ LANGUAGE sql;

SELECT sql_add(100, 200) AS sql_sum;

DROP FUNCTION IF EXISTS sql_concat(text, text);
CREATE FUNCTION sql_concat(a text, b text) RETURNS text AS $$
SELECT a || ' ' || b;
$$ LANGUAGE sql;

SELECT sql_concat('Hello', 'SQL') AS sql_greeting;

-- ============================================
-- SECTION 8: IF/ELSIF/ELSE Control Flow
-- ============================================

DROP FUNCTION IF EXISTS check_value(integer);
CREATE FUNCTION check_value(n integer) RETURNS text AS $$
BEGIN
    IF n > 0 THEN
        RETURN 'positive';
    ELSIF n < 0 THEN
        RETURN 'negative';
    ELSE
        RETURN 'zero';
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT check_value(5) AS pos_check;
SELECT check_value(-3) AS neg_check;
SELECT check_value(0) AS zero_check;

-- ============================================
-- SECTION 9: Function with DECLARE variables
-- ============================================

DROP FUNCTION IF EXISTS with_vars();
CREATE FUNCTION with_vars() RETURNS text AS $$
DECLARE
    counter integer DEFAULT 0;
    message text := 'start';
BEGIN
    counter := counter + 1;
    message := message || ':done';
    RETURN message || ':' || counter::text;
END;
$$ LANGUAGE plpgsql;

SELECT with_vars() AS var_result;

-- ============================================
-- SECTION 10: Function in CASE expression
-- ============================================

SELECT id, 
    CASE WHEN is_large(val) THEN 'large' ELSE 'small' END AS size_label
FROM udf_nums_test ORDER BY id;

-- ============================================
-- SECTION 11: Function in subquery
-- ============================================

SELECT * FROM (
    SELECT id, double_val(val) AS dval FROM udf_nums_test
) sub WHERE dval > 50 ORDER BY id;

-- ============================================
-- SECTION 12: Function with boolean return
-- ============================================

DROP FUNCTION IF EXISTS is_even(integer);
CREATE FUNCTION is_even(n integer) RETURNS boolean AS $$
BEGIN
    IF n % 2 = 0 THEN
        RETURN true;
    ELSE
        RETURN false;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT is_even(4) AS four_even;
SELECT is_even(7) AS seven_even;

-- ============================================
-- SECTION 13: DROP FUNCTION variations
-- ============================================

DROP FUNCTION IF EXISTS to_drop();
CREATE FUNCTION to_drop() RETURNS integer AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql;
DROP FUNCTION to_drop();

DROP FUNCTION IF EXISTS nonexistent_fn();

-- ============================================
-- SECTION 14: Function with ORDER BY
-- ============================================

SELECT id, val FROM udf_nums_test ORDER BY double_val(val) DESC;

-- ============================================
-- SECTION 15: Combined with built-in functions
-- ============================================

SELECT UPPER(greet('world')) AS upper_greet;

-- ============================================
-- CLEANUP
-- ============================================

DROP TABLE IF EXISTS udf_nums_test;
DROP FUNCTION IF EXISTS basic_add(integer, integer);
DROP FUNCTION IF EXISTS basic_multiply(integer, integer);
DROP FUNCTION IF EXISTS double_val(integer);
DROP FUNCTION IF EXISTS is_large(integer);
DROP FUNCTION IF EXISTS greet(text);
DROP FUNCTION IF EXISTS sql_add(integer, integer);
DROP FUNCTION IF EXISTS sql_concat(text, text);
DROP FUNCTION IF EXISTS check_value(integer);
DROP FUNCTION IF EXISTS with_vars();
DROP FUNCTION IF EXISTS is_even(integer);
