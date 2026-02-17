-- pg_cron named job upsert semantics
-- Scheduling with an existing name should update (not duplicate)

DROP EXTENSION IF EXISTS pg_cron;
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Create named job
SELECT cron.schedule('upsert_test', '* * * * *', 'SELECT 1');

-- Upsert: same name should update schedule and command, return same job_id
SELECT cron.schedule('upsert_test', '*/10 * * * *', 'SELECT 2');

-- Only one job with that name should exist
SELECT count(*) FROM cron.job WHERE jobname = 'upsert_test';

-- Schedule and command should reflect the upsert
SELECT schedule, command FROM cron.job WHERE jobname = 'upsert_test';

-- Cleanup
SELECT cron.unschedule('upsert_test');
DROP EXTENSION pg_cron;
