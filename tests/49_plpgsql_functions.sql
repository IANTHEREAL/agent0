-- Test PL/pgSQL function execution

-- Simple function returning a constant
CREATE FUNCTION get_answer() RETURNS integer AS $$
BEGIN
    RETURN 42;
END;
$$ LANGUAGE plpgsql;

SELECT get_answer();

-- Function with parameters
CREATE FUNCTION add_numbers(a integer, b integer) RETURNS integer AS $$
BEGIN
    RETURN a + b;
END;
$$ LANGUAGE plpgsql;

SELECT add_numbers(10, 20);

-- Function with DECLARE block and variables
CREATE FUNCTION calculate_sum(n integer) RETURNS integer AS $$
DECLARE
    result integer := 0;
    i integer := 1;
BEGIN
    result := n * 2;
    RETURN result;
END;
$$ LANGUAGE plpgsql;

SELECT calculate_sum(5);

-- Function with IF/THEN/ELSE
CREATE FUNCTION check_sign(n integer) RETURNS text AS $$
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

SELECT check_sign(5);
SELECT check_sign(-3);
SELECT check_sign(0);

-- Function with RAISE EXCEPTION
CREATE FUNCTION must_be_positive(n integer) RETURNS integer AS $$
BEGIN
    IF n < 0 THEN
        RAISE EXCEPTION 'Value must be positive';
    END IF;
    RETURN n;
END;
$$ LANGUAGE plpgsql;

SELECT must_be_positive(10);

-- SQL language function (simpler)
CREATE FUNCTION double_it(x integer) RETURNS integer AS $$
SELECT x * 2;
$$ LANGUAGE sql;

SELECT double_it(7);

-- Function used in expression
SELECT 'The answer is: ' || get_answer()::text;

-- Function in WHERE clause
CREATE TABLE test_plpgsql_nums (val integer);
INSERT INTO test_plpgsql_nums VALUES (10), (20), (30), (40), (50);
SELECT * FROM test_plpgsql_nums WHERE val > add_numbers(10, 15) ORDER BY val;

-- Nested IF statements
CREATE FUNCTION grade_score(score integer) RETURNS text AS $$
BEGIN
    IF score >= 90 THEN
        RETURN 'A';
    ELSIF score >= 80 THEN
        RETURN 'B';
    ELSIF score >= 70 THEN
        RETURN 'C';
    ELSIF score >= 60 THEN
        RETURN 'D';
    ELSE
        RETURN 'F';
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT grade_score(95);
SELECT grade_score(85);
SELECT grade_score(75);
SELECT grade_score(65);
SELECT grade_score(55);

-- Function with multiple variables
CREATE FUNCTION swap_and_sum(x integer, y integer) RETURNS integer AS $$
DECLARE
    temp integer;
    a integer := x;
    b integer := y;
BEGIN
    temp := a;
    a := b;
    b := temp;
    RETURN a + b;
END;
$$ LANGUAGE plpgsql;

SELECT swap_and_sum(3, 7);

-- Function with string operations
CREATE FUNCTION greet(name text) RETURNS text AS $$
DECLARE
    greeting text := 'Hello, ';
BEGIN
    RETURN greeting || name || '!';
END;
$$ LANGUAGE plpgsql;

SELECT greet('World');
SELECT greet('PostgreSQL');

-- Function with boolean logic
CREATE FUNCTION is_even(n integer) RETURNS boolean AS $$
BEGIN
    IF n % 2 = 0 THEN
        RETURN true;
    ELSE
        RETURN false;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT is_even(4);
SELECT is_even(7);

-- Function returning NULL
CREATE FUNCTION maybe_null(n integer) RETURNS integer AS $$
BEGIN
    IF n > 0 THEN
        RETURN n;
    ELSE
        RETURN NULL;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT maybe_null(5);
SELECT maybe_null(-5);

-- Function with float operations
CREATE FUNCTION circle_area(radius float) RETURNS float AS $$
DECLARE
    pi float := 3.14159;
BEGIN
    RETURN pi * radius * radius;
END;
$$ LANGUAGE plpgsql;

SELECT circle_area(2.0);

-- SQL function with multiple parameters
CREATE FUNCTION sql_multiply(a integer, b integer) RETURNS integer AS $$
SELECT a * b;
$$ LANGUAGE sql;

SELECT sql_multiply(6, 7);

-- Function used in ORDER BY
SELECT val, is_even(val) as even FROM test_plpgsql_nums ORDER BY is_even(val), val;

-- Function in CASE expression
SELECT val, 
    CASE WHEN is_even(val) THEN 'even' ELSE 'odd' END as parity
FROM test_plpgsql_nums ORDER BY val;

-- Chained function calls
SELECT add_numbers(add_numbers(1, 2), add_numbers(3, 4));

-- Function with default variable value
CREATE FUNCTION with_default() RETURNS integer AS $$
DECLARE
    x integer DEFAULT 100;
BEGIN
    RETURN x;
END;
$$ LANGUAGE plpgsql;

SELECT with_default();

-- Absolute value function
CREATE FUNCTION my_abs(n integer) RETURNS integer AS $$
BEGIN
    IF n < 0 THEN
        RETURN -n;
    ELSE
        RETURN n;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT my_abs(5);
SELECT my_abs(-5);
SELECT my_abs(0);

-- Max of two numbers
CREATE FUNCTION my_max(a integer, b integer) RETURNS integer AS $$
BEGIN
    IF a > b THEN
        RETURN a;
    ELSE
        RETURN b;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT my_max(10, 20);
SELECT my_max(30, 15);
SELECT my_max(7, 7);

-- Factorial (iterative simulation via variable)
CREATE FUNCTION simple_factorial(n integer) RETURNS integer AS $$
DECLARE
    result integer := 1;
BEGIN
    IF n <= 1 THEN
        RETURN 1;
    ELSIF n = 2 THEN
        RETURN 2;
    ELSIF n = 3 THEN
        RETURN 6;
    ELSIF n = 4 THEN
        RETURN 24;
    ELSIF n = 5 THEN
        RETURN 120;
    ELSE
        RETURN -1;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT simple_factorial(1);
SELECT simple_factorial(3);
SELECT simple_factorial(5);

-- Cleanup
DROP TABLE test_plpgsql_nums;
DROP FUNCTION get_answer;
DROP FUNCTION add_numbers;
DROP FUNCTION calculate_sum;
DROP FUNCTION check_sign;
DROP FUNCTION must_be_positive;
DROP FUNCTION double_it;
DROP FUNCTION grade_score;
DROP FUNCTION swap_and_sum;
DROP FUNCTION greet;
DROP FUNCTION is_even;
DROP FUNCTION maybe_null;
DROP FUNCTION circle_area;
DROP FUNCTION sql_multiply;
DROP FUNCTION with_default;
DROP FUNCTION my_abs;
DROP FUNCTION my_max;
DROP FUNCTION simple_factorial;
