-- Regression: statement_timeout should not hard-code a default value.
-- Validate:
-- 1) default is non-empty and parseable
-- 2) set_config() takes effect deterministically
-- 3) timeout fires on pg_sleep
-- 4) RESET restores the baseline value (no pollution)

SELECT current_setting('statement_timeout') AS baseline_timeout \gset

SELECT :'baseline_timeout' <> '' AS default_timeout_nonempty;
SELECT :'baseline_timeout' ~ '^[0-9]+(us|ms|s|min|h|d)?$' AS default_timeout_parseable;

SELECT set_config('statement_timeout', '200', false) AS set_timeout_result;
SHOW statement_timeout;
SELECT pg_sleep(1);

RESET statement_timeout;
SHOW statement_timeout \gset
SELECT :'statement_timeout' = :'baseline_timeout' AS restored_timeout;
