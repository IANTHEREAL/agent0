-- current_setting() + set_config() for session GUCs (MVP)

-- Defaults
SELECT current_setting('search_path') AS search_path;
SELECT current_setting('unknown.setting', true) AS missing_ok;

-- set_config updates and returns the new value (PostgreSQL semantics)
SELECT set_config('statement_timeout', '200', false) AS prev_timeout;
SELECT current_setting('statement_timeout') AS statement_timeout;

SELECT pg_catalog.set_config('search_path', 'public, ss_s1', false) AS prev_sp;
SELECT current_setting('search_path') AS search_path;

-- === Expression contexts (PG-validated) ===
SELECT current_setting('timezone') <> '' AS tz_notempty;
SELECT length(current_setting('search_path')) > 0 AS sp_len;
SELECT CASE WHEN current_setting('standard_conforming_strings') = 'on'
       THEN true ELSE false END AS scs_check;

-- missing_ok in expression context
SELECT current_setting('nonexistent.param', true) IS NULL AS unknown_null;

-- missing_ok as text literal (PG coerces unknown → boolean)
SELECT current_setting('nonexistent.param', 'true') IS NULL AS text_missing_ok;

-- NULL propagation (PG strict function semantics)
SELECT current_setting(NULL::text) IS NULL AS null_name;
SELECT current_setting('timezone', NULL::boolean) IS NULL AS null_missing_ok;

-- Prepared statement: reflects SET changes between executions
PREPARE cs_test AS SELECT current_setting('timezone');
SET timezone = 'Asia/Shanghai';
EXECUTE cs_test;
SET timezone = 'UTC';
EXECUTE cs_test;
DEALLOCATE cs_test;
RESET timezone;
