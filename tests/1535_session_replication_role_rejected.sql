-- session_replication_role must be explicitly rejected (#1535).
-- db9 does not implement PostgreSQL's trigger-suppression semantics for this
-- GUC, so accepting it through the generic SET path is dangerous.

-- 1) SET must fail
SET session_replication_role = replica;

-- 2) SET LOCAL must also fail
BEGIN;
SET LOCAL session_replication_role = 'replica';
COMMIT;

-- 3) set_config() must also fail
SELECT set_config('session_replication_role', 'replica', false);

-- 4) SHOW must return an error (not registered as a known GUC)
SHOW session_replication_role;

-- 5) RESET is a no-op (harmless), should not error
RESET session_replication_role;
