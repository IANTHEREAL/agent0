-- Worker-queue based cron scheduling
-- Tests that cron jobs are scheduled through the unified worker queue

-- Clean start
DROP EXTENSION IF EXISTS pg_cron;
CREATE EXTENSION pg_cron;

-- Schedule a job
SELECT cron.schedule('worker_test_job', '*/5 * * * *', 'SELECT 1');

-- Verify job is visible
SELECT jobname, schedule, command, active FROM cron.job WHERE jobname = 'worker_test_job';

-- Alter the job schedule
SELECT cron.alter_job(1, '*/10 * * * *');
SELECT jobname, schedule FROM cron.job WHERE jobid = 1;

-- Unschedule the job
SELECT cron.unschedule('worker_test_job');
SELECT count(*) FROM cron.job WHERE jobname = 'worker_test_job';

-- Cleanup
DROP EXTENSION pg_cron;
