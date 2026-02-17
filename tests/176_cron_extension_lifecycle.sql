-- pg_cron extension lifecycle tests
-- Tests: CREATE/DROP idempotency, jobs cleared on DROP, re-create after DROP

DROP EXTENSION IF EXISTS pg_cron;

-- Test: CREATE EXTENSION
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Test: CREATE EXTENSION IF NOT EXISTS (idempotent, no error)
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Test: schedule jobs, then DROP should clear them
SELECT cron.schedule('lifecycle_a', '* * * * *', 'SELECT 1');
SELECT cron.schedule('lifecycle_b', '*/5 * * * *', 'SELECT 2');
SELECT count(*) FROM cron.job;

DROP EXTENSION pg_cron;

-- Test: DROP IF EXISTS when not installed (no error)
DROP EXTENSION IF EXISTS pg_cron;

-- Test: re-CREATE after DROP, tables should be empty
CREATE EXTENSION IF NOT EXISTS pg_cron;
SELECT count(*) FROM cron.job;
SELECT count(*) FROM cron.job_run_details;

-- Test: scheduling works after re-create
SELECT cron.schedule('after_recreate', '0 * * * *', 'SELECT 42');
SELECT count(*) FROM cron.job;

-- Cleanup
SELECT cron.unschedule('after_recreate');
DROP EXTENSION pg_cron;
