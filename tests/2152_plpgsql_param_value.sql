-- Issue #2152: PL/pgSQL parameters should work as VALUES operands in embedded SQL.

DROP FUNCTION IF EXISTS param_value_breaks(text);
DROP TABLE IF EXISTS t_params;

CREATE TABLE t_params(id text);

CREATE OR REPLACE FUNCTION param_value_breaks(project_id text)
RETURNS text
LANGUAGE plpgsql
AS $$
BEGIN
  INSERT INTO t_params(id) VALUES (project_id);
  RETURN project_id;
END;
$$;

SELECT param_value_breaks('demo-swarm') AS inserted_value;
SELECT id FROM t_params;

DROP FUNCTION param_value_breaks(text);
DROP TABLE t_params;
