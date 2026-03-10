-- Issue #1530: RESET with quoted identifiers must match PostgreSQL behavior.
-- Covers: RESET "is_superuser", RESET "ALL", RESET "session"."authorization",
--         and SQLSTATE / error message parity.

-- 1) RESET is_superuser (unquoted) → ERROR 55P02
RESET is_superuser;

-- 2) RESET "is_superuser" (quoted) → ERROR 55P02 (same as unquoted)
RESET "is_superuser";

-- 3) Readback: is_superuser unchanged after rejected resets
SELECT 'is_superuser=' || current_setting('is_superuser');

-- 4) RESET "ALL" (quoted) → ERROR 42704 (unrecognized parameter, not the keyword)
RESET "ALL";

-- 5) RESET ALL (unquoted keyword) → OK
RESET ALL;

-- 6) RESET "session"."authorization" → OK (resets custom GUC, not the pseudo-GUC)
RESET "session"."authorization";

-- 7) RESET session_authorization (unquoted) → OK (PG allows RESET for this)
RESET session_authorization;
