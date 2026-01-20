-- UDF tests for different parameter types and return types

-- ============================================
-- SECTION 1: Various Integer Types
-- ============================================

DROP FUNCTION IF EXISTS test_int4(int4);
CREATE FUNCTION test_int4(n int4) RETURNS int4 AS $$
BEGIN
    RETURN n * 2;
END;
$$ LANGUAGE plpgsql;

SELECT test_int4(100) AS int4_result;

DROP FUNCTION IF EXISTS test_int8(int8);
CREATE FUNCTION test_int8(n int8) RETURNS int8 AS $$
BEGIN
    RETURN n * 2;
END;
$$ LANGUAGE plpgsql;

SELECT test_int8(1000000000000) AS int8_result;

DROP FUNCTION IF EXISTS test_smallint(smallint);
CREATE FUNCTION test_smallint(n smallint) RETURNS smallint AS $$
BEGIN
    RETURN n + 1;
END;
$$ LANGUAGE plpgsql;

SELECT test_smallint(100::smallint) AS smallint_result;

DROP FUNCTION IF EXISTS test_bigint(bigint);
CREATE FUNCTION test_bigint(n bigint) RETURNS bigint AS $$
BEGIN
    RETURN n + 1;
END;
$$ LANGUAGE plpgsql;

SELECT test_bigint(9999999999) AS bigint_result;

-- ============================================
-- SECTION 2: Float Types
-- ============================================

DROP FUNCTION IF EXISTS test_real(real);
CREATE FUNCTION test_real(n real) RETURNS real AS $$
BEGIN
    RETURN n * 1.5;
END;
$$ LANGUAGE plpgsql;

SELECT test_real(2.0) AS real_result;

DROP FUNCTION IF EXISTS test_float8(float8);
CREATE FUNCTION test_float8(n float8) RETURNS float8 AS $$
BEGIN
    RETURN n / 3.0;
END;
$$ LANGUAGE plpgsql;

SELECT test_float8(9.0) AS float8_result;

DROP FUNCTION IF EXISTS test_double_precision(double precision);
CREATE FUNCTION test_double_precision(n double precision) RETURNS double precision AS $$
BEGIN
    RETURN n * n;
END;
$$ LANGUAGE plpgsql;

SELECT test_double_precision(1.5) AS double_result;

-- ============================================
-- SECTION 3: Text Types
-- ============================================

DROP FUNCTION IF EXISTS test_text(text);
CREATE FUNCTION test_text(s text) RETURNS text AS $$
BEGIN
    RETURN 'Hello, ' || s;
END;
$$ LANGUAGE plpgsql;

SELECT test_text('World') AS text_result;

DROP FUNCTION IF EXISTS test_varchar(varchar);
CREATE FUNCTION test_varchar(s varchar) RETURNS varchar AS $$
BEGIN
    RETURN s || '!';
END;
$$ LANGUAGE plpgsql;

SELECT test_varchar('Test') AS varchar_result;

DROP FUNCTION IF EXISTS test_character_varying(character varying);
CREATE FUNCTION test_character_varying(s character varying) RETURNS character varying AS $$
BEGIN
    RETURN UPPER(s);
END;
$$ LANGUAGE plpgsql;

SELECT test_character_varying('lowercase') AS charvar_result;

-- ============================================
-- SECTION 4: Boolean Type
-- ============================================

DROP FUNCTION IF EXISTS test_bool(bool);
CREATE FUNCTION test_bool(b bool) RETURNS bool AS $$
BEGIN
    RETURN NOT b;
END;
$$ LANGUAGE plpgsql;

SELECT test_bool(true) AS bool_true;
SELECT test_bool(false) AS bool_false;

DROP FUNCTION IF EXISTS test_boolean(boolean);
CREATE FUNCTION test_boolean(b boolean) RETURNS boolean AS $$
BEGIN
    IF b THEN
        RETURN false;
    ELSE
        RETURN true;
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT test_boolean(true) AS boolean_true;
SELECT test_boolean(false) AS boolean_false;

-- ============================================
-- SECTION 5: Multiple Parameters
-- ============================================

