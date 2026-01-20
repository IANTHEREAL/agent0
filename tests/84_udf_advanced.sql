-- Advanced UDF tests based on current plpgsql.rs implementation

-- ============================================
-- SECTION 1: RAISE NOTICE and RAISE EXCEPTION
-- ============================================

DROP FUNCTION IF EXISTS test_raise_exception(integer);
CREATE FUNCTION test_raise_exception(n integer) RETURNS integer AS $$
BEGIN
    IF n < 0 THEN
        RAISE EXCEPTION 'Value must be non-negative, got: %', n;
    END IF;
    RETURN n * 2;
END;
$$ LANGUAGE plpgsql;

SELECT test_raise_exception(5) AS valid_result;

DROP FUNCTION IF EXISTS test_raise_simple(integer);
CREATE FUNCTION test_raise_simple(n integer) RETURNS text AS $$
BEGIN
    IF n = 0 THEN
        RAISE 'Zero is not allowed';
    END IF;
    RETURN 'OK';
END;
$$ LANGUAGE plpgsql;

SELECT test_raise_simple(1) AS ok_result;

-- ============================================
-- SECTION 2: Nested IF Statements
-- ============================================

DROP FUNCTION IF EXISTS nested_if_test(integer, integer);
CREATE FUNCTION nested_if_test(a integer, b integer) RETURNS text AS $$
BEGIN
    IF a > 0 THEN
        IF b > 0 THEN
            RETURN 'both positive';
        ELSE
            RETURN 'a positive, b non-positive';
        END IF;
    ELSE
        IF b > 0 THEN
            RETURN 'a non-positive, b positive';
        ELSE
            RETURN 'both non-positive';
        END IF;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT nested_if_test(1, 1) AS both_pos;
SELECT nested_if_test(1, -1) AS a_pos;
SELECT nested_if_test(-1, 1) AS b_pos;
SELECT nested_if_test(-1, -1) AS both_neg;

DROP FUNCTION IF EXISTS deep_nested_if(integer);
CREATE FUNCTION deep_nested_if(n integer) RETURNS text AS $$
BEGIN
    IF n > 100 THEN
        IF n > 200 THEN
            IF n > 300 THEN
                RETURN 'very high';
            ELSE
                RETURN 'high';
            END IF;
        ELSE
            RETURN 'medium high';
        END IF;
    ELSE
        RETURN 'low';
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT deep_nested_if(350) AS very_high;
SELECT deep_nested_if(250) AS high;
SELECT deep_nested_if(150) AS medium_high;
SELECT deep_nested_if(50) AS low;

-- ============================================
-- SECTION 3: Multiple Variable Assignments
-- ============================================

DROP FUNCTION IF EXISTS multi_assign();
CREATE FUNCTION multi_assign() RETURNS text AS $$
DECLARE
    x integer := 1;
    y integer := 2;
    z integer := 3;
    result text;
BEGIN
    x := x + 10;
    y := y + 20;
    z := z + 30;
    result := x::text || ',' || y::text || ',' || z::text;
    RETURN result;
END;
$$ LANGUAGE plpgsql;

SELECT multi_assign() AS multi_result;

DROP FUNCTION IF EXISTS chain_assign();
CREATE FUNCTION chain_assign() RETURNS integer AS $$
DECLARE
    a integer := 1;
    b integer;
    c integer;
BEGIN
    b := a + 5;
    c := b * 2;
    a := c - 3;
    RETURN a;
END;
$$ LANGUAGE plpgsql;

SELECT chain_assign() AS chain_result;

DROP FUNCTION IF EXISTS swap_values(integer, integer);
CREATE FUNCTION swap_values(x integer, y integer) RETURNS text AS $$
DECLARE
    temp integer;
    a integer := x;
    b integer := y;
BEGIN
    temp := a;
    a := b;
    b := temp;
    RETURN 'swapped: ' || a::text || ', ' || b::text;
END;
$$ LANGUAGE plpgsql;

SELECT swap_values(10, 20) AS swapped;

-- ============================================
-- SECTION 4: Type Casting in Functions
-- ============================================

DROP FUNCTION IF EXISTS int_to_text(integer);
CREATE FUNCTION int_to_text(n integer) RETURNS text AS $$
BEGIN
    RETURN 'Number: ' || n::text;
END;
$$ LANGUAGE plpgsql;

SELECT int_to_text(42) AS int_text;

DROP FUNCTION IF EXISTS float_to_int(float);
CREATE FUNCTION float_to_int(f float) RETURNS integer AS $$
DECLARE
    result integer;
BEGIN
    result := f::integer;
    RETURN result;
END;
$$ LANGUAGE plpgsql;

SELECT float_to_int(3.7) AS float_int;

DROP FUNCTION IF EXISTS bool_to_text(boolean);
CREATE FUNCTION bool_to_text(b boolean) RETURNS text AS $$
BEGIN
    IF b THEN
        RETURN 'yes';
    ELSE
        RETURN 'no';
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT bool_to_text(true) AS bool_yes;
SELECT bool_to_text(false) AS bool_no;

