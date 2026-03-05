-- PR #1471: COPY FROM fs9:// transaction cleanup after early-return errors.
--
-- The fix (ade9355f) wraps all fallible work after session.begin() in an async
-- block so that every `?` is caught by rollback_autocommit_or_mark_failed.
--
-- This test triggers the INSERT privilege check — a previously-unguarded `?`
-- error point that could leave an autocommit transaction open, poisoning the
-- session for subsequent statements.

SET client_min_messages = warning;

-- Setup: create a target table and an unprivileged role (no INSERT grant).
DROP TABLE IF EXISTS t_copy_fs9_priv_1471;
DROP ROLE IF EXISTS r_copy_fs9_noperm_1471;

CREATE TABLE t_copy_fs9_priv_1471 (id INT);
CREATE ROLE r_copy_fs9_noperm_1471 LOGIN PASSWORD 'test';

-- Grant SELECT only — not INSERT — so the privilege check inside the COPY
-- handler's async block fails with SQLSTATE 42501.
GRANT SELECT ON t_copy_fs9_priv_1471 TO r_copy_fs9_noperm_1471;

SET ROLE r_copy_fs9_noperm_1471;

-- ============================================================
-- Test 1: Autocommit — session stays usable after privilege error
-- ============================================================

-- COPY triggers: session.begin() → INSERT privilege check → 42501 error.
-- Before the fix, the `?` on require_table_privilege was unguarded and
-- the autocommit transaction would leak.
COPY t_copy_fs9_priv_1471 FROM 'fs9://does_not_matter.csv';

-- If the autocommit transaction was properly cleaned up, SAVEPOINT errors
-- because we are NOT inside a transaction block.
-- A leaked transaction would let SAVEPOINT silently succeed (false green).
SAVEPOINT s1;

-- ============================================================
-- Test 2: Explicit txn — txn becomes aborted, ROLLBACK recovers
-- ============================================================

BEGIN;

COPY t_copy_fs9_priv_1471 FROM 'fs9://does_not_matter.csv';

-- The transaction should now be marked failed.
SELECT 1 AS should_fail;

ROLLBACK;

-- After ROLLBACK the session is usable again.
SELECT 2 AS after_rollback;

RESET ROLE;

-- Cleanup
DROP TABLE t_copy_fs9_priv_1471;
DROP ROLE r_copy_fs9_noperm_1471;