DROP FUNCTION IF EXISTS three_params(integer, integer, integer);
CREATE FUNCTION three_params(a integer, b integer, c integer) RETURNS integer AS $$
BEGIN
    RETURN a + b + c;
END;
$$ LANGUAGE plpgsql;

SELECT three_params(1, 2, 3) AS three_sum;

DROP FUNCTION IF EXISTS mixed_params(integer, text, boolean);
CREATE FUNCTION mixed_params(num integer, str text, flag boolean) RETURNS text AS $$
BEGIN
    IF flag THEN
        RETURN str || ': ' || num::text;
    ELSE
        RETURN 'disabled';
    END IF;
END;
$$ LANGUAGE plpgsql;

SELECT mixed_params(42, 'Value', true) AS mixed_enabled;
SELECT mixed_params(42, 'Value', false) AS mixed_disabled;

DROP FUNCTION IF EXISTS five_params(int, int, int, int, int);
CREATE FUNCTION five_params(a int, b int, c int, d int, e int) RETURNS int AS $$
BEGIN
    RETURN a * b + c * d + e;
END;
$$ LANGUAGE plpgsql;

SELECT five_params(1, 2, 3, 4, 5) AS five_result;

-- ============================================
-- SECTION 6: No Parameters
-- ============================================

DROP FUNCTION IF EXISTS no_params();
CREATE FUNCTION no_params() RETURNS integer AS $$
BEGIN
    RETURN 42;
END;
$$ LANGUAGE plpgsql;

SELECT no_params() AS no_param_result;

DROP FUNCTION IF EXISTS no_params_text();
CREATE FUNCTION no_params_text() RETURNS text AS $$
BEGIN
    RETURN 'Hello from function';
END;
$$ LANGUAGE plpgsql;

SELECT no_params_text() AS no_param_text;

-- ============================================
-- SECTION 7: Parameter Name Variations
-- ============================================

DROP FUNCTION IF EXISTS long_param_name(integer);
CREATE FUNCTION long_param_name(very_long_parameter_name_here integer) RETURNS integer AS $$
BEGIN
    RETURN very_long_parameter_name_here * 2;
END;
$$ LANGUAGE plpgsql;

SELECT long_param_name(10) AS long_name_result;

DROP FUNCTION IF EXISTS numeric_like_name(integer);
CREATE FUNCTION numeric_like_name(val123 integer) RETURNS integer AS $$
BEGIN
    RETURN val123 + 100;
END;
$$ LANGUAGE plpgsql;

SELECT numeric_like_name(5) AS numeric_name_result;

DROP FUNCTION IF EXISTS underscore_name(integer);
CREATE FUNCTION underscore_name(_value integer) RETURNS integer AS $$
BEGIN
    RETURN _value * 3;
END;
$$ LANGUAGE plpgsql;

SELECT underscore_name(7) AS underscore_result;

-- ============================================
-- SECTION 8: SQL Language Functions Various Types
-- ============================================

DROP FUNCTION IF EXISTS sql_int(integer);
CREATE FUNCTION sql_int(n integer) RETURNS integer AS $$
SELECT n + 100;
$$ LANGUAGE sql;

SELECT sql_int(5) AS sql_int_result;

DROP FUNCTION IF EXISTS sql_text(text);
CREATE FUNCTION sql_text(s text) RETURNS text AS $$
SELECT s || ' - modified';
$$ LANGUAGE sql;

SELECT sql_text('input') AS sql_text_result;

DROP FUNCTION IF EXISTS sql_bool(boolean);
CREATE FUNCTION sql_bool(b boolean) RETURNS boolean AS $$
SELECT NOT b;
$$ LANGUAGE sql;

SELECT sql_bool(true) AS sql_bool_result;

DROP FUNCTION IF EXISTS sql_multi(integer, text);
CREATE FUNCTION sql_multi(n integer, s text) RETURNS text AS $$
SELECT s || ': ' || n::text;
$$ LANGUAGE sql;

SELECT sql_multi(42, 'Number') AS sql_multi_result;

-- ============================================
-- SECTION 9: DECLARE with Various Types
-- ============================================

