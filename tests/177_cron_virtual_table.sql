-- pg_cron virtual table tests
-- Tests: all columns from cron.job, cron.job_run_details schema, filtering, aggregation

DROP EXTENSION IF EXISTS pg_cron;
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Setup: create multiple jobs with varying schedules
SELECT cron.schedule('vtab_every_min', '* * * * *', 'SELECT 1');
SELECT cron.schedule('vtab_hourly', '0 * * * *', 'SELECT 2');
SELECT cron.schedule('vtab_daily', '0 0 * * *', 'VACUUM');

-- Disable one job
SELECT cron.alter_job(2, NULL, NULL, NULL, NULL, false);

-- Test: SELECT all columns from cron.job
SELECT jobid, schedule, command, nodename, nodeport, database, username, active, jobname
FROM cron.job ORDER BY jobid;

-- Test: filter active jobs
SELECT jobname FROM cron.job WHERE active = true ORDER BY jobname;

-- Test: filter inactive jobs
SELECT jobname FROM cron.job WHERE active = false;

-- Test: filter by schedule pattern
SELECT jobname, schedule FROM cron.job WHERE schedule = '* * * * *';

-- Test: filter by command
SELECT jobname FROM cron.job WHERE command = 'VACUUM';

-- Test: count all jobs
SELECT count(*) FROM cron.job;

-- Test: count active jobs
SELECT count(*) FROM cron.job WHERE active = true;

-- Test: cron.job_run_details columns exist (empty table)
SELECT jobid, runid, job_pid, database, username, command, status, return_message, start_time, end_time
FROM cron.job_run_details ORDER BY runid;

-- Test: count on empty run_details
SELECT count(*) FROM cron.job_run_details;

-- Test: filter cron.job by jobid
SELECT jobname FROM cron.job WHERE jobid = 1;

-- Test: filter cron.job by jobname
SELECT schedule, command FROM cron.job WHERE jobname = 'vtab_daily';

-- Cleanup
SELECT cron.unschedule('vtab_every_min');
SELECT cron.unschedule('vtab_hourly');
SELECT cron.unschedule('vtab_daily');
DROP EXTENSION pg_cron;