-- ============================================
-- SECTION 5: NULL Handling
-- ============================================

DROP FUNCTION IF EXISTS null_check(integer);
CREATE FUNCTION null_check(n integer) RETURNS text AS $$
BEGIN
    IF n IS NULL THEN
        RETURN 'is null';
    ELSE
        RETURN 'not null: ' || n::text;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT null_check(5) AS not_null;
SELECT null_check(NULL) AS is_null;

DROP FUNCTION IF EXISTS coalesce_test(integer, integer);
CREATE FUNCTION coalesce_test(a integer, b integer) RETURNS integer AS $$
DECLARE
    result integer;
BEGIN
    IF a IS NOT NULL THEN
        result := a;
    ELSIF b IS NOT NULL THEN
        result := b;
    ELSE
        result := 0;
    END IF;
    RETURN result;
END;
$$ LANGUAGE plpgsql;

SELECT coalesce_test(10, 20) AS first;
SELECT coalesce_test(NULL, 20) AS second;
SELECT coalesce_test(NULL, NULL) AS zero;

DROP FUNCTION IF EXISTS return_null_explicit();
CREATE FUNCTION return_null_explicit() RETURNS integer AS $$
BEGIN
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

SELECT return_null_explicit() AS null_return;

-- ============================================
-- SECTION 6: Variable Substitution Edge Cases
-- ============================================

DROP FUNCTION IF EXISTS var_in_string(integer);
CREATE FUNCTION var_in_string(n integer) RETURNS text AS $$
BEGIN
    RETURN 'The number n is: ' || n::text;
END;
$$ LANGUAGE plpgsql;

SELECT var_in_string(42) AS var_str;

DROP FUNCTION IF EXISTS similar_var_names(integer, integer);
CREATE FUNCTION similar_var_names(n integer, nn integer) RETURNS text AS $$
BEGIN
    RETURN 'n=' || n::text || ', nn=' || nn::text;
END;
$$ LANGUAGE plpgsql;

SELECT similar_var_names(1, 11) AS similar_vars;

DROP FUNCTION IF EXISTS var_boundaries(integer);
CREATE FUNCTION var_boundaries(x integer) RETURNS text AS $$
DECLARE
    xx integer := x * 2;
    xxx integer := x * 3;
BEGIN
    RETURN 'x=' || x::text || ', xx=' || xx::text || ', xxx=' || xxx::text;
END;
$$ LANGUAGE plpgsql;

SELECT var_boundaries(5) AS boundaries;

DROP FUNCTION IF EXISTS underscore_vars(integer);
CREATE FUNCTION underscore_vars(my_val integer) RETURNS text AS $$
DECLARE
    my_val_doubled integer;
BEGIN
    my_val_doubled := my_val * 2;
    RETURN 'val=' || my_val::text || ', doubled=' || my_val_doubled::text;
END;
$$ LANGUAGE plpgsql;

SELECT underscore_vars(7) AS underscore_result;

-- ============================================
-- SECTION 7: Complex Expressions in RETURN
-- ============================================

DROP FUNCTION IF EXISTS complex_return(integer, integer);
CREATE FUNCTION complex_return(a integer, b integer) RETURNS integer AS $$
BEGIN
    RETURN (a + b) * (a - b);
END;
$$ LANGUAGE plpgsql;

SELECT complex_return(10, 3) AS complex_expr;

DROP FUNCTION IF EXISTS string_concat_return(text, text);
CREATE FUNCTION string_concat_return(first text, last text) RETURNS text AS $$
BEGIN
    RETURN first || ' ' || last || '!';
END;
$$ LANGUAGE plpgsql;

SELECT string_concat_return('Hello', 'World') AS concat_str;

-- ============================================
-- SECTION 8: SQL Functions with Expressions
-- ============================================

DROP FUNCTION IF EXISTS sql_expr_func(integer);
CREATE FUNCTION sql_expr_func(n integer) RETURNS integer AS $$
SELECT n * n + n;
$$ LANGUAGE sql;

SELECT sql_expr_func(5) AS sql_expr;

DROP FUNCTION IF EXISTS sql_case_func(integer);
CREATE FUNCTION sql_case_func(n integer) RETURNS text AS $$
SELECT CASE WHEN n > 0 THEN 'positive' WHEN n < 0 THEN 'negative' ELSE 'zero' END;
$$ LANGUAGE sql;

SELECT sql_case_func(5) AS sql_pos;
SELECT sql_case_func(-5) AS sql_neg;
SELECT sql_case_func(0) AS sql_zero;

DROP FUNCTION IF EXISTS sql_coalesce_func(integer);
CREATE FUNCTION sql_coalesce_func(n integer) RETURNS integer AS $$
SELECT COALESCE(n, 0);
$$ LANGUAGE sql;

SELECT sql_coalesce_func(10) AS sql_val;
SELECT sql_coalesce_func(NULL) AS sql_null;

