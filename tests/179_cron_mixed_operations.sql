-- pg_cron mixed anonymous/named jobs and realistic workflows

DROP EXTENSION IF EXISTS pg_cron;
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Mix: anonymous job (no name)
SELECT cron.schedule('*/5 * * * *', 'SELECT 1');

-- Mix: named job
SELECT cron.schedule('cleanup', '0 3 * * *', 'DELETE FROM logs WHERE ts < now() - interval ''30 days''');

-- Mix: another anonymous
SELECT cron.schedule('0 * * * *', 'VACUUM');

-- Verify: 3 jobs total (2 anonymous with NULL jobname, 1 named)
SELECT count(*) FROM cron.job;
SELECT count(*) FROM cron.job WHERE jobname IS NULL;
SELECT count(*) FROM cron.job WHERE jobname IS NOT NULL;

-- Workflow: disable named job, verify, re-enable
SELECT cron.alter_job(2, NULL, NULL, NULL, NULL, false);
SELECT jobname, active FROM cron.job WHERE jobid = 2;

SELECT cron.alter_job(2, NULL, NULL, NULL, NULL, true);
SELECT jobname, active FROM cron.job WHERE jobid = 2;

-- Workflow: unschedule anonymous by id
SELECT cron.unschedule(1);
SELECT count(*) FROM cron.job;

-- Workflow: unschedule named by name
SELECT cron.unschedule('cleanup');
SELECT count(*) FROM cron.job;

-- Workflow: unschedule last anonymous by id
SELECT cron.unschedule(3);
SELECT count(*) FROM cron.job;

-- Re-schedule after full cleanup: IDs should continue incrementing
SELECT cron.schedule('new_job', '*/30 * * * *', 'SELECT 42');
SELECT jobid, jobname FROM cron.job;

-- Unschedule the re-scheduled job, then schedule again
SELECT cron.unschedule('new_job');
SELECT cron.schedule('new_job', '0 6 * * *', 'SELECT 99');
SELECT jobid, schedule, command FROM cron.job WHERE jobname = 'new_job';

-- Cleanup
SELECT cron.unschedule('new_job');
DROP EXTENSION pg_cron;
