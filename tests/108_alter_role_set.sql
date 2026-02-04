-- ALTER ROLE ... SET ...
-- ALTER ROLE ... IN DATABASE ... SET ...
-- Verify via pg_catalog.pg_db_role_setting

SET client_min_messages = warning;

-- Cleanup from previous runs (cluster-scoped objects)
DROP ROLE IF EXISTS ars_role;

CREATE ROLE ars_role;

ALTER ROLE ars_role SET statement_timeout = 1000;
ALTER ROLE ars_role IN DATABASE postgres SET statement_timeout = 2000;

SELECT
  r.rolname,
  CASE WHEN s.setdatabase = 0 THEN '<all>' ELSE d.datname END AS database,
  unnest(s.setconfig) AS setconfig
FROM pg_catalog.pg_db_role_setting AS s
JOIN pg_catalog.pg_roles AS r ON r.oid = s.setrole
LEFT JOIN pg_catalog.pg_database AS d ON d.oid = s.setdatabase
WHERE r.rolname = 'ars_role'
ORDER BY database, setconfig;

-- Cleanup
DROP ROLE IF EXISTS ars_role;
