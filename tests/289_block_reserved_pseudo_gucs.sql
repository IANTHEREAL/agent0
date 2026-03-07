-- #1530: reserved pseudo-GUC compatibility.
-- is_superuser remains read-only, while session_authorization allows DEFAULT/RESET.

SET is_superuser = on;
SET is_superuser = DEFAULT;
SET LOCAL is_superuser = on;
SET LOCAL is_superuser = DEFAULT;
SELECT set_config('is_superuser', 'on', false);
SELECT set_config('is_superuser', 'off', true);
RESET is_superuser;

-- db9 known divergence: PG 17.x returns 'role "nobody" does not exist' (role resolved first).
-- db9 returns 'parameter "session_authorization" cannot be changed' (GUC guard fires before role lookup).
-- Tracked in: https://github.com/c4pt0r/db9-server/issues/1595
-- DIVERGENCE: session_authorization write error message differs from PG 17.x
SET session_authorization = 'nobody';
SET session_authorization = DEFAULT;
RESET session_authorization;

-- Dotted name is a regular custom GUC namespace and must remain writable.
-- PostgreSQL also allows current_setting('session.authorization') after SET.
SET "session.authorization" = 'ok';
SELECT current_setting('session.authorization') AS dotted_value;
RESET "session.authorization";

-- Underscore pseudo-GUC readback stays available.
SELECT current_setting('session_authorization') AS pseudo_value;

-- RESET ALL must not error on reserved pseudo-GUCs.
RESET ALL;