-- ============================================
-- SECTION 9: Boolean Expressions
-- ============================================

DROP FUNCTION IF EXISTS bool_and_test(boolean, boolean);
CREATE FUNCTION bool_and_test(a boolean, b boolean) RETURNS boolean AS $$
BEGIN
    IF a AND b THEN
        RETURN true;
    ELSE
        RETURN false;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT bool_and_test(true, true) AS both_true;
SELECT bool_and_test(true, false) AS one_false;
SELECT bool_and_test(false, false) AS both_false;

DROP FUNCTION IF EXISTS bool_or_test(boolean, boolean);
CREATE FUNCTION bool_or_test(a boolean, b boolean) RETURNS boolean AS $$
BEGIN
    IF a OR b THEN
        RETURN true;
    ELSE
        RETURN false;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT bool_or_test(true, false) AS one_true;
SELECT bool_or_test(false, false) AS none_true;

DROP FUNCTION IF EXISTS bool_not_test(boolean);
CREATE FUNCTION bool_not_test(a boolean) RETURNS boolean AS $$
BEGIN
    IF NOT a THEN
        RETURN true;
    ELSE
        RETURN false;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT bool_not_test(false) AS not_false;
SELECT bool_not_test(true) AS not_true;

-- ============================================
-- SECTION 10: Arithmetic Operations
-- ============================================

DROP FUNCTION IF EXISTS arith_test(integer, integer);
CREATE FUNCTION arith_test(a integer, b integer) RETURNS text AS $$
DECLARE
    sum_val integer;
    diff_val integer;
    prod_val integer;
    div_val integer;
    mod_val integer;
BEGIN
    sum_val := a + b;
    diff_val := a - b;
    prod_val := a * b;
    div_val := a / b;
    mod_val := a % b;
    RETURN sum_val::text || ',' || diff_val::text || ',' || prod_val::text || ',' || div_val::text || ',' || mod_val::text;
END;
$$ LANGUAGE plpgsql;

SELECT arith_test(17, 5) AS arith_result;

-- ============================================
-- SECTION 11: Comparison Operations
-- ============================================

DROP FUNCTION IF EXISTS compare_test(integer, integer);
CREATE FUNCTION compare_test(a integer, b integer) RETURNS text AS $$
BEGIN
    IF a = b THEN
        RETURN 'equal';
    ELSIF a > b THEN
        RETURN 'greater';
    ELSIF a < b THEN
        RETURN 'less';
    ELSE
        RETURN 'unknown';
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT compare_test(5, 5) AS eq;
SELECT compare_test(10, 5) AS gt;
SELECT compare_test(3, 7) AS lt;

DROP FUNCTION IF EXISTS range_test(integer);
CREATE FUNCTION range_test(n integer) RETURNS text AS $$
BEGIN
    IF n >= 1 AND n <= 10 THEN
        RETURN 'in range';
    ELSE
        RETURN 'out of range';
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT range_test(5) AS in_range;
SELECT range_test(15) AS out_range;

-- ============================================
-- CLEANUP
-- ============================================

DROP FUNCTION IF EXISTS test_raise_exception(integer);
DROP FUNCTION IF EXISTS test_raise_simple(integer);
DROP FUNCTION IF EXISTS nested_if_test(integer, integer);
DROP FUNCTION IF EXISTS deep_nested_if(integer);
DROP FUNCTION IF EXISTS multi_assign();
DROP FUNCTION IF EXISTS chain_assign();
DROP FUNCTION IF EXISTS swap_values(integer, integer);
DROP FUNCTION IF EXISTS int_to_text(integer);
DROP FUNCTION IF EXISTS float_to_int(float);
DROP FUNCTION IF EXISTS bool_to_text(boolean);
DROP FUNCTION IF EXISTS null_check(integer);
DROP FUNCTION IF EXISTS coalesce_test(integer, integer);
DROP FUNCTION IF EXISTS return_null_explicit();
DROP FUNCTION IF EXISTS var_in_string(integer);
DROP FUNCTION IF EXISTS similar_var_names(integer, integer);
DROP FUNCTION IF EXISTS var_boundaries(integer);
DROP FUNCTION IF EXISTS underscore_vars(integer);
DROP FUNCTION IF EXISTS complex_return(integer, integer);
DROP FUNCTION IF EXISTS string_concat_return(text, text);
DROP FUNCTION IF EXISTS sql_expr_func(integer);
DROP FUNCTION IF EXISTS sql_case_func(integer);
DROP FUNCTION IF EXISTS sql_coalesce_func(integer);
DROP FUNCTION IF EXISTS bool_and_test(boolean, boolean);
DROP FUNCTION IF EXISTS bool_or_test(boolean, boolean);
DROP FUNCTION IF EXISTS bool_not_test(boolean);
DROP FUNCTION IF EXISTS arith_test(integer, integer);
DROP FUNCTION IF EXISTS compare_test(integer, integer);
DROP FUNCTION IF EXISTS range_test(integer);
