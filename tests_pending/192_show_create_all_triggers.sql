-- PostgreSQL compatible tests from show_create_all_triggers
-- 16 tests

-- Test 1: statement (line 3)
-- CockroachDB uses multi-database syntax (`CREATE DATABASE` + `USE`).
-- PostgreSQL doesn't support `USE`, so model the database as a schema instead.
CREATE SCHEMA d;

-- Test 2: statement (line 6)
SET search_path TO d;

-- Ensure plpgsql exists for trigger functions (some environments don't install
-- it into template1).
SET client_min_messages = warning;
CREATE EXTENSION IF NOT EXISTS plpgsql;
RESET client_min_messages;

-- Test 3: statement (line 9)
CREATE TABLE t1 (a INT, b INT, c INT);

-- Test 4: statement (line 12)
CREATE TABLE t2 (a INT, b INT, c INT);

-- Test 5: statement (line 15)
INSERT INTO t1 VALUES (1, 2, 3);

-- Test 6: statement (line 18)
INSERT INTO t2 VALUES (4, 5, 6);

-- Test 7: statement (line 21)
CREATE OR REPLACE FUNCTION trigger_func()
RETURNS TRIGGER AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    RAISE NOTICE 'Row inserted: %', NEW;
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Test 8: statement (line 32)
CREATE TRIGGER t1_trigger
  AFTER INSERT ON t1
  FOR EACH ROW
  EXECUTE FUNCTION trigger_func();

-- Test 9: query (line 38)
SELECT pg_get_triggerdef(t.oid, true) AS create_statement
FROM pg_trigger t
JOIN pg_class c ON c.oid = t.tgrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE NOT t.tgisinternal
  AND n.nspname = current_schema()
ORDER BY c.relname, t.tgname;

-- Test 10: statement (line 44)
CREATE TRIGGER t2_trigger
  AFTER INSERT ON t2
  FOR EACH ROW
  EXECUTE FUNCTION trigger_func();

-- Test 11: query (line 50)
SELECT pg_get_triggerdef(t.oid, true) AS create_statement
FROM pg_trigger t
JOIN pg_class c ON c.oid = t.tgrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE NOT t.tgisinternal
  AND n.nspname = current_schema()
ORDER BY c.relname, t.tgname;

-- Test 12: statement (line 57)
CREATE OR REPLACE FUNCTION trigger_func2()
RETURNS TRIGGER AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    RAISE NOTICE 'Row inserted: %', NEW;
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Test 13: statement (line 68)
CREATE TRIGGER t1_trigger2
  AFTER INSERT ON t1
  FOR EACH ROW
  EXECUTE FUNCTION trigger_func2();

-- Test 14: query (line 74)
SELECT pg_get_triggerdef(t.oid, true) AS create_statement
FROM pg_trigger t
JOIN pg_class c ON c.oid = t.tgrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE NOT t.tgisinternal
  AND n.nspname = current_schema()
ORDER BY c.relname, t.tgname;

-- Test 15: statement (line 82)
DROP TRIGGER t1_trigger2 ON t1;

-- Test 16: query (line 85)
SELECT pg_get_triggerdef(t.oid, true) AS create_statement
FROM pg_trigger t
JOIN pg_class c ON c.oid = t.tgrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE NOT t.tgisinternal
  AND n.nspname = current_schema()
ORDER BY c.relname, t.tgname;
