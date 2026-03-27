-- B4: PL/pgSQL RETURNING ... INTO support (#2165)
-- Tests INSERT/UPDATE/DELETE ... RETURNING expr INTO var

-- Setup
CREATE TABLE IF NOT EXISTS ret_into_test (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    val INT DEFAULT 0
);
DELETE FROM ret_into_test;

-- Test 1: INSERT ... RETURNING single column INTO variable
CREATE OR REPLACE FUNCTION p_test_insert_returning_into()
RETURNS INT AS $$
DECLARE
    p_new_id INT;
BEGIN
    INSERT INTO ret_into_test (name, val) VALUES ('alice', 10) RETURNING id INTO p_new_id;
    RETURN p_new_id;
END;
$$ LANGUAGE plpgsql;

SELECT p_test_insert_returning_into() AS insert_id;

-- Test 2: INSERT ... RETURNING multiple columns INTO variables
CREATE OR REPLACE FUNCTION p_test_insert_returning_multi()
RETURNS TEXT AS $$
DECLARE
    p_id INT;
    p_name TEXT;
BEGIN
    INSERT INTO ret_into_test (name, val) VALUES ('bob', 20) RETURNING id, name INTO p_id, p_name;
    RETURN p_name || ':' || p_id::TEXT;
END;
$$ LANGUAGE plpgsql;

SELECT p_test_insert_returning_multi() AS insert_multi;

-- Test 3: UPDATE ... RETURNING INTO variable
CREATE OR REPLACE FUNCTION p_test_update_returning_into()
RETURNS INT AS $$
DECLARE
    p_updated_val INT;
BEGIN
    UPDATE ret_into_test SET val = val + 100 WHERE name = 'alice' RETURNING val INTO p_updated_val;
    RETURN p_updated_val;
END;
$$ LANGUAGE plpgsql;

SELECT p_test_update_returning_into() AS update_val;

-- Test 4: DELETE ... RETURNING INTO variable
CREATE OR REPLACE FUNCTION p_test_delete_returning_into()
RETURNS TEXT AS $$
DECLARE
    p_deleted_name TEXT;
BEGIN
    DELETE FROM ret_into_test WHERE name = 'bob' RETURNING name INTO p_deleted_name;
    RETURN p_deleted_name;
END;
$$ LANGUAGE plpgsql;

SELECT p_test_delete_returning_into() AS delete_name;

-- Test 5: RETURNING expression INTO (not just column)
CREATE OR REPLACE FUNCTION p_test_returning_expr_into()
RETURNS TEXT AS $$
DECLARE
    p_result TEXT;
BEGIN
    INSERT INTO ret_into_test (name, val) VALUES ('charlie', 30) RETURNING name || ':' || val::TEXT INTO p_result;
    RETURN p_result;
END;
$$ LANGUAGE plpgsql;

SELECT p_test_returning_expr_into() AS expr_result;

-- Test 6: RETURNING INTO with no matching rows (UPDATE on non-existent)
CREATE OR REPLACE FUNCTION p_test_returning_no_rows()
RETURNS TEXT AS $$
DECLARE
    p_name TEXT := 'default';
BEGIN
    UPDATE ret_into_test SET val = 999 WHERE name = 'nonexistent' RETURNING name INTO p_name;
    RETURN COALESCE(p_name, 'null_result');
END;
$$ LANGUAGE plpgsql;

SELECT p_test_returning_no_rows() AS no_rows;

-- Test 7: Multi-row UPDATE ... RETURNING INTO must error (PostgreSQL semantics)
CREATE OR REPLACE FUNCTION p_test_multi_row_returning_into()
RETURNS TEXT AS $$
DECLARE
    p_val INT;
BEGIN
    INSERT INTO ret_into_test (name, val) VALUES ('multi1', 1), ('multi2', 2);
    UPDATE ret_into_test SET val = val + 1000 WHERE name LIKE 'multi%' RETURNING val INTO p_val;
    RETURN p_val::TEXT;
END;
$$ LANGUAGE plpgsql;

SELECT p_test_multi_row_returning_into() AS multi_row_error;

-- Test 8: DECLARE with DEFAULT string literal containing 'default' keyword
CREATE OR REPLACE FUNCTION p_test_default_string_literal()
RETURNS TEXT AS $$
DECLARE
    p_status TEXT DEFAULT 'default';
BEGIN
    RETURN p_status;
END;
$$ LANGUAGE plpgsql;

SELECT p_test_default_string_literal() AS default_literal;

-- Cleanup
DROP FUNCTION IF EXISTS p_test_insert_returning_into();
DROP FUNCTION IF EXISTS p_test_insert_returning_multi();
DROP FUNCTION IF EXISTS p_test_update_returning_into();
DROP FUNCTION IF EXISTS p_test_delete_returning_into();
DROP FUNCTION IF EXISTS p_test_returning_expr_into();
DROP FUNCTION IF EXISTS p_test_returning_no_rows();
DROP FUNCTION IF EXISTS p_test_multi_row_returning_into();
DROP FUNCTION IF EXISTS p_test_default_string_literal();
DROP TABLE IF EXISTS ret_into_test;
