-- RESET "ALL" (quoted) must error as unknown parameter, not reset all settings.
-- RESET unknown_param must also error. PostgreSQL parity (#1662).

-- 1) Set a custom value so we can verify RESET ALL (unquoted) works
SET statement_timeout = '5000';
SHOW statement_timeout;

-- 2) RESET ALL (unquoted keyword) resets everything
RESET ALL;
SHOW statement_timeout;

-- 3) RESET "ALL" (quoted) → error: unrecognized configuration parameter "ALL"
RESET "ALL";

-- 4) RESET with completely unknown parameter → same error
RESET definitely_missing_guc_1662;

-- 4b) Mixed-case quoted unknown param preserves original case (#1662 QG P1)
RESET "FooBar_NoSuchGuc";

-- 5) RESET known GUC with quotes still works (quoting preserves case but GUC lookup is case-insensitive)
SET statement_timeout = '7000';
RESET "statement_timeout";
SHOW statement_timeout;
