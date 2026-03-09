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

-- Same-statement visibility: set_config(local) outside txn visible to
-- current_setting() in the same SELECT (PG parity: both return new value).
RESET statement_timeout;
SELECT set_config('statement_timeout', v, true) AS local_set,
       current_setting('statement_timeout') AS same_stmt_visible
FROM (VALUES ('550')) AS t(v);
SELECT current_setting('statement_timeout') AS next_stmt_reset;

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

-- ── Blocker: NULL value → RESET, returns boot-default not prior value (PG parity) ──
RESET statement_timeout;
SELECT set_config('statement_timeout', v, false) IS NULL AS null_value_is_null
FROM (VALUES (NULL::text)) t(v);

-- NULL value after prior SET must return boot default '0', not '450ms'.
SET statement_timeout = 450;
SELECT set_config('statement_timeout', v, false) AS null_resets_to_default
FROM (VALUES (NULL::text)) t(v);
SELECT current_setting('statement_timeout') AS after_null_reset;

-- ── Blocker: NULL is_local → treat as false, not NULL return (PG parity) ──
RESET statement_timeout;
SELECT set_config('statement_timeout', '100', b) IS NULL AS null_is_local_is_null
FROM (VALUES (NULL::boolean)) t(b);
RESET statement_timeout;

-- ── Blocker: NULL name → error, not NULL return (PG parity) ──
SELECT set_config(n, '100', false) FROM (VALUES (NULL::text)) t(n);

-- ── Blocker: text literal 'default' for search_path stays literal (PG parity) ──
SELECT set_config('search_path', v, false) AS set_default_literal
FROM (VALUES ('default')) t(v);

-- ── Blocker: reserved pseudo-GUC + NULL value must still be rejected ──
SELECT set_config('is_superuser', v, false)
FROM (VALUES (NULL::text)) t(v);

-- ── Blocker: transaction-local NULL reset must not leak past COMMIT ──
RESET statement_timeout;
SET statement_timeout = 750;
BEGIN;
SELECT set_config('statement_timeout', v, true) AS local_null_reset
FROM (VALUES (NULL::text)) t(v);
SELECT current_setting('statement_timeout') AS inside_tx_after_local_null_reset;
COMMIT;
SELECT current_setting('statement_timeout') AS after_commit_must_be_750;

-- ── Blocker R3: set_config('session_authorization', NULL, false) must return current role ──
SELECT set_config('session_authorization', v, false) AS sa_null_reset,
       current_setting('session_authorization') AS sa_same_stmt
FROM (VALUES (NULL::text)) t(v);

-- ── set_config('session_authorization', NULL, true) same behavior ──
SELECT set_config('session_authorization', v, true) AS sa_null_local,
       current_setting('session_authorization') AS sa_same_stmt_local
FROM (VALUES (NULL::text)) t(v);

-- ── Blocker R4: after SET ROLE, set_config('session_authorization', NULL, false)
-- must use session-auth (login role) semantics, not current-role semantics (PG parity) ──
CREATE ROLE __sa_r4_role;
SET ROLE __sa_r4_role;
SELECT set_config('session_authorization', v, false) AS sa_after_set_role,
       current_setting('session_authorization') AS sa_same_stmt_after_role
FROM (VALUES (NULL::text)) t(v);
RESET ROLE;
DROP ROLE __sa_r4_role;

-- ── Blocker R5: custom dotted GUC NULL-reset must persist as empty string (PG parity) ──
SELECT set_config('custom.test_ns', 'abc', false) FROM (VALUES (1)) t(x);
SELECT set_config('custom.test_ns', v, false) AS custom_null_reset
FROM (VALUES (NULL::text)) t(v);
SELECT current_setting('custom.test_ns', true) IS NULL AS custom_null_is_null;
SELECT current_setting('custom.test_ns', true) = '' AS custom_null_is_empty;

-- ── Blocker R7: internal _reset_default.* keys must NOT be visible via current_setting ──
-- Non-user-set keys: missing_ok=true → NULL
SELECT current_setting('_reset_default.statement_timeout', true) IS NULL AS reset_key_hidden_missing_ok;
-- Non-user-set keys: missing_ok omitted → error "unrecognized configuration parameter"
SELECT current_setting('_reset_default.statement_timeout') AS reset_key_must_error;

-- ── Blocker R8: user-set _reset_default.* keys MUST be visible (PG custom GUC parity) ──
-- In PostgreSQL, _reset_default.* is not a reserved namespace; user-set custom
-- GUCs with this prefix are visible via current_setting like any other custom GUC.
SELECT set_config('_reset_default.my_custom', 'user_value', false) FROM (VALUES (1)) t(x);
SELECT current_setting('_reset_default.my_custom') AS user_set_reset_key;

RESET search_path;
RESET statement_timeout;
SHOW statement_timeout \gset
SELECT :'statement_timeout' = :'baseline_timeout' AS restored_timeout;
