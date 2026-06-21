-- pg_cron basic CRUD tests
-- Tests: CREATE/DROP EXTENSION, schedule, unschedule, alter_job, cron.job, cron.job_run_details

-- Clean start
DROP EXTENSION IF EXISTS pg_cron;

-- Test: CREATE EXTENSION
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Test: cron.schedule() anonymous job (returns job_id)
SELECT cron.schedule('* * * * *', 'SELECT 1') AS anon_job_id
\gset
SELECT :anon_job_id > 0 AS scheduled;

-- Test: cron.schedule() named job (returns job_id)
SELECT cron.schedule('test_job', '*/5 * * * *', 'SELECT 1') AS named_job_id
\gset
SELECT :named_job_id > 0 AND :named_job_id <> :anon_job_id AS scheduled;

-- Test: SELECT from cron.job (virtual table)
SELECT c.jobid = :anon_job_id AS is_anon, c.schedule, c.command, c.active, c.jobname
FROM cron.job c
WHERE c.jobid IN (:anon_job_id, :named_job_id)
ORDER BY c.jobid;

-- Test: cron.alter_job() - disable anonymous job
SELECT cron.alter_job(:anon_job_id, NULL, NULL, NULL, NULL, false);
SELECT active FROM cron.job WHERE jobid = :anon_job_id;

-- Test: cron.alter_job() - re-enable anonymous job
SELECT cron.alter_job(:anon_job_id, NULL, NULL, NULL, NULL, true);
SELECT active FROM cron.job WHERE jobid = :anon_job_id;

-- Test: cron.unschedule() by id
SELECT cron.unschedule(:anon_job_id);
SELECT count(*) FROM cron.job WHERE jobid = :anon_job_id;

-- Test: cron.unschedule() by name
SELECT cron.unschedule('test_job');
SELECT count(*) FROM cron.job WHERE jobname = 'test_job';

-- Test: cron.job_run_details (empty — no executions yet)
SELECT count(*) FROM cron.job_run_details;

-- Cleanup
DROP EXTENSION pg_cron;
