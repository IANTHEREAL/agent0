-- Ensure _db9_sys_export_ddl exports trigger functions and keeps function before trigger.

DROP TRIGGER IF EXISTS t_ddl_export_1502_bi ON t_ddl_export_1502;
DROP FUNCTION IF EXISTS ddl_export_1502_fn();
DROP TABLE IF EXISTS t_ddl_export_1502;

CREATE TABLE t_ddl_export_1502 (id INT PRIMARY KEY);

CREATE OR REPLACE FUNCTION ddl_export_1502_fn()
RETURNS TRIGGER
AS $$
BEGIN
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER t_ddl_export_1502_bi
BEFORE INSERT ON t_ddl_export_1502
FOR EACH ROW
EXECUTE FUNCTION ddl_export_1502_fn();

SELECT object_type, object_name
FROM _db9_sys_export_ddl()
WHERE (object_type, object_name) IN (
    ('function', 'public.ddl_export_1502_fn'),
    ('trigger', 'public.t_ddl_export_1502.t_ddl_export_1502_bi')
)
ORDER BY ddl_order;

SELECT (
  (SELECT ddl_order FROM _db9_sys_export_ddl()
   WHERE object_type = 'function' AND object_name = 'public.ddl_export_1502_fn')
  <
  (SELECT ddl_order FROM _db9_sys_export_ddl()
   WHERE object_type = 'trigger' AND object_name = 'public.t_ddl_export_1502.t_ddl_export_1502_bi')
) AS function_before_trigger;

SELECT ddl_sql
FROM _db9_sys_export_ddl()
WHERE object_type = 'function' AND object_name = 'public.ddl_export_1502_fn';

DROP TABLE t_ddl_export_1502;
DROP FUNCTION ddl_export_1502_fn();
