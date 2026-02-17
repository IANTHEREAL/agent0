-- pg_cron boundary conditions and edge cases

DROP EXTENSION IF EXISTS pg_cron;
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Edge: command with single quotes (escaped)
SELECT cron.schedule('quotes_job', '0 0 * * *', 'SELECT ''hello world''');
SELECT command FROM cron.job WHERE jobname = 'quotes_job';

-- Edge: command with semicolons
SELECT cron.schedule('multi_stmt', '0 1 * * *', 'SELECT 1; SELECT 2');
SELECT command FROM cron.job WHERE jobname = 'multi_stmt';

-- Edge: long command string
SELECT cron.schedule('long_cmd', '0 2 * * *', 'INSERT INTO audit_log (action, details, created_at) VALUES (''scheduled_task'', ''This is a very long command string that tests the boundary of what can be stored'', NOW())');
SELECT length(command) > 50 as is_long FROM cron.job WHERE jobname = 'long_cmd';

-- Edge: alter_job with only job_id (1 arg, no changes = no-op)
SELECT cron.alter_job(1);
SELECT schedule, command, active FROM cron.job WHERE jobid = 1;

-- Edge: many jobs (10 jobs)
SELECT cron.schedule('batch_1', '1 * * * *', 'SELECT 1');
SELECT cron.schedule('batch_2', '2 * * * *', 'SELECT 2');
SELECT cron.schedule('batch_3', '3 * * * *', 'SELECT 3');
SELECT cron.schedule('batch_4', '4 * * * *', 'SELECT 4');
SELECT cron.schedule('batch_5', '5 * * * *', 'SELECT 5');
SELECT cron.schedule('batch_6', '6 * * * *', 'SELECT 6');
SELECT cron.schedule('batch_7', '7 * * * *', 'SELECT 7');
SELECT count(*) FROM cron.job;

-- Edge: unschedule returns false for already-deleted job
SELECT cron.unschedule('batch_1');
SELECT cron.unschedule('batch_1');

-- Edge: unschedule by id returns false for non-existent
SELECT cron.unschedule(99999);

-- Edge: schedule after unschedule reuses name but gets new id
SELECT cron.unschedule('batch_2');
SELECT cron.schedule('batch_2', '*/20 * * * *', 'SELECT 200');
SELECT schedule, command FROM cron.job WHERE jobname = 'batch_2';

-- Cleanup
SELECT cron.unschedule('quotes_job');
SELECT cron.unschedule('multi_stmt');
SELECT cron.unschedule('long_cmd');
SELECT cron.unschedule('batch_2');
SELECT cron.unschedule('batch_3');
SELECT cron.unschedule('batch_4');
SELECT cron.unschedule('batch_5');
SELECT cron.unschedule('batch_6');
SELECT cron.unschedule('batch_7');
DROP EXTENSION pg_cron;
