-- PostgreSQL compatible tests from target_names
-- 8 tests

SET client_min_messages = warning;

-- CockroachDB includes helper functions used by this test; define minimal
-- Postgres equivalents to preserve expression/column labeling.
CREATE OR REPLACE FUNCTION iferror(anyelement, anyelement)
RETURNS anyelement
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT $1;
$$;

CREATE OR REPLACE FUNCTION iserror(anyelement)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT false;
$$;

CREATE OR REPLACE FUNCTION if(boolean, anyelement, anyelement)
RETURNS anyelement
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE WHEN $1 THEN $2 ELSE $3 END;
$$;

CREATE COLLATION IF NOT EXISTS "en_US" (provider = icu, locale = 'en-US');

DROP TABLE IF EXISTS t;
CREATE TABLE t (
    a INT[],
    t TEXT
);
-- Keep t non-empty so result formatting includes a data row, but avoid runtime
-- errors from casts in CASE expressions (NULL concatenation stays NULL).
INSERT INTO t VALUES (ARRAY[1,2,3], NULL);

DROP ROLE IF EXISTS target_names_user;
CREATE ROLE target_names_user;
GRANT SELECT ON t TO target_names_user;
SET ROLE target_names_user;

-- Test 1: query (line 5)
SELECT *, a, a[0], (((((((((a))))))))), t COLLATE "en_US" FROM t;

-- Test 2: query (line 11)
SELECT array_length(a, 1),
       nullif(a, a),
       row(1,2,3),
       coalesce(a,a),
       iferror(a, a),
       iserror(a),
       if(true, a, a),
       current_user
FROM t;

-- Test 3: query (line 25)
SELECT 123, '123', 123.0, TRUE, FALSE, NULL;

-- Test 4: query (line 39)
SELECT (pg_get_keywords()).word FROM t;

-- Test 5: query (line 45)
SELECT array[1,2,3], array(select 1);

-- Test 6: query (line 52)
SELECT EXISTS(SELECT * FROM t);

-- Test 7: query (line 59)
SELECT CASE 1 WHEN 2 THEN 3 END,
       CASE 1 WHEN 2 THEN 3 ELSE a[0] END,
       CASE 1 WHEN 2 THEN 3 ELSE length(t) END,
       CASE 1 WHEN 2 THEN 3 ELSE (t||'a')::INT END,
       CASE 1 WHEN 2 THEN 3 ELSE 4 END
  FROM t;

-- Test 8: query (line 70)
SELECT (SELECT 123 AS a),
       (VALUES (cos(1)::INT)),
       (SELECT cos(0)::INT);

RESET ROLE;
REVOKE ALL ON t FROM target_names_user;
DROP ROLE target_names_user;
RESET client_min_messages;
