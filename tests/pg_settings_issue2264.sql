-- Test: pg_settings virtual table (issue #2264)
-- pg_dump fails because pg_settings does not exist.
-- Verify the table exists, returns rows, and the exact pg_dump query works.

-- Basic: table exists and has rows
SELECT count(*) > 0 AS has_settings FROM pg_settings;

-- pg_dump's exact query: should return 0 rows (not an error)
SELECT set_config(name, 'view, foreign-table', false) FROM pg_settings WHERE name = 'restrict_nonsystem_relation_kind';

-- Known setting lookup
SELECT name, setting, context, vartype FROM pg_settings WHERE name = 'server_version';

-- Session-awareness: SET then verify
SET timezone = 'UTC';
SELECT setting FROM pg_settings WHERE name = 'timezone';

-- Timeout GUC: setting is numeric, unit is 'ms' (PostgreSQL parity)
SELECT name, setting, unit FROM pg_settings WHERE name = 'lock_timeout';
