-- PostgreSQL compatible tests from show_create_redact
-- 38 tests

SET client_min_messages = warning;

-- Redact literals in a SQL fragment (best-effort; not a full SQL parser).
CREATE OR REPLACE FUNCTION pg_redact_sql(in_sql text)
RETURNS text
LANGUAGE sql
AS $$
  SELECT regexp_replace(
           regexp_replace(
             regexp_replace(
               in_sql,
               '''([^'']|'''')*''',  -- single-quoted strings ('' is escaped quote)
               '''<redacted>''',
               'g'
             ),
             E'\\y[0-9]+(\\.[0-9]+)?([eE][+-]?[0-9]+)?\\y',
             '<redacted>',
             'g'
           ),
           E'\\y(true|false)\\y',
           '<redacted>',
           'gi'
         );
$$;

CREATE OR REPLACE FUNCTION pg_show_create_table_redacted(tab regclass)
RETURNS TABLE(kind text, name text, detail text)
LANGUAGE sql
AS $$
  WITH
  cols AS (
    SELECT
      'column'::text AS kind,
      a.attname::text AS name,
      pg_catalog.format_type(a.atttypid, a.atttypmod)
        || CASE WHEN a.attnotnull THEN ' NOT NULL' ELSE '' END
        || CASE
             WHEN d.oid IS NULL THEN ''
             WHEN a.attgenerated <> '' THEN
               ' GENERATED ALWAYS AS (' || pg_redact_sql(pg_catalog.pg_get_expr(d.adbin, d.adrelid)) || ') STORED'
             ELSE
               ' DEFAULT ' || pg_redact_sql(pg_catalog.pg_get_expr(d.adbin, d.adrelid))
           END AS detail
    FROM pg_catalog.pg_attribute a
    LEFT JOIN pg_catalog.pg_attrdef d
      ON d.adrelid = a.attrelid AND d.adnum = a.attnum
    WHERE a.attrelid = tab AND a.attnum > 0 AND NOT a.attisdropped
  ),
  cons AS (
    SELECT
      'constraint'::text AS kind,
      c.conname::text AS name,
      pg_redact_sql(pg_catalog.pg_get_constraintdef(c.oid)) AS detail
    FROM pg_catalog.pg_constraint c
    WHERE c.conrelid = tab
  ),
  idx AS (
    SELECT
      'index'::text AS kind,
      i.relname::text AS name,
      pg_redact_sql(pg_catalog.pg_get_indexdef(i.oid)) AS detail
    FROM pg_catalog.pg_index x
    JOIN pg_catalog.pg_class i ON i.oid = x.indexrelid
    WHERE x.indrelid = tab
  )
  SELECT kind, name, detail
  FROM (
    SELECT * FROM cols
    UNION ALL SELECT * FROM cons
    UNION ALL SELECT * FROM idx
  ) s
  ORDER BY kind, name;
$$;

CREATE OR REPLACE FUNCTION pg_show_create_view_redacted(view_oid regclass)
RETURNS TABLE(create_statement text)
LANGUAGE sql
AS $$
  SELECT format(
    'CREATE OR REPLACE VIEW %s AS %s',
    view_oid::text,
    pg_redact_sql(pg_catalog.pg_get_viewdef(view_oid, true))
  );
$$;

-- Test 1: statement (line 3)
-- CockroachDB-specific session setting:
-- SET create_table_with_schema_locked=false;

-- Minimal base tables referenced later in this file.
CREATE TABLE a (a TEXT, j JSONB DEFAULT '{}'::jsonb);
CREATE TABLE jk (k TEXT, jk JSONB);
CREATE TABLE m (m TEXT DEFAULT 'secret');

-- Test 2: query (line 9)
SELECT * FROM pg_show_create_table_redacted('public.a'::regclass);

-- Test 3: statement (line 18)
CREATE TABLE b (b BOOLEAN DEFAULT false);

-- Test 4: query (line 21)
SELECT * FROM pg_show_create_table_redacted('public.b'::regclass);

-- Test 5: statement (line 30)
CREATE TABLE c (c CHAR DEFAULT 'c');

-- Test 6: query (line 33)
SELECT * FROM pg_show_create_table_redacted('public.c'::regclass);

-- Test 7: statement (line 42)
CREATE TABLE d (d DATE DEFAULT '1999-12-31');

-- Test 8: query (line 45)
SELECT * FROM pg_show_create_table_redacted('public.d'::regclass);

