-- Issue #1623: reserved pseudo-GUC blocking with positive-path coverage.
-- Verifies that is_superuser and session_authorization are properly
-- write-protected, while readback and non-reserved GUCs remain functional.

-- ── Negative: all SET/RESET forms on is_superuser must error ──
SET is_superuser = 'on';
SET is_superuser TO 'off';
SET is_superuser TO DEFAULT;
RESET is_superuser;

-- ── Negative: SET session_authorization to nonexistent role ──
SET session_authorization = 'nonexistent_role_289';

-- ── Positive: current_setting() readback on reserved pseudo-GUCs ──
SELECT current_setting('is_superuser') IS NOT NULL AS is_superuser_readable;
SELECT current_setting('session_authorization') IS NOT NULL AS session_auth_readable;

-- ── Positive: SET session_authorization TO DEFAULT is allowed ──
SET session_authorization TO DEFAULT;
SELECT current_setting('session_authorization') IS NOT NULL AS session_auth_after_reset;

-- ── Positive: non-reserved GUC SET/RESET unaffected by pseudo-GUC blocking ──
SET statement_timeout = 500;
SELECT 'timeout_set=' || current_setting('statement_timeout') AS timeout_verify;
RESET statement_timeout;
