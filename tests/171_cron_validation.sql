-- pg_cron expression validation tests
-- Verifies valid/invalid cron schedule expressions

DROP EXTENSION IF EXISTS pg_cron;
CREATE EXTENSION IF NOT EXISTS pg_cron;

-- Valid: every minute
SELECT cron.schedule('valid_1', '* * * * *', 'SELECT 1');

-- Valid: every 5 minutes
SELECT cron.schedule('valid_2', '*/5 * * * *', 'SELECT 1');

-- Valid: 3 AM on weekdays
SELECT cron.schedule('valid_3', '0 3 * * 1-5', 'SELECT 1');

-- Valid: :00 and :30 during business hours
SELECT cron.schedule('valid_4', '0,30 9-17 * * *', 'SELECT 1');

-- All 4 valid jobs should exist
SELECT count(*) FROM cron.job;

-- Invalid: 6-field expression (seconds not supported)
SELECT cron.schedule('* * * * * *', 'SELECT 1');

-- Invalid: empty string
SELECT cron.schedule('', 'SELECT 1');

-- Cleanup
SELECT cron.unschedule('valid_1');
SELECT cron.unschedule('valid_2');
SELECT cron.unschedule('valid_3');
SELECT cron.unschedule('valid_4');
DROP EXTENSION pg_cron;
