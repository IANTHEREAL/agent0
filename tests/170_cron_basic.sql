-- pg_cron basic CRUD tests
-- Tests: CREATE/DROP EXTENSION, schedule, unschedule, alter_job, cron.job, cron.job_run_details

-- Clean start
DROP EXTENSION IF EXISTS pg_cron;

-- Test: CREATE EXTENSION
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Test: cron.schedule() anonymous job (returns job_id)
SELECT cron.schedule('* * * * *', 'SELECT 1');

-- Test: cron.schedule() named job (returns job_id)
SELECT cron.schedule('test_job', '*/5 * * * *', 'SELECT 1');

-- Test: SELECT from cron.job (virtual table)
SELECT jobid, schedule, command, active, jobname FROM cron.job ORDER BY jobid;

-- Test: cron.alter_job() — disable job 1
SELECT cron.alter_job(1, NULL, NULL, NULL, NULL, false);
SELECT active FROM cron.job WHERE jobid = 1;

-- Test: cron.alter_job() — re-enable job 1
SELECT cron.alter_job(1, NULL, NULL, NULL, NULL, true);
SELECT active FROM cron.job WHERE jobid = 1;

-- Test: cron.unschedule() by id
SELECT cron.unschedule(1);
SELECT count(*) FROM cron.job WHERE jobid = 1;

-- Test: cron.unschedule() by name
SELECT cron.unschedule('test_job');
SELECT count(*) FROM cron.job WHERE jobname = 'test_job';

-- Test: cron.job_run_details (empty — no executions yet)
SELECT count(*) FROM cron.job_run_details;

-- Cleanup
DROP EXTENSION pg_cron;
