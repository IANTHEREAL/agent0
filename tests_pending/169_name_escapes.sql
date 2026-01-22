-- PostgreSQL compatible tests from name_escapes
-- 8 tests

SET client_min_messages = warning;

-- Cockroach's `SHOW CREATE TABLE/VIEW` returns a row with the create statement
-- text. PostgreSQL doesn't have an exact equivalent, so we emulate the output
-- from system catalogs (with proper identifier escaping).
DROP FUNCTION IF EXISTS crdb_show_create_table(regclass);
CREATE FUNCTION crdb_show_create_table(tbl regclass)
RETURNS TABLE(table_name text, create_statement text)
LANGUAGE plpgsql
AS $$
DECLARE
  ns text;
  rel text;
  cols text := '';
  col record;
  con record;
  idx record;
BEGIN
  SELECT n.nspname, c.relname
    INTO ns, rel
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE c.oid = tbl;

  table_name := quote_ident(ns) || '.' || quote_ident(rel);

  FOR col IN
    SELECT a.attname,
           format_type(a.atttypid, a.atttypmod) AS typ,
           a.attnotnull,
           pg_get_expr(ad.adbin, ad.adrelid) AS defexpr
    FROM pg_attribute a
    LEFT JOIN pg_attrdef ad
      ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
    WHERE a.attrelid = tbl
      AND a.attnum > 0
      AND NOT a.attisdropped
    ORDER BY a.attnum
  LOOP
    IF cols <> '' THEN
      cols := cols || E',\n  ';
    ELSE
      cols := '  ';
    END IF;

    cols := cols || quote_ident(col.attname) || ' ' || col.typ;
    IF col.defexpr IS NOT NULL THEN
      cols := cols || ' DEFAULT ' || col.defexpr;
    END IF;
    IF col.attnotnull THEN
      cols := cols || ' NOT NULL';
    END IF;
  END LOOP;

  create_statement := format('CREATE TABLE %s (%s', table_name, cols);

  FOR con IN
    SELECT conname, pg_get_constraintdef(oid, true) AS condef
    FROM pg_constraint
    WHERE conrelid = tbl
      AND contype IN ('p','u','c','f','x')
    ORDER BY contype, conname
  LOOP
    create_statement := create_statement
      || E',\n  CONSTRAINT '
      || quote_ident(con.conname)
      || ' '
      || con.condef;
  END LOOP;

  create_statement := create_statement || E'\n);';

  FOR idx IN
    SELECT pg_get_indexdef(i.indexrelid) AS idxdef
    FROM pg_index i
    WHERE i.indrelid = tbl
      AND NOT i.indisprimary
    ORDER BY i.indexrelid::regclass::text
  LOOP
    create_statement := create_statement || E'\n' || idx.idxdef || ';';
  END LOOP;

  RETURN NEXT;
END;
$$;

DROP FUNCTION IF EXISTS crdb_show_create_view(regclass);
CREATE FUNCTION crdb_show_create_view(vw regclass)
RETURNS TABLE(view_name text, create_statement text)
LANGUAGE plpgsql
AS $$
DECLARE
  ns text;
  rel text;
  def text;
BEGIN
  SELECT n.nspname, c.relname
    INTO ns, rel
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE c.oid = vw;

  view_name := quote_ident(ns) || '.' || quote_ident(rel);
  def := pg_get_viewdef(vw, true);
  create_statement := format('CREATE VIEW %s AS %s;', view_name, def);
  RETURN NEXT;
END;
$$;

-- Test 1: statement (line 3)
DROP VIEW IF EXISTS ";--alsoconcerning";
DROP TABLE IF EXISTS ";--dontask";
DROP TABLE IF EXISTS ";--notbetter" CASCADE;
DROP TABLE IF EXISTS "woo; DROP USER humpty;" CASCADE;

CREATE TABLE "woo; DROP USER humpty;" (x INT PRIMARY KEY);

CREATE TABLE ";--notbetter" (
  x INT,
  y INT,
  "welp; DROP USER dumpty;--" INT,
  CONSTRAINT "getmeoutofhere;--" PRIMARY KEY (x),
  CONSTRAINT "helpme; DROP USER alice;--" CHECK (x IS NOT NULL)
);
CREATE INDEX "id; DROP USER queenofhearts;--" ON ";--notbetter" (x);

-- onlyif config schema-locked-disabled

-- Test 2: query (line 21)
SELECT * FROM crdb_show_create_table('";--notbetter"'::regclass);

-- Test 3: query (line 44)
SELECT * FROM crdb_show_create_table('";--notbetter"'::regclass);

-- Test 4: statement (line 67)
CREATE VIEW ";--alsoconcerning" AS SELECT x AS a, y AS b FROM ";--notbetter";

-- Test 5: query (line 70)
SELECT * FROM crdb_show_create_view('";--alsoconcerning"'::regclass);

-- Test 6: statement (line 79)
CREATE TABLE ";--dontask" AS SELECT x AS a, y AS b FROM ";--notbetter";

-- onlyif config schema-locked-disabled

-- Test 7: query (line 83)
SELECT * FROM crdb_show_create_table('";--dontask"'::regclass);

-- Test 8: query (line 94)
SELECT * FROM crdb_show_create_table('";--dontask"'::regclass);

RESET client_min_messages;
