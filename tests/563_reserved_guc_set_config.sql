-- Issue #1565: set_config() must reject writes to reserved pseudo-GUCs
-- (is_superuser), and enforce permission semantics for session_authorization.

-- 1. is_superuser: always immutable
SELECT set_config('is_superuser', 'on', false);

-- 2. session_authorization: same-user set succeeds (PG parity)
SELECT set_config('session_authorization', current_user, false);

-- 3. session_authorization: different-user set returns permission error
SELECT set_config('session_authorization', 'evil_user', false);

-- 4. SET LOCAL is_superuser must also error
SELECT set_config('is_superuser', 'on', true);

-- 5. Readback: is_superuser must still reflect the server-assigned value
SELECT 'is_superuser=' || current_setting('is_superuser');
