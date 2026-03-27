-- Issue #2152: SQL keywords inside PL/pgSQL body must not confuse outer block matching.

DROP FUNCTION IF EXISTS ddl_in_plpgsql_breaks();
DROP TABLE IF EXISTS t_ddl_breaks;

CREATE OR REPLACE FUNCTION ddl_in_plpgsql_breaks()
RETURNS text
LANGUAGE plpgsql
AS $$
BEGIN
  CREATE TABLE IF NOT EXISTS t_ddl_breaks(id int);
  RETURN 'ok';
END;
$$;

SELECT ddl_in_plpgsql_breaks() AS call_result;
INSERT INTO t_ddl_breaks VALUES (1);
SELECT count(*) AS row_count FROM t_ddl_breaks;

DROP FUNCTION ddl_in_plpgsql_breaks();
DROP TABLE t_ddl_breaks;