-- Test 9: statement (line 54)
CREATE TABLE i (i INT DEFAULT 0);

-- Test 10: query (line 57)
SELECT * FROM pg_show_create_table_redacted('public.i'::regclass);

-- Test 11: statement (line 66)
CREATE TABLE j (j JSONB DEFAULT '{}'::jsonb);

-- Test 12: query (line 69)
SELECT * FROM pg_show_create_table_redacted('public.j'::regclass);

-- Test 13: statement (line 78)
CREATE TABLE n (n INT DEFAULT NULL);

-- Test 14: query (line 81)
SELECT * FROM pg_show_create_table_redacted('public.n'::regclass);

-- Test 15: statement (line 92)
CREATE TABLE ef (e INT, f INT GENERATED ALWAYS AS (e + 1) STORED);

-- Test 16: query (line 95)
SELECT * FROM pg_show_create_table_redacted('public.ef'::regclass);

-- Test 17: statement (line 105)
-- PostgreSQL doesn't have a built-in GEOMETRY type; use a built-in point instead.
CREATE TABLE g (g POINT GENERATED ALWAYS AS (point '(0,0)') STORED);

-- Test 18: query (line 108)
SELECT * FROM pg_show_create_table_redacted('public.g'::regclass);

-- Test 19: statement (line 117)
CREATE TABLE hi (
  h INTERVAL PRIMARY KEY,
  i INTERVAL GENERATED ALWAYS AS (h - interval '12:00:00') STORED
);

-- Test 20: query (line 120)
SELECT * FROM pg_show_create_table_redacted('public.hi'::regclass);

-- Test 21: query (line 133)
SELECT * FROM pg_show_create_table_redacted('public.jk'::regclass);

-- Test 22: statement (line 147)
CREATE TABLE k (k INT PRIMARY KEY, CHECK (k > 0), CHECK (true));

-- Test 23: query (line 150)
SELECT * FROM pg_show_create_table_redacted('public.k'::regclass);

-- Test 24: statement (line 160)
CREATE TABLE dl (d DECIMAL PRIMARY KEY, l DECIMAL, CHECK (d != l + 2.0));

-- Test 25: query (line 163)
SELECT * FROM pg_show_create_table_redacted('public.dl'::regclass);

-- Test 26: query (line 179)
SELECT * FROM pg_show_create_table_redacted('public.m'::regclass);

-- Test 27: statement (line 189)
CREATE TABLE no (n NUMERIC PRIMARY KEY, o DOUBLE PRECISION);
CREATE UNIQUE INDEX no_o_expr_idx ON no ((o / 1.1e7));

-- Test 28: query (line 192)
SELECT * FROM pg_show_create_table_redacted('public.no'::regclass);

-- Test 29: statement (line 205)
CREATE TABLE p (p UUID PRIMARY KEY);
CREATE INDEX p_partial_idx ON p (p) WHERE p <> 'acde070d-8c4c-4f0d-9d8a-162843c10333'::uuid;

-- Test 30: query (line 208)
SELECT * FROM pg_show_create_table_redacted('public.p'::regclass);

-- Test 31: statement (line 219)
CREATE VIEW q (q) AS SELECT 0;

-- Test 32: query (line 222)
SELECT * FROM pg_show_create_view_redacted('public.q'::regclass);

-- Test 33: statement (line 229)
CREATE VIEW r (r) AS
  SELECT TIMESTAMP '1999-12-31 23:59:59' + i FROM hi WHERE h <> interval '00:00:01';

-- Test 34: query (line 232)
SELECT * FROM pg_show_create_view_redacted('public.r'::regclass);

-- Test 35: statement (line 239)
CREATE VIEW s (s) AS
  SELECT CASE WHEN b THEN 'abc' ELSE 'def' END FROM b
  ORDER BY b AND NOT false
  LIMIT 10;

-- Test 36: query (line 242)
SELECT * FROM pg_show_create_view_redacted('public.s'::regclass);

-- Test 37: statement (line 256)
CREATE VIEW t (t, u) AS
  SELECT jk_tbl.jk || '[null]'::jsonb, a_tbl.j -> a_tbl.a
  FROM a AS a_tbl
  JOIN jk AS jk_tbl ON a_tbl.a = jk_tbl.k || 'u'
  ORDER BY concat(a_tbl.a, 'ut');

-- Test 38: query (line 259)
SELECT * FROM pg_show_create_view_redacted('public.t'::regclass);

RESET client_min_messages;
