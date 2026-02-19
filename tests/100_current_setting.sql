-- current_setting() + set_config() for session GUCs (MVP)

-- Defaults
SELECT current_setting('search_path') AS search_path;
SELECT current_setting('statement_timeout') AS statement_timeout;
SELECT current_setting('unknown.setting', true) AS missing_ok;

-- set_config updates and returns the new value (PostgreSQL semantics)
SELECT set_config('statement_timeout', '200', false) AS prev_timeout;
SELECT current_setting('statement_timeout') AS statement_timeout;

SELECT pg_catalog.set_config('search_path', 'public, ss_s1', false) AS prev_sp;
SELECT current_setting('search_path') AS search_path;