DROP FUNCTION IF EXISTS declare_int();
CREATE FUNCTION declare_int() RETURNS integer AS $$
DECLARE
    x integer := 10;
    y int := 20;
    z int4 := 30;
BEGIN
    RETURN x + y + z;
END;
$$ LANGUAGE plpgsql;

SELECT declare_int() AS declare_int_result;

DROP FUNCTION IF EXISTS declare_text();
CREATE FUNCTION declare_text() RETURNS text AS $$
DECLARE
    s1 text := 'Hello';
    s2 varchar := 'World';
BEGIN
    RETURN s1 || ' ' || s2;
END;
$$ LANGUAGE plpgsql;

SELECT declare_text() AS declare_text_result;

DROP FUNCTION IF EXISTS declare_bool();
CREATE FUNCTION declare_bool() RETURNS boolean AS $$
DECLARE
    flag1 boolean := true;
    flag2 bool := false;
BEGIN
    RETURN flag1 AND NOT flag2;
END;
$$ LANGUAGE plpgsql;

SELECT declare_bool() AS declare_bool_result;

DROP FUNCTION IF EXISTS declare_float();
CREATE FUNCTION declare_float() RETURNS float AS $$
DECLARE
    x float := 1.5;
    y real := 2.5;
    z double precision := 3.5;
BEGIN
    RETURN x + y + z;
END;
$$ LANGUAGE plpgsql;

SELECT declare_float() AS declare_float_result;

-- ============================================
-- SECTION 10: Expressions as Default Values
-- ============================================

DROP FUNCTION IF EXISTS default_expr();
CREATE FUNCTION default_expr() RETURNS integer AS $$
DECLARE
    x integer DEFAULT 5 + 5;
    y integer := 3 * 4;
BEGIN
    RETURN x + y;
END;
$$ LANGUAGE plpgsql;

SELECT default_expr() AS default_expr_result;

DROP FUNCTION IF EXISTS default_string_expr();
CREATE FUNCTION default_string_expr() RETURNS text AS $$
DECLARE
    greeting text DEFAULT 'Hello' || ' ' || 'World';
BEGIN
    RETURN greeting;
END;
$$ LANGUAGE plpgsql;

SELECT default_string_expr() AS default_string_result;

-- ============================================
-- CLEANUP
-- ============================================

DROP FUNCTION IF EXISTS test_int4(int4);
DROP FUNCTION IF EXISTS test_int8(int8);
DROP FUNCTION IF EXISTS test_smallint(smallint);
DROP FUNCTION IF EXISTS test_bigint(bigint);
DROP FUNCTION IF EXISTS test_real(real);
DROP FUNCTION IF EXISTS test_float8(float8);
DROP FUNCTION IF EXISTS test_double_precision(double precision);
DROP FUNCTION IF EXISTS test_text(text);
DROP FUNCTION IF EXISTS test_varchar(varchar);
DROP FUNCTION IF EXISTS test_character_varying(character varying);
DROP FUNCTION IF EXISTS test_bool(bool);
DROP FUNCTION IF EXISTS test_boolean(boolean);
DROP FUNCTION IF EXISTS three_params(integer, integer, integer);
DROP FUNCTION IF EXISTS mixed_params(integer, text, boolean);
DROP FUNCTION IF EXISTS five_params(int, int, int, int, int);
DROP FUNCTION IF EXISTS no_params();
DROP FUNCTION IF EXISTS no_params_text();
DROP FUNCTION IF EXISTS long_param_name(integer);
DROP FUNCTION IF EXISTS numeric_like_name(integer);
DROP FUNCTION IF EXISTS underscore_name(integer);
DROP FUNCTION IF EXISTS sql_int(integer);
DROP FUNCTION IF EXISTS sql_text(text);
DROP FUNCTION IF EXISTS sql_bool(boolean);
DROP FUNCTION IF EXISTS sql_multi(integer, text);
DROP FUNCTION IF EXISTS declare_int();
DROP FUNCTION IF EXISTS declare_text();
DROP FUNCTION IF EXISTS declare_bool();
DROP FUNCTION IF EXISTS declare_float();
DROP FUNCTION IF EXISTS default_expr();
DROP FUNCTION IF EXISTS default_string_expr();
