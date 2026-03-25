-- session_replication_role must be explicitly rejected (#1535).
-- db9 does not implement PostgreSQL's trigger-suppression semantics for this
-- GUC, so accepting it through the generic SET path is dangerous.

-- 1) SET to 'origin' must succeed (it is the only accepted value)
SET session_replication_role = origin;

-- 2) SHOW must return 'origin'
SHOW session_replication_role;

-- 3) SET to 'replica' must fail
SET session_replication_role = replica;

-- 4) SET LOCAL must also fail for non-origin values
BEGIN;
SET LOCAL session_replication_role = 'replica';
COMMIT;

-- 5) set_config() must also fail for non-origin values
SELECT set_config('session_replication_role', 'replica', false);

-- 6) RESET is a no-op (harmless), should not error
RESET session_replication_role;
