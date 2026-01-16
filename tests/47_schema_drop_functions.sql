-- DROP SCHEMA RESTRICT should consider stored functions (and table-drop should clean triggers).

DROP TABLE IF EXISTS ft_schema_drop.t;
DROP FUNCTION IF EXISTS ft_schema_drop.f();
DROP SCHEMA IF EXISTS ft_schema_drop;

CREATE SCHEMA ft_schema_drop;
SET search_path TO ft_schema_drop, public;

CREATE TABLE t (id INT PRIMARY KEY, updated_at TIMESTAMP);

CREATE OR REPLACE FUNCTION f()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.updated_at = NOW();
  RETURN NEW;
END;
$$;

CREATE TRIGGER trg BEFORE UPDATE ON t
FOR EACH ROW EXECUTE PROCEDURE f();

DROP TABLE t;
SELECT 'TRIGGER_LEFT=' || count(*) FROM pg_catalog.pg_trigger WHERE tgname = 'trg';

DROP SCHEMA ft_schema_drop;

DROP FUNCTION f();
DROP SCHEMA ft_schema_drop;
SELECT 'SCHEMA_LEFT=' || count(*) FROM information_schema.schemata WHERE schema_name = 'ft_schema_drop';

