-- pg_cron error handling tests
-- Tests: no extension, NULL args, wrong types, arg count, non-existent jobs

-- Test: schedule without extension installed
DROP EXTENSION IF EXISTS pg_cron;
SELECT cron.schedule('* * * * *', 'SELECT 1');

-- Test: unschedule without extension installed
SELECT cron.unschedule(1);

-- Test: alter_job without extension installed
SELECT cron.alter_job(1, NULL, NULL, NULL, NULL, false);

-- Install extension for remaining tests
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Test: unschedule non-existent job by id (returns false)
SELECT cron.unschedule(9999);

-- Test: unschedule non-existent job by name (returns false)
SELECT cron.unschedule('no_such_job');

-- Test: schedule_in_database not supported
SELECT cron.schedule_in_database('job1', '* * * * *', 'SELECT 1', 'mydb');

-- Test: schedule with wrong number of args (0 args - too few)
SELECT cron.schedule();

-- Test: schedule with wrong number of args (4 args - too many)
SELECT cron.schedule('name', '* * * * *', 'SELECT 1', 'extra');

-- Test: unschedule with wrong number of args (0 args)
SELECT cron.unschedule();

-- Test: unschedule with wrong number of args (2 args)
SELECT cron.unschedule(1, 2);

-- Test: alter_job with no args
SELECT cron.alter_job();

-- Cleanup
DROP EXTENSION pg_cron;
