-- PostgreSQL compatible tests from udf_rewrite
-- 40 tests

SET client_min_messages = warning;

-- Test 1: statement (line 3)
CREATE SEQUENCE seq;

-- Test 2: statement (line 6)
CREATE TYPE weekday AS ENUM ('monday', 'tuesday', 'wednesday', 'thursday', 'friday');

-- Test 3: statement (line 9)
CREATE TABLE t_rewrite (
  v INT DEFAULT 0,
  w weekday DEFAULT 'monday'::weekday
);

-- CockroachDB helper: return the stored body for a function/procedure by name.
CREATE OR REPLACE FUNCTION get_body_str(obj_name TEXT) RETURNS TEXT
LANGUAGE SQL STABLE AS $$
  SELECT p.prosrc
  FROM pg_proc p
  JOIN pg_namespace n ON n.oid = p.pronamespace
  WHERE p.proname = obj_name
    AND n.nspname = ANY (current_schemas(true))
  ORDER BY p.oid DESC
  LIMIT 1;
$$;

-- Test 4: statement (line 21)
-- SET use_declarative_schema_changer = 'off';

-- Test 5: statement (line 35)
-- SET use_declarative_schema_changer = 'on';

-- Test 6: statement (line 40)
CREATE FUNCTION f_rewrite() RETURNS INT AS
$$
  SELECT nextval('seq');
$$ LANGUAGE SQL;

-- Test 7: query (line 46)
SELECT get_body_str('f_rewrite');

-- Test 8: statement (line 51)
DROP FUNCTION f_rewrite();

-- Test 9: statement (line 54)
CREATE FUNCTION f_rewrite() RETURNS INT AS
$$
  INSERT INTO t_rewrite(v) VALUES (nextval('seq')) RETURNING v;
$$ LANGUAGE SQL;

-- Test 10: query (line 60)
SELECT get_body_str('f_rewrite');

-- Test 11: statement (line 65)
DROP FUNCTION f_rewrite();

-- Test 12: statement (line 68)
CREATE FUNCTION f_rewrite() RETURNS weekday AS
$$
  SELECT 'wednesday'::weekday;
$$ LANGUAGE SQL;

-- Test 13: query (line 74)
SELECT get_body_str('f_rewrite');

-- Test 14: statement (line 79)
DROP FUNCTION f_rewrite();

-- Test 15: statement (line 82)
CREATE FUNCTION f_rewrite() RETURNS weekday AS
$$
  UPDATE t_rewrite SET w = 'thursday'::weekday WHERE w = 'wednesday'::weekday RETURNING w;
$$ LANGUAGE SQL;

-- Test 16: query (line 88)
SELECT get_body_str('f_rewrite');

-- Test 17: statement (line 93)
DROP FUNCTION f_rewrite();

-- Test 18: statement (line 96)
CREATE FUNCTION f_rewrite(OUT out_weekday weekday) AS
$$
  SELECT 'thursday'::weekday;
$$ LANGUAGE SQL;

-- Test 19: query (line 102)
SELECT get_body_str('f_rewrite');

-- Test 20: statement (line 107)
DROP FUNCTION f_rewrite();

-- Test 21: statement (line 110)
CREATE FUNCTION f_rewrite(INOUT a weekday) AS
$$
  SELECT 'thursday'::weekday;
$$ LANGUAGE SQL;

-- Test 22: query (line 116)
SELECT get_body_str('f_rewrite');

-- Test 23: statement (line 121)
DROP FUNCTION f_rewrite(weekday);

-- Test 24: statement (line 128)
CREATE PROCEDURE p_rewrite() AS
$$
  INSERT INTO t_rewrite(v) VALUES (nextval('seq')) RETURNING v;
$$ LANGUAGE SQL;

-- Test 25: query (line 134)
SELECT get_body_str('p_rewrite');

-- Test 26: statement (line 139)
DROP PROCEDURE p_rewrite();

-- Test 27: statement (line 142)
CREATE PROCEDURE p_rewrite() AS
$$
  UPDATE t_rewrite SET w = 'thursday'::weekday WHERE w = 'wednesday'::weekday RETURNING w;
$$ LANGUAGE SQL;

-- Test 28: query (line 148)
SELECT get_body_str('p_rewrite');

-- Test 29: statement (line 153)
DROP PROCEDURE p_rewrite();

-- Test 30: statement (line 156)
CREATE PROCEDURE p_rewrite(OUT out_weekday weekday) AS
$$
  SELECT 'thursday'::weekday;
$$ LANGUAGE SQL;

-- Test 31: query (line 162)
SELECT get_body_str('p_rewrite');

-- Test 32: statement (line 167)
DROP PROCEDURE p_rewrite();

-- Test 33: statement (line 170)
CREATE PROCEDURE p_rewrite(INOUT a weekday) AS
$$
  SELECT 'thursday'::weekday;
$$ LANGUAGE SQL;

-- Test 34: query (line 176)
SELECT get_body_str('p_rewrite');

-- Test 35: statement (line 181)
DROP PROCEDURE p_rewrite(weekday);

-- Test 36: statement (line 188)
CREATE FUNCTION nested_func() RETURNS INT AS $$
  SELECT 1;
$$ LANGUAGE SQL;

-- Test 37: statement (line 193)
CREATE PROCEDURE p_rewrite() AS $$
  SELECT nested_func();
  SELECT * FROM (SELECT nested_func() AS nested_func) AS s;
$$ LANGUAGE SQL;

-- Test 38: query (line 200)
SELECT get_body_str('p_rewrite');

-- Test 39: statement (line 209)
CREATE OR REPLACE FUNCTION f_rewrite() RETURNS INT AS $$
  DO $foo$
    BEGIN
      INSERT INTO t_rewrite(v) VALUES (nextval('seq')) RETURNING v;
      DO $bar$
        BEGIN
          INSERT INTO t_rewrite(v) VALUES (nextval('seq')) RETURNING v;
        END;
      $bar$;
    END;
  $foo$;
  SELECT 1;
$$ LANGUAGE SQL;

-- Test 40: query (line 224)
SELECT get_body_str('f_rewrite');

RESET client_min_messages;
