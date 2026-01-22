-- PostgreSQL compatible tests from notice
-- 12 tests

-- CockroachDB has `crdb_internal.notice(...)` for emitting client messages at
-- various severities. PostgreSQL doesn't, so provide a small shim.
SET client_min_messages = warning;
CREATE EXTENSION IF NOT EXISTS plpgsql;
RESET client_min_messages;

CREATE SCHEMA IF NOT EXISTS crdb_internal;

CREATE OR REPLACE FUNCTION crdb_internal.notice(msg text)
RETURNS text
LANGUAGE plpgsql
AS $$
BEGIN
  RAISE NOTICE '%', msg;
  RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION crdb_internal.notice(level text, msg text)
RETURNS text
LANGUAGE plpgsql
AS $$
DECLARE
  thresh_setting text;
  thresh_rank int;
  msg_rank int;
BEGIN
  thresh_setting := lower(current_setting('client_min_messages'));
  thresh_rank := CASE thresh_setting
    WHEN 'debug5' THEN 1
    WHEN 'debug4' THEN 2
    WHEN 'debug3' THEN 3
    WHEN 'debug2' THEN 4
    WHEN 'debug1' THEN 5
    WHEN 'log' THEN 6
    WHEN 'notice' THEN 7
    WHEN 'warning' THEN 8
    WHEN 'error' THEN 9
    ELSE 7
  END;

  msg_rank := CASE lower(level)
    WHEN 'debug5' THEN 1
    WHEN 'debug4' THEN 2
    WHEN 'debug3' THEN 3
    WHEN 'debug2' THEN 4
    WHEN 'debug1' THEN 5
    WHEN 'log' THEN 6
    WHEN 'notice' THEN 7
    WHEN 'warning' THEN 8
    WHEN 'error' THEN 9
    ELSE 7
  END;

  -- We can't emit DEBUG1 vs DEBUG2 distinctly from plpgsql, so approximate by
  -- conditionally emitting DEBUG messages based on the current threshold.
  IF msg_rank < thresh_rank THEN
    RETURN NULL;
  END IF;

  CASE lower(level)
    WHEN 'warning' THEN
      RAISE WARNING '%', msg;
    WHEN 'notice' THEN
      RAISE NOTICE '%', msg;
    WHEN 'log' THEN
      RAISE LOG '%', msg;
    WHEN 'info' THEN
      RAISE INFO '%', msg;
    WHEN 'debug1' THEN
      RAISE DEBUG '%', msg;
    WHEN 'debug2' THEN
      RAISE DEBUG '%', msg;
    WHEN 'debug3' THEN
      RAISE DEBUG '%', msg;
    WHEN 'debug4' THEN
      RAISE DEBUG '%', msg;
    WHEN 'debug5' THEN
      RAISE DEBUG '%', msg;
    ELSE
      RAISE NOTICE '%', msg;
  END CASE;

  RETURN NULL;
END;
$$;

-- Test 1: query (line 3)
SELECT crdb_internal.notice('hi');
SELECT crdb_internal.notice('i am....');
SELECT crdb_internal.notice('otan!!!');

-- Test 2: query (line 12)
SELECT crdb_internal.notice('debug1', 'do not see this');
SELECT crdb_internal.notice('warning', 'but you see this');
SELECT crdb_internal.notice('debug2', 'and never this');

-- Test 3: statement (line 17)
SET client_min_messages = 'debug1';

-- Test 4: query (line 20)
SELECT crdb_internal.notice('debug1', 'now you see this');
SELECT crdb_internal.notice('warning', 'and you see this');
SELECT crdb_internal.notice('debug2', 'and never this');

-- Avoid pulling in PostgreSQL-internal DEBUG output for the rest of the file.
RESET client_min_messages;

-- Test 5: statement (line 26)
-- CockroachDB's database-qualified names map more closely to PG schemas.
CREATE SCHEMA d;
CREATE TABLE d.t (x int);

-- Test 6: query (line 32)
ALTER TABLE d.t RENAME TO t2;

-- Test 7: statement (line 39)
CREATE TYPE color AS ENUM ('white');

-- Test 8: statement (line 42)
ALTER TYPE color ADD VALUE 'black';

-- Test 9: query (line 47)
ALTER TYPE color ADD VALUE IF NOT EXISTS 'black';

-- Test 10: statement (line 52)
CREATE MATERIALIZED VIEW v AS SELECT 1 AS col;
CREATE UNIQUE INDEX v_col_uq ON v (col);

-- Test 11: query (line 57)
REFRESH MATERIALIZED VIEW CONCURRENTLY v;

-- Test 12: query (line 62)
UNLISTEN temp;
