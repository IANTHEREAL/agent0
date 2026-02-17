-- pg_cron alter_job comprehensive tests
-- Tests: schedule change, command change, active toggle, multi-param, errors

DROP EXTENSION IF EXISTS pg_cron;
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Setup: create a job to alter
SELECT cron.schedule('alter_test', '* * * * *', 'SELECT 1');

-- Verify initial state
SELECT schedule, command, active FROM cron.job WHERE jobname = 'alter_test';

-- Test: change schedule only
SELECT cron.alter_job(1, '*/15 * * * *', NULL, NULL, NULL, NULL);
SELECT schedule, command FROM cron.job WHERE jobname = 'alter_test';

-- Test: change command only
SELECT cron.alter_job(1, NULL, 'SELECT 2', NULL, NULL, NULL);
SELECT schedule, command FROM cron.job WHERE jobname = 'alter_test';

-- Test: change schedule and command together
SELECT cron.alter_job(1, '0 3 * * *', 'VACUUM', NULL, NULL, NULL);
SELECT schedule, command FROM cron.job WHERE jobname = 'alter_test';

-- Test: disable then re-enable
SELECT cron.alter_job(1, NULL, NULL, NULL, NULL, false);
SELECT active FROM cron.job WHERE jobname = 'alter_test';

SELECT cron.alter_job(1, NULL, NULL, NULL, NULL, true);
SELECT active FROM cron.job WHERE jobname = 'alter_test';

-- Test: change schedule + toggle active in one call
SELECT cron.alter_job(1, '30 2 * * 1', NULL, NULL, NULL, false);
SELECT schedule, active FROM cron.job WHERE jobname = 'alter_test';

-- Test: alter_job on non-existent job
SELECT cron.alter_job(9999, NULL, NULL, NULL, NULL, false);

-- Test: alter_job with invalid schedule
SELECT cron.alter_job(1, 'not a cron expr', NULL, NULL, NULL, NULL);

-- Test: alter_job with cross-database (not supported)
SELECT cron.alter_job(1, NULL, NULL, 'other_db', NULL, NULL);

-- Cleanup
SELECT cron.unschedule('alter_test');
DROP EXTENSION pg_cron;
