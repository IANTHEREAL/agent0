-- Regression #1559: non-fast-path set_config() must set GUC and return applied value.

SELECT current_setting('statement_timeout') AS baseline_timeout \gset

-- Non-fast path: FROM clause prevents tableless set_config fast-path.
SELECT set_config('statement_timeout', v, false) AS set_result
FROM (VALUES ('450')) AS t(v);

SHOW statement_timeout;
SELECT current_setting('statement_timeout') AS current_timeout;

-- Same simple-query batch: SHOW must observe prior set_config mutation immediately.
RESET statement_timeout;
SELECT current_setting('statement_timeout') AS current_timeout_after_reset;
SELECT set_config('statement_timeout', '450', false);
SHOW statement_timeout;

-- LOCAL in non-fast-path outside transaction must not leak to later statements.
RESET statement_timeout;
SELECT current_setting('statement_timeout') AS baseline_local_outside \gset
SELECT set_config('statement_timeout', v, true) AS local_set_result
FROM (VALUES ('550')) AS t(v);
SELECT current_setting('statement_timeout') AS current_timeout_after_local_outside;
SELECT current_setting('statement_timeout') = :'baseline_local_outside' AS local_outside_not_leaked;

-- Explicit empty search_path through non-fast-path must stay empty (not rewritten to public).
SELECT set_config('search_path', v, false) AS set_empty_search_path
FROM (VALUES ('')) AS t(v);
SHOW search_path;
SELECT current_setting('search_path') = '' AS search_path_is_empty;

-- Invalid value through non-fast-path should surface proper GUC validation error.
SELECT set_config('statement_timeout', v, false)
FROM (VALUES ('not_a_timeout')) AS t(v);

-- Reserved pseudo-GUC writes must also be blocked through non-fast-path.
SELECT set_config('is_superuser', 'off', false)
FROM (VALUES (1)) AS t(x);

RESET statement_timeout;
SHOW statement_timeout \gset
SELECT :'statement_timeout' = :'baseline_timeout' AS restored_timeout;
