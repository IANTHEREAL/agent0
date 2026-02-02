-- DROP FUNCTION should respect trigger dependencies (RESTRICT/CASCADE).

DROP TABLE IF EXISTS ft_drop_fn_deps.t;
DROP FUNCTION IF EXISTS ft_drop_fn_deps.ft_drop_fn_deps_set_updated_at();
DROP SCHEMA IF EXISTS ft_drop_fn_deps;

CREATE SCHEMA ft_drop_fn_deps;
SET search_path TO ft_drop_fn_deps, public;

CREATE TABLE t (id INT PRIMARY KEY, updated_at TIMESTAMP);

CREATE OR REPLACE FUNCTION ft_drop_fn_deps_set_updated_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.updated_at = NOW();
  RETURN NEW;
END;
$$;

CREATE TRIGGER ft_drop_fn_deps_trg BEFORE UPDATE ON t
FOR EACH ROW EXECUTE PROCEDURE ft_drop_fn_deps_set_updated_at();

-- RESTRICT default: should error, leaving trigger/function intact.
DROP FUNCTION ft_drop_fn_deps_set_updated_at();

SELECT 'TRIGGER_STILL=' || count(*) FROM pg_catalog.pg_trigger WHERE tgname = 'ft_drop_fn_deps_trg';
SELECT 'FUNC_STILL=' || count(*) FROM pg_catalog.pg_proc WHERE proname = 'ft_drop_fn_deps_set_updated_at';

-- CASCADE: should drop dependent trigger(s).
DROP FUNCTION ft_drop_fn_deps_set_updated_at() CASCADE;

SELECT 'TRIGGER_LEFT=' || count(*) FROM pg_catalog.pg_trigger WHERE tgname = 'ft_drop_fn_deps_trg';
SELECT 'FUNC_LEFT=' || count(*) FROM pg_catalog.pg_proc WHERE proname = 'ft_drop_fn_deps_set_updated_at';

DROP TABLE t;
DROP SCHEMA ft_drop_fn_deps;
